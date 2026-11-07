import { useMemo } from 'react';
import type { Fill, Order, PositionView, Quote, SymbolId, Timeframe } from '@mochiprep/shared';
import { api } from '@/lib/api';
import { buildMarkers, buildPriceLines, buildSignalLines, fillsByBar, summarize } from '@/lib/trade-overlay';
import { useApp, useQuery } from '@/store';

const FILL_HISTORY = 500;

/** Everything drawn on a chart for one account and pair. Pass no account to draw nothing. */
export function useTradeOverlay(accountId: string | undefined, symbol: SymbolId, timeframe: Timeframe, quote: Quote | undefined) {
  const { data: fills } = useQuery(() => (accountId ? api.get<Fill[]>(`/accounts/${accountId}/fills?limit=${FILL_HISTORY}`) : Promise.resolve([])), [accountId], ['orders']);
  const { data: positions } = useQuery(() => (accountId ? api.get<PositionView[]>(`/accounts/${accountId}/positions`) : Promise.resolve([])), [accountId], ['positions']);
  const { data: orders } = useQuery(() => (accountId ? api.get<Order[]>(`/accounts/${accountId}/orders?limit=200`) : Promise.resolve([])), [accountId], ['orders']);

  const mark = quote ? Number(quote.mark) : null;
  // Markers change only with fills. Lines and PnL follow every quote.
  const markers = useMemo(() => buildMarkers(fills ?? [], symbol, timeframe), [fills, symbol, timeframe]);
  const notes = useMemo(() => fillsByBar(fills ?? [], symbol, timeframe), [fills, symbol, timeframe]);
  const signals = useApp((s) => s.signals);
  const lines = useMemo(
    () => [...buildPriceLines(positions ?? [], orders ?? [], symbol, mark), ...buildSignalLines(signals.filter((s) => s.accountId === accountId), symbol)],
    [positions, orders, signals, accountId, symbol, mark],
  );
  const summary = useMemo(() => summarize(fills ?? [], positions ?? [], symbol, mark), [fills, positions, symbol, mark]);
  return { markers, notes, lines, summary };
}
