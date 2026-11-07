import type { DatabaseSync } from 'node:sqlite';
import { auditLedger, PaperExecutionEngine, type EngineEvent } from '@mochiprep/core';
import type { DataSource, OrderRequest, PaperAccountConfig, Quote } from '@mochiprep/shared';
import { openDatabase } from '../src/db/database.js';
import { SqliteStore } from '../src/db/store.js';

/**
 * Scripted runs of the paper engine. Each op is replayed against a fresh in-memory database by
 * this file (Node) and by `crates/mochi-store/tests/paper_scenarios.rs` (Rust), and the two
 * must record the same results, errors, events and raw rows.
 */
export type Op =
  // Sets the latest quote for its source and symbol; feed=true also passes it to onQuote.
  | { op: 'quote'; quote: Quote; feed: boolean }
  // Forgets the latest quote, which is the only way to reach the engine's "no market data" paths.
  | { op: 'clearQuote'; source: DataSource; symbol: string }
  | { op: 'now'; source: DataSource; ts: number }
  | {
      op: 'createAccount';
      args: {
        name: string;
        source: DataSource;
        initialBalance?: string;
        isDemo?: boolean;
        config?: Partial<PaperAccountConfig>;
      };
    }
  | { op: 'resetAccount'; accountId: string }
  | { op: 'placeOrder'; request: OrderRequest }
  | { op: 'cancelOrder'; orderId: string }
  | { op: 'closePosition'; accountId: string; symbol: string; fraction: number; clientOrderId: string }
  | { op: 'setProtection'; accountId: string; symbol: string; stopLoss: string | null; takeProfit: string | null }
  | { op: 'recordEquity'; accountId: string }
  // Writes a trigger price straight to the orders table, to reach states the validated API
  // refuses to create (a stop and a target that overlap).
  | { op: 'setTrigger'; orderId: string; triggerPrice: string }
  | { op: 'read'; what: 'balance' | 'positions' | 'openOrders' | 'dailyRisk'; accountId: string };

export interface Scenario {
  name: string;
  /** 'every' records the raw rows after each op; 'end' only after the last one. */
  snapshot: 'every' | 'end';
  ops: Op[];
}

export interface Step {
  result?: unknown;
  error?: string;
  events: EngineEvent[];
  rows?: Record<string, unknown[]>;
}

const TABLES = ['accounts', 'orders', 'fills', 'positions', 'ledger_events', 'equity_history'];
const HOUR = 3_600_000;
const T = HOUR;
// IDs come from one counter per scenario: createAccount takes acct_1 and its deposit evt_2.
const ACCOUNT = 'acct_1';

function makeQuote(ts: number, bid = '99', ask = '101', mark = '100'): Quote {
  return { source: 'REPLAY', symbol: 'BTCUSDT', bid, ask, last: mark, mark, markEstimated: false, fundingRate: '0', nextFundingTime: null, ts };
}

const quote = (ts: number, bid?: string, ask?: string, mark?: string): Op => ({
  op: 'quote',
  quote: makeQuote(ts, bid, ask, mark),
  feed: false,
});

/** Sets the latest quote and passes it to onQuote. */
const feed = (ts: number, bid?: string, ask?: string, mark?: string, extra: Partial<Quote> = {}): Op => ({
  op: 'quote',
  quote: { ...makeQuote(ts, bid, ask, mark), ...extra },
  feed: true,
});

const read = (what: 'balance' | 'positions' | 'openOrders' | 'dailyRisk', accountId = ACCOUNT): Op => ({
  op: 'read',
  what,
  accountId,
});

const place = (overrides: Partial<OrderRequest> & Pick<OrderRequest, 'clientOrderId'>): Op => ({
  op: 'placeOrder',
  request: { accountId: ACCOUNT, symbol: 'BTCUSDT', side: 'BUY', type: 'MARKET', quantity: '1', leverage: 10, ...overrides },
});

/** The setup every paper-engine.test.ts test starts from. */
const setup = (): Op[] => [
  { op: 'now', source: 'REPLAY', ts: T },
  quote(T),
  {
    op: 'createAccount',
    args: {
      name: 'test', source: 'REPLAY', initialBalance: '10000',
      config: { takerFeeRate: '0.001', makerFeeRate: '0.0002', slippageBps: '0', impactBpsPerMillion: '0' },
    },
  },
];

const scenario = (name: string, ops: Op[], snapshot: Scenario['snapshot'] = 'every'): Scenario => ({
  name,
  snapshot,
  ops: [...setup(), ...ops],
});

export const SCENARIOS: Scenario[] = [
  // ---- from apps/server/test/paper-engine.test.ts ---------------------------------------
  scenario('charges entry and exit taker fees and realizes long and short PnL', [
    place({ clientOrderId: 'long-open' }),
    quote(T, '109', '110', '110'),
    place({ clientOrderId: 'long-close', side: 'SELL', reduceOnly: true }),
    place({ clientOrderId: 'short-open', side: 'SELL' }),
    quote(T, '99', '100', '100'),
    place({ clientOrderId: 'short-close', side: 'BUY', reduceOnly: true }),
  ]),
  scenario('releases margin proportionally on partial close', [
    place({ clientOrderId: 'open', quantity: '2', leverage: 4 }),
    quote(T, '105', '106', '105'),
    place({ clientOrderId: 'partial', side: 'SELL', quantity: '0.5', reduceOnly: true, leverage: 4 }),
  ]),
  scenario('is idempotent by clientOrderId and does not duplicate a fill', [
    place({ clientOrderId: 'same' }),
    place({ clientOrderId: 'same' }),
  ]),
  scenario('rolls back an insufficient-margin attempt and leaves only a rejected order', [
    place({ clientOrderId: 'too-big', quantity: '10000', leverage: 1 }),
  ]),
  scenario('reserves a resting limit, releases on cancel, and fills maker at its limit', [
    place({ clientOrderId: 'rest', type: 'LIMIT', price: '90' }),
    read('balance'),
    { op: 'cancelOrder', orderId: 'ord_3' },
    read('balance'),
    place({ clientOrderId: 'maker', type: 'LIMIT', price: '90' }),
    read('openOrders'),
    feed(T + 1, '89', '90', '90'),
    read('positions'),
    read('balance'),
  ]),
  scenario('fills a marketable limit as taker and never worse than its limit', [
    place({ clientOrderId: 'cross', type: 'LIMIT', price: '101' }),
  ]),
  scenario('charges the maker rate when a resting reduce-only exit fills', [
    place({ clientOrderId: 'entry' }),
    place({ clientOrderId: 'maker-exit', side: 'SELL', type: 'LIMIT', price: '110', reduceOnly: true }),
    feed(T + 1, '110', '111', '105'),
    read('balance'),
  ]),
  scenario('caps reduce-only quantity and rejects it when flat', [
    place({ clientOrderId: 'flat', side: 'SELL', reduceOnly: true }),
    place({ clientOrderId: 'open' }),
    place({ clientOrderId: 'cap', side: 'SELL', quantity: '3', reduceOnly: true }),
  ]),
  scenario('splits a flip into close and open economics with correct fee and margin', [
    place({ clientOrderId: 'open' }),
    quote(T, '109', '110', '110'),
    place({ clientOrderId: 'flip', side: 'SELL', quantity: '2' }),
  ]),
  scenario('triggers protection at the next observed quote and cancels its sibling', [
    place({ clientOrderId: 'protected', stopLoss: '95', takeProfit: '110' }),
    read('positions'),
    feed(T + 1, '93', '94', '94'),
    read('positions'),
    read('openOrders'),
  ]),
  scenario('evaluates stop-loss before take-profit when both are triggered on one quote', [
    place({ clientOrderId: 'protected', stopLoss: '95', takeProfit: '110' }),
    // Ids: ord_3 is the entry, protect_8/ord_9 the stop and protect_10/ord_11 the target.
    { op: 'setTrigger', orderId: 'ord_9', triggerPrice: '105' },
    { op: 'setTrigger', orderId: 'ord_11', triggerPrice: '95' },
    feed(T + 1, '99', '101', '100'),
  ]),
  scenario('applies positive and negative funding once per boundary with opposite long/short direction', [
    place({ clientOrderId: 'long' }),
    { op: 'now', source: 'REPLAY', ts: 8 * HOUR },
    feed(8 * HOUR, '99', '101', '100', { fundingRate: '0.001' }),
    feed(8 * HOUR, '99', '101', '100', { fundingRate: '0.001' }),
    read('balance'),
    place({ clientOrderId: 'close', side: 'SELL', reduceOnly: true }),
    place({ clientOrderId: 'short', side: 'SELL' }),
    { op: 'now', source: 'REPLAY', ts: 16 * HOUR },
    feed(16 * HOUR, '99', '101', '100', { fundingRate: '-0.001' }),
    read('balance'),
    read('dailyRisk'),
  ]),
  scenario('credits shorts at positive funding and longs at negative funding', [
    {
      op: 'createAccount',
      args: { name: 'funding-credit', source: 'REPLAY', initialBalance: '10000', config: { slippageBps: '0', impactBpsPerMillion: '0' } },
    },
    place({ clientOrderId: 'short', side: 'SELL', accountId: 'acct_3' }),
    { op: 'now', source: 'REPLAY', ts: 8 * HOUR },
    feed(8 * HOUR, undefined, undefined, undefined, { fundingRate: '0.001' }),
    read('balance', 'acct_3'),
    place({ clientOrderId: 'close-short', side: 'BUY', reduceOnly: true, accountId: 'acct_3' }),
    place({ clientOrderId: 'long', accountId: 'acct_3' }),
    { op: 'now', source: 'REPLAY', ts: 16 * HOUR },
    feed(16 * HOUR, undefined, undefined, undefined, { fundingRate: '-0.001' }),
    read('balance', 'acct_3'),
  ]),
  scenario('liquidates on mark, caps gap loss at isolated margin, and cancels reduce-only orders', [
    place({ clientOrderId: 'open', stopLoss: '80' }),
    place({ clientOrderId: 'reduce', side: 'SELL', type: 'LIMIT', price: '120', reduceOnly: true }),
    feed(T + 1, '99', '101', '1', { last: '100' }),
    read('balance'),
  ]),
  scenario('takes exactly the isolated margin from the wallet when a gap passes the liquidation price', [
    place({ clientOrderId: 'open' }),
    feed(T + 1, '1', '2', '1'),
    read('balance'),
  ]),
  scenario('forfeits the maintenance remainder as a penalty when liquidated at the threshold', [
    place({ clientOrderId: 'open' }),
    read('positions'),
    // 91.405 is this long's liquidation price: entry 101, margin 10.1, maintenance 0.5%.
    feed(T + 1, '90', '92', '91.405'),
    read('balance'),
  ]),
  scenario('liquidates a short when mark rises through its threshold, not when it falls', [
    place({ clientOrderId: 'open', side: 'SELL' }),
    feed(T + 1, '89', '91', '90'),
    read('positions'),
    feed(T + 2, '500', '501', '500'),
    read('balance'),
  ]),
  // The fill settles at the liquidation price rounded to the 0.1 tick. At low prices that
  // rounding can move further than the maintenance buffer (entry x 0.5%), so the raw loss
  // exceeds the margin and the cap binds: the penalty is zero and no penalty row is written.
  scenario('liquidation cap binds when tick rounding exceeds maintenance (LONG)', [
    // Entry 3.7, margin 0.37, maintenance 0.0185, liquidation 3.3485, filled at 3.3: raw -0.4.
    quote(T, '3.6', '3.7', '3.7'),
    place({ clientOrderId: 'open' }),
    feed(T + 1, '2.9', '3.1', '3'),
    read('balance'),
    read('positions'),
  ]),
  scenario('liquidation cap binds when tick rounding exceeds maintenance (SHORT)', [
    // Entry 3.7, margin 0.37, maintenance 0.0185, liquidation 4.0515, filled at 4.1: raw -0.4.
    quote(T, '3.7', '3.8', '3.7'),
    place({ clientOrderId: 'open', side: 'SELL' }),
    feed(T + 1, '4.1', '4.3', '4.2'),
    read('balance'),
    read('positions'),
  ]),
  scenario('liquidation loss equals the margin exactly when rounding lands on the bankruptcy price', [
    // Entry 5, margin 0.5, liquidation 4.525, filled at 4.5: raw -0.5 = -margin, penalty 0.
    quote(T, '4.9', '5', '5'),
    place({ clientOrderId: 'open' }),
    feed(T + 1, '4.4', '4.6', '4.5'),
    read('balance'),
    read('positions'),
  ]),
  scenario('triggers a short stop only when price rises to it, filling at the ask', [
    place({ clientOrderId: 'open', side: 'SELL', stopLoss: '105', takeProfit: '90' }),
    feed(T + 1, '98', '100', '99'),
    read('positions'),
    feed(T + 2, '105', '107', '106'),
    read('positions'),
  ]),
  scenario('triggers a short take-profit only when price falls to it', [
    place({ clientOrderId: 'open', side: 'SELL', stopLoss: '105', takeProfit: '90' }),
    feed(T + 1, '89', '91', '90'),
    read('balance'),
  ]),
  scenario('averages entry when adding, and a stop then closes the whole enlarged position', [
    place({ clientOrderId: 'first', leverage: 2, stopLoss: '95' }),
    quote(T, '119', '121', '120'),
    place({ clientOrderId: 'second', leverage: 2 }),
    read('positions'),
    feed(T + 1, '93', '95', '94'),
    read('positions'),
    read('balance'),
  ]),
  scenario('refuses to fill against a quote older than a minute', [
    quote(T - 60_001),
    place({ clientOrderId: 'stale' }),
  ]),

  // ---- error paths ----------------------------------------------------------------------
  scenario('cancel an unknown order', [{ op: 'cancelOrder', orderId: 'ord_missing' }], 'end'),
  scenario('cancel a filled order', [place({ clientOrderId: 'open' }), { op: 'cancelOrder', orderId: 'ord_3' }]),
  scenario('setProtection with no position, no quote, and a stop on the wrong side', [
    { op: 'setProtection', accountId: ACCOUNT, symbol: 'BTCUSDT', stopLoss: '95', takeProfit: null },
    place({ clientOrderId: 'open' }),
    { op: 'setProtection', accountId: ACCOUNT, symbol: 'BTCUSDT', stopLoss: '105', takeProfit: null },
    { op: 'setProtection', accountId: ACCOUNT, symbol: 'BTCUSDT', stopLoss: null, takeProfit: '90' },
    { op: 'clearQuote', source: 'REPLAY', symbol: 'BTCUSDT' },
    { op: 'setProtection', accountId: ACCOUNT, symbol: 'BTCUSDT', stopLoss: '95', takeProfit: null },
    { op: 'setProtection', accountId: 'acct_missing', symbol: 'BTCUSDT', stopLoss: '95', takeProfit: null },
  ]),
  scenario('rejects leverage 0 and 101', [
    place({ clientOrderId: 'zero', leverage: 0 }),
    place({ clientOrderId: 'too-high', leverage: 101 }),
  ]),
  scenario('rejects a quantity below the minimum', [place({ clientOrderId: 'dust', quantity: '0.00005' })]),
  scenario('rejects a limit price that rounds to zero, and a limit with no price', [
    place({ clientOrderId: 'tiny', type: 'LIMIT', price: '0.04' }),
    place({ clientOrderId: 'none', type: 'LIMIT' }),
    place({ clientOrderId: 'empty', type: 'LIMIT', price: '' }),
  ]),

  // ---- the rest of the order path ---------------------------------------------------------
  scenario('rejects without market data, and fails on an unknown account or instrument', [
    place({ clientOrderId: 'eth', symbol: 'ETHUSDT' }),
    place({ clientOrderId: 'ghost', accountId: 'acct_missing' }),
    place({ clientOrderId: 'doge', symbol: 'DOGEUSDT' }),
    place({ clientOrderId: 'after' }),
  ]),
  scenario('sets, replaces and removes protection on an open position', [
    place({ clientOrderId: 'open' }),
    { op: 'setProtection', accountId: ACCOUNT, symbol: 'BTCUSDT', stopLoss: '95', takeProfit: '110' },
    { op: 'setProtection', accountId: ACCOUNT, symbol: 'BTCUSDT', stopLoss: '90', takeProfit: null },
    { op: 'setProtection', accountId: ACCOUNT, symbol: 'BTCUSDT', stopLoss: null, takeProfit: null },
  ]),
  scenario('attaches protection on entry, rejects misplaced or reduce-only protection', [
    place({ clientOrderId: 'bad-stop', stopLoss: '102' }),
    place({ clientOrderId: 'bad-target', side: 'SELL', takeProfit: '100' }),
    place({ clientOrderId: 'protected', stopLoss: '95', takeProfit: '110' }),
    place({ clientOrderId: 'reduce-sl', side: 'SELL', reduceOnly: true, stopLoss: '120' }),
    place({ clientOrderId: 'add', quantity: '0.5', stopLoss: '96' }),
    place({ clientOrderId: 'exit', side: 'SELL', quantity: '1.5', reduceOnly: true }),
  ]),
  scenario('a flip carries the attached protection to the new side', [
    place({ clientOrderId: 'open', stopLoss: '95' }),
    place({ clientOrderId: 'flip', side: 'SELL', quantity: '2', stopLoss: '110', takeProfit: '90' }),
  ]),
  scenario('a resting reduce-only limit reserves nothing and is canceled when the position closes', [
    place({ clientOrderId: 'open' }),
    place({ clientOrderId: 'exit-limit', side: 'SELL', type: 'LIMIT', price: '120', reduceOnly: true }),
    place({ clientOrderId: 'resting-buy', type: 'LIMIT', price: '95', quantity: '0.5' }),
    place({ clientOrderId: 'close', side: 'SELL', reduceOnly: true }),
  ]),
  scenario('rejects a resting limit whose reservation exceeds the available balance', [
    place({ clientOrderId: 'huge-limit', type: 'LIMIT', price: '90', quantity: '200', leverage: 1 }),
    place({ clientOrderId: 'ok-limit', type: 'LIMIT', price: '90', quantity: '100', leverage: 1 }),
    place({ clientOrderId: 'blocked', quantity: '10', leverage: 1 }),
  ]),
  scenario('closes a position by fraction', [
    { op: 'closePosition', accountId: ACCOUNT, symbol: 'BTCUSDT', fraction: 0.5, clientOrderId: 'nothing' },
    place({ clientOrderId: 'open', quantity: '0.3333', stopLoss: '90' }),
    { op: 'closePosition', accountId: ACCOUNT, symbol: 'BTCUSDT', fraction: 0.5, clientOrderId: 'half' },
    { op: 'closePosition', accountId: ACCOUNT, symbol: 'BTCUSDT', fraction: 0.0001, clientOrderId: 'dust' },
    { op: 'closePosition', accountId: ACCOUNT, symbol: 'BTCUSDT', fraction: 1, clientOrderId: 'rest' },
  ]),
  scenario('slippage and impact move the market fill and round against the trader', [
    {
      op: 'createAccount',
      args: { name: 'defaults', source: 'REPLAY' },
    },
    {
      op: 'placeOrder',
      request: { accountId: 'acct_3', clientOrderId: 'slip-buy', symbol: 'BTCUSDT', side: 'BUY', type: 'MARKET', quantity: '3', leverage: 7 },
    },
    {
      op: 'placeOrder',
      request: { accountId: 'acct_3', clientOrderId: 'slip-sell', symbol: 'BTCUSDT', side: 'SELL', type: 'MARKET', quantity: '1.2345', leverage: 7, reduceOnly: true },
    },
    {
      op: 'placeOrder',
      request: { accountId: 'acct_3', clientOrderId: 'capped', symbol: 'BTCUSDT', side: 'BUY', type: 'LIMIT', price: '101.05', quantity: '1', leverage: 3 },
    },
  ]),
  scenario('resets an account, and creates live, demo and empty accounts', [
    place({ clientOrderId: 'open' }),
    place({ clientOrderId: 'rest', type: 'LIMIT', price: '90' }),
    { op: 'now', source: 'REPLAY', ts: T + 5000 },
    { op: 'resetAccount', accountId: ACCOUNT },
    { op: 'resetAccount', accountId: 'acct_missing' },
    { op: 'now', source: 'LIVE', ts: 1_700_000_000_000 },
    { op: 'createAccount', args: { name: 'live demo', source: 'LIVE', initialBalance: '2500.5', isDemo: true, config: { slippageBps: '2' } } },
    { op: 'createAccount', args: { name: 'empty', source: 'REPLAY', initialBalance: '0' } },
    place({ clientOrderId: 'after-reset' }),
  ]),
  scenario('keeps the ledger auditable through a fixed pseudo-random trade sequence', pseudoRandomOps(), 'end'),

  // ---- market events and read models ------------------------------------------------------
  scenario('cancels a resting limit that can no longer be margined when it fills', [
    {
      op: 'createAccount',
      args: {
        name: 'thin', source: 'REPLAY', initialBalance: '100',
        config: { takerFeeRate: '0.001', makerFeeRate: '0.0002', slippageBps: '0', impactBpsPerMillion: '0' },
      },
    },
    place({ clientOrderId: 'long', accountId: 'acct_3', quantity: '0.4', leverage: 1 }),
    place({ clientOrderId: 'add', accountId: 'acct_3', type: 'LIMIT', price: '50', leverage: 1 }),
    read('balance', 'acct_3'),
    { op: 'now', source: 'REPLAY', ts: 8 * HOUR },
    // Funding drains the wallet first, so the limit crossing on this same quote is short of margin.
    feed(8 * HOUR, '49', '50', '100', { fundingRate: '0.3' }),
    read('balance', 'acct_3'),
    read('openOrders', 'acct_3'),
  ]),
  scenario('records equity and daily risk across a UTC day boundary', [
    place({ clientOrderId: 'open' }),
    quote(T, '104', '106', '105'),
    { op: 'recordEquity', accountId: ACCOUNT },
    { op: 'now', source: 'REPLAY', ts: T + 30 * 60_000 },
    { op: 'recordEquity', accountId: ACCOUNT },
    { op: 'recordEquity', accountId: ACCOUNT },
    read('dailyRisk'),
    { op: 'now', source: 'REPLAY', ts: 8 * HOUR },
    // No funding rate: the boundary passes with nothing paid.
    feed(8 * HOUR, '104', '106', '105', { fundingRate: null }),
    { op: 'now', source: 'REPLAY', ts: 25 * HOUR },
    feed(25 * HOUR, '109', '111', '110', { fundingRate: '0.0001' }),
    read('dailyRisk'),
    read('balance'),
    read('positions'),
    { op: 'recordEquity', accountId: ACCOUNT },
    { op: 'recordEquity', accountId: 'acct_missing' },
    read('dailyRisk', 'acct_missing'),
  ]),
  scenario('reports a stale quote in whole seconds', [
    quote(T - 90_499),
    place({ clientOrderId: 'stale-90' }),
    quote(T - 90_500),
    place({ clientOrderId: 'stale-91' }),
    quote(T - 60_000),
    place({ clientOrderId: 'just-fresh' }),
  ]),
];

/** Raw rows with SQLite's own types, so TEXT '1' and INTEGER 1 stay different. */
function snapshot(db: DatabaseSync): Record<string, unknown[]> {
  const rows: Record<string, unknown[]> = {};
  for (const table of TABLES) rows[table] = db.prepare(`SELECT * FROM ${table} ORDER BY rowid`).all();
  return rows;
}

/**
 * The fixed pseudo-random run from paper-engine.test.ts. What it trades depends on whether a
 * position is open after each quote, so the ops are chosen by driving a scratch engine.
 */
function pseudoRandomOps(): Op[] {
  const runner = createRunner();
  try {
    for (const op of setup()) runner.apply(op);
    const ops: Op[] = [];
    const push = (op: Op) => {
      ops.push(op);
      runner.apply(op);
    };
    let seed = 0x12345678;
    const random = () => ((seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0) / 2 ** 32);
    let now = T;
    for (let i = 0; i < 80; i++) {
      const mid = 80 + Math.floor(random() * 40);
      now += 60_000;
      push({ op: 'now', source: 'REPLAY', ts: now });
      push(feed(now, String(mid - 1), String(mid + 1), String(mid)));
      const position = runner.store.getOpenPosition(ACCOUNT, 'BTCUSDT');
      if (position && random() < 0.55) {
        const side = position.side === 'LONG' ? 'SELL' : 'BUY';
        push(place({ clientOrderId: `r-${i}`, side, quantity: random() < 0.5 ? '0.25' : '2', reduceOnly: true }));
      } else if (!position) {
        push(place({ clientOrderId: `r-${i}`, side: random() < 0.5 ? 'BUY' : 'SELL', quantity: '0.5', leverage: 5 }));
      }
    }
    return ops;
  } finally {
    runner.db.close();
  }
}

/** A fresh in-memory engine plus the quote book, clock and event log the ops drive. */
function createRunner() {
  const db = openDatabase(':memory:');
  const store = new SqliteStore(db);
  const quotes = new Map<string, Quote>();
  const clock = new Map<DataSource, number>();
  let counter = 0;
  const events: EngineEvent[] = [];
  const engine = new PaperExecutionEngine({
    store,
    getQuote: (source, symbol) => quotes.get(`${source}:${symbol}`),
    now: (source) => clock.get(source) ?? 0,
    newId: (prefix) => `${prefix}_${++counter}`,
    // A JSON round trip freezes the event as it was emitted, like the Rust side's owned copy.
    onEvent: (event) => events.push(JSON.parse(JSON.stringify(event)) as EngineEvent),
  });

  const apply = (op: Op): unknown => {
    switch (op.op) {
      case 'quote':
        quotes.set(`${op.quote.source}:${op.quote.symbol}`, op.quote);
        if (op.feed) engine.onQuote(op.quote);
        return undefined;
      case 'clearQuote':
        quotes.delete(`${op.source}:${op.symbol}`);
        return undefined;
      case 'now':
        clock.set(op.source, op.ts);
        return undefined;
      case 'createAccount':
        return engine.createAccount(op.args);
      case 'resetAccount':
        return engine.resetAccount(op.accountId);
      case 'placeOrder':
        return engine.placeOrderSync(op.request);
      case 'cancelOrder':
        engine.cancelOrderSync(op.orderId);
        return undefined;
      case 'closePosition':
        return engine.closePosition(op.accountId, op.symbol, op.fraction, op.clientOrderId);
      case 'setProtection':
        engine.setProtection(op.accountId, op.symbol, op.stopLoss, op.takeProfit);
        return undefined;
      case 'recordEquity':
        engine.recordEquity(op.accountId);
        return undefined;
      case 'setTrigger':
        db.prepare('UPDATE orders SET trigger_price=? WHERE id=?').run(op.triggerPrice, op.orderId);
        return undefined;
      case 'read':
        switch (op.what) {
          case 'balance': return engine.balanceOf(op.accountId);
          case 'positions': return engine.positionViews(op.accountId);
          case 'openOrders': return engine.openOrders(op.accountId);
          case 'dailyRisk': return engine.dailyRiskSync(op.accountId);
        }
    }
  };
  /** The events emitted since the last call. */
  const takeEvents = () => events.splice(0);
  return { db, store, apply, takeEvents };
}

export function runScenario(scenario: Scenario) {
  const runner = createRunner();
  const { db, store } = runner;
  try {
    const steps: Step[] = scenario.ops.map((op, i) => {
      const step: Step = { events: [] };
      try {
        const result = runner.apply(op);
        if (result !== undefined) step.result = JSON.parse(JSON.stringify(result));
      } catch (error) {
        step.error = error instanceof Error ? error.message : String(error);
      }
      step.events = runner.takeEvents();
      if (scenario.snapshot === 'every' || i === scenario.ops.length - 1) step.rows = snapshot(db);
      return step;
    });

    const audit: Record<string, string[]> = {};
    for (const account of store.listAccounts()) audit[account.id] = auditLedger(account, store.listLedger(account.id));
    return { ...scenario, steps, audit };
  } finally {
    db.close();
  }
}

export function paperScenariosFixture() {
  return SCENARIOS.map(runScenario);
}
