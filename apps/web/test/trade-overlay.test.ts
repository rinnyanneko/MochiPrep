import { describe, expect, it } from 'vitest';
import type { DecisionRecord, Fill, Order, PositionView } from '@mochiprep/shared';
import { barTimeOf, buildMarkers, buildPriceLines, buildSignalLines, fillsByBar, openPnl, summarize } from '../src/lib/trade-overlay';

const MIN15 = 900_000;
const fill = (overrides: Partial<Fill>): Fill => ({
  id: 'f', orderId: 'o', accountId: 'a', symbol: 'BTCUSDT', side: 'BUY', quantity: '0.5', price: '100',
  fee: '0.1', realizedPnl: '0', liquidity: 'TAKER', ts: MIN15 + 60_000, ...overrides,
});
const position = (overrides: Partial<PositionView> = {}) =>
  ({
    symbol: 'BTCUSDT', side: 'LONG', quantity: '2', entryPrice: '100', margin: '50', liquidationPrice: '76',
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
    expect(buy).toMatchObject({ text: 'B', tone: 'up', shape: 'arrowUp', position: 'belowBar', barTime: MIN15 });
    expect(sell).toMatchObject({ text: 'S', tone: 'down', shape: 'arrowDown', position: 'aboveBar' });
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
    expect(marker).toMatchObject({ text: 'Liq −50.00', tone: 'down', shape: 'circle' });
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
      { id: 'entry', price: 100, label: 'LONG 2  +20.00 (+40.00%)', tone: 'up' },
      { id: 'tp', price: 120, label: 'TP', tone: 'up' },
      { id: 'sl', price: 90, label: 'SL', tone: 'down' },
      { id: 'liq', price: 76, label: 'Liq', tone: 'warn' },
    ]);
  });

  it('turns the entry line red on a loss', () => {
    const [entry] = buildPriceLines([position()], [], 'BTCUSDT', 90);
    expect(entry).toMatchObject({ label: 'LONG 2  −20.00 (−40.00%)', tone: 'down' });
  });

  it('keeps the same line id as PnL moves, so the line updates in place', () => {
    const ids = (mark: number) => buildPriceLines([position()], [limit()], 'BTCUSDT', mark).map((l) => l.id);
    expect(ids(101)).toEqual(ids(150));
  });

  it('leaves out protection that does not exist', () => {
    const lines = buildPriceLines([position({ stopLoss: null, takeProfit: null, liquidationPrice: '0' })], [], 'BTCUSDT', 110);
    expect(lines.map((l) => l.id)).toEqual(['entry']);
  });

  it('shows no PnL before a quote arrives', () => {
    expect(buildPriceLines([position()], [], 'BTCUSDT', null)[0]).toMatchObject({ label: 'LONG 2', tone: 'neutral' });
  });

  it('draws resting limits, but not filled orders or other pairs', () => {
    const lines = buildPriceLines([], [limit(), limit({ id: 'o2', status: 'FILLED' }), limit({ id: 'o3', symbol: 'ETHUSDT' }), limit({ id: 'o4', type: 'STOP_MARKET' })], 'BTCUSDT', 100);
    expect(lines).toEqual([{ id: 'limit:o1', price: 95, label: 'Limit buy 1', tone: 'info' }]);
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
      { id: 'signal:s1:tp', price: 110, label: 'Suggested TP', tone: 'up' },
      { id: 'signal:s1:sl', price: 95, label: 'Suggested SL', tone: 'down' },
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
