import { TIMEFRAME_MS, type DecisionRecord, type Fill, type Order, type PositionView, type SymbolId, type Timeframe } from '@mochiprep/shared';
import { qty, signedMoney } from './format';

export type ChartTone = 'up' | 'down' | 'neutral' | 'warn' | 'info';

export interface PriceLineSpec {
  /** Stable across updates, so a moving PnL label edits the line rather than recreating it. */
  id: string;
  price: number;
  label: string;
  tone: ChartTone;
}

export interface TradeMarker {
  id: string;
  /** Open time of the bar the fill belongs to. */
  barTime: number;
  /** Beside the bar, not on the fill price: text on a candle body is unreadable. */
  position: 'aboveBar' | 'belowBar';
  text: string;
  tone: ChartTone;
  shape: 'arrowUp' | 'arrowDown' | 'circle';
}

export interface TradeSummary {
  fillCount: number;
  /** Realized PnL minus fees, from fills. Funding is not included. */
  realizedNet: number;
  unrealized: number | null;
}

/** Quotes are stamped at the end of a period, so a fill at 12:15:00 belongs to the 12:00 bar. */
export const barTimeOf = (ts: number, timeframe: Timeframe) => {
  const size = TIMEFRAME_MS[timeframe];
  return Math.floor((ts - 1) / size) * size;
};

export function buildMarkers(fills: Fill[], symbol: SymbolId, timeframe: Timeframe): TradeMarker[] {
  return fills
    .filter((fill) => fill.symbol === symbol)
    .sort((a, b) => a.ts - b.ts)
    .map((fill) => {
      const pnl = Number(fill.realizedPnl);
      const letter = fill.side === 'BUY' ? 'B' : 'S';
      const place = { id: fill.id, barTime: barTimeOf(fill.ts, timeframe), position: fill.side === 'BUY' ? 'belowBar' : 'aboveBar' } as const;
      if (fill.liquidity === 'LIQUIDATION') {
        return { ...place, text: `Liq ${signedMoney(pnl)}`, tone: 'down', shape: 'circle' };
      }
      return {
        ...place,
        // Only a closing fill has realized PnL. An entry just says which way it went.
        text: pnl === 0 ? letter : `${letter} ${signedMoney(pnl)}`,
        tone: fill.side === 'BUY' ? 'up' : 'down',
        shape: fill.side === 'BUY' ? 'arrowUp' : 'arrowDown',
      };
    });
}

/** Fills per bar, for the hover details. */
export function fillsByBar(fills: Fill[], symbol: SymbolId, timeframe: Timeframe): Map<number, Fill[]> {
  const bars = new Map<number, Fill[]>();
  for (const fill of fills) {
    if (fill.symbol !== symbol) continue;
    const bar = barTimeOf(fill.ts, timeframe);
    bars.set(bar, [...(bars.get(bar) ?? []), fill]);
  }
  return bars;
}

export function openPnl(position: PositionView, mark: number) {
  const direction = position.side === 'LONG' ? 1 : -1;
  const pnl = (mark - Number(position.entryPrice)) * Number(position.quantity) * direction;
  const margin = Number(position.margin);
  return { pnl, roePct: margin > 0 ? (pnl / margin) * 100 : null };
}

export function buildPriceLines(positions: PositionView[], orders: Order[], symbol: SymbolId, mark: number | null): PriceLineSpec[] {
  const lines: PriceLineSpec[] = [];
  for (const position of positions) {
    if (position.symbol !== symbol) continue;
    const size = `${position.side} ${qty(position.quantity)}`;
    if (mark === null) {
      lines.push({ id: 'entry', price: Number(position.entryPrice), label: size, tone: 'neutral' });
    } else {
      const { pnl, roePct } = openPnl(position, mark);
      const roe = roePct === null ? '' : ` (${roePct > 0 ? '+' : roePct < 0 ? '−' : ''}${Math.abs(roePct).toFixed(2)}%)`;
      lines.push({ id: 'entry', price: Number(position.entryPrice), label: `${size}  ${signedMoney(pnl)}${roe}`, tone: pnl >= 0 ? 'up' : 'down' });
    }
    if (position.takeProfit) lines.push({ id: 'tp', price: Number(position.takeProfit), label: 'TP', tone: 'up' });
    if (position.stopLoss) lines.push({ id: 'sl', price: Number(position.stopLoss), label: 'SL', tone: 'down' });
    if (Number(position.liquidationPrice) > 0) lines.push({ id: 'liq', price: Number(position.liquidationPrice), label: 'Liq', tone: 'warn' });
  }
  for (const order of orders) {
    if (order.symbol !== symbol || order.status !== 'OPEN' || order.type !== 'LIMIT' || !order.price) continue;
    lines.push({ id: `limit:${order.id}`, price: Number(order.price), label: `Limit ${order.side === 'BUY' ? 'buy' : 'sell'} ${qty(order.quantity)}`, tone: 'info' });
  }
  return lines;
}

/** Suggested entry and exits. A CLOSE has no price of its own, so it stays in the signal list. */
export function buildSignalLines(signals: DecisionRecord[], symbol: SymbolId): PriceLineSpec[] {
  return signals.flatMap((signal) => {
    const ai = signal.decision;
    const entry = signal.risk?.sized?.referencePrice;
    if (signal.symbol !== symbol || !ai || !entry) return [];
    const key = `signal:${signal.id}`;
    const lines: PriceLineSpec[] = [{ id: `${key}:entry`, price: Number(entry), label: `Suggested ${ai.action}`, tone: 'info' }];
    if (ai.takeProfit !== null) lines.push({ id: `${key}:tp`, price: ai.takeProfit, label: 'Suggested TP', tone: 'up' });
    if (ai.stopLoss !== null) lines.push({ id: `${key}:sl`, price: ai.stopLoss, label: 'Suggested SL', tone: 'down' });
    return lines;
  });
}

export function summarize(fills: Fill[], positions: PositionView[], symbol: SymbolId, mark: number | null): TradeSummary {
  const mine = fills.filter((fill) => fill.symbol === symbol);
  const position = positions.find((p) => p.symbol === symbol);
  return {
    fillCount: mine.length,
    realizedNet: mine.reduce((sum, fill) => sum + Number(fill.realizedPnl) - Number(fill.fee), 0),
    unrealized: position && mark !== null ? openPnl(position, mark).pnl : null,
  };
}

