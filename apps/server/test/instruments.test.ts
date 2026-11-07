import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { DatabaseSync } from 'node:sqlite';
import { parseOkxInstrument, type MarketDataProvider } from '@mochiprep/market-data';
import { DEFAULT_RISK_SETTINGS, INSTRUMENTS, unregisterInstrument, type Quote, type SymbolId } from '@mochiprep/shared';
import { buildApp } from '../src/app';
import { openDatabase } from '../src/db/database';
import { Runtime } from '../src/runtime';

const row = (overrides: Record<string, string> = {}) => ({
  instId: 'DOGE-USDT-SWAP', settleCcy: 'USDT', ctType: 'linear', state: 'live', ruleType: 'normal', instCategory: '1',
  ctValCcy: 'DOGE', ctVal: '1000', lotSz: '0.01', minSz: '0.01', tickSz: '0.00001', lever: '50', ...overrides,
});

describe('parseOkxInstrument', () => {
  it('builds base-unit steps from contract size and lot size', () => {
    expect(parseOkxInstrument(row())).toMatchObject({
      symbol: 'DOGEUSDT', baseCurrency: 'DOGE', quantityStep: '10', minQuantity: '10', priceTick: '0.00001',
      maxLeverage: 50, venueContractValue: '1000', venueLotSize: '0.01',
    });
  });

  it('keeps exact decimals for very large contracts', () => {
    const spec = parseOkxInstrument(row({ instId: 'PEPE-USDT-SWAP', ctValCcy: 'PEPE', ctVal: '10000000', lotSz: '0.1', minSz: '0.1', tickSz: '0.0000000001' }));
    expect(spec).toMatchObject({ quantityStep: '1000000', priceTick: '0.0000000001' });
  });

  it.each([
    ['a stock contract', { instCategory: '3' }],
    ['a commodity contract', { instCategory: '4' }],
    ['a pre-market contract', { ruleType: 'pre_market' }],
    ['a suspended contract', { state: 'suspend' }],
    ['an inverse contract', { ctType: 'inverse' }],
    ['a contract settled in another coin', { settleCcy: 'BTC' }],
    ['a contract valued in another currency', { ctValCcy: 'USD' }],
    ['a dated future', { instId: 'DOGE-USDT-261225' }],
  ])('refuses %s', (_label, overrides) => {
    expect(parseOkxInstrument(row(overrides))).toBeNull();
  });
});

describe('managing pairs', () => {
  const DOGE = parseOkxInstrument(row())!;
  let db: DatabaseSync;
  let runtime: Runtime;
  let app: ReturnType<typeof buildApp>['app'];
  let token: string;
  let dir: string;
  const tracked: string[] = [];
  const untracked: string[] = [];

  const feed = {
    source: 'LIVE',
    name: 'fake',
    start() {},
    stop() {},
    now: () => Date.now(),
    getCandles: async () => [],
    getQuote: (symbol: SymbolId): Quote => ({
      source: 'LIVE', symbol, bid: '0.09', ask: '0.11', last: '0.1', mark: '0.1', markEstimated: false, fundingRate: '0', nextFundingTime: null, ts: Date.now(),
    }),
    status: () => ({ source: 'LIVE', provider: 'fake', state: 'CONNECTED', lastMessageAt: Date.now() }),
    onQuote: () => () => {},
    onStatus: () => () => {},
    listInstruments: vi.fn(async () => [DOGE]),
    trackSymbol: (s: string) => tracked.push(s),
    untrackSymbol: (s: string) => untracked.push(s),
  } as unknown as MarketDataProvider;

  const boot = () => {
    db = openDatabase(join(dir, 'state.sqlite'));
    runtime = new Runtime({ db, live: feed, replayMinutes: 5_800 });
    ({ app, token } = buildApp(runtime, { host: '127.0.0.1', port: 8787, dataDir: '.', allowedOrigins: ['http://localhost:8787'] }));
  };
  const call = (method: 'GET' | 'POST' | 'DELETE', url: string, payload?: object) =>
    app.inject({ method, url, headers: { host: 'localhost:8787', authorization: `Bearer ${token}` }, ...(payload ? { payload } : {}) });

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'mochiprep-pairs-'));
    tracked.length = 0;
    untracked.length = 0;
    boot();
  });

  afterEach(async () => {
    await app.close();
    await runtime.stop();
    db.close();
    unregisterInstrument('DOGEUSDT');
    rmSync(dir, { recursive: true, force: true });
  });

  it('adds a pair from OKX, subscribes the feed, and lists it', async () => {
    expect((await call('GET', '/api/instruments/available')).json().map((s: { symbol: string }) => s.symbol)).toEqual(['DOGEUSDT']);
    const response = await call('POST', '/api/instruments', { symbol: 'DOGEUSDT' });
    expect(response.statusCode).toBe(200);
    expect(tracked).toEqual(['DOGEUSDT']);
    expect((await call('GET', '/api/instruments')).json().map((s: { symbol: string }) => s.symbol)).toContain('DOGEUSDT');
    expect((await call('GET', '/api/instruments/available')).json()).toEqual([]);
  });

  it('reads fresh specs rather than a cached list when adding', async () => {
    await call('POST', '/api/instruments', { symbol: 'DOGEUSDT' });
    expect(feed.listInstruments).toHaveBeenLastCalledWith(true);
  });

  it('refuses a pair OKX does not offer, and malformed input', async () => {
    expect((await call('POST', '/api/instruments', { symbol: 'NOPEUSDT' })).json().error).toContain('not a live USDT crypto perpetual');
    expect((await call('POST', '/api/instruments', { symbol: '../etc' })).statusCode).toBe(400);
    expect(INSTRUMENTS.NOPEUSDT).toBeUndefined();
  });

  it('survives a restart and resubscribes', async () => {
    await call('POST', '/api/instruments', { symbol: 'DOGEUSDT' });
    await app.close();
    await runtime.stop();
    db.close();
    unregisterInstrument('DOGEUSDT');
    tracked.length = 0;
    boot();
    expect(INSTRUMENTS.DOGEUSDT).toMatchObject({ venueContractValue: '1000' });
    expect(tracked).toEqual(['DOGEUSDT']);
  });

  it('trades an added pair on a live-data paper account, on the pair step', async () => {
    await call('POST', '/api/instruments', { symbol: 'DOGEUSDT' });
    const account = runtime.engine.createAccount({ name: 'p', source: 'LIVE' });
    const response = await call('POST', '/api/orders', {
      accountId: account.id, clientOrderId: 'c1', symbol: 'DOGEUSDT', side: 'BUY', type: 'MARKET', quantity: '125', leverage: 3,
    });
    expect(response.json().order).toMatchObject({ status: 'FILLED', quantity: '120' });
  });

  it('keeps added pairs off the replay tape', async () => {
    await call('POST', '/api/instruments', { symbol: 'DOGEUSDT' });
    const account = runtime.engine.createAccount({ name: 'r', source: 'REPLAY' });
    const order = await call('POST', '/api/orders', {
      accountId: account.id, clientOrderId: 'c1', symbol: 'DOGEUSDT', side: 'BUY', type: 'MARKET', quantity: '10', leverage: 3,
    });
    expect(order.json().error).toContain('no replay data');
    expect((await call('GET', '/api/market/candles?source=REPLAY&symbol=DOGEUSDT&timeframe=15m')).statusCode).toBe(400);
    expect(() => runtime.createAgent({
      name: 'x', providerId: 'mock', model: 'mock-momentum-v1', symbol: 'DOGEUSDT', timeframe: '15m', strategy: '',
      accountId: account.id, risk: DEFAULT_RISK_SETTINGS, intervalSec: 900,
    })).toThrow(/no replay data/);
  });

  it('refuses orders on a pair nobody added', async () => {
    const account = runtime.engine.createAccount({ name: 'p', source: 'LIVE' });
    const response = await call('POST', '/api/orders', {
      accountId: account.id, clientOrderId: 'c1', symbol: 'DOGEUSDT', side: 'BUY', type: 'MARKET', quantity: '10', leverage: 3,
    });
    expect(response.json().error).toContain('not an added pair');
  });

  describe('removing', () => {
    beforeEach(async () => {
      await call('POST', '/api/instruments', { symbol: 'DOGEUSDT' });
    });

    it('removes an unused pair and unsubscribes', async () => {
      expect((await call('DELETE', '/api/instruments/DOGEUSDT')).statusCode).toBe(200);
      expect(untracked).toEqual(['DOGEUSDT']);
      expect(INSTRUMENTS.DOGEUSDT).toBeUndefined();
      expect(runtime.store.listInstruments()).toEqual([]);
    });

    it('never removes a built-in pair', async () => {
      expect((await call('DELETE', '/api/instruments/BTCUSDT')).json().error).toContain('built in');
      expect(INSTRUMENTS.BTCUSDT).toBeDefined();
    });

    it('refuses while an agent, a position, or an order uses it', async () => {
      const account = runtime.engine.createAccount({ name: 'p', source: 'LIVE' });
      const place = (extra: object) => runtime.engine.placeOrderSync({
        accountId: account.id, clientOrderId: crypto.randomUUID(), symbol: 'DOGEUSDT', side: 'BUY', type: 'MARKET', quantity: '100', leverage: 3, ...extra,
      });

      const resting = place({ type: 'LIMIT', price: '0.01' });
      expect((await call('DELETE', '/api/instruments/DOGEUSDT')).json().error).toContain('open DOGEUSDT orders');
      runtime.engine.cancelOrderSync(resting.order.id);

      place({});
      expect((await call('DELETE', '/api/instruments/DOGEUSDT')).json().error).toContain('open DOGEUSDT position');
      runtime.engine.closePosition(account.id, 'DOGEUSDT', 1, 'close');

      const agent = runtime.createAgent({
        name: 'x', providerId: 'mock', model: 'mock-momentum-v1', symbol: 'DOGEUSDT', timeframe: '15m', strategy: '',
        accountId: account.id, risk: DEFAULT_RISK_SETTINGS, intervalSec: 900,
      });
      expect((await call('DELETE', '/api/instruments/DOGEUSDT')).json().error).toContain('Agents still use');
      runtime.store.deleteAgent(agent.id);
      expect((await call('DELETE', '/api/instruments/DOGEUSDT')).statusCode).toBe(200);
    });
  });
});
