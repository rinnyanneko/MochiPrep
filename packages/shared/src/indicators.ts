/** Pure indicator maths. Output arrays align with input; null = not enough data yet. */
export type Series = (number | null)[];

export function ema(values: number[], period: number): Series {
  const out: Series = new Array(values.length).fill(null);
  if (values.length < period) return out;
  let prev = values.slice(0, period).reduce((a, b) => a + b, 0) / period;
  out[period - 1] = prev;
  const k = 2 / (period + 1);
  for (let i = period; i < values.length; i++) {
    prev = values[i]! * k + prev * (1 - k);
    out[i] = prev;
  }
  return out;
}

/** Wilder RSI. */
export function rsi(values: number[], period = 14): Series {
  const out: Series = new Array(values.length).fill(null);
  if (values.length <= period) return out;
  let gain = 0;
  let loss = 0;
  for (let i = 1; i <= period; i++) {
    const d = values[i]! - values[i - 1]!;
    if (d >= 0) gain += d;
    else loss -= d;
  }
  gain /= period;
  loss /= period;
  const value = () => (loss === 0 ? (gain === 0 ? 50 : 100) : 100 - 100 / (1 + gain / loss));
  out[period] = value();
  for (let i = period + 1; i < values.length; i++) {
    const d = values[i]! - values[i - 1]!;
    gain = (gain * (period - 1) + Math.max(d, 0)) / period;
    loss = (loss * (period - 1) + Math.max(-d, 0)) / period;
    out[i] = value();
  }
  return out;
}

export interface MacdResult {
  macd: Series;
  signal: Series;
  histogram: Series;
}

export function macd(values: number[], fast = 12, slow = 26, signalPeriod = 9): MacdResult {
  const fastE = ema(values, fast);
  const slowE = ema(values, slow);
  const line: Series = values.map((_, i) =>
    fastE[i] == null || slowE[i] == null ? null : fastE[i]! - slowE[i]!,
  );
  const firstIdx = line.findIndex((v) => v != null);
  const signal: Series = new Array(values.length).fill(null);
  if (firstIdx >= 0) {
    const sig = ema(line.slice(firstIdx) as number[], signalPeriod);
    sig.forEach((v, i) => (signal[firstIdx + i] = v));
  }
  const histogram: Series = line.map((v, i) => (v == null || signal[i] == null ? null : v - signal[i]!));
  return { macd: line, signal, histogram };
}

export function atr(highs: number[], lows: number[], closes: number[], period = 14): Series {
  const out: Series = new Array(closes.length).fill(null);
  if (closes.length <= period) return out;
  const tr = closes.map((_, i) =>
    i === 0
      ? highs[0]! - lows[0]!
      : Math.max(
          highs[i]! - lows[i]!,
          Math.abs(highs[i]! - closes[i - 1]!),
          Math.abs(lows[i]! - closes[i - 1]!),
        ),
  );
  let prev = tr.slice(1, period + 1).reduce((a, b) => a + b, 0) / period;
  out[period] = prev;
  for (let i = period + 1; i < closes.length; i++) {
    prev = (prev * (period - 1) + tr[i]!) / period;
    out[i] = prev;
  }
  return out;
}

const last = (s: Series): number | null => (s.length ? (s[s.length - 1] ?? null) : null);

export interface IndicatorSnapshot extends Record<string, number | null> {
  ema20: number | null;
  ema50: number | null;
  rsi14: number | null;
  macd: number | null;
  macdSignal: number | null;
  macdHistogram: number | null;
  atr14: number | null;
}

export function indicatorSnapshot(candles: { high: number; low: number; close: number }[]): IndicatorSnapshot {
  const closes = candles.map((c) => c.close);
  const m = macd(closes);
  return {
    ema20: last(ema(closes, 20)),
    ema50: last(ema(closes, 50)),
    rsi14: last(rsi(closes, 14)),
    macd: last(m.macd),
    macdSignal: last(m.signal),
    macdHistogram: last(m.histogram),
    atr14: last(atr(candles.map((c) => c.high), candles.map((c) => c.low), closes, 14)),
  };
}
