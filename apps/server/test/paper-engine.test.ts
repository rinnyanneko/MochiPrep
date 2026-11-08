import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import type { DatabaseSync } from 'node:sqlite';
import { AgentInputSchema, DEFAULT_RISK_SETTINGS, ProtectionSchema, type OrderRequest, type Quote } from '@mochiprep/shared';
import { auditLedger, d, PaperExecutionEngine } from '@mochiprep/core';
import { openDatabase } from '../src/db/database';
import { SqliteStore } from '../src/db/store';

const HOUR = 3_600_000;

describe('PaperExecutionEngine with SQLite', () => {
  let db: DatabaseSync;
  let store: SqliteStore;
  let engine: PaperExecutionEngine;
  let now: number;
  let q: Quote;
  let seq: number;
  let accountId: string;

  beforeEach(() => {
    db = openDatabase(':memory:');
    store = new SqliteStore(db);
    now = HOUR;
    seq = 0;
    q = makeQuote(now);
    engine = new PaperExecutionEngine({
      store,
      getQuote: () => q,
      now: () => now,
      newId: (prefix) => `${prefix}_${++seq}`,
    });
    accountId = engine.createAccount({
      name: 'test', source: 'REPLAY', initialBalance: '10000',
      config: { takerFeeRate: '0.001', makerFeeRate: '0.0002', slippageBps: '0', impactBpsPerMillion: '0' },
    }).id;
  });

  afterEach(() => db.close());

  const request = (overrides: Partial<OrderRequest> = {}): OrderRequest => ({
    accountId, clientOrderId: `client-${seq + 1}`, symbol: 'BTCUSDT', side: 'BUY', type: 'MARKET',
    quantity: '1', leverage: 10, ...overrides,
  });

  it('charges entry and exit taker fees and realizes long and short PnL', () => {
    engine.placeOrderSync(request({ clientOrderId: 'long-open' }));
    q = makeQuote(now, '109', '110', '110');
    engine.placeOrderSync(request({ clientOrderId: 'long-close', side: 'SELL', reduceOnly: true }));
    let balance = engine.balanceOf(accountId);
    expect(balance.realizedPnl).toBe('8');
    expect(balance.feesPaid).toBe('0.21');

    engine.placeOrderSync(request({ clientOrderId: 'short-open', side: 'SELL' }));
    q = makeQuote(now, '99', '100', '100');
    engine.placeOrderSync(request({ clientOrderId: 'short-close', side: 'BUY', reduceOnly: true }));
    balance = engine.balanceOf(accountId);
    expect(balance.realizedPnl).toBe('17');
    expect(balance.feesPaid).toBe('0.419');
  });

  it('releases margin proportionally on partial close and refreshes the remainder liquidation price', () => {
    engine.placeOrderSync(request({ clientOrderId: 'open', quantity: '2', leverage: 4 }));
    const before = store.getOpenPosition(accountId, 'BTCUSDT')!;
    q = makeQuote(now, '105', '106', '105');
    engine.placeOrderSync(request({ clientOrderId: 'partial', side: 'SELL', quantity: '0.5', reduceOnly: true, leverage: 4 }));
    const after = store.getOpenPosition(accountId, 'BTCUSDT')!;
    expect(after.quantity).toBe('1.5');
    expect(after.margin).toBe('37.875');
    expect(after.liquidationPrice).toBe(before.liquidationPrice);
    expect(after.liquidationPrice).toBe('76.255');
    expect(engine.balanceOf(accountId).usedMargin).toBe('37.875');
  });

  it('is idempotent by clientOrderId and does not duplicate a fill', () => {
    const first = engine.placeOrderSync(request({ clientOrderId: 'same' }));
    const second = engine.placeOrderSync(request({ clientOrderId: 'same' }));
    expect(second.duplicate).toBe(true);
    expect(second.order.id).toBe(first.order.id);
    expect(store.listFills(accountId, 10)).toHaveLength(1);
  });

  it('rolls back an insufficient-margin attempt and leaves only a rejected order', () => {
    const ledgerBefore = store.listLedger(accountId);
    const result = engine.placeOrderSync(request({ clientOrderId: 'too-big', quantity: '10000', leverage: 1 }));
    expect(result.order.status).toBe('REJECTED');
    expect(result.order.rejectReason).toContain('INSUFFICIENT_MARGIN');
    expect(store.listFills(accountId, 10)).toHaveLength(0);
    expect(store.listOpenPositions(accountId)).toHaveLength(0);
    expect(store.listLedger(accountId)).toEqual(ledgerBefore);
    expect(store.listOrders(accountId, 10)).toHaveLength(1);
  });

  it('reserves a resting limit, releases on cancel, and fills maker at its limit', () => {
    const resting = engine.placeOrderSync(request({ clientOrderId: 'rest', type: 'LIMIT', price: '90' }));
    expect(resting.order.status).toBe('OPEN');
    expect(resting.order.reservedMargin).toBe('9.018');
    expect(engine.balanceOf(accountId).orderMargin).toBe('9.018');
    engine.cancelOrderSync(resting.order.id);
    expect(engine.balanceOf(accountId).orderMargin).toBe('0');

    const maker = engine.placeOrderSync(request({ clientOrderId: 'maker', type: 'LIMIT', price: '90' }));
    q = makeQuote(now + 1, '89', '90', '90');
    engine.onQuote(q);
    const fill = store.listFillsByOrder(maker.order.id)[0]!;
    expect(fill.liquidity).toBe('MAKER');
    expect(fill.price).toBe('90');
    expect(fill.fee).toBe('0.018');
  });

  it('fills a marketable limit as taker and never worse than its limit', () => {
    const result = engine.placeOrderSync(request({ clientOrderId: 'cross', type: 'LIMIT', price: '101' }));
    expect(result.fills[0]!.liquidity).toBe('TAKER');
    expect(d(result.fills[0]!.price).lte('101')).toBe(true);
  });

  it('charges the maker rate when a resting reduce-only exit fills', () => {
    engine.placeOrderSync(request({ clientOrderId: 'entry' }));
    const exit = engine.placeOrderSync(request({
      clientOrderId: 'maker-exit', side: 'SELL', type: 'LIMIT', price: '110', reduceOnly: true,
    }));
    q = makeQuote(now + 1, '110', '111', '105');
    engine.onQuote(q);
    const fill = store.listFillsByOrder(exit.order.id)[0]!;
    expect(fill).toMatchObject({ liquidity: 'MAKER', price: '110', fee: '0.022', realizedPnl: '9' });
    expect(engine.balanceOf(accountId).feesPaid).toBe('0.123');
  });

  it('caps reduce-only quantity and rejects it when flat', () => {
    const flat = engine.placeOrderSync(request({ clientOrderId: 'flat', side: 'SELL', reduceOnly: true }));
    expect(flat.order.status).toBe('REJECTED');
    engine.placeOrderSync(request({ clientOrderId: 'open' }));
    const close = engine.placeOrderSync(request({ clientOrderId: 'cap', side: 'SELL', quantity: '3', reduceOnly: true }));
    expect(close.order.quantity).toBe('1');
    expect(store.listOpenPositions(accountId)).toHaveLength(0);
  });

  it('splits a flip into close and open economics with correct fee and margin', () => {
    engine.placeOrderSync(request({ clientOrderId: 'open' }));
    q = makeQuote(now, '109', '110', '110');
    const flip = engine.placeOrderSync(request({ clientOrderId: 'flip', side: 'SELL', quantity: '2' }));
    expect(flip.fills[0]!.fee).toBe('0.218');
    expect(flip.fills[0]!.realizedPnl).toBe('8');
    const position = store.getOpenPosition(accountId, 'BTCUSDT')!;
    expect(position.side).toBe('SHORT');
    expect(position.quantity).toBe('1');
    expect(position.entryPrice).toBe('109');
    expect(position.margin).toBe('10.9');
    expect(engine.balanceOf(accountId).realizedPnl).toBe('8');
  });

  it('triggers protection at the next observed quote and cancels its sibling', () => {
    engine.placeOrderSync(request({ clientOrderId: 'protected', stopLoss: '95', takeProfit: '110' }));
    q = makeQuote(now + 1, '93', '94', '94');
    engine.onQuote(q);
    const close = store.listFills(accountId, 10).find((f) => f.side === 'SELL')!;
    expect(close.price).toBe('93');
    expect(store.listOpenPositions(accountId)).toHaveLength(0);
    expect(store.listOpenOrders(accountId)).toHaveLength(0);
    expect(store.listOrders(accountId, 10).filter((o) => o.type === 'TAKE_PROFIT_MARKET')[0]!.status).toBe('CANCELED');
  });

  it('evaluates stop-loss before take-profit when both are triggered on one quote', () => {
    engine.placeOrderSync(request({ clientOrderId: 'protected', stopLoss: '95', takeProfit: '110' }));
    const orders = store.listOpenOrders(accountId);
    const stop = orders.find((o) => o.type === 'STOP_MARKET')!;
    const takeProfit = orders.find((o) => o.type === 'TAKE_PROFIT_MARKET')!;
    // This overlapping trigger state cannot be created through the validated API, but verifies
    // deterministic pessimistic ordering if persisted state ever contains it.
    db.prepare('UPDATE orders SET trigger_price=? WHERE id=?').run('105', stop.id);
    db.prepare('UPDATE orders SET trigger_price=? WHERE id=?').run('95', takeProfit.id);
    q = makeQuote(now + 1, '99', '101', '100');
    engine.onQuote(q);
    expect(store.getOrder(stop.id)!.status).toBe('FILLED');
    expect(store.getOrder(takeProfit.id)!.status).toBe('CANCELED');
  });

  it('applies positive and negative funding once per boundary with opposite long/short direction', () => {
    engine.placeOrderSync(request({ clientOrderId: 'long' }));
    now = 8 * HOUR;
    q = { ...makeQuote(8 * HOUR, '99', '101', '100'), fundingRate: '0.001' };
    engine.onQuote(q);
    engine.onQuote(q);
    expect(engine.balanceOf(accountId).fundingPaid).toBe('0.1');
    expect(store.listLedger(accountId).filter((e) => e.type === 'FUNDING')).toHaveLength(1);

    engine.placeOrderSync(request({ clientOrderId: 'close', side: 'SELL', reduceOnly: true }));
    engine.placeOrderSync(request({ clientOrderId: 'short', side: 'SELL' }));
    now = 16 * HOUR;
    q = { ...makeQuote(16 * HOUR, '99', '101', '100'), fundingRate: '-0.001' };
    engine.onQuote(q);
    expect(engine.balanceOf(accountId).fundingPaid).toBe('0.2');
  });

  it('credits shorts at positive funding and longs at negative funding', () => {
    accountId = engine.createAccount({ name: 'funding-credit', source: 'REPLAY', initialBalance: '10000', config: { slippageBps: '0', impactBpsPerMillion: '0' } }).id;
    engine.placeOrderSync(request({ clientOrderId: 'short', side: 'SELL' }));
    now = 8 * HOUR;
    q = { ...makeQuote(now), fundingRate: '0.001' };
    engine.onQuote(q);
    expect(engine.balanceOf(accountId).fundingPaid).toBe('-0.1');

    engine.placeOrderSync(request({ clientOrderId: 'close-short', side: 'BUY', reduceOnly: true }));
    engine.placeOrderSync(request({ clientOrderId: 'long' }));
    now = 16 * HOUR;
    q = { ...makeQuote(now), fundingRate: '-0.001' };
    engine.onQuote(q);
    expect(engine.balanceOf(accountId).fundingPaid).toBe('-0.2');
  });

  it('liquidates on mark, caps gap loss at isolated margin, and cancels reduce-only orders', () => {
    engine.placeOrderSync(request({ clientOrderId: 'open', stopLoss: '80' }));
    const position = store.getOpenPosition(accountId, 'BTCUSDT')!;
    const extra = engine.placeOrderSync(request({ clientOrderId: 'reduce', side: 'SELL', type: 'LIMIT', price: '120', reduceOnly: true }));
    q = { ...makeQuote(now + 1, '99', '101', '100'), last: '100', mark: '1' };
    engine.onQuote(q);
    const closed = store.listClosedPositions(accountId, 10)[0]!;
    expect(closed.status).toBe('LIQUIDATED');
    expect(closed.realizedPnl).toBe(d(position.margin).neg().toFixed());
    expect(store.getOrder(extra.order.id)!.status).toBe('CANCELED');
    expect(engine.balanceOf(accountId).usedMargin).toBe('0');
  });

  const expectAudited = () => expect(auditLedger(store.getAccount(accountId)!, store.listLedger(accountId))).toEqual([]);

  it('takes exactly the isolated margin from the wallet when a gap passes the liquidation price', () => {
    const entry = engine.placeOrderSync(request({ clientOrderId: 'open' })).fills[0]!;
    const { margin } = store.getOpenPosition(accountId, 'BTCUSDT')!;
    q = makeQuote(now + 1, '1', '2', '1');
    engine.onQuote(q);
    const balance = engine.balanceOf(accountId);
    expect(balance.walletBalance).toBe(d('10000').minus(entry.fee).minus(margin).toFixed());
    expect(balance.realizedPnl).toBe(d(margin).neg().toFixed());
    // Settled at the liquidation price, so a gap can never turn the penalty into a credit.
    const penalty = store.listLedger(accountId).find((e) => e.type === 'LIQUIDATION_PENALTY')!;
    expect(d(penalty.amount).lt(0)).toBe(true);
    expectAudited();
  });

  it('forfeits the maintenance remainder as a penalty when liquidated at the threshold', () => {
    const entry = engine.placeOrderSync(request({ clientOrderId: 'open' })).fills[0]!;
    const position = store.getOpenPosition(accountId, 'BTCUSDT')!;
    q = makeQuote(now + 1, '90', '92', position.liquidationPrice);
    engine.onQuote(q);
    expect(store.listClosedPositions(accountId, 1)[0]!.status).toBe('LIQUIDATED');
    expect(engine.balanceOf(accountId).walletBalance).toBe(d('10000').minus(entry.fee).minus(position.margin).toFixed());
    const penalty = store.listLedger(accountId).find((e) => e.type === 'LIQUIDATION_PENALTY')!;
    expect(d(penalty.amount).lt(0)).toBe(true);
    expectAudited();
  });

  it('liquidates a short when mark rises through its threshold, not when it falls', () => {
    const entry = engine.placeOrderSync(request({ clientOrderId: 'open', side: 'SELL' })).fills[0]!;
    const position = store.getOpenPosition(accountId, 'BTCUSDT')!;
    expect(d(position.liquidationPrice).gt(position.entryPrice)).toBe(true);
    q = makeQuote(now + 1, '89', '91', '90');
    engine.onQuote(q);
    expect(store.getOpenPosition(accountId, 'BTCUSDT')).toBeDefined();
    q = makeQuote(now + 2, '500', '501', '500');
    engine.onQuote(q);
    expect(store.listClosedPositions(accountId, 1)[0]!.status).toBe('LIQUIDATED');
    expect(engine.balanceOf(accountId).walletBalance).toBe(d('10000').minus(entry.fee).minus(position.margin).toFixed());
    expectAudited();
  });

  it('triggers a short stop only when price rises to it, filling at the ask', () => {
    engine.placeOrderSync(request({ clientOrderId: 'open', side: 'SELL', stopLoss: '105', takeProfit: '90' }));
    q = makeQuote(now + 1, '98', '100', '99');
    engine.onQuote(q);
    expect(store.getOpenPosition(accountId, 'BTCUSDT')).toBeDefined();
    q = makeQuote(now + 2, '105', '107', '106');
    engine.onQuote(q);
    expect(store.getOpenPosition(accountId, 'BTCUSDT')).toBeUndefined();
    expect(store.listFills(accountId, 1)[0]).toMatchObject({ side: 'BUY', price: '107' });
    expectAudited();
  });

  it('triggers a short take-profit only when price falls to it', () => {
    engine.placeOrderSync(request({ clientOrderId: 'open', side: 'SELL', stopLoss: '105', takeProfit: '90' }));
    q = makeQuote(now + 1, '89', '91', '90');
    engine.onQuote(q);
    expect(store.getOpenPosition(accountId, 'BTCUSDT')).toBeUndefined();
    expect(store.listFills(accountId, 1)[0]).toMatchObject({ side: 'BUY', price: '91', realizedPnl: '8' });
  });

  it('averages entry when adding, and a stop then closes the whole enlarged position', () => {
    engine.placeOrderSync(request({ clientOrderId: 'first', leverage: 2, stopLoss: '95' }));
    q = makeQuote(now, '119', '121', '120');
    engine.placeOrderSync(request({ clientOrderId: 'second', leverage: 2 }));
    const position = store.getOpenPosition(accountId, 'BTCUSDT')!;
    expect(position).toMatchObject({ quantity: '2', entryPrice: '111', margin: '111', liquidationPrice: '56.055' });
    q = makeQuote(now + 1, '93', '95', '94');
    engine.onQuote(q);
    expect(store.getOpenPosition(accountId, 'BTCUSDT')).toBeUndefined();
    expect(store.listFills(accountId, 1)[0]).toMatchObject({ quantity: '2', price: '93', realizedPnl: '-36' });
    expectAudited();
  });

  it('refuses to fill against a quote older than a minute', () => {
    q = makeQuote(now - 60_001);
    const result = engine.placeOrderSync(request({ clientOrderId: 'stale' }));
    expect(result.order.status).toBe('REJECTED');
    expect(result.order.rejectReason).toContain('MARKET_DATA_STALE');
    expect(store.listFills(accountId, 10)).toHaveLength(0);
  });

  it('stores and reads the trailing fields of an order', () => {
    const base = engine.placeOrderSync(request({ clientOrderId: 'trail-base' })).order;
    const order = {
      ...base,
      id: 'trail-1',
      clientOrderId: 'trail-1',
      type: 'TRAILING_STOP_MARKET' as const,
      trailingKind: 'RATIO' as const,
      trailingCallback: '0.02',
      trailingActivationPrice: '105',
      trailingExtreme: '110.5',
      attachedTrailingStop: { kind: 'SPREAD' as const, callback: '1.5', activationPrice: null },
    };
    store.insertOrder(order);
    expect(store.getOrder('trail-1')).toEqual(order);
    store.saveOrder({ ...order, trailingExtreme: '112' });
    expect(store.getOrder('trail-1')!.trailingExtreme).toBe('112');
    // An ordinary order keeps them all empty.
    expect(store.getOrder(base.id)).toMatchObject({ trailingKind: null, trailingCallback: null, trailingActivationPrice: null, trailingExtreme: null, attachedTrailingStop: null });
  });

  it('accepts a protection body without trailingStop and still rejects unknown keys', () => {
    expect(ProtectionSchema.parse({ stopLoss: '90', takeProfit: null })).toEqual({ stopLoss: '90', takeProfit: null });
    expect(ProtectionSchema.parse({ stopLoss: null, takeProfit: null, trailingStop: null }).trailingStop).toBeNull();
    expect(ProtectionSchema.safeParse({ stopLoss: '90', takeProfit: null, extra: 1 }).success).toBe(false);
  });

  it('keeps allowTrailingStop through save and load, off by default', () => {
    store.insertProvider({ id: 'p1', name: 'p', kind: 'OPENAI', baseUrl: null, defaultModel: 'm', inputPricePerMTok: null, outputPricePerMTok: null, createdAt: 1 });
    const agent = {
      id: 'a1', name: 'a', providerId: 'p1', model: 'm', symbol: 'BTCUSDT', timeframe: '15m' as const, strategy: '', accountId,
      risk: DEFAULT_RISK_SETTINGS, intervalSec: 900, executionMode: 'SIGNAL' as const, useNews: false, allowTrailingStop: false,
      status: 'IDLE' as const, running: false, nextRunAt: null, lastRunAt: null, statusDetail: null, consecutiveErrors: 0, isDemo: false, createdAt: 1,
    };
    store.insertAgent(agent);
    expect(store.getAgent('a1')!.allowTrailingStop).toBe(false);
    store.saveAgent({ ...agent, allowTrailingStop: true });
    expect(store.getAgent('a1')!.allowTrailingStop).toBe(true);
    expect(AgentInputSchema.parse({ name: 'n', providerId: 'p1', model: 'm', symbol: 'BTCUSDT', timeframe: '15m', strategy: '', accountId, intervalSec: 900 }).allowTrailingStop).toBe(false);
  });

  it('keeps the ledger auditable through a fixed pseudo-random trade sequence', () => {
    let seed = 0x12345678;
    const random = () => ((seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0) / 2 ** 32);
    for (let i = 0; i < 80; i++) {
      const mid = 80 + Math.floor(random() * 40);
      now += 60_000;
      q = makeQuote(now, String(mid - 1), String(mid + 1), String(mid));
      engine.onQuote(q);
      const position = store.getOpenPosition(accountId, 'BTCUSDT');
      if (position && random() < 0.55) {
        engine.placeOrderSync(request({ clientOrderId: `r-${i}`, side: position.side === 'LONG' ? 'SELL' : 'BUY', quantity: random() < 0.5 ? '0.25' : '2', reduceOnly: true }));
      } else if (!position) {
        engine.placeOrderSync(request({ clientOrderId: `r-${i}`, side: random() < 0.5 ? 'BUY' : 'SELL', quantity: '0.5', leverage: 5 }));
      }
    }
    expect(auditLedger(store.getAccount(accountId)!, store.listLedger(accountId))).toEqual([]);
  });
});

function makeQuote(ts: number, bid = '99', ask = '101', mark = '100'): Quote {
  return { source: 'REPLAY', symbol: 'BTCUSDT', bid, ask, last: mark, mark, markEstimated: false, fundingRate: '0', nextFundingTime: null, ts };
}
