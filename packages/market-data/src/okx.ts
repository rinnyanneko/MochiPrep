import { d } from '@mochiprep/core';
import {
  DEFAULT_SYMBOLS,
  type Candle,
  type ConnectionState,
  type FeedStatus,
  type InstrumentSpecification,
  type Quote,
  type SymbolId,
  type Timeframe,
} from '@mochiprep/shared';
import type { MarketDataProvider } from './provider';

const REST_BASE = 'https://www.okx.com';
const WS_URL = 'wss://ws.okx.com/ws/v5/public';
const BARS: Record<Timeframe, string> = { '1m': '1m', '5m': '5m', '15m': '15m', '1h': '1H', '4h': '4H' };
const PING_MS = 20_000;
const SILENCE_LIMIT_MS = 30_000;
const REST_POLL_MS = 5_000;
const CANDLE_CACHE_MS = 3_000;
const MAX_BACKOFF_MS = 30_000;

export const toInstId = (symbol: SymbolId) => `${symbol.replace(/USDT$/, '')}-USDT-SWAP`;

const INSTRUMENT_CACHE_MS = 300_000;
/** Inferred from OKX data, not documentation: 1 is crypto, 3 stocks, 4 commodities. */
const CRYPTO_CATEGORY = '1';

/**
 * Turns an OKX SWAP row into a spec, or null if MochiPrep must not trade it. Only live, linear,
 * USDT-settled crypto contracts qualify: pre-market, stock and commodity contracts have trading
 * rules the paper engine and risk checks do not model.
 */
export function parseOkxInstrument(row: Record<string, string>): InstrumentSpecification | null {
  const match = /^([A-Z0-9]+)-USDT-SWAP$/.exec(row.instId ?? '');
  if (!match) return null;
  const base = match[1]!;
  const eligible = row.settleCcy === 'USDT' && row.ctType === 'linear' && row.state === 'live'
    && row.ruleType === 'normal' && row.instCategory === CRYPTO_CATEGORY && row.ctValCcy === base;
  if (!eligible || !row.ctVal || !row.lotSz || !row.minSz || !row.tickSz) return null;
  return {
    symbol: `${base}USDT`,
    marketType: 'CRYPTO_PERP',
    baseCurrency: base,
    quoteCurrency: 'USDT',
    settlementCurrency: 'USDT',
    contractSize: '1',
    quantityStep: d(row.ctVal).mul(row.lotSz).toFixed(),
    minQuantity: d(row.ctVal).mul(row.minSz).toFixed(),
    priceTick: row.tickSz,
    maxLeverage: Number(row.lever) || 1,
    maintenanceMarginRate: '0.005',
    fundingIntervalHours: 8,
    venueContractValue: row.ctVal,
    venueLotSize: row.lotSz,
  };
}

/** OKX returns newest first as arrays of strings: [ts,o,h,l,c,vol,volCcy,volCcyQuote,confirm]. */
export function parseOkxCandles(rows: string[][]): Candle[] {
  return rows
    .map((r) => ({
      time: Number(r[0]),
      open: Number(r[1]),
      high: Number(r[2]),
      low: Number(r[3]),
      close: Number(r[4]),
      // Index 6 is size in base currency; index 5 would be contracts.
      volume: Number(r[6]),
      closed: r[8] === '1',
    }))
    .sort((a, b) => a.time - b.time);
}

const channels = (symbol: SymbolId) =>
  ['tickers', 'mark-price', 'funding-rate'].map((channel) => ({ channel, instId: toInstId(symbol) }));

interface Partial {
  bid?: string;
  ask?: string;
  last?: string;
  mark?: string;
  fundingRate?: string;
  nextFundingTime?: number;
  ts?: number;
}

export function buildQuote(symbol: SymbolId, p: Partial): Quote | undefined {
  if (!p.bid || !p.ask || !p.last || !p.ts) return undefined;
  const mid = ((Number(p.bid) + Number(p.ask)) / 2).toString();
  return {
    source: 'LIVE',
    symbol,
    bid: p.bid,
    ask: p.ask,
    last: p.last,
    mark: p.mark ?? mid,
    markEstimated: !p.mark,
    fundingRate: p.fundingRate ?? null,
    nextFundingTime: p.nextFundingTime ?? null,
    ts: p.ts,
  };
}

export interface OkxOptions {
  fetchImpl?: typeof fetch;
  WebSocketImpl?: typeof WebSocket;
  restBase?: string;
  wsUrl?: string;
  /** Called with ms; the delay before reconnect attempt n. Overridable for tests. */
  backoff?: (attempt: number) => number;
}

/** Public OKX USDT-swap data. No API key. WebSocket first, REST polling while it is down. */
export class OkxMarketData implements MarketDataProvider {
  readonly source = 'LIVE' as const;
  readonly name = 'OKX public';
  private readonly fetchImpl: typeof fetch;
  private readonly WS: typeof WebSocket;
  private readonly restBase: string;
  private readonly wsUrl: string;
  private readonly backoff: (attempt: number) => number;

  private readonly symbols = new Set<SymbolId>(DEFAULT_SYMBOLS);
  private readonly state = new Map<SymbolId, Partial>();
  private instrumentCache: { at: number; specs: InstrumentSpecification[] } | null = null;
  private readonly quoteListeners = new Set<(q: Quote) => void>();
  private readonly statusListeners = new Set<(s: FeedStatus) => void>();
  private readonly candleCache = new Map<string, { at: number; candles: Candle[] }>();

  private socket: WebSocket | null = null;
  private running = false;
  private attempts = 0;
  private lastMessageAt: number | null = null;
  private connection: ConnectionState = 'DISCONNECTED';
  private detail: string | undefined;
  private reconnectTimer: NodeJS.Timeout | null = null;
  private pingTimer: NodeJS.Timeout | null = null;
  private pollTimer: NodeJS.Timeout | null = null;

  constructor(options: OkxOptions = {}) {
    this.fetchImpl = options.fetchImpl ?? fetch;
    this.WS = options.WebSocketImpl ?? WebSocket;
    this.restBase = options.restBase ?? REST_BASE;
    this.wsUrl = options.wsUrl ?? WS_URL;
    this.backoff = options.backoff ?? ((n) => Math.min(1000 * 2 ** n, MAX_BACKOFF_MS));
  }

  start() {
    if (this.running) return;
    this.running = true;
    this.connect();
    this.pollTimer = setInterval(() => {
      if (this.connection !== 'CONNECTED') void this.pollRest();
    }, REST_POLL_MS);
    void this.pollRest();
  }

  stop() {
    this.running = false;
    for (const timer of [this.reconnectTimer, this.pingTimer, this.pollTimer]) if (timer) clearInterval(timer);
    this.reconnectTimer = this.pingTimer = this.pollTimer = null;
    this.socket?.close();
    this.socket = null;
    this.setConnection('DISCONNECTED', 'Feed stopped');
  }

  now() {
    return Date.now();
  }

  private connect() {
    if (!this.running) return;
    this.setConnection('CONNECTING');
    let socket: WebSocket;
    try {
      socket = new this.WS(this.wsUrl);
    } catch (error) {
      this.scheduleReconnect(String(error));
      return;
    }
    this.socket = socket;
    socket.onopen = () => {
      this.attempts = 0;
      this.lastMessageAt = Date.now();
      socket.send(JSON.stringify({ op: 'subscribe', args: [...this.symbols].flatMap(channels) }));
      this.setConnection('CONNECTED');
      this.pingTimer = setInterval(() => this.heartbeat(socket), PING_MS);
    };
    socket.onmessage = (event) => this.handleMessage(String(event.data));
    socket.onerror = () => {
      this.detail = 'WebSocket error';
    };
    socket.onclose = () => {
      if (this.pingTimer) clearInterval(this.pingTimer);
      this.pingTimer = null;
      if (this.socket === socket) this.socket = null;
      if (this.running) this.scheduleReconnect(this.detail ?? 'Connection closed');
    };
  }

  private heartbeat(socket: WebSocket) {
    if (this.lastMessageAt && Date.now() - this.lastMessageAt > SILENCE_LIMIT_MS) {
      socket.close();
      return;
    }
    if (socket.readyState === socket.OPEN) socket.send('ping');
  }

  private scheduleReconnect(reason: string) {
    if (!this.running || this.reconnectTimer) return;
    this.setConnection('DEGRADED', `${reason}; reconnecting, REST polling meanwhile`);
    const delay = this.backoff(this.attempts++);
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay);
  }

  /** Exposed for tests: everything from the socket lands here. */
  handleMessage(raw: string) {
    this.lastMessageAt = Date.now();
    if (raw === 'pong') return;
    let message: { arg?: { channel?: string; instId?: string }; data?: Record<string, string>[]; event?: string };
    try {
      message = JSON.parse(raw);
    } catch {
      return;
    }
    const symbol = [...this.symbols].find((s) => toInstId(s) === message.arg?.instId);
    const row = message.data?.[0];
    if (!symbol || !row) return;
    const current = this.state.get(symbol) ?? {};
    switch (message.arg?.channel) {
      case 'tickers':
        this.applyTicker(symbol, current, row);
        break;
      case 'mark-price':
        this.state.set(symbol, { ...current, mark: row.markPx });
        break;
      case 'funding-rate':
        this.state.set(symbol, {
          ...current,
          fundingRate: row.fundingRate,
          nextFundingTime: Number(row.fundingTime),
        });
        break;
    }
  }

  private applyTicker(symbol: SymbolId, current: Partial, row: Record<string, string>) {
    const next: Partial = { ...current, bid: row.bidPx, ask: row.askPx, last: row.last, ts: Number(row.ts) };
    this.state.set(symbol, next);
    const quote = buildQuote(symbol, next);
    if (quote) for (const listener of this.quoteListeners) listener(quote);
  }

  private async get<T>(path: string): Promise<T[]> {
    const response = await this.fetchImpl(`${this.restBase}${path}`, { signal: AbortSignal.timeout(8_000) });
    if (!response.ok) throw new Error(`OKX HTTP ${response.status}`);
    const body = (await response.json()) as { code: string; msg: string; data: T[] };
    if (body.code !== '0') throw new Error(`OKX error ${body.code}: ${body.msg}`);
    return body.data;
  }

  private async pollRest() {
    await Promise.allSettled(
      [...this.symbols].map(async (symbol) => {
        const instId = toInstId(symbol);
        const [ticker, mark, funding] = await Promise.all([
          this.get<Record<string, string>>(`/api/v5/market/ticker?instId=${instId}`),
          this.get<Record<string, string>>(`/api/v5/public/mark-price?instType=SWAP&instId=${instId}`),
          this.get<Record<string, string>>(`/api/v5/public/funding-rate?instId=${instId}`),
        ]);
        this.lastMessageAt = Date.now();
        const current: Partial = {
          ...this.state.get(symbol),
          mark: mark[0]?.markPx,
          fundingRate: funding[0]?.fundingRate,
          nextFundingTime: funding[0] ? Number(funding[0].fundingTime) : undefined,
        };
        if (ticker[0]) this.applyTicker(symbol, current, ticker[0]);
      }),
    );
  }

  async getCandles(symbol: SymbolId, timeframe: Timeframe, limit: number): Promise<Candle[]> {
    const key = `${symbol}:${timeframe}:${limit}`;
    const cached = this.candleCache.get(key);
    if (cached && Date.now() - cached.at < CANDLE_CACHE_MS) return cached.candles;
    const rows = await this.get<string[]>(
      `/api/v5/market/candles?instId=${toInstId(symbol)}&bar=${BARS[timeframe]}&limit=${Math.min(limit, 300)}`,
    );
    const candles = parseOkxCandles(rows);
    this.candleCache.set(key, { at: Date.now(), candles });
    return candles;
  }

  /** Everything MochiPrep could trade on OKX. Pass fresh when the result decides a real spec. */
  async listInstruments(fresh = false): Promise<InstrumentSpecification[]> {
    if (!fresh && this.instrumentCache && Date.now() - this.instrumentCache.at < INSTRUMENT_CACHE_MS) {
      return this.instrumentCache.specs;
    }
    const rows = await this.get<Record<string, string>>('/api/v5/public/instruments?instType=SWAP');
    const specs = rows.flatMap((row) => parseOkxInstrument(row) ?? []).sort((a, b) => a.symbol.localeCompare(b.symbol));
    this.instrumentCache = { at: Date.now(), specs };
    return specs;
  }

  trackSymbol(symbol: SymbolId) {
    if (this.symbols.has(symbol)) return;
    this.symbols.add(symbol);
    this.send('subscribe', symbol);
    if (this.running) void this.pollRest();
  }

  untrackSymbol(symbol: SymbolId) {
    this.send('unsubscribe', symbol);
    this.symbols.delete(symbol);
    this.state.delete(symbol);
  }

  private send(op: 'subscribe' | 'unsubscribe', symbol: SymbolId) {
    if (this.socket?.readyState === this.socket?.OPEN) this.socket?.send(JSON.stringify({ op, args: channels(symbol) }));
  }

  getQuote(symbol: SymbolId) {
    const partial = this.state.get(symbol);
    return partial ? buildQuote(symbol, partial) : undefined;
  }

  status(): FeedStatus {
    return {
      source: 'LIVE',
      provider: this.name,
      state: this.connection,
      lastMessageAt: this.lastMessageAt,
      ...(this.detail ? { detail: this.detail } : {}),
    };
  }

  private setConnection(state: ConnectionState, detail?: string) {
    this.connection = state;
    this.detail = detail;
    const status = this.status();
    for (const listener of this.statusListeners) listener(status);
  }

  onQuote(listener: (quote: Quote) => void) {
    this.quoteListeners.add(listener);
    return () => void this.quoteListeners.delete(listener);
  }

  onStatus(listener: (status: FeedStatus) => void) {
    this.statusListeners.add(listener);
    return () => void this.statusListeners.delete(listener);
  }
}

