import {
  CandlestickSeries,
  ColorType,
  createChart,
  createSeriesMarkers,
  CrosshairMode,
  HistogramSeries,
  LineSeries,
  TickMarkType,
  type AutoscaleInfo,
  type IChartApi,
  type IPriceLine,
  type ISeriesApi,
  type ISeriesMarkersPluginApi,
  type Time,
  type UTCTimestamp,
} from 'lightweight-charts';
import { useEffect, useMemo, useRef, useState } from 'react';
import { ema, macd, rsi, TIMEFRAME_MS, type Candle, type Fill, type Quote, type Series, type SymbolId, type Timeframe } from '@mochiprep/shared';
import { chartTickLabel, formatDateTime, type TickKind, type TimeZonePref } from '@/lib/display';
import { money, pnlClass, price as formatPrice, priceDigits, qty, signedMoney } from '@/lib/format';
import type { ChartTone, PriceLineSpec, TradeMarker, TradeSummary } from '@/lib/trade-overlay';
import { cn } from '@/lib/utils';
import { useApp } from '@/store';

const css = (name: string) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
const seconds = (ms: number) => Math.floor(ms / 1000) as UTCTimestamp;

const TICK_KINDS: Record<TickMarkType, TickKind> = {
  [TickMarkType.Year]: 'year',
  [TickMarkType.Month]: 'month',
  [TickMarkType.DayOfMonth]: 'day',
  [TickMarkType.Time]: 'time',
  [TickMarkType.TimeWithSeconds]: 'seconds',
};

/** Chart data stays in UTC seconds; only the labels follow the chosen zone. */
function baseOptions(zone: TimeZonePref) {
  return {
    autoSize: true,
    localization: { locale: 'en-US', timeFormatter: (time: Time) => formatDateTime((time as number) * 1000, zone) },
    layout: {
      background: { type: ColorType.Solid, color: 'transparent' },
      textColor: css('--ink-3'),
      fontFamily: 'JetBrains Mono Variable, monospace',
      fontSize: 11,
      // Required by the Lightweight Charts license: it must credit and link to TradingView.
      attributionLogo: true,
      panes: { separatorColor: css('--line') },
    },
    grid: { vertLines: { visible: false }, horzLines: { color: css('--line') } },
    rightPriceScale: { borderVisible: false },
    timeScale: {
      borderVisible: false,
      timeVisible: true,
      secondsVisible: false,
      tickMarkFormatter: (time: Time, type: TickMarkType) => chartTickLabel((time as number) * 1000, TICK_KINDS[type], zone),
    },
    crosshair: { mode: CrosshairMode.Normal },
  };
}

const line = (series: Series, candles: Candle[]) =>
  series.flatMap((value, i) => (value === null ? [] : [{ time: seconds(candles[i]!.time), value }]));

const toneColors = (): Record<ChartTone, string> => ({
  rise: css('--rise'),
  fall: css('--fall'),
  danger: css('--down'),
  neutral: css('--ink-3'),
  warn: css('--warn'),
  info: css('--info'),
});

const LABEL_HEIGHT = 18;
const LABEL_GAP = 2;
const LABEL_TONES: Record<ChartTone, string> = {
  rise: 'bg-rise-soft text-rise',
  fall: 'bg-fall-soft text-fall',
  danger: 'bg-down-soft text-down',
  neutral: 'bg-surface-3 text-ink-2',
  warn: 'bg-warn-soft text-warn',
  info: 'bg-info-soft text-info',
};

/**
 * Charts read colors once from CSS and labels once at creation, so a change of theme, color flip
 * or zone rebuilds them. The key names everything that does.
 */
function useLook() {
  const theme = useApp((s) => s.theme);
  const flip = useApp((s) => s.flipColors);
  const zone = useApp((s) => s.timeZone);
  return { zone, key: `${theme}|${flip}|${zone}` };
}

const NO_MARKERS: TradeMarker[] = [];
const NO_NOTES = new Map<number, Fill[]>();

interface Props {
  candles: Candle[];
  symbol: SymbolId;
  timeframe: Timeframe;
  quote: Quote | undefined;
  showIndicators?: boolean;
  priceLines?: PriceLineSpec[];
  markers?: TradeMarker[];
  /** Fills per bar open time, shown when hovering a bar. */
  notes?: Map<number, Fill[]>;
}

/** Candles and volume, plus EMA overlays and RSI / MACD panes. The forming bar follows live quotes. */
export function MarketChart({ candles, symbol, timeframe, quote, showIndicators = true, priceLines = [], markers = NO_MARKERS, notes = NO_NOTES }: Props) {
  const host = useRef<HTMLDivElement>(null);
  const chart = useRef<IChartApi | null>(null);
  const series = useRef<Record<string, ISeriesApi<any>>>({});
  const lastBar = useRef<Candle | null>(null);
  const markerPlugin = useRef<ISeriesMarkersPluginApi<Time> | null>(null);
  const drawnLines = useRef(new Map<string, IPriceLine>());
  const notesRef = useRef(notes);
  notesRef.current = notes;
  const linesRef = useRef(priceLines);
  linesRef.current = priceLines;
  const labelNodes = useRef(new Map<string, HTMLSpanElement>());
  const [hovered, setHovered] = useState<Fill[] | null>(null);
  const look = useLook();
  const digits = priceDigits(symbol);

  useEffect(() => {
    const options = baseOptions(look.zone);
    const instance = createChart(host.current!, { ...options, timeScale: { ...options.timeScale, barSpacing: 9 } });
    chart.current = instance;
    const up = css('--rise');
    const down = css('--fall');
    const format = { type: 'price', precision: digits, minMove: 10 ** -digits } as const;
    const s: Record<string, ISeriesApi<any>> = {};
    s.candles = instance.addSeries(CandlestickSeries, {
      upColor: up,
      downColor: down,
      wickUpColor: up,
      wickDownColor: down,
      borderVisible: false,
      priceFormat: format,
      // Keeps TP, SL and limits on screen. Liquidation is usually too far away to include.
      autoscaleInfoProvider: (base: () => AutoscaleInfo | null) => {
        const info = base();
        const prices = linesRef.current.filter((line) => line.id !== 'liq').map((line) => line.price);
        if (!info?.priceRange || prices.length === 0) return info;
        return {
          ...info,
          priceRange: {
            minValue: Math.min(info.priceRange.minValue, ...prices),
            maxValue: Math.max(info.priceRange.maxValue, ...prices),
          },
        };
      },
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
    markerPlugin.current = createSeriesMarkers(s.candles as ISeriesApi<'Candlestick', Time>, []);
    instance.subscribeCrosshairMove((param) => {
      const bar = typeof param.time === 'number' ? notesRef.current.get(param.time * 1000) : undefined;
      setHovered(bar ?? null);
    });
    return () => {
      instance.remove();
      chart.current = null;
      markerPlugin.current = null;
      drawnLines.current.clear();
    };
  }, [look.key, showIndicators, digits]);

  useEffect(() => {
    const s = series.current;
    if (!s.candles || candles.length === 0) return;
    const up = css('--rise');
    const down = css('--fall');
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
  }, [candles, look.key, showIndicators, digits]);

  useEffect(() => {
    const target = series.current.candles;
    if (!target) return;
    const colors = toneColors();
    const drawn = drawnLines.current;
    const wanted = new Set(priceLines.map((line) => line.id));
    for (const [id, line] of drawn) {
      if (!wanted.has(id)) {
        target.removePriceLine(line);
        drawn.delete(id);
      }
    }
    for (const spec of priceLines) {
      // No title: the library can only draw it at the right edge, over the newest candles.
      const options = { price: spec.price, color: colors[spec.tone], title: '' };
      const existing = drawn.get(spec.id);
      if (existing) existing.applyOptions(options);
      else drawn.set(spec.id, target.createPriceLine({ ...options, lineWidth: 1, lineStyle: 2, axisLabelVisible: true }));
    }
    // Lines are not chart data, so the price scale must be told to refit around them.
    chart.current?.priceScale('right').applyOptions({ autoScale: true });
  }, [priceLines, look.key, digits, symbol, showIndicators]);

  // The price scale refits without any event, so labels follow it frame by frame.
  useEffect(() => {
    let frame = 0;
    const place = () => {
      frame = requestAnimationFrame(place);
      const target = series.current.candles;
      const paneHeight = chart.current?.panes()[0]?.getHeight() ?? 0;
      if (!target) return;
      const placed = linesRef.current
        .map((line) => ({ node: labelNodes.current.get(line.id), y: target.priceToCoordinate(line.price) }))
        .sort((a, b) => (a.y ?? 0) - (b.y ?? 0));
      let lowest = -Infinity;
      for (const { node, y } of placed) {
        if (!node) continue;
        const visible = y !== null && y >= 0 && y <= paneHeight;
        node.style.visibility = visible ? 'visible' : 'hidden';
        if (!visible) continue;
        // Close prices would stack their labels unreadably, so push each one clear of the last.
        const top = Math.max(y - LABEL_HEIGHT / 2, lowest + LABEL_GAP);
        node.style.transform = `translateY(${top}px)`;
        lowest = top + LABEL_HEIGHT;
      }
    };
    place();
    return () => cancelAnimationFrame(frame);
  }, []);

  useEffect(() => {
    const colors = toneColors();
    const bars = new Set(candles.map((c) => c.time));
    // A marker off the loaded range has no bar to sit on.
    markerPlugin.current?.setMarkers(
      markers
        .filter((m) => bars.has(m.barTime))
        .map((m) => ({
          id: m.id,
          time: seconds(m.barTime),
          position: m.position,
          shape: m.shape,
          size: 1.4,
          color: colors[m.tone],
          text: m.text,
        })),
    );
  }, [markers, candles, look.key, digits, showIndicators]);

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

  return (
    <div className="relative size-full">
      <div ref={host} className="size-full" data-testid="market-chart" />
      {priceLines.map((line) => (
        <span
          key={line.id}
          ref={(node) => {
            if (node) labelNodes.current.set(line.id, node);
            else labelNodes.current.delete(line.id);
          }}
          data-testid="chart-line-label"
          className={cn('num pointer-events-none invisible absolute top-0 left-2 z-[5] rounded-md px-1.5 text-[11px] leading-[18px] whitespace-nowrap', LABEL_TONES[line.tone])}
        >
          {line.label}
        </span>
      ))}
      {hovered && (
        <div className="pointer-events-none absolute top-2 left-2 z-10 rounded-xl border border-line bg-surface-2 px-3 py-2 text-xs" data-testid="chart-trade-tip">
          {hovered.map((fill) => (
            <div key={fill.id} className="num flex gap-3 whitespace-nowrap">
              <span className={cn('font-semibold', fill.side === 'BUY' ? 'text-rise' : 'text-fall')}>{fill.liquidity === 'LIQUIDATION' ? 'Liquidated' : fill.side}</span>
              <span>{qty(fill.quantity)} @ {formatPrice(fill.price, symbol)}</span>
              <span className="text-ink-3">fee {money(fill.fee, 4)}</span>
              {Number(fill.realizedPnl) !== 0 && <span className={pnlClass(fill.realizedPnl)}>{signedMoney(fill.realizedPnl)}</span>}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** Pair totals above a chart, so the markers have a number to add up to. */
export function ChartTradeSummary({ summary }: { summary: TradeSummary }) {
  return (
    <span className="num flex flex-wrap gap-x-3 text-xs text-ink-3" data-testid="chart-trade-summary">
      <span>{summary.fillCount} {summary.fillCount === 1 ? 'fill' : 'fills'}</span>
      <span>Realized net <span className={pnlClass(summary.realizedNet)}>{signedMoney(summary.realizedNet)}</span></span>
      {summary.unrealized !== null && <span>Open <span className={pnlClass(summary.unrealized)}>{signedMoney(summary.unrealized)}</span></span>}
    </span>
  );
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
  const look = useLook();
  const [hover, setHover] = useState<{ time: number; values: Record<string, number> } | null>(null);
  const key = series.map((s) => s.id).join('|');

  useEffect(() => {
    const instance = createChart(host.current!, baseOptions(look.zone));
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
  }, [key, look.key, baseline]);

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
