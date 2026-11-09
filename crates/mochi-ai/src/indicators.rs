use crate::IndicatorSnapshot;
use mochi_domain::Candle;
type Series = Vec<Option<f64>>;
fn ema(values: &[f64], period: usize) -> Series {
    let mut out = vec![None; values.len()];
    if values.len() < period {
        return out;
    }
    let mut prev = values[..period].iter().sum::<f64>() / period as f64;
    out[period - 1] = Some(prev);
    let k = 2. / (period + 1) as f64;
    for i in period..values.len() {
        prev = values[i] * k + prev * (1. - k);
        out[i] = Some(prev);
    }
    out
}
fn rsi(values: &[f64], period: usize) -> Series {
    let mut out = vec![None; values.len()];
    if values.len() <= period {
        return out;
    }
    let (mut gain, mut loss) = (0., 0.);
    for i in 1..=period {
        let d = values[i] - values[i - 1];
        if d >= 0. {
            gain += d;
        } else {
            loss -= d;
        }
    }
    gain /= period as f64;
    loss /= period as f64;
    fn value(g: f64, l: f64) -> f64 {
        if l == 0. {
            if g == 0. {
                50.
            } else {
                100.
            }
        } else {
            100. - 100. / (1. + g / l)
        }
    }
    out[period] = Some(value(gain, loss));
    for i in period + 1..values.len() {
        let d = values[i] - values[i - 1];
        gain = (gain * (period - 1) as f64 + d.max(0.)) / period as f64;
        loss = (loss * (period - 1) as f64 + (-d).max(0.)) / period as f64;
        out[i] = Some(value(gain, loss));
    }
    out
}
fn macd(values: &[f64]) -> (Series, Series, Series) {
    let fast = ema(values, 12);
    let slow = ema(values, 26);
    let line: Series = fast
        .iter()
        .zip(slow)
        .map(|(a, b)| a.zip(b).map(|(a, b)| a - b))
        .collect();
    let mut signal = vec![None; values.len()];
    if let Some(first) = line.iter().position(Option::is_some) {
        let sig = ema(
            &line[first..].iter().map(|x| x.unwrap()).collect::<Vec<_>>(),
            9,
        );
        signal[first..].copy_from_slice(&sig);
    }
    let histogram = line
        .iter()
        .zip(&signal)
        .map(|(a, b)| a.zip(*b).map(|(a, b)| a - b))
        .collect();
    (line, signal, histogram)
}
fn atr(c: &[Candle], period: usize) -> Series {
    let mut out = vec![None; c.len()];
    if c.len() <= period {
        return out;
    }
    let tr: Vec<f64> = c
        .iter()
        .enumerate()
        .map(|(i, k)| {
            if i == 0 {
                k.high - k.low
            } else {
                (k.high - k.low)
                    .max((k.high - c[i - 1].close).abs())
                    .max((k.low - c[i - 1].close).abs())
            }
        })
        .collect();
    let mut prev = tr[1..=period].iter().sum::<f64>() / period as f64;
    out[period] = Some(prev);
    for i in period + 1..c.len() {
        prev = (prev * (period - 1) as f64 + tr[i]) / period as f64;
        out[i] = Some(prev);
    }
    out
}
pub fn indicator_snapshot(candles: &[Candle]) -> IndicatorSnapshot {
    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
    let (m, s, h) = macd(&closes);
    fn last(s: Series) -> Option<f64> {
        s.last().copied().flatten()
    }
    [
        ("ema20", last(ema(&closes, 20))),
        ("ema50", last(ema(&closes, 50))),
        ("rsi14", last(rsi(&closes, 14))),
        ("macd", last(m)),
        ("macdSignal", last(s)),
        ("macdHistogram", last(h)),
        ("atr14", last(atr(candles, 14))),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v))
    .collect()
}
