import {
  CandlestickSeries,
  ColorType,
  createChart,
  CrosshairMode,
  HistogramSeries,
  LineSeries,
  type IChartApi,
  type ISeriesApi,
  type UTCTimestamp,
} from 'lightweight-charts';
import { useEffect, useMemo, useRef, useState } from 'react';
import { ema, macd, rsi, TIMEFRAME_MS, type Candle, type Quote, type Series, type SymbolId, type Timeframe } from '@mochiprep/shared';
import { money, priceDigits } from '@/lib/format';
import { useApp } from '@/store';

const css = (name: string) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
const seconds = (ms: number) => Math.floor(ms / 1000) as UTCTimestamp;

function baseOptions() {
  return {
    autoSize: true,
    localization: { locale: 'en-US' },
    layout: {
      background: { type: ColorType.Solid, color: 'transparent' },
      textColor: css('--ink-3'),
      fontFamily: 'JetBrains Mono Variable, monospace',
      fontSize: 11,
      attributionLogo: false,
      panes: { separatorColor: css('--line') },
    },
    grid: { vertLines: { visible: false }, horzLines: { color: css('--line') } },
    rightPriceScale: { borderVisible: false },
    timeScale: { borderVisible: false, timeVisible: true, secondsVisible: false },
    crosshair: { mode: CrosshairMode.Normal },
  };
}

const line = (series: Series, candles: Candle[]) =>
  series.flatMap((value, i) => (value === null ? [] : [{ time: seconds(candles[i]!.time), value }]));

export interface PriceLineSpec {
  price: number;
  label: string;
  tone: 'up' | 'down' | 'neutral';
}

interface Props {
  candles: Candle[];
  symbol: SymbolId;
  timeframe: Timeframe;
  quote: Quote | undefined;
  showIndicators?: boolean;
  priceLines?: PriceLineSpec[];
}

/** Candles and volume, plus EMA overlays and RSI / MACD panes. The forming bar follows live quotes. */
export function MarketChart({ candles, symbol, timeframe, quote, showIndicators = true, priceLines = [] }: Props) {
  const host = useRef<HTMLDivElement>(null);
  const chart = useRef<IChartApi | null>(null);
  const series = useRef<Record<string, ISeriesApi<any>>>({});
  const lastBar = useRef<Candle | null>(null);
  const theme = useApp((s) => s.theme);
  const digits = priceDigits(symbol);

  useEffect(() => {
    const instance = createChart(host.current!, baseOptions());
    chart.current = instance;
    const up = css('--up');
    const down = css('--down');
    const format = { type: 'price', precision: digits, minMove: 10 ** -digits } as const;
    const s: Record<string, ISeriesApi<any>> = {};
    s.candles = instance.addSeries(CandlestickSeries, {
      upColor: up,
      downColor: down,
      wickUpColor: up,
      wickDownColor: down,
      borderVisible: false,
      priceFormat: format,
    });
    s.volume = instance.addSeries(HistogramSeries, { priceScaleId: 'vol', priceFormat: { type: 'volume' }, lastValueVisible: false, priceLineVisible: false });
    instance.priceScale('vol').applyOptions({ scaleMargins: { top: 0.82, bottom: 0 } });
    if (showIndicators) {
      const overlay = { lineWidth: 2, priceLineVisible: false, lastValueVisible: false, crosshairMarkerVisible: false } as const;
      s.ema20 = instance.addSeries(LineSeries, { ...overlay, color: css('--series-2'), priceFormat: format });
      s.ema50 = instance.addSeries(LineSeries, { ...overlay, color: css('--series-3'), priceFormat: format });
      s.rsi = instance.addSeries(LineSeries, { lineWidth: 2, color: css('--series-5'), priceLineVisible: false }, 1);
      s.rsi.createPriceLine({ price: 70, color: css('--ink-3'), lineWidth: 1, lineStyle: 2, axisLabelVisible: false, title: '' });
      s.rsi.createPriceLine({ price: 30, color: css('--ink-3'), lineWidth: 1, lineStyle: 2, axisLabelVisible: false, title: '' });
      s.macdHist = instance.addSeries(HistogramSeries, { priceLineVisible: false, lastValueVisible: false }, 2);
      s.macd = instance.addSeries(LineSeries, { lineWidth: 2, color: css('--series-2'), priceLineVisible: false, lastValueVisible: false }, 2);
      s.signal = instance.addSeries(LineSeries, { lineWidth: 2, color: css('--series-3'), priceLineVisible: false, lastValueVisible: false }, 2);
      const panes = instance.panes();
      panes[0]?.setStretchFactor(5);
      panes[1]?.setStretchFactor(1.3);
      panes[2]?.setStretchFactor(1.3);
    }
    series.current = s;
    return () => {
      instance.remove();
      chart.current = null;
    };
  }, [theme, showIndicators, digits]);

  useEffect(() => {
    const s = series.current;
    if (!s.candles || candles.length === 0) return;
    const up = css('--up');
    const down = css('--down');
    s.candles.setData(candles.map((c) => ({ time: seconds(c.time), open: c.open, high: c.high, low: c.low, close: c.close })));
    s.volume!.setData(
      candles.map((c) => ({ time: seconds(c.time), value: c.volume, color: `${c.close >= c.open ? up : down}55` })),
    );
    if (s.ema20) {
      const closes = candles.map((c) => c.close);
      const m = macd(closes);
      s.ema20.setData(line(ema(closes, 20), candles));
      s.ema50!.setData(line(ema(closes, 50), candles));
      s.rsi!.setData(line(rsi(closes), candles));
      s.macd!.setData(line(m.macd, candles));
      s.signal!.setData(line(m.signal, candles));
      s.macdHist!.setData(
        m.histogram.flatMap((value, i) =>
          value === null ? [] : [{ time: seconds(candles[i]!.time), value, color: value >= 0 ? `${up}99` : `${down}99` }],
        ),
      );
    }
    lastBar.current = { ...candles[candles.length - 1]! };
  }, [candles, theme, showIndicators, digits]);

  useEffect(() => {
    const target = series.current.candles;
    if (!target) return;
    const colors = { up: css('--up'), down: css('--down'), neutral: css('--ink-3') };
    const created = priceLines.map((p) =>
      target.createPriceLine({ price: p.price, color: colors[p.tone], lineWidth: 1, lineStyle: 2, axisLabelVisible: true, title: p.label }),
    );
    return () => created.forEach((l) => target.removePriceLine(l));
  }, [priceLines, theme, candles.length === 0, digits]);

  useEffect(() => {
    const bar = lastBar.current;
    const target = series.current.candles;
    if (!quote || !bar || !target || quote.symbol !== symbol) return;
    const price = Number(quote.last);
    const size = TIMEFRAME_MS[timeframe];
    const bucket = Math.floor((quote.ts - 1) / size) * size;
    if (bucket < bar.time) return;
    const next = bucket === bar.time
      ? { ...bar, high: Math.max(bar.high, price), low: Math.min(bar.low, price), close: price }
      : { ...bar, time: bucket, open: bar.close, high: Math.max(bar.close, price), low: Math.min(bar.close, price), close: price, volume: 0 };
    lastBar.current = next;
    target.update({ time: seconds(next.time), open: next.open, high: next.high, low: next.low, close: next.close });
  }, [quote, symbol, timeframe]);

  return <div ref={host} className="size-full" data-testid="market-chart" />;
}

export interface EquitySeries {
  id: string;
  name: string;
  color: string;
  points: { ts: number; equity: number }[];
}

/** One line per series on a shared USDT axis, with a legend that reads out the crosshair. */
export function EquityChart({ series, baseline }: { series: EquitySeries[]; baseline?: number }) {
  const host = useRef<HTMLDivElement>(null);
  const chart = useRef<IChartApi | null>(null);
  const handles = useRef(new Map<string, ISeriesApi<'Line'>>());
  const fitted = useRef(false);
  const theme = useApp((s) => s.theme);
  const [hover, setHover] = useState<{ time: number; values: Record<string, number> } | null>(null);
  const key = series.map((s) => s.id).join('|');

  useEffect(() => {
    const instance = createChart(host.current!, baseOptions());
    chart.current = instance;
    const map = new Map<string, ISeriesApi<'Line'>>();
    series.forEach((s, i) => {
      const handle = instance.addSeries(LineSeries, {
        color: css(s.color),
        lineWidth: 2,
        priceLineVisible: false,
        lastValueVisible: series.length <= 4,
        crosshairMarkerRadius: 4,
        priceFormat: { type: 'price', precision: 2, minMove: 0.01 },
      });
      if (i === 0 && baseline !== undefined) {
        handle.createPriceLine({ price: baseline, color: css('--ink-3'), lineWidth: 1, lineStyle: 2, axisLabelVisible: false, title: '' });
      }
      map.set(s.id, handle);
    });
    handles.current = map;
    fitted.current = false;
    instance.subscribeCrosshairMove((param) => {
      if (!param.time) return setHover(null);
      const values: Record<string, number> = {};
      for (const [id, handle] of map) {
        const point = param.seriesData.get(handle) as { value?: number } | undefined;
        if (point?.value !== undefined) values[id] = point.value;
      }
      setHover({ time: (param.time as number) * 1000, values });
    });
    return () => instance.remove();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, theme, baseline]);

  useEffect(() => {
    for (const s of series) {
      const seen = new Set<number>();
      const data = s.points.flatMap((p) => {
        const time = seconds(p.ts);
        if (seen.has(time)) return [];
        seen.add(time);
        return [{ time, value: p.equity }];
      });
      handles.current.get(s.id)?.setData(data);
    }
    if (series.some((s) => s.points.length > 1)) chart.current?.timeScale().fitContent();
  }, [series]);

  const latest = useMemo(
    () => Object.fromEntries(series.map((s) => [s.id, s.points[s.points.length - 1]?.equity])),
    [series],
  );

  return (
    <div className="flex size-full flex-col">
      {series.length > 1 && (
        <div className="flex flex-wrap gap-x-4 gap-y-1 px-5 pb-2 text-xs">
          {series.map((s) => (
            <span key={s.id} className="inline-flex items-center gap-1.5 text-ink-2">
              <span className="h-0.5 w-3.5 rounded-full" style={{ background: `var(${s.color})` }} />
              {s.name}
              <span className="num text-ink">{money(hover?.values[s.id] ?? latest[s.id])}</span>
            </span>
          ))}
        </div>
      )}
      <div ref={host} className="min-h-0 flex-1" data-testid="equity-chart" />
    </div>
  );
}
