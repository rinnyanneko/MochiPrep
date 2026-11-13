import { createHash } from 'node:crypto';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { buildQuote, generateTape, OkxMarketData, parseOkxCandles, ReplayMarketData } from '../src/index';

afterEach(() => vi.useRealTimers());

describe('deterministic replay', () => {
  it('keeps the seeded tape stable', () => {
    const tape = generateTape('BTCUSDT', 64);
    // Hashed as candle objects: the hash predates the column store and must not change.
    const candles = Array.from({ length: tape.length }, (_, i) => tape.candle(i));
    const hash = createHash('sha256').update(JSON.stringify(candles)).digest('hex');
    expect(hash).toBe('ed818a88167cf6b92d82163c069ab259dd0b5259da8c1409343e8f5915977e8c');
  });

  it('advances only when stepped and produces repeatable quotes', () => {
    const a = new ReplayMarketData(5_800);
    const b = new ReplayMarketData(5_800);
    expect(a.now()).toBe(b.now());
    expect(a.step()).toEqual(b.step());
    expect(a.position).toBe(5_761);
  });
});

describe('OKX parsing', () => {
  it('sorts candles oldest first and maps base volume and confirmation', () => {
    const candles = parseOkxCandles([
      ['2000', '2', '3', '1', '2.5', '10', '20', '50', '0'],
      ['1000', '1', '2', '0.5', '1.5', '11', '21', '51', '1'],
    ]);
    expect(candles.map((c) => c.time)).toEqual([1000, 2000]);
    expect(candles[0]).toMatchObject({ volume: 21, closed: true });
    expect(candles[1]).toMatchObject({ volume: 20, closed: false });
  });

  it('requires a complete ticker and estimates a missing mark from the mid', () => {
    expect(buildQuote('BTCUSDT', { bid: '99', ask: '101', last: '100' })).toBeUndefined();
    expect(buildQuote('BTCUSDT', { bid: '99', ask: '101', last: '100', ts: 123 })).toMatchObject({
      mark: '100', markEstimated: true, fundingRate: null, nextFundingTime: null,
    });
    expect(buildQuote('BTCUSDT', { bid: '99', ask: '101', last: '100', mark: '100.5', ts: 123 })).toMatchObject({
      mark: '100.5', markEstimated: false,
    });
  });

  it('merges websocket ticker, mark, and funding messages', () => {
    const feed = new OkxMarketData();
    const quotes: unknown[] = [];
    feed.onQuote((quote) => quotes.push(quote));
    feed.handleMessage(JSON.stringify({ arg: { channel: 'mark-price', instId: 'BTC-USDT-SWAP' }, data: [{ markPx: '100.5' }] }));
    feed.handleMessage(JSON.stringify({ arg: { channel: 'funding-rate', instId: 'BTC-USDT-SWAP' }, data: [{ fundingRate: '0.001', fundingTime: '999' }] }));
    feed.handleMessage(JSON.stringify({ arg: { channel: 'tickers', instId: 'BTC-USDT-SWAP' }, data: [{ bidPx: '99', askPx: '101', last: '100', ts: '123' }] }));
    expect(quotes).toEqual([expect.objectContaining({ mark: '100.5', markEstimated: false, fundingRate: '0.001', nextFundingTime: 999 })]);
    expect(feed.getQuote('BTCUSDT')).toMatchObject({ bid: '99', ask: '101' });
  });

  it('reconnects a closed websocket and publishes REST fallback quotes', async () => {
    class FakeSocket {
      static instances: FakeSocket[] = [];
      static OPEN = 1;
      OPEN = 1;
      readyState = 1;
      onopen: (() => void) | null = null;
      onmessage: ((event: { data: string }) => void) | null = null;
      onerror: (() => void) | null = null;
      onclose: (() => void) | null = null;
      constructor(_url: string) { FakeSocket.instances.push(this); }
      send(_value: string) {}
      close() { this.onclose?.(); }
    }
    const fetchImpl = vi.fn(async (url: string) => {
      const data = url.includes('/ticker?')
        ? [{ bidPx: '99', askPx: '101', last: '100', ts: '123' }]
        : url.includes('mark-price') ? [{ markPx: '100.5' }] : [{ fundingRate: '0.001', fundingTime: '999' }];
      return { ok: true, json: async () => ({ code: '0', msg: '', data }) } as Response;
    });
    const feed = new OkxMarketData({
      fetchImpl: fetchImpl as unknown as typeof fetch,
      WebSocketImpl: FakeSocket as unknown as typeof WebSocket,
      backoff: () => 0,
    });
    const quotes: unknown[] = [];
    feed.onQuote((quote) => quotes.push(quote));
    feed.start();
    await vi.waitFor(() => expect(quotes.length).toBeGreaterThan(0));
    expect(quotes.length).toBeGreaterThan(0);
    FakeSocket.instances[0]!.onclose?.();
    await vi.waitFor(() => expect(FakeSocket.instances).toHaveLength(2));
    feed.stop();
  });
});

describe('OKX WebSocket handshake timeout', () => {
  /** A socket whose handshake never completes unless the test opens it. Closing it while
   *  CONNECTING fires error, then close, as a real WebSocket does. */
  class HungSocket {
    static instances: HungSocket[] = [];
    OPEN = 1;
    readyState = 0;
    closed = false;
    onopen: (() => void) | null = null;
    onmessage: ((event: { data: string }) => void) | null = null;
    onerror: (() => void) | null = null;
    onclose: (() => void) | null = null;
    constructor(_url: string) { HungSocket.instances.push(this); }
    send(_value: string) {}
    open() { this.readyState = 1; this.onopen?.(); }
    close() {
      this.closed = true;
      if (this.readyState === 0) this.onerror?.();
      this.readyState = 3;
      this.onclose?.();
    }
  }
  const fetchImpl = async () => ({ ok: true, json: async () => ({ code: '0', msg: '', data: [] }) }) as unknown as Response;
  const startFeed = () => {
    HungSocket.instances = [];
    vi.useFakeTimers();
    const feed = new OkxMarketData({ fetchImpl, WebSocketImpl: HungSocket as unknown as typeof WebSocket });
    feed.start();
    return feed;
  };
  const DETAIL = 'WebSocket handshake timed out; reconnecting, REST polling meanwhile';

  it('closes a hung handshake at 10 s, goes DEGRADED and redials with the backoff', async () => {
    const feed = startFeed();
    await vi.advanceTimersByTimeAsync(9_999);
    expect(feed.status().state).toBe('CONNECTING');
    expect(HungSocket.instances[0]!.closed).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    expect(HungSocket.instances[0]!.closed).toBe(true);
    expect(feed.status()).toMatchObject({ state: 'DEGRADED', detail: DETAIL });
    // Backoff for attempt 0 is 1 s.
    await vi.advanceTimersByTimeAsync(999);
    expect(HungSocket.instances).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(HungSocket.instances).toHaveLength(2);
    expect(feed.status().state).toBe('CONNECTING');
    // The attempt count is not reset by a timeout: the next redial waits 2 s.
    await vi.advanceTimersByTimeAsync(10_000);
    expect(feed.status()).toMatchObject({ state: 'DEGRADED', detail: DETAIL });
    await vi.advanceTimersByTimeAsync(1_999);
    expect(HungSocket.instances).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1);
    expect(HungSocket.instances).toHaveLength(3);
    feed.stop();
  });

  it('never times out a socket that opened in time', async () => {
    const feed = startFeed();
    await vi.advanceTimersByTimeAsync(9_000);
    HungSocket.instances[0]!.open();
    expect(feed.status().state).toBe('CONNECTED');
    await vi.advanceTimersByTimeAsync(15_000);
    expect(feed.status().state).toBe('CONNECTED');
    expect(HungSocket.instances).toHaveLength(1);
    expect(HungSocket.instances[0]!.closed).toBe(false);
    feed.stop();
  });

  it('stopping during the handshake leaves no timer behind', async () => {
    const feed = startFeed();
    feed.stop();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(feed.status().state).toBe('DISCONNECTED');
    expect(HungSocket.instances).toHaveLength(1);
  });
});

describe('OKX pair subscriptions', () => {
  class Socket {
    static instances: Socket[] = [];
    OPEN = 1;
    readyState = 1;
    sent: string[] = [];
    onopen: (() => void) | null = null;
    onmessage: ((event: { data: string }) => void) | null = null;
    onerror: (() => void) | null = null;
    onclose: (() => void) | null = null;
    constructor(_url: string) { Socket.instances.push(this); }
    send(value: string) { this.sent.push(value); }
    close() {}
  }
  const start = () => {
    Socket.instances = [];
    const fetchImpl = vi.fn(async () => ({ ok: true, json: async () => ({ code: '0', msg: '', data: [] }) }) as Response);
    const feed = new OkxMarketData({ fetchImpl: fetchImpl as unknown as typeof fetch, WebSocketImpl: Socket as unknown as typeof WebSocket });
    return { feed, fetchImpl };
  };

  it('subscribes an added pair on the open socket and accepts its ticks', () => {
    const { feed } = start();
    feed.start();
    const socket = Socket.instances[0]!;
    socket.onopen!();
    feed.trackSymbol('DOGEUSDT');
    expect(JSON.parse(socket.sent.at(-1)!)).toMatchObject({ op: 'subscribe', args: expect.arrayContaining([{ channel: 'tickers', instId: 'DOGE-USDT-SWAP' }]) });
    feed.handleMessage(JSON.stringify({ arg: { channel: 'tickers', instId: 'DOGE-USDT-SWAP' }, data: [{ bidPx: '1', askPx: '2', last: '1.5', ts: '9' }] }));
    expect(feed.getQuote('DOGEUSDT')).toMatchObject({ last: '1.5' });
    feed.stop();
  });

  it('includes pairs tracked before the socket opened', () => {
    const { feed } = start();
    feed.trackSymbol('DOGEUSDT');
    feed.start();
    const socket = Socket.instances[0]!;
    socket.onopen!();
    expect(socket.sent[0]).toContain('DOGE-USDT-SWAP');
    feed.stop();
  });

  it('stops accepting ticks for a removed pair', () => {
    const { feed } = start();
    feed.trackSymbol('DOGEUSDT');
    feed.untrackSymbol('DOGEUSDT');
    const quotes: unknown[] = [];
    feed.onQuote((q) => quotes.push(q));
    feed.handleMessage(JSON.stringify({ arg: { channel: 'tickers', instId: 'DOGE-USDT-SWAP' }, data: [{ bidPx: '1', askPx: '2', last: '1.5', ts: '9' }] }));
    expect(quotes).toEqual([]);
    expect(feed.getQuote('DOGEUSDT')).toBeUndefined();
  });

  it('filters the OKX list and caches it unless asked for fresh data', async () => {
    const { feed, fetchImpl } = start();
    await feed.listInstruments();
    await feed.listInstruments();
    expect(fetchImpl).toHaveBeenCalledTimes(1);
    await feed.listInstruments(true);
    expect(fetchImpl).toHaveBeenCalledTimes(2);
  });
});
