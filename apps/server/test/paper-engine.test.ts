import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import type { DatabaseSync } from 'node:sqlite';
import type { OrderRequest, Quote } from '@mochiprep/shared';
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
