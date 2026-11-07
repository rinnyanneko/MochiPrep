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
  scenario('reserves a resting limit and releases it on cancel', [
    place({ clientOrderId: 'rest', type: 'LIMIT', price: '90' }),
    { op: 'cancelOrder', orderId: 'ord_3' },
    place({ clientOrderId: 'maker', type: 'LIMIT', price: '90' }),
  ]),
  scenario('fills a marketable limit as taker and never worse than its limit', [
    place({ clientOrderId: 'cross', type: 'LIMIT', price: '101' }),
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

export function runScenario(scenario: Scenario) {
  const db = openDatabase(':memory:');
  try {
    const store = new SqliteStore(db);
    const quotes = new Map<string, Quote>();
    const clock = new Map<DataSource, number>();
    let counter = 0;
    let events: EngineEvent[] = [];
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
        case 'read':
          switch (op.what) {
            case 'balance': return engine.balanceOf(op.accountId);
            case 'positions': return engine.positionViews(op.accountId);
            case 'openOrders': return engine.openOrders(op.accountId);
            case 'dailyRisk': return engine.dailyRiskSync(op.accountId);
          }
      }
    };

    const steps: Step[] = scenario.ops.map((op, i) => {
      events = [];
      const step: Step = { events };
      try {
        const result = apply(op);
        if (result !== undefined) step.result = JSON.parse(JSON.stringify(result));
      } catch (error) {
        step.error = error instanceof Error ? error.message : String(error);
      }
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
