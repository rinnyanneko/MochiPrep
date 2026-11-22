import { describe, expect, it } from 'vitest';
import type { DecisionRecord, Fill, Order, PositionView } from '@mochiprep/shared';
import { barTimeOf, buildMarkers, buildPriceLines, buildSignalLines, fillsByBar, isLiquidationLine, openPnl, summarize } from '../src/lib/trade-overlay';

const MIN15 = 900_000;
const fill = (overrides: Partial<Fill>): Fill => ({
  id: 'f', orderId: 'o', accountId: 'a', symbol: 'BTCUSDT', side: 'BUY', quantity: '0.5', price: '100',
  fee: '0.1', realizedPnl: '0', liquidity: 'TAKER', ts: MIN15 + 60_000, ...overrides,
});
const position = (overrides: Partial<PositionView> = {}) =>
  ({
    id: 'p1', symbol: 'BTCUSDT', side: 'LONG', quantity: '2', entryPrice: '100', margin: '50', liquidationPrice: '76',
    stopLoss: '90', takeProfit: '120', ...overrides,
  }) as PositionView;
const limit = (overrides: Partial<Order> = {}) =>
  ({ id: 'o1', symbol: 'BTCUSDT', side: 'BUY', type: 'LIMIT', status: 'OPEN', price: '95', quantity: '1', ...overrides }) as Order;

describe('barTimeOf', () => {
  it('puts a fill inside the bar it happened in', () => {
    expect(barTimeOf(MIN15 + 1, '15m')).toBe(MIN15);
    expect(barTimeOf(2 * MIN15 - 1, '15m')).toBe(MIN15);
  });

  it('puts a fill stamped exactly on a boundary in the bar that just ended', () => {
    expect(barTimeOf(2 * MIN15, '15m')).toBe(MIN15);
  });
});

describe('buildMarkers', () => {
  it('puts buys under the bar and sells over it, labelled by side', () => {
    const [buy, sell] = buildMarkers([fill({ id: '1' }), fill({ id: '2', side: 'SELL', ts: MIN15 + 120_000 })], 'BTCUSDT', '15m');
    expect(buy).toMatchObject({ text: 'B', tone: 'rise', shape: 'arrowUp', position: 'belowBar', barTime: MIN15 });
    expect(sell).toMatchObject({ text: 'S', tone: 'fall', shape: 'arrowDown', position: 'aboveBar' });
  });

  it('shows realized PnL on a closing fill, profit or loss', () => {
    const [win, loss] = buildMarkers(
      [fill({ id: '1', side: 'SELL', realizedPnl: '12.345' }), fill({ id: '2', ts: MIN15 + 120_000, realizedPnl: '-3.1' })],
      'BTCUSDT',
      '15m',
    );
    expect(win!.text).toBe('S +12.35');
    expect(loss!.text).toBe('B −3.10');
  });

  it('marks a liquidation distinctly', () => {
    const [marker] = buildMarkers([fill({ side: 'SELL', liquidity: 'LIQUIDATION', realizedPnl: '-50' })], 'BTCUSDT', '15m');
    expect(marker).toMatchObject({ text: 'Liq −50.00', tone: 'danger', shape: 'circle' });
  });

  it('keeps only the pair on screen and sorts by time, as the chart requires', () => {
    const markers = buildMarkers(
      [fill({ id: 'late', ts: 3 * MIN15 }), fill({ id: 'other', symbol: 'ETHUSDT' }), fill({ id: 'early', ts: MIN15 + 5 })],
      'BTCUSDT',
      '15m',
    );
    expect(markers.map((m) => m.id)).toEqual(['early', 'late']);
  });
});

describe('fillsByBar', () => {
  it('groups fills sharing a bar for the hover details', () => {
    const bars = fillsByBar([fill({ id: '1' }), fill({ id: '2' }), fill({ id: '3', ts: 5 * MIN15 + 1 }), fill({ id: '4', symbol: 'ETHUSDT' })], 'BTCUSDT', '15m');
    expect(bars.get(MIN15)?.map((f) => f.id)).toEqual(['1', '2']);
    expect(bars.size).toBe(2);
  });
});

describe('openPnl', () => {
  it('is positive for a long when mark rises, and reports return on margin', () => {
    expect(openPnl(position(), 110)).toEqual({ pnl: 20, roePct: 40 });
  });

  it('is positive for a short when mark falls', () => {
    expect(openPnl(position({ side: 'SHORT' }), 90).pnl).toBe(20);
    expect(openPnl(position({ side: 'SHORT' }), 110).pnl).toBe(-20);
  });

  it('omits return on margin when margin is unknown', () => {
    expect(openPnl(position({ margin: '0' }), 110).roePct).toBeNull();
  });
});

describe('buildPriceLines', () => {
  it('draws entry with live PnL, TP, SL and liquidation', () => {
    expect(buildPriceLines([position()], [], 'BTCUSDT', 110)).toEqual([
      { id: 'entry:p1', price: 100, label: 'LONG 2', tone: 'rise', pnl: { text: '+20.00 (+40.00%)', tone: 'rise' } },
      { id: 'tp:p1', price: 120, label: 'TP', tone: 'rise' },
      { id: 'sl:p1', price: 90, label: 'SL', tone: 'fall' },
      { id: 'liq:p1', price: 76, label: 'Liq', tone: 'warn' },
    ]);
  });

  it('colors the entry line by direction and only the PnL by result', () => {
    const [long] = buildPriceLines([position()], [], 'BTCUSDT', 90);
    expect(long).toMatchObject({ tone: 'rise', pnl: { text: '−20.00 (−40.00%)', tone: 'fall' } });
    const [short] = buildPriceLines([position({ side: 'SHORT' })], [], 'BTCUSDT', 90);
    expect(short).toMatchObject({ label: 'SHORT 2', tone: 'fall', pnl: { text: '+20.00 (+40.00%)', tone: 'rise' } });
  });

  it('draws an active trailing stop at its trigger', () => {
    const trailing = { kind: 'RATIO', callback: '0.01', activationPrice: null, extreme: '112', triggerPrice: '110.88' } as const;
    const lines = buildPriceLines([position({ trailingStop: trailing })], [], 'BTCUSDT', 111);
    expect(lines).toContainEqual({ id: 'trail:p1', price: 110.88, label: 'Trailing SL', tone: 'fall' });
  });

  it('draws a trailing stop that waits for its activation price there', () => {
    const waiting = { kind: 'RATIO', callback: '0.01', activationPrice: '115', extreme: null, triggerPrice: null } as const;
    const lines = buildPriceLines([position({ trailingStop: waiting })], [], 'BTCUSDT', 111);
    expect(lines).toContainEqual({ id: 'trail:p1', price: 115, label: 'Trailing SL activates', tone: 'info' });
  });

  it('keeps the same line id as PnL moves, so the line updates in place', () => {
    const ids = (mark: number) => buildPriceLines([position()], [limit()], 'BTCUSDT', mark).map((l) => l.id);
    expect(ids(101)).toEqual(ids(150));
  });

  it('leaves out protection that does not exist', () => {
    const lines = buildPriceLines([position({ stopLoss: null, takeProfit: null, liquidationPrice: '0' })], [], 'BTCUSDT', 110);
    expect(lines.map((l) => l.id)).toEqual(['entry:p1']);
  });

  it('shows no PnL before a quote arrives', () => {
    const [entry] = buildPriceLines([position()], [], 'BTCUSDT', null);
    expect(entry).toEqual({ id: 'entry:p1', price: 100, label: 'LONG 2', tone: 'rise' });
  });

  it('draws resting limits, but not filled orders or other pairs', () => {
    const lines = buildPriceLines([], [limit(), limit({ id: 'o2', status: 'FILLED' }), limit({ id: 'o3', symbol: 'ETHUSDT' }), limit({ id: 'o4', type: 'STOP_MARKET' })], 'BTCUSDT', 100);
    expect(lines).toEqual([{ id: 'limit:o1', price: 95, label: 'Limit buy 1', tone: 'info' }]);
  });

  it('gives each position on the same pair its own line ids, so neither replaces the other', () => {
    const lines = buildPriceLines([position(), position({ id: 'p2', side: 'SHORT', entryPrice: '105', liquidationPrice: '130' })], [], 'BTCUSDT', 110);
    const ids = lines.map((l) => l.id);
    expect(new Set(ids).size).toBe(ids.length);
    expect(lines.filter((l) => l.id.startsWith('entry:')).map((l) => l.price)).toEqual([100, 105]);
  });

  it('marks every liquidation line, and only those, as left out of autoscale', () => {
    const lines = buildPriceLines([position(), position({ id: 'p2', liquidationPrice: '60' })], [limit()], 'BTCUSDT', 110);
    expect(lines.filter(isLiquidationLine).map((l) => l.price)).toEqual([76, 60]);
  });

  it('ignores positions on other pairs', () => {
    expect(buildPriceLines([position({ symbol: 'ETHUSDT' })], [], 'BTCUSDT', 110)).toEqual([]);
  });
});

describe('buildSignalLines', () => {
  const signal = (overrides: object = {}) =>
    ({
      id: 's1', symbol: 'BTCUSDT',
      decision: { action: 'LONG', stopLoss: 95, takeProfit: 110 },
      risk: { sized: { referencePrice: '100' } },
      ...overrides,
    }) as unknown as DecisionRecord;

  it('draws the suggested entry, stop and target', () => {
    expect(buildSignalLines([signal()], 'BTCUSDT')).toEqual([
      { id: 'signal:s1:entry', price: 100, label: 'Suggested LONG', tone: 'info' },
      { id: 'signal:s1:tp', price: 110, label: 'Suggested TP', tone: 'rise' },
      { id: 'signal:s1:sl', price: 95, label: 'Suggested SL', tone: 'fall' },
    ]);
  });

  it('draws nothing for a CLOSE, which has no price of its own', () => {
    expect(buildSignalLines([signal({ risk: { sized: null } })], 'BTCUSDT')).toEqual([]);
  });

  it('ignores other pairs', () => {
    expect(buildSignalLines([signal({ symbol: 'ETHUSDT' })], 'BTCUSDT')).toEqual([]);
  });
});

describe('summarize', () => {
  it('nets fees out of realized PnL for this pair only', () => {
    const summary = summarize(
      [fill({ id: '1', fee: '1' }), fill({ id: '2', realizedPnl: '10', fee: '1' }), fill({ id: '3', symbol: 'ETHUSDT', realizedPnl: '99' })],
      [position()],
      'BTCUSDT',
      110,
    );
    expect(summary).toEqual({ fillCount: 2, realizedNet: 8, unrealized: 20 });
  });

  it('has no unrealized PnL while flat', () => {
    expect(summarize([], [], 'BTCUSDT', 110).unrealized).toBeNull();
  });
});
