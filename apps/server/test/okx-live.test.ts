import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { DatabaseSync } from 'node:sqlite';
import { createHmac } from 'node:crypto';
import type { AiProvider, AiRequest } from '@mochiprep/ai-providers';
import { parseOkxInstrument, type MarketDataProvider } from '@mochiprep/market-data';
import {
  ARM_PHRASE,
  DEFAULT_LIVE_LIMITS,
  DEFAULT_RISK_SETTINGS,
  unregisterInstrument,
  type Candle,
  type OrderRequest,
  type Quote,
  type SymbolId,
} from '@mochiprep/shared';
import { buildApp } from '../src/app';
import { openDatabase } from '../src/db/database';
import { Runtime } from '../src/runtime';

const CREDS = { apiKey: 'okx-key-abcdef123456', secretKey: 'okx-secret-abcdef123456', passphrase: 'okx-pass-abcdef123456' };
const PRICE = 75_000;

/** A small in-memory OKX: verifies signatures and fills market orders immediately. */
class FakeOkx {
  perm = 'read_only,trade';
  posMode = 'net_mode';
  acctLv = '2';
  equity = 1000;
  down = false;
  /** Applies the next order on the exchange, then loses the response. */
  loseNextResponse = false;
  /** Loses the next order before it reaches the exchange. */
  dropNextRequest = false;
  rejectNext: { sCode: string; sMsg: string } | null = null;
  /** Same refusal, but the way OKX reports it with an overall success code. */
  rejectRowNext: { sCode: string; sMsg: string } | null = null;
  /** Makes order lookups fail with a code that does not mean "not found". */
  lookupErrorCode: string | null = null;
  ctValOverride: string | null = null;
  pepeCtValOverride: string | null = null;
  leverageCalls: { instId: string; lever: string; mgnMode: string }[] = [];
  placed: Record<string, any>[] = [];
  orders = new Map<string, Record<string, string>>();
  position: Record<string, string> | null = null;
  simulatedHeaders: (string | null)[] = [];

  private json(data: unknown[], code = '0', msg = '') {
    return new Response(JSON.stringify({ code, msg, data }), { status: 200 });
  }

  fetch: typeof fetch = async (input, init) => {
    const url = new URL(String(input));
    const path = url.pathname + url.search;
    const headers = new Headers(init?.headers);
    const body = (init?.body as string | undefined) ?? '';
    if (this.down) throw new TypeError('fetch failed');
    if (headers.get('OK-ACCESS-KEY') !== CREDS.apiKey || headers.get('OK-ACCESS-PASSPHRASE') !== CREDS.passphrase) return this.json([], '50119', 'bad key');
    const prehash = `${headers.get('OK-ACCESS-TIMESTAMP')}${init!.method}${path}${body}`;
    const expected = createHmac('sha256', CREDS.secretKey).update(prehash).digest('base64');
    if (headers.get('OK-ACCESS-SIGN') !== expected) return this.json([], '50113', 'bad signature');
    this.simulatedHeaders.push(headers.get('x-simulated-trading'));

    if (url.pathname.endsWith('/account/config')) return this.json([{ uid: '42', perm: this.perm, posMode: this.posMode, acctLv: this.acctLv }]);
    if (url.pathname.endsWith('/public/instruments')) {
      const instId = url.searchParams.get('instId')!;
      const ctVal = { 'BTC-USDT-SWAP': '0.01', 'ETH-USDT-SWAP': '0.1', 'SOL-USDT-SWAP': '1', 'PEPE-USDT-SWAP': '10000000' }[instId];
      const isPepe = instId.startsWith('PEPE');
      return this.json([{ ctVal: this.ctValOverride ?? (isPepe ? this.pepeCtValOverride : null) ?? ctVal, lotSz: isPepe ? '0.1' : '0.01' }]);
    }
    if (url.pathname.endsWith('/account/balance')) {
      return this.json([{ details: [{ ccy: 'USDT', eq: String(this.equity), availEq: String(this.equity), upl: '0' }] }]);
    }
    if (url.pathname.endsWith('/account/positions')) return this.json(this.position ? [this.position] : []);
    if (url.pathname.endsWith('/account/set-leverage')) {
      this.leverageCalls.push(JSON.parse(body));
      return this.json([{}]);
    }
    if (url.pathname.endsWith('/trade/order') && init!.method === 'GET') {
      if (this.lookupErrorCode) return this.json([], this.lookupErrorCode, 'Systems are busy');
      const order = this.orders.get(url.searchParams.get('clOrdId')!);
      return order ? this.json([order]) : this.json([], '51603', 'Order does not exist');
    }
    if (url.pathname.endsWith('/trade/order')) return this.place(JSON.parse(body));
    return this.json([], '404', `Unhandled ${path}`);
  };

  private place(order: Record<string, any>) {
    if (this.dropNextRequest) {
      this.dropNextRequest = false;
      throw new TypeError('socket hang up');
    }
    this.placed.push(order);
    if (this.rejectNext) {
      const row = this.rejectNext;
      this.rejectNext = null;
      return new Response(JSON.stringify({ code: '1', msg: '', data: [{ ...row, ordId: '' }] }));
    }
    if (this.rejectRowNext) {
      const row = this.rejectRowNext;
      this.rejectRowNext = null;
      return this.json([{ ...row, ordId: '' }]);
    }
    const ordId = String(this.placed.length);
    this.orders.set(order.clOrdId, { ordId, clOrdId: order.clOrdId, state: 'filled', accFillSz: order.sz, avgPx: String(PRICE), fee: '-0.05', pnl: '0' });
    const signed = Number(order.sz) * (order.side === 'buy' ? 1 : -1);
    const size = Number(this.position?.pos ?? 0) + signed;
    this.position = size === 0 ? null : { instId: order.instId, pos: String(size), avgPx: String(PRICE), markPx: String(PRICE), upl: '0', lever: '3', liqPx: '50000', margin: '10', mgnMode: 'isolated' };
    if (this.loseNextResponse) {
      this.loseNextResponse = false;
      throw new TypeError('socket hang up');
    }
    return this.json([{ ordId, clOrdId: order.clOrdId, sCode: '0', sMsg: '' }]);
  }
}

const PEPE = parseOkxInstrument({
  instId: 'PEPE-USDT-SWAP', settleCcy: 'USDT', ctType: 'linear', state: 'live', ruleType: 'normal', instCategory: '1',
  ctValCcy: 'PEPE', ctVal: '10000000', lotSz: '0.1', minSz: '0.1', tickSz: '0.0000000001', lever: '50',
})!;

class FakeFeed {
  readonly source = 'LIVE' as const;
  readonly name = 'fake';
  quoteAgeMs = 0;
  start() {}
  stop() {}
  now() { return Date.now(); }
  getQuote(symbol: SymbolId): Quote {
    return { source: 'LIVE', symbol, bid: String(PRICE - 1), ask: String(PRICE + 1), last: String(PRICE), mark: String(PRICE), markEstimated: false, fundingRate: '0', nextFundingTime: null, ts: Date.now() - this.quoteAgeMs };
  }
  async getCandles(): Promise<Candle[]> {
    const size = 900_000;
    const last = Math.floor(Date.now() / size) * size - size;
    return Array.from({ length: 60 }, (_, i) => ({ time: last - (59 - i) * size, open: PRICE, high: PRICE + 50, low: PRICE - 50, close: PRICE, volume: 1, closed: true }));
  }
  async listInstruments() { return [PEPE]; }
  status() { return { source: 'LIVE' as const, provider: 'fake', state: 'CONNECTED' as const, lastMessageAt: Date.now() }; }
  onQuote() { return () => {}; }
  onStatus() { return () => {}; }
}

describe('OKX execution', () => {
  let okx: FakeOkx;
  let feed: FakeFeed;
  let db: DatabaseSync;
  let runtime: Runtime;
  let aiRequests: AiRequest[];
  let dir: string;
  let dbPath: string;

  const longDecision: AiProvider = {
    kind: 'OPENAI',
    async complete(request) {
      aiRequests.push(request);
      return {
        text: JSON.stringify({ symbol: 'BTCUSDT', action: 'LONG', orderType: 'MARKET', entryPrice: null, stopLoss: PRICE * 0.99, takeProfit: PRICE * 1.02, reason: 'test', confidence: 0.7, expiresAt: null }),
        inputTokens: 1,
        outputTokens: 1,
      };
    },
  };

  const boot = () => {
    db = openDatabase(dbPath);
    runtime = new Runtime({
      db,
      live: feed as unknown as MarketDataProvider,
      replayMinutes: 5_800,
      providerFactory: () => longDecision,
      okx: { fetchImpl: okx.fetch, baseUrl: 'https://okx.invalid' },
    });
    if (!runtime.store.getProvider('remote')) {
      runtime.store.insertProvider({ id: 'remote', name: 'r', kind: 'OPENAI', baseUrl: null, defaultModel: 'm', inputPricePerMTok: null, outputPricePerMTok: null, createdAt: 1 });
      runtime.setKey('remote', 'ai-key');
    }
  };

  const connect = (environment: 'OKX_DEMO' | 'OKX_LIVE' = 'OKX_LIVE', limits = DEFAULT_LIVE_LIMITS) =>
    runtime.connectOkx({ ...CREDS, name: 'OKX', environment, limits });

  const order = (accountId: string, overrides: Partial<OrderRequest> = {}): OrderRequest => ({
    accountId, clientOrderId: crypto.randomUUID(), symbol: 'BTCUSDT', side: 'BUY', type: 'MARKET',
    quantity: '0.001', leverage: 3, stopLoss: String(PRICE - 1000), ...overrides,
  });

  const agentOn = (accountId: string) =>
    runtime.createAgent({
      name: 'auto', providerId: 'remote', model: 'm', symbol: 'BTCUSDT', timeframe: '15m', strategy: '',
      accountId, risk: DEFAULT_RISK_SETTINGS, intervalSec: 60,
    });

  beforeEach(() => {
    okx = new FakeOkx();
    feed = new FakeFeed();
    aiRequests = [];
    dir = mkdtempSync(join(tmpdir(), 'mochiprep-okx-'));
    dbPath = join(dir, 'state.sqlite');
    boot();
  });

  afterEach(async () => {
    await runtime.stop();
    db.close();
    rmSync(dir, { recursive: true, force: true });
  });

  describe('connecting', () => {
    it.each([
      ['a key that can withdraw', { perm: 'read_only,withdraw,trade' }, /withdrawal/],
      ['a read-only key', { perm: 'read_only' }, /no trade permission/],
      ['hedge mode', { posMode: 'long_short_mode' }, /one-way/],
      ['a spot-mode account', { acctLv: '1' }, /Spot mode/],
    ])('refuses %s and creates no account', async (_label, patch, message) => {
      Object.assign(okx, patch);
      await expect(connect()).rejects.toThrow(message);
      expect(runtime.store.listAccounts()).toHaveLength(0);
    });

    it('refuses to size orders if OKX contract specs changed', async () => {
      okx.ctValOverride = '0.001';
      await expect(connect()).rejects.toThrow(/contract specs/);
      expect(runtime.store.listAccounts()).toHaveLength(0);
    });

    it('signs every request, and marks only demo requests as simulated', async () => {
      await connect('OKX_LIVE');
      expect(okx.simulatedHeaders.every((h) => h === null)).toBe(true);
      okx.simulatedHeaders = [];
      await connect('OKX_DEMO');
      expect(okx.simulatedHeaders.every((h) => h === '1')).toBe(true);
    });

    it('never exposes credentials through status', async () => {
      await connect();
      const text = JSON.stringify([runtime.exchangeStatuses(), runtime.store.listAccounts()]);
      for (const value of Object.values(CREDS)) expect(text).not.toContain(value);
    });
  });

  describe('real-money safety', () => {
    it('sends nothing until armed, and only the exact phrase arms', async () => {
      const account = await connect();
      const result = await runtime.execution(account.id).placeOrder(order(account.id));
      expect(result.order.status).toBe('REJECTED');
      expect(result.order.rejectReason).toContain('NOT_ARMED');
      expect(okx.placed).toHaveLength(0);
      expect(() => runtime.arm(account.id, 'yes')).toThrow();
      runtime.arm(account.id, ARM_PHRASE);
      expect((await runtime.execution(account.id).placeOrder(order(account.id))).order.status).toBe('FILLED');
    });

    it('sends a well-formed isolated order with exchange-side protection', async () => {
      const account = await connect();
      runtime.arm(account.id, ARM_PHRASE);
      await runtime.execution(account.id).placeOrder(order(account.id, { takeProfit: String(PRICE + 2000) }));
      const sent = okx.placed[0]!;
      expect(sent).toMatchObject({ instId: 'BTC-USDT-SWAP', tdMode: 'isolated', side: 'buy', ordType: 'market', sz: '0.1' });
      expect(sent.clOrdId).toMatch(/^[a-f0-9]{32}$/);
      expect(sent.attachAlgoOrds[0]).toEqual({ slTriggerPx: '74000', slOrdPx: '-1', tpTriggerPx: '77000', tpOrdPx: '-1' });
    });

    it.each([
      ['a missing stop', { stopLoss: null }, 'LIMIT_STOP_REQUIRED'],
      ['a stop on the wrong side', { stopLoss: String(PRICE + 500) }, 'LIMIT_STOP_INVALID'],
      ['too much notional', { quantity: '0.01' }, 'LIMIT_NOTIONAL'],
      ['too much leverage', { leverage: 10 }, 'LIMIT_LEVERAGE'],
    ])('enforces the account ceiling on %s', async (_label, patch, code) => {
      const account = await connect();
      runtime.arm(account.id, ARM_PHRASE);
      const result = await runtime.execution(account.id).placeOrder(order(account.id, patch));
      expect(result.order.rejectReason).toContain(code);
      expect(okx.placed).toHaveLength(0);
    });

    it('sets isolated leverage before an opening order', async () => {
      const account = await connect();
      runtime.arm(account.id, ARM_PHRASE);
      await runtime.execution(account.id).placeOrder(order(account.id));
      expect(okx.leverageCalls).toEqual([{ instId: 'BTC-USDT-SWAP', lever: '3', mgnMode: 'isolated' }]);
    });

    it('enforces the open position limit across symbols', async () => {
      const account = await connect('OKX_LIVE', { ...DEFAULT_LIVE_LIMITS, maxOpenPositions: 1 });
      runtime.arm(account.id, ARM_PHRASE);
      await runtime.execution(account.id).placeOrder(order(account.id));
      const result = await runtime.execution(account.id).placeOrder(order(account.id, { symbol: 'ETHUSDT' }));
      expect(result.order.rejectReason).toContain('LIMIT_POSITIONS');
      expect(okx.placed).toHaveLength(1);
    });

    it('refuses an order larger than available margin', async () => {
      const account = await connect('OKX_LIVE', { ...DEFAULT_LIVE_LIMITS, maxNotional: 1_000_000 });
      runtime.arm(account.id, ARM_PHRASE);
      const result = await runtime.execution(account.id).placeOrder(order(account.id, { quantity: '0.1' }));
      expect(result.order.rejectReason).toContain('INSUFFICIENT_MARGIN');
      expect(okx.placed).toHaveLength(0);
    });

    it('refuses a reduce-only order on the same side as the position', async () => {
      const account = await connect();
      runtime.arm(account.id, ARM_PHRASE);
      await runtime.execution(account.id).placeOrder(order(account.id));
      const result = await runtime.execution(account.id).placeOrder(order(account.id, { reduceOnly: true, stopLoss: null }));
      expect(result.order.rejectReason).toContain('REDUCE_ONLY_NO_POSITION');
      expect(okx.placed).toHaveLength(1);
    });

    it('refuses new exposure on a stale quote', async () => {
      const account = await connect();
      runtime.arm(account.id, ARM_PHRASE);
      feed.quoteAgeMs = 20_000;
      const result = await runtime.execution(account.id).placeOrder(order(account.id));
      expect(result.order.rejectReason).toContain('MARKET_DATA_STALE');
      expect(okx.placed).toHaveLength(0);
    });

    it('stops new positions once the daily loss limit is reached', async () => {
      const account = await connect();
      runtime.arm(account.id, ARM_PHRASE);
      okx.equity = 970;
      const engine = runtime.execution(account.id) as unknown as { sync(): Promise<void> };
      await engine.sync();
      const result = await runtime.execution(account.id).placeOrder(order(account.id));
      expect(result.order.rejectReason).toContain('LIMIT_DAILY_LOSS');
    });

    it('still lets a disarmed account exit, clipped to the position size', async () => {
      const account = await connect();
      runtime.arm(account.id, ARM_PHRASE);
      await runtime.execution(account.id).placeOrder(order(account.id));
      runtime.disarm();
      const blocked = await runtime.execution(account.id).placeOrder(order(account.id, { symbol: 'ETHUSDT', stopLoss: '1' }));
      expect(blocked.order.rejectReason).toContain('NOT_ARMED');
      const exit = await runtime.execution(account.id).placeOrder(order(account.id, { side: 'SELL', quantity: '5', reduceOnly: true, stopLoss: null }));
      expect(exit.order.status).toBe('FILLED');
      expect(okx.placed[1]).toMatchObject({ sz: '0.1', reduceOnly: true });
      expect(okx.position).toBeNull();
    });
  });

  describe('added pairs', () => {
    afterEach(() => unregisterInstrument('PEPEUSDT'));

    it('converts to contracts from the pair spec, and sends prices in plain decimals on the tick', async () => {
      await runtime.addInstrument('PEPEUSDT');
      const account = await connect();
      runtime.arm(account.id, ARM_PHRASE);
      feed.getQuote = ((symbol: SymbolId): Quote => ({
        source: 'LIVE', symbol, bid: '0.0000099', ask: '0.0000101', last: '0.00001', mark: '0.00001',
        markEstimated: false, fundingRate: '0', nextFundingTime: null, ts: Date.now(),
      })) as typeof feed.getQuote;
      await runtime.execution(account.id).placeOrder(order(account.id, {
        symbol: 'PEPEUSDT', quantity: '3500000', stopLoss: '0.00000901234',
      }));
      // 0.35 contracts, floored to this pair's 0.1 lot.
      expect(okx.placed[0]).toMatchObject({ instId: 'PEPE-USDT-SWAP', sz: '0.3' });
      expect(okx.placed[0]!.attachAlgoOrds[0].slTriggerPx).toBe('0.0000090123');
    });

    it('keeps exponent notation out of an agent order', async () => {
      await runtime.addInstrument('PEPEUSDT');
      const account = runtime.engine.createAccount({ name: 'paper', source: 'LIVE' });
      const quote = feed.getQuote.bind(feed);
      feed.getQuote = ((symbol: SymbolId): Quote => ({ ...quote(symbol), bid: '0.00000099', ask: '0.00000101', last: '0.000001', mark: '0.000001' })) as typeof feed.getQuote;
      const tiny: AiProvider = {
        kind: 'OPENAI',
        complete: async () => ({
          text: JSON.stringify({ symbol: 'PEPEUSDT', action: 'LONG', orderType: 'MARKET', entryPrice: null, stopLoss: 0.0000009, takeProfit: null, reason: 'test', confidence: 0.5, expiresAt: null }),
          inputTokens: 1,
          outputTokens: 1,
        }),
      };
      vi.spyOn(runtime, 'providerFor').mockReturnValue(tiny);
      const agent = runtime.createAgent({ name: 'pepe', providerId: 'remote', model: 'm', symbol: 'PEPEUSDT', timeframe: '15m', strategy: '', accountId: account.id, risk: DEFAULT_RISK_SETTINGS, intervalSec: 60 });
      const record = await runtime.runner.runCycle(agent.id, 'RUN_ONCE');
      expect(record!.outcome).toBe('EXECUTED');
      const placed = runtime.store.listOrders(account.id, 10).find((o) => o.type === 'MARKET')!;
      expect(placed.attachedStopLoss).toBe('0.0000009');
    });

    it('refuses to connect if a saved pair changed on OKX', async () => {
      await runtime.addInstrument('PEPEUSDT');
      okx.pepeCtValOverride = '20000000';
      await expect(connect()).rejects.toThrow(/contract specs for PEPEUSDT/);
    });
  });

  describe('duplicate protection and ambiguous outcomes', () => {
    let accountId: string;
    beforeEach(async () => {
      accountId = (await connect()).id;
      runtime.arm(accountId, ARM_PHRASE);
    });

    it('sends one order for one idempotency key', async () => {
      const request = order(accountId);
      const first = await runtime.execution(accountId).placeOrder(request);
      const second = await runtime.execution(accountId).placeOrder(request);
      expect(second.duplicate).toBe(true);
      expect(second.order.id).toBe(first.order.id);
      expect(okx.placed).toHaveLength(1);
    });

    it('serializes concurrent orders so only one passes the position check', async () => {
      const engine = runtime.execution(accountId);
      const results = await Promise.all([engine.placeOrder(order(accountId)), engine.placeOrder(order(accountId))]);
      expect(results.map((r) => r.order.status).sort()).toEqual(['FILLED', 'REJECTED']);
      expect(okx.placed).toHaveLength(1);
    });

    it('resolves a lost response by lookup and never resends', async () => {
      okx.loseNextResponse = true;
      const result = await runtime.execution(accountId).placeOrder(order(accountId));
      expect(okx.placed).toHaveLength(1);
      expect(result.order.status).toBe('FILLED');
      expect(result.fills).toHaveLength(1);
    });

    it('keeps an order that never arrived OPEN, then closes it once OKX has no record', async () => {
      okx.dropNextRequest = true;
      const result = await runtime.execution(accountId).placeOrder(order(accountId));
      expect(result.order.status).toBe('OPEN');
      expect(okx.placed).toHaveLength(0);
      db.prepare('UPDATE orders SET created_at=? WHERE id=?').run(Date.now() - 61_000, result.order.id);
      await (runtime.execution(accountId) as unknown as { sync(): Promise<void> }).sync();
      expect(runtime.store.getOrder(result.order.id)).toMatchObject({ status: 'CANCELED', rejectReason: 'OKX has no record of this order' });
      expect(okx.placed).toHaveLength(0);
    });

    it('does not give up on an order when the lookup itself fails', async () => {
      okx.dropNextRequest = true;
      okx.lookupErrorCode = '50001';
      const result = await runtime.execution(accountId).placeOrder(order(accountId));
      db.prepare('UPDATE orders SET created_at=? WHERE id=?').run(Date.now() - 61_000, result.order.id);
      await (runtime.execution(accountId) as unknown as { sync(): Promise<void> }).sync().catch(() => undefined);
      expect(runtime.store.getOrder(result.order.id)!.status).toBe('OPEN');
    });

    it('treats a per-order refusal inside a successful response as rejected', async () => {
      okx.rejectRowNext = { sCode: '51121', sMsg: 'Order quantity must be a multiple of the lot size' };
      const result = await runtime.execution(accountId).placeOrder(order(accountId));
      expect(result.order.status).toBe('REJECTED');
      expect(result.order.rejectReason).toContain('lot size');
    });

    it('records an explicit refusal from OKX', async () => {
      okx.rejectNext = { sCode: '51008', sMsg: 'Insufficient balance' };
      const result = await runtime.execution(accountId).placeOrder(order(accountId));
      expect(result.order.status).toBe('REJECTED');
      expect(result.order.rejectReason).toContain('Insufficient balance');
    });
  });

  describe('AI agents', () => {
    it('trades automatically on OKX demo without arming', async () => {
      const account = await connect('OKX_DEMO');
      const agent = agentOn(account.id);
      runtime.assertCanRun(agent);
      runtime.runner.start(agent.id);
      await runtime.runner.tick('LIVE');
      const [decision] = runtime.store.listDecisions({ agentId: agent.id, limit: 1 });
      expect(decision).toMatchObject({ outcome: 'EXECUTED', venue: 'OKX_DEMO' });
      expect(okx.placed).toHaveLength(1);
    });

    it('trades automatically with real money once armed, sized inside the account ceiling', async () => {
      const account = await connect('OKX_LIVE');
      runtime.arm(account.id, ARM_PHRASE);
      const agent = agentOn(account.id);
      runtime.runner.start(agent.id);
      await runtime.runner.tick('LIVE');
      const [decision] = runtime.store.listDecisions({ agentId: agent.id, limit: 1 });
      expect(decision).toMatchObject({ outcome: 'EXECUTED', venue: 'OKX_LIVE' });
      expect(Number(decision!.risk!.sized!.notional)).toBeLessThanOrEqual(DEFAULT_LIVE_LIMITS.maxNotional);
      expect(okx.placed[0]!.attachAlgoOrds[0].slTriggerPx).toBe(String(PRICE * 0.99));
    });

    it('still trades automatically on a paper account', async () => {
      const account = runtime.engine.createAccount({ name: 'paper', source: 'LIVE' });
      const agent = agentOn(account.id);
      runtime.runner.start(agent.id);
      await runtime.runner.tick('LIVE');
      expect(runtime.store.listDecisions({ agentId: agent.id, limit: 1 })[0]).toMatchObject({ outcome: 'EXECUTED', venue: 'PAPER' });
      expect(okx.placed).toHaveLength(0);
    });

    it('will not schedule an agent on an unarmed real account', async () => {
      const account = await connect('OKX_LIVE');
      expect(() => runtime.assertCanRun(agentOn(account.id))).toThrow(/not armed/);
    });

    it('refuses to start an agent on an unarmed real account through the API', async () => {
      const account = await connect('OKX_LIVE');
      const agent = agentOn(account.id);
      const { app, token } = buildApp(runtime, { host: '127.0.0.1', port: 8787, dataDir: '.', allowedOrigins: ['http://localhost:8787'] });
      const response = await app.inject({ method: 'POST', url: `/api/agents/${agent.id}/start`, headers: { host: 'localhost:8787', authorization: `Bearer ${token}` } });
      expect(response.statusCode).toBe(400);
      expect(runtime.store.getAgent(agent.id)!.running).toBe(false);
      await app.close();
    });

    it('never shows the exchange credentials to the model', async () => {
      const account = await connect('OKX_LIVE');
      runtime.arm(account.id, ARM_PHRASE);
      await runtime.runner.runCycle(agentOn(account.id).id, 'RUN_ONCE');
      const seen = JSON.stringify(aiRequests);
      expect(aiRequests).toHaveLength(1);
      for (const value of Object.values(CREDS)) expect(seen).not.toContain(value);
    });

    it('pauses agents and refuses orders when sync with OKX is lost', async () => {
      const account = await connect('OKX_LIVE');
      runtime.arm(account.id, ARM_PHRASE);
      const agent = agentOn(account.id);
      runtime.runner.start(agent.id);
      okx.down = true;
      await (runtime as unknown as { syncExchanges(): Promise<void> }).syncExchanges();
      expect(runtime.store.getAgent(agent.id)).toMatchObject({ running: false, status: 'PAUSED', statusDetail: 'Lost sync with OKX' });
      okx.down = false;
      const result = await runtime.execution(account.id).placeOrder(order(account.id));
      expect(result.order.rejectReason).toContain('EXCHANGE_NOT_SYNCED');
      expect(okx.placed).toHaveLength(0);
    });

    it('Stop All disarms real accounts through the API', async () => {
      const account = await connect('OKX_LIVE');
      runtime.arm(account.id, ARM_PHRASE);
      const { app, token } = buildApp(runtime, { host: '127.0.0.1', port: 8787, dataDir: '.', allowedOrigins: ['http://localhost:8787'] });
      const response = await app.inject({ method: 'POST', url: '/api/agents/stop-all', headers: { host: 'localhost:8787', authorization: `Bearer ${token}` } });
      expect(response.statusCode).toBe(200);
      expect(runtime.exchangeStatuses()[0]).toMatchObject({ armed: false });
      await app.close();
    });
  });

  it('forgets exchange credentials and arming on restart, and never writes them to disk', async () => {
    const account = await connect('OKX_LIVE');
    runtime.arm(account.id, ARM_PHRASE);
    const agent = agentOn(account.id);
    runtime.runner.start(agent.id);
    await runtime.execution(account.id).placeOrder(order(account.id));

    for (const file of [dbPath, `${dbPath}-wal`]) {
      if (!existsSync(file)) continue;
      const bytes = readFileSync(file);
      for (const value of Object.values(CREDS)) expect(bytes.includes(Buffer.from(value))).toBe(false);
    }

    await runtime.stop();
    db.close();
    boot();

    expect(runtime.exchangeStatuses()[0]).toMatchObject({ connected: false, armed: false });
    expect(() => runtime.execution(account.id)).toThrow(/not connected/);
    expect(runtime.store.getAgent(agent.id)).toMatchObject({ running: false, status: 'PAUSED' });
    expect(() => runtime.assertCanRun(runtime.store.getAgent(agent.id)!)).toThrow(/not connected/);

    const reconnected = await connect('OKX_LIVE');
    expect(reconnected.id).toBe(account.id);
    expect(runtime.exchangeStatuses()[0]).toMatchObject({ connected: true, armed: false });
  });
});
