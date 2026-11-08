import { useState } from 'react';
import { TIMEFRAMES, type Candle, type SymbolId, type Timeframe } from '@mochiprep/shared';
import { ChartTradeSummary, MarketChart } from '@/components/charts';
import { NewsPanel } from '@/components/NewsPanel';
import { AccountPicker, useCurrentAccount } from '@/components/trading';
import { useTradeOverlay } from '@/components/useTradeOverlay';
import { FeedBadge, PageTitle } from '@/components/Shell';
import { Badge, Card, Segmented, Select, Stat } from '@/components/ui';
import { api } from '@/lib/api';
import { pct, pnlClass, price } from '@/lib/format';
import { useApp, useDateTime, useQuery, useSymbols } from '@/store';
import { useEffect } from 'react';

export function useCandles(symbol: SymbolId, timeframe: Timeframe, limit = 400) {
  const source = useApp((s) => s.source);
  const [tick, setTick] = useState(0);
  // Periodic reload brings in closed-bar volume and indicator values the quote stream lacks.
  useEffect(() => {
    const timer = setInterval(() => setTick((t) => t + 1), source === 'LIVE' ? 30_000 : 5_000);
    return () => clearInterval(timer);
  }, [source]);
  return useQuery(
    () => api.get<Candle[]>(`/market/candles?source=${source}&symbol=${symbol}&timeframe=${timeframe}&limit=${limit}`),
    [source, symbol, timeframe, tick],
  );
}

export function MarketsPage() {
  const dateTime = useDateTime();
  const source = useApp((s) => s.source);
  const symbols = useSymbols();
  const [picked, setSymbol] = useState<SymbolId>('BTCUSDT');
  const symbol = symbols.includes(picked) ? picked : 'BTCUSDT';
  const [timeframe, setTimeframe] = useState<Timeframe>('15m');
  const quote = useApp((s) => s.quotes[source][symbol]);
  const { data: candles, error } = useCandles(symbol, timeframe);
  const account = useCurrentAccount();
  const [showTrades, setShowTrades] = useState(true);
  const overlay = useTradeOverlay(showTrades ? account?.id : undefined, symbol, timeframe, quote);

  const first = candles?.[Math.max(0, candles.length - Math.round(86_400_000 / { '1m': 60_000, '5m': 300_000, '15m': 900_000, '1h': 3_600_000, '4h': 14_400_000 }[timeframe]))];
  const change = quote && first ? ((Number(quote.last) - first.open) / first.open) * 100 : null;

  return (
    <>
      <PageTitle
        title="Markets"
        description={source === 'REPLAY' ? 'Synthetic replay tape. These prices are not real.' : 'USDT perpetuals from the OKX public feed. No API key needed.'}
        action={
          <div className="flex flex-wrap items-center gap-2">
            <Select aria-label="Symbol" className="h-8 w-36 text-xs" value={symbol} onChange={(e) => setSymbol(e.target.value)}>
              {symbols.map((s) => <option key={s} value={s}>{s}</option>)}
            </Select>
            <Segmented label="Timeframe" value={timeframe} onChange={setTimeframe} options={TIMEFRAMES.map((t) => ({ value: t, label: t }))} />
          </div>
        }
      />
      <div className="mb-3 grid grid-cols-2 gap-3 md:grid-cols-5">
        <Stat label={`${symbol} last`} value={<span data-testid="ticker-last">{price(quote?.last, symbol)}</span>} sub={change === null ? undefined : <span className={pnlClass(change)}>{pct(change)} 24h</span>} />
        <Stat label="Bid / Ask" value={<span className="text-sm">{price(quote?.bid, symbol)} / {price(quote?.ask, symbol)}</span>} />
        <Stat label="Mark price" value={price(quote?.mark, symbol)} sub={quote?.markEstimated ? 'Estimated from mid' : undefined} />
        <Stat label="Funding rate" value={quote?.fundingRate ? pct(Number(quote.fundingRate) * 100, 4) : '–'} sub={quote?.nextFundingTime ? `Next ${dateTime(quote.nextFundingTime)}` : undefined} />
        <Stat label="Connection" value={<FeedBadge />} sub={quote ? `Quote ${dateTime(quote.ts)}` : 'Waiting for a quote'} />
      </div>
      <Card className="p-3">
        <div className="mb-2 flex flex-wrap gap-2 px-2 text-xs text-ink-2">
          <span className="inline-flex items-center gap-1.5"><span className="h-0.5 w-3.5 rounded-full" style={{ background: 'var(--series-2)' }} />EMA 20</span>
          <span className="inline-flex items-center gap-1.5"><span className="h-0.5 w-3.5 rounded-full" style={{ background: 'var(--series-3)' }} />EMA 50</span>
          <span className="text-ink-3">Volume in the main pane, then RSI 14, then MACD 12/26/9 with signal.</span>
          {source === 'REPLAY' && <Badge tone="info">Synthetic</Badge>}
        </div>
        {account && (
          <div className="mb-2 flex flex-wrap items-center gap-3 px-2">
            <label className="flex items-center gap-2 text-xs text-ink-2">
              <input type="checkbox" checked={showTrades} onChange={(e) => setShowTrades(e.target.checked)} className="accent-[var(--accent)]" />
              Show trades
            </label>
            {showTrades && <AccountPicker className="h-8 w-52 text-xs" />}
            {showTrades && <ChartTradeSummary summary={overlay.summary} />}
          </div>
        )}
        <div className="h-[640px]">
          {error ? (
            <p className="px-5 py-20 text-center text-[13px] text-ink-3">Could not load candles: {error}</p>
          ) : (
            <MarketChart candles={candles ?? []} symbol={symbol} timeframe={timeframe} quote={quote} priceLines={overlay.lines} markers={overlay.markers} notes={overlay.notes} />
          )}
        </div>
      </Card>
      {source === 'LIVE' && <NewsPanel symbol={symbol} />}
    </>
  );
}
