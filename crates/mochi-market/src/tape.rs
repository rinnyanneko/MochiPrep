//! The bundled demo tape: a seeded random walk through trend, chop and crash regimes.
//! A line-by-line port of `packages/market-data/src/synthetic.ts`. Demo mode must replay
//! bit-for-bit as before, so every float expression keeps the TypeScript operand order
//! (Rust never fuses multiply-adds, so the same order gives the same bits).

use mochi_domain::jsnum::{js_round, js_to_fixed};
use mochi_domain::{default_instrument, Candle};

/// 2026-01-01T00:00:00Z. The tape is synthetic, so this date means nothing about real markets.
pub const TAPE_START: i64 = 1_767_225_600_000;
pub const TAPE_MINUTES: usize = 18 * 1440;
/// History shown before the first playable bar, so indicators are warm.
pub const WARMUP_MINUTES: usize = 4 * 1440;

struct Seed {
    seed: i32,
    price: f64,
    volume: f64,
}

fn seed_for(symbol: &str) -> Option<Seed> {
    match symbol {
        "BTCUSDT" => Some(Seed {
            seed: 0x6d6f6368,
            price: 68_000.0,
            volume: 42.0,
        }),
        "ETHUSDT" => Some(Seed {
            seed: 0x69707265,
            price: 3_400.0,
            volume: 520.0,
        }),
        "SOLUSDT" => Some(Seed {
            seed: 0x70303031,
            price: 150.0,
            volume: 6_400.0,
        }),
        _ => None,
    }
}

/// Integer-only PRNG, so the tape is identical on every machine. JS runs this on int32 values
/// (`| 0`, `Math.imul`) with `>>>` as an unsigned shift, which is what the casts below mirror.
struct Mulberry32 {
    a: i32,
}

impl Mulberry32 {
    fn next(&mut self) -> f64 {
        self.a = self.a.wrapping_add(0x6d2b79f5);
        let a = self.a;
        let mut t = (a ^ ((a as u32) >> 15) as i32).wrapping_mul(1 | a);
        t = t.wrapping_add((t ^ ((t as u32) >> 7) as i32).wrapping_mul(61 | t)) ^ t;
        f64::from((t ^ ((t as u32) >> 14) as i32) as u32) / 4294967296.0
    }

    /// Sum of six uniforms, summed left to right, minus its mean of 3.
    fn gaussian(&mut self) -> f64 {
        self.next() + self.next() + self.next() + self.next() + self.next() + self.next() - 3.0
    }
}

/// One-minute bars stored column-wise, like the TypeScript `Float64Array`s.
/// Times are implicit, `TAPE_START + i * 60_000`, since the tape has no gaps.
///
/// The columns are private and only grow together through `push`, so they always have one
/// length and `candle(i)` can index all five.
pub struct Tape {
    open: Vec<f64>,
    high: Vec<f64>,
    low: Vec<f64>,
    close: Vec<f64>,
    volume: Vec<f64>,
}

impl Tape {
    fn with_capacity(minutes: usize) -> Self {
        Tape {
            open: Vec::with_capacity(minutes),
            high: Vec::with_capacity(minutes),
            low: Vec::with_capacity(minutes),
            close: Vec::with_capacity(minutes),
            volume: Vec::with_capacity(minutes),
        }
    }

    fn push(&mut self, open: f64, high: f64, low: f64, close: f64, volume: f64) {
        self.open.push(open);
        self.high.push(high);
        self.low.push(low);
        self.close.push(close);
        self.volume.push(volume);
    }

    pub fn open(&self) -> &[f64] {
        &self.open
    }

    pub fn high(&self) -> &[f64] {
        &self.high
    }

    pub fn low(&self) -> &[f64] {
        &self.low
    }

    pub fn close(&self) -> &[f64] {
        &self.close
    }

    pub fn volume(&self) -> &[f64] {
        &self.volume
    }

    pub fn len(&self) -> usize {
        self.open.len()
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    pub fn time(&self, i: usize) -> i64 {
        TAPE_START + i as i64 * 60_000
    }

    pub fn candle(&self, i: usize) -> Candle {
        Candle {
            time: self.time(i),
            open: self.open[i],
            high: self.high[i],
            low: self.low[i],
            close: self.close[i],
            volume: self.volume[i],
            closed: true,
        }
    }
}

/// `None` for symbols without bundled data (anything but `DEFAULT_SYMBOLS`).
pub fn generate_tape(symbol: &str, minutes: usize) -> Option<Tape> {
    let Seed {
        seed,
        price: start_price,
        volume: base_volume,
    } = seed_for(symbol)?;
    let price_tick = default_instrument(symbol)?.price_tick;
    let tick: f64 = price_tick.parse().ok()?;
    let decimals = price_tick.split('.').nth(1).map_or(0, str::len);
    // Round to the tick the way JS does, through a decimal string, so float noise is cut identically.
    let round_tick = |v: f64| -> f64 {
        js_to_fixed(js_round(v / tick) * tick, decimals)
            .parse()
            .expect("toFixed output is a number")
    };
    let mut rng = Mulberry32 { a: seed };

    let mut tape = Tape::with_capacity(minutes);
    let mut price = start_price;
    let mut drift = 0.0_f64;
    let mut vol = 0.0005_f64;
    let mut regime_left: i32 = 0;
    for _ in 0..minutes {
        // JS `regimeLeft-- <= 0` compares the old value, then decrements.
        let expired = regime_left <= 0;
        regime_left -= 1;
        if expired {
            let pick = rng.next();
            regime_left = 180 + (rng.next() * 900.0).floor() as i32;
            if pick < 0.3 {
                (drift, vol) = (0.00006, 0.0006);
            } else if pick < 0.6 {
                (drift, vol) = (-0.00006, 0.0007);
            } else if pick < 0.92 {
                (drift, vol) = (0.0, 0.0004);
            } else {
                (drift, vol, regime_left) = (-0.00035, 0.0016, 120);
            }
        }
        let open = price;
        let mut high = open;
        let mut low = open;
        for _ in 0..4 {
            price *= 1.0 + drift / 4.0 + (rng.gaussian() * vol) / 2.0;
            high = high.max(price);
            low = low.min(price);
        }
        let volume =
            base_volume * (0.6 + rng.next()) * (1.0 + ((price - open).abs() / open) * 900.0);
        tape.push(
            round_tick(open),
            round_tick(high),
            round_tick(low),
            round_tick(price),
            js_to_fixed(volume, 3)
                .parse()
                .expect("toFixed output is a number"),
        );
    }
    Some(tape)
}
