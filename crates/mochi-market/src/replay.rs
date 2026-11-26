//! Offline deterministic feed over the bundled tape. A port of the quote and candle parts of
//! `packages/market-data/src/replay.ts`; listeners and feed status belong to the server runtime.
//! Quotes drive simulated fills, so float expressions keep the TypeScript operand order.

use crate::tape::{generate_tape, Tape, TAPE_MINUTES, WARMUP_MINUTES};
use mochi_domain::jsnum::{js_round, js_to_fixed};
use mochi_domain::{default_instrument, Candle, DataSource, Quote, Timeframe, DEFAULT_SYMBOLS};

const FUNDING_MS: i64 = 8 * 3_600_000;
const SPREAD_FRACTION: f64 = 0.00005;
const MINUTE_MS: i64 = 60_000;

/// A tape too short to play: the cursor starts after the warmup, so at least one bar must follow.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("A replay tape needs more than {WARMUP_MINUTES} minutes, got {0}")]
pub struct TapeTooShort(pub usize);

/// Time only moves when `step()` is called, so a caller can hold the tape still while slow AI
/// calls finish, and two runs of the same tape see identical quotes. One quote per 1m bar,
/// taken at the close: wicks are never treated as tradable prices.
pub struct ReplayMarketData {
    /// The built-in pairs, `DEFAULT_SYMBOLS` order unless the caller chose others. Custom pairs
    /// only exist on live data.
    symbols: Vec<&'static str>,
    /// One tape per pair in `symbols`, or none while the replay's data is not loaded yet.
    tapes: Vec<(&'static str, Tape)>,
    cursor: usize,
}

impl ReplayMarketData {
    /// TypeScript accepts any length and then reads past the end of a short tape; here a tape
    /// with no bar after the warmup is refused.
    pub fn new(minutes: Option<usize>) -> Result<Self, TapeTooShort> {
        Self::with_symbols(minutes, &DEFAULT_SYMBOLS)
    }

    /// A tape of only some default pairs, in the given order. Tests that replay recordings
    /// from the Node backend, which shipped three pairs, use this to see the tape Node had.
    pub fn with_symbols(
        minutes: Option<usize>,
        symbols: &[&'static str],
    ) -> Result<Self, TapeTooShort> {
        let minutes = minutes.unwrap_or(TAPE_MINUTES);
        if minutes <= WARMUP_MINUTES {
            return Err(TapeTooShort(minutes));
        }
        assert!(!symbols.is_empty(), "a replay needs at least one pair");
        let tapes = symbols
            .iter()
            .map(|&symbol| {
                let tape = generate_tape(symbol, minutes).expect("every default symbol has a tape");
                (symbol, tape)
            })
            .collect();
        Ok(Self {
            symbols: symbols.to_vec(),
            tapes,
            cursor: WARMUP_MINUTES,
        })
    }

    /// A replay of `symbols` whose data comes later (see [`Self::from_tapes`]). Until then it has
    /// no bars: no quotes, no candles, and it is always at its end.
    pub fn unloaded(symbols: &[&'static str]) -> Self {
        Self {
            symbols: symbols.to_vec(),
            tapes: Vec::new(),
            cursor: 0,
        }
    }

    /// A replay over recorded bars, one tape per pair. Every tape must start at the same time
    /// and have the same length, since the pairs share one clock.
    pub fn from_tapes(tapes: Vec<(&'static str, Tape)>) -> Result<Self, String> {
        let Some((_, first)) = tapes.first() else {
            return Err("A replay needs at least one pair".into());
        };
        if first.len() <= WARMUP_MINUTES {
            return Err(TapeTooShort(first.len()).to_string());
        }
        let (start, len) = (first.start(), first.len());
        if let Some((symbol, _)) = tapes
            .iter()
            .find(|(_, t)| t.start() != start || t.len() != len)
        {
            return Err(format!(
                "The {symbol} replay bars do not line up with the others"
            ));
        }
        Ok(Self {
            symbols: tapes.iter().map(|(symbol, _)| *symbol).collect(),
            tapes,
            cursor: WARMUP_MINUTES,
        })
    }

    pub fn is_loaded(&self) -> bool {
        !self.tapes.is_empty()
    }

    /// The pairs on this tape, which are the built-in ones.
    pub fn symbols(&self) -> Vec<&'static str> {
        self.symbols.clone()
    }

    /// All tapes share one clock, so the first one answers time questions. `None` until loaded.
    fn clock(&self) -> Option<&Tape> {
        self.tapes.first().map(|(_, tape)| tape)
    }

    fn tape(&self, symbol: &str) -> Option<&Tape> {
        self.tapes
            .iter()
            .find(|(name, _)| *name == symbol)
            .map(|(_, tape)| tape)
    }

    pub fn len(&self) -> usize {
        self.clock().map_or(0, Tape::len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn position(&self) -> usize {
        self.cursor
    }

    pub fn at_end(&self) -> bool {
        self.cursor + 1 >= self.len()
    }

    /// The times below are 0 until the data is loaded: no clock has started.
    pub fn first_playable_time(&self) -> i64 {
        self.clock()
            .map_or(0, |clock| clock.time(WARMUP_MINUTES) + MINUTE_MS)
    }

    pub fn end_time(&self) -> i64 {
        self.clock()
            .map_or(0, |clock| clock.time(clock.len() - 1) + MINUTE_MS)
    }

    pub fn now(&self) -> i64 {
        self.clock()
            .map_or(0, |clock| clock.time(self.cursor) + MINUTE_MS)
    }

    /// Takes a signed index because callers pass arbitrary numbers; it is clamped to the playable range.
    pub fn seek(&mut self, index: i64) {
        if !self.is_loaded() {
            return;
        }
        let last = self.len() as i64 - 1;
        self.cursor = index.max(WARMUP_MINUTES as i64).min(last) as usize;
    }

    /// Advances one minute and returns one quote per symbol. Empty at tape end.
    pub fn step(&mut self) -> Vec<Quote> {
        if self.at_end() {
            return Vec::new();
        }
        self.cursor += 1;
        self.tapes
            .iter()
            .map(|(symbol, _)| self.get_quote(symbol).unwrap())
            .collect()
    }

    /// None for any pair outside the tape.
    pub fn get_quote(&self, symbol: &str) -> Option<Quote> {
        let tape = self.tape(symbol)?;
        let candle = tape.candle(self.cursor);
        let instrument = default_instrument(symbol)?;
        let tick: f64 = instrument.price_tick.parse().unwrap();
        let decimals = instrument
            .price_tick
            .split('.')
            .nth(1)
            .map_or(0, |fraction| fraction.len());
        let half = tick.max(js_round(candle.close * SPREAD_FRACTION / tick) * tick);
        let ts = candle.time + MINUTE_MS;
        Some(Quote {
            source: DataSource::Replay,
            symbol: symbol.to_string(),
            bid: js_to_fixed(candle.close - half, decimals),
            ask: js_to_fixed(candle.close + half, decimals),
            last: js_to_fixed(candle.close, decimals),
            mark: js_to_fixed(candle.close, decimals),
            mark_estimated: false,
            funding_rate: Some(self.funding_rate(tape)),
            next_funding_time: Some((ts / FUNDING_MS + 1) * FUNDING_MS),
            ts,
        })
    }

    /// Follows the tape's own 8h return, like a perp premium would, clamped to +-0.05%.
    fn funding_rate(&self, tape: &Tape) -> String {
        let back = self.cursor.saturating_sub(480);
        let close = tape.close();
        let change = (close[self.cursor] - close[back]) / close[back];
        let rate = (0.0001 + change * 0.01).clamp(-0.0005, 0.0005);
        js_to_fixed(rate, 6)
    }

    /// Aggregates 1m bars into `timeframe` buckets, newest first, then returns them oldest first.
    /// The newest bucket is partial (up to the cursor) and `closed` is false until its end has passed.
    pub fn get_candles(&self, symbol: &str, timeframe: Timeframe, limit: usize) -> Vec<Candle> {
        let Some(tape) = self.tape(symbol) else {
            return Vec::new();
        };
        let size = timeframe.ms();
        let now = self.now();
        let mut out: Vec<Candle> = Vec::new();
        // Signed so the walk can step below index 0, like the TypeScript loop.
        let mut i = self.cursor as i64;
        while i >= 0 && out.len() < limit {
            let bucket = tape.time(i as usize).div_euclid(size) * size;
            let mut bar = tape.candle(i as usize);
            bar.time = bucket;
            bar.closed = bucket + size <= now;
            i -= 1;
            while i >= 0 && tape.time(i as usize) >= bucket {
                let at = i as usize;
                bar.open = tape.open()[at];
                bar.high = bar.high.max(tape.high()[at]);
                bar.low = bar.low.min(tape.low()[at]);
                bar.volume += tape.volume()[at];
                i -= 1;
            }
            // Round through text like `Number(x.toFixed(3))` so the value matches TypeScript.
            bar.volume = js_to_fixed(bar.volume, 3).parse().unwrap();
            out.push(bar);
        }
        out.reverse();
        out
    }
}
