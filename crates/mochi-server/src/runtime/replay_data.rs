//! The replay's bars. A runtime built with [`super::RuntimeOptions::with_live_feed`] replays
//! recorded OKX history over dates the owner picks: nothing is downloaded or held in memory
//! until then. Only minutes not already saved are fetched, and what arrives is merged with what
//! is there, so a range that overlaps an earlier one reuses it. Saved ranges can be deleted.
//! Every other runtime replays the bundled synthetic tape from the start.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use mochi_domain::{Candle, DataSource, ServerEvent, Topic};
use mochi_market::replay::ReplayMarketData;
use mochi_market::tape::{Tape, WARMUP_MINUTES};
use mochi_store::{
    count_replay_bars, delete_replay_span, get_setting, load_replay_bars, replay_bar_before,
    replay_coverage, replay_coverage_of, save_replay_bars, set_setting, Span,
};
use serde::{Deserialize, Serialize};

use super::{lock, unix_ms, Command, Runtime};
use crate::error::ApiError;

const DAY_MS: i64 = 86_400_000;
const MINUTE_MS: i64 = 60_000;
const WINDOW_KEY: &str = "replay.window";
/// Minutes fetched before a window's first minute, for the close a quiet first minute repeats.
const LEAD_MINUTES: i64 = 60;
/// The longest range the owner can pick. Every pair's bars are held in memory while it plays
/// (about 40 bytes a minute a pair), and a fresh download of it takes close to an hour.
pub const MAX_RANGE_DAYS: i64 = 90;
/// The days before the picked range that are downloaded too, so indicators start warm.
pub const WARMUP_DAYS: i64 = WARMUP_MINUTES as i64 / 1440;
/// OKX's most bars per history request.
const BARS_PER_REQUEST: u64 = mochi_market::okx_public::HISTORY_PAGE as u64;
/// Measured on 2026-11-22: 2,592 requests took 595 s, pauses for OKX's rate limit included.
const SECONDS_PER_REQUEST: f64 = 0.23;
/// Measured on 2026-11-22: 259,200 saved bars took 14.6 MB of database file.
pub const BYTES_PER_BAR: u64 = 57;
/// Said by everything that needs the replay's bars before they are loaded.
pub const NOT_LOADED: &str =
    "The replay data is not loaded yet. Open the replay to choose its dates.";
const OFFLINE: &str =
    "Offline, the replay plays the bundled synthetic tape: there is no OKX history to load.";

/// The dates the owner picks: whole UTC days, `from` included, `to` excluded.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ReplayRange {
    pub from: i64,
    pub to: i64,
}

/// What a replay covers: the picked range with its warm-up days in front.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Window {
    start: i64,
    minutes: usize,
}

impl Window {
    fn of(range: ReplayRange) -> Self {
        let start = range.from - WARMUP_DAYS * DAY_MS;
        Window {
            start,
            minutes: ((range.to - start) / MINUTE_MS) as usize,
        }
    }
    fn span(self) -> Span {
        (self.start, self.start + self.minutes as i64 * MINUTE_MS)
    }
    fn range(self) -> DateRange {
        DateRange {
            from: self.start + WARMUP_DAYS * DAY_MS,
            to: self.span().1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DateRange {
    pub from: i64,
    pub to: i64,
}

/// What loading a range would take, before anything is fetched.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayPlan {
    /// The picked dates.
    pub range: DateRange,
    /// What is downloaded and held: the picked dates and the warm-up days before them.
    pub span: DateRange,
    pub pairs: usize,
    /// Minutes still to fetch, summed over the pairs.
    pub missing_minutes: u64,
    pub requests: u64,
    /// A guess from the measured pace; the network and OKX decide the real one.
    pub seconds: u64,
    /// Database space the missing minutes will take.
    pub download_bytes: u64,
    /// Database space of what is already saved in the span and gets reused.
    pub saved_bytes: u64,
}

/// One stretch of saved bars.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedRange {
    pub start: i64,
    pub end: i64,
    /// Every built-in pair is saved over the whole stretch; else only some are.
    pub complete: bool,
    pub bytes: u64,
    /// The loaded replay plays part of it, so it cannot be deleted now.
    pub in_use: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayDataView {
    /// False when the replay plays the synthetic tape and has no history to manage.
    pub history: bool,
    /// The dates of the loaded replay.
    pub loaded: Option<DateRange>,
    /// The dates last picked, which a restart loads again.
    pub chosen: Option<DateRange>,
    /// Whether every minute of `chosen` is saved, so it loads with no download.
    pub chosen_ready: bool,
    pub saved: Vec<SavedRange>,
    /// The latest `to` the owner can pick: the last UTC midnight, since OKX's history ends there.
    pub latest: i64,
    pub max_days: i64,
    pub warmup_days: i64,
    pub bytes_per_bar: u64,
}

/// A download in progress: how many of `total` minutes, over every pair, are done.
pub(super) struct Download {
    done: Arc<AtomicUsize>,
    total: usize,
}

impl Download {
    pub(super) fn fraction(&self) -> f64 {
        (self.done.load(Ordering::Relaxed) as f64 / self.total.max(1) as f64).clamp(0.0, 1.0)
    }
}

fn last_midnight() -> i64 {
    unix_ms().div_euclid(DAY_MS) * DAY_MS
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::bad_request(message)
}

/// The parts of `span` that `covered` (sorted, not overlapping) leaves out.
fn gaps(span: Span, covered: &[Span]) -> Vec<Span> {
    let mut out = Vec::new();
    let mut at = span.0;
    for &(s, e) in covered {
        if e <= at || s >= span.1 {
            continue;
        }
        if s > at {
            out.push((at, s));
        }
        at = at.max(e);
    }
    if at < span.1 {
        out.push((at, span.1));
    }
    out
}

impl Runtime {
    pub(super) fn replay_loaded(&self) -> bool {
        lock(&self.replay).is_loaded()
    }

    /// Refuses work that needs the replay's bars before they are loaded.
    pub fn assert_replay_loaded(&self) -> Result<(), ApiError> {
        if self.replay_loaded() {
            Ok(())
        } else {
            Err(bad(NOT_LOADED))
        }
    }

    fn check_range(&self, range: ReplayRange) -> Result<Window, ApiError> {
        if range.from % DAY_MS != 0 || range.to % DAY_MS != 0 {
            return Err(bad("The replay's dates must be whole UTC days"));
        }
        if range.from >= range.to {
            return Err(bad("The replay must end after it starts"));
        }
        if range.to > last_midnight() {
            return Err(bad(
                "The replay must end by the last UTC midnight: later minutes are not history yet",
            ));
        }
        if (range.to - range.from) / DAY_MS > MAX_RANGE_DAYS {
            return Err(bad(format!(
                "The replay covers at most {MAX_RANGE_DAYS} days"
            )));
        }
        Ok(Window::of(range))
    }

    fn chosen_window(&self) -> Result<Option<Window>, ApiError> {
        get_setting::<Window>(self.connection(), WINDOW_KEY).map_err(|e| self.api_error(e))
    }

    /// Each pair's spans of `window` that are not saved yet. A gap at the window's first
    /// minute starts an hour earlier: if that minute had no trade, the bar before it gives the
    /// price it repeats.
    fn missing(&self, window: Window) -> Result<Vec<(&'static str, Span)>, ApiError> {
        let mut out = Vec::new();
        for symbol in lock(&self.replay).symbols() {
            let covered =
                replay_coverage_of(self.connection(), symbol).map_err(|e| self.api_error(e))?;
            for gap in gaps(window.span(), &covered) {
                let start = if gap.0 == window.start {
                    gap.0 - LEAD_MINUTES * MINUTE_MS
                } else {
                    gap.0
                };
                out.push((symbol, (start, gap.1)));
            }
        }
        Ok(out)
    }

    /// What loading `range` would download and take on disk.
    pub fn plan_replay(&self, range: ReplayRange) -> Result<ReplayPlan, ApiError> {
        if !self.replay_history {
            return Err(bad(OFFLINE));
        }
        let window = self.check_range(range)?;
        let missing = self.missing(window)?;
        let missing_minutes: u64 = missing
            .iter()
            .map(|(_, (s, e))| ((e - s) / MINUTE_MS) as u64)
            .sum();
        let requests: u64 = missing
            .iter()
            .map(|(_, (s, e))| (((e - s) / MINUTE_MS) as u64).div_ceil(BARS_PER_REQUEST))
            .sum();
        let saved_bars =
            count_replay_bars(self.connection(), window.span()).map_err(|e| self.api_error(e))?;
        Ok(ReplayPlan {
            range: window.range(),
            span: DateRange {
                from: window.span().0,
                to: window.span().1,
            },
            pairs: lock(&self.replay).symbols().len(),
            missing_minutes,
            requests,
            seconds: (requests as f64 * SECONDS_PER_REQUEST).ceil() as u64,
            download_bytes: missing_minutes * BYTES_PER_BAR,
            saved_bytes: saved_bars * BYTES_PER_BAR,
        })
    }

    /// Loads the replay over `range`, or over the dates last picked when `range` is `None`:
    /// from the database when every minute is saved, else by downloading the missing minutes
    /// from OKX in the background and merging them in.
    pub fn load_replay(&mut self, range: Option<ReplayRange>) -> Result<(), ApiError> {
        if !self.replay_history {
            return match range {
                None => Ok(()),
                Some(_) => Err(bad(OFFLINE)),
            };
        }
        if self.replay_download.is_some() {
            return Err(ApiError::conflict("A replay download is already running"));
        }
        let window = match range {
            Some(range) => self.check_range(range)?,
            None => self
                .chosen_window()?
                .ok_or_else(|| bad("Choose the replay's dates first"))?,
        };
        if self.replay_window == Some(window) && self.replay_loaded() {
            // Picked again after other dates failed to download: these are the dates to keep.
            set_setting(self.connection(), WINDOW_KEY, &window).map_err(|e| self.api_error(e))?;
            return Ok(());
        }
        if self.replay_loaded() {
            self.assert_replay_replaceable()?;
        }
        if self.chosen_window()? != Some(window) {
            set_setting(self.connection(), WINDOW_KEY, &window).map_err(|e| self.api_error(e))?;
            // A cursor counts minutes into the old dates; the new ones start after the warm-up.
            set_setting(self.connection(), "replay.cursor", &(WARMUP_MINUTES as i64))
                .map_err(|e| self.api_error(e))?;
        }
        self.replay_error = None;
        let missing = self.missing(window)?;
        if missing.is_empty() {
            let tapes = self.assemble(window)?;
            return self.install_replay(window, tapes);
        }
        let total: usize = missing
            .iter()
            .map(|(_, (s, e))| ((e - s) / MINUTE_MS) as usize)
            .sum();
        let done = Arc::new(AtomicUsize::new(0));
        self.replay_download = Some(Download {
            done: done.clone(),
            total,
        });
        self.emit(ServerEvent::Replay {
            state: self.replay_state(),
        });
        let live = self.live.clone();
        self.tasks.push(Box::pin(async move {
            let mut fetched = Vec::new();
            let mut error = None;
            for (symbol, span) in missing {
                let minutes = ((span.1 - span.0) / MINUTE_MS) as usize;
                let before = done.load(Ordering::Relaxed);
                let progress = |n: usize| done.store(before + n, Ordering::Relaxed);
                match live.history_bars(symbol, span.0, minutes, &progress).await {
                    Ok(bars) => fetched.push((symbol, span, bars)),
                    Err(e) => {
                        error = Some(format!("Could not download the {symbol} replay data: {e}"));
                        break;
                    }
                }
            }
            Box::new(move |r: &mut Runtime| r.finish_download(window, fetched, error)) as Command
        }));
        Ok(())
    }

    /// Replacing the loaded bars moves replay time, so nothing may be trading on them.
    fn assert_replay_replaceable(&mut self) -> Result<(), ApiError> {
        if self.replay_batch.is_some() || !self.replay_queue.is_empty() {
            return Err(ApiError::conflict(
                "The replay is still stepping; try again in a moment",
            ));
        }
        for agent in
            mochi_store::rows::list_agents(self.connection()).map_err(|e| self.api_error(e))?
        {
            if agent.running
                && self.require_account(&agent.account_id)?.source == DataSource::Replay
            {
                return Err(bad(format!(
                    "Stop {} first: it runs on the replay",
                    agent.name
                )));
            }
        }
        self.set_playing(false);
        Ok(())
    }

    /// Saves what arrived, so a failed download resumes where it stopped, then loads.
    fn finish_download(
        &mut self,
        window: Window,
        fetched: Vec<(&'static str, Span, Vec<Candle>)>,
        error: Option<String>,
    ) {
        self.replay_download = None;
        for (symbol, span, bars) in fetched {
            if let Err(e) = save_replay_bars(self.connection(), symbol, span, &bars) {
                let e = self.api_error(e);
                return self.replay_failed(e.message().to_string());
            }
        }
        if let Some(error) = error {
            return self.replay_failed(self.redact(&error));
        }
        // The owner may have picked other dates while this one downloaded.
        if self.chosen_window().ok().flatten() != Some(window) {
            return self.emit(ServerEvent::Replay {
                state: self.replay_state(),
            });
        }
        let result = (|| {
            if self.replay_loaded() {
                self.assert_replay_replaceable()?;
            }
            let tapes = self.assemble(window)?;
            self.install_replay(window, tapes)
        })();
        if let Err(e) = result {
            self.replay_failed(e.message().to_string());
        }
    }

    /// Every pair's tape over `window`, from the database. A minute without a trade repeats
    /// the last close with no volume: the price did not move.
    fn assemble(&self, window: Window) -> Result<Vec<(&'static str, Tape)>, ApiError> {
        let mut tapes = Vec::new();
        for symbol in lock(&self.replay).symbols() {
            let bars = load_replay_bars(self.connection(), symbol, window.span())
                .map_err(|e| self.api_error(e))?;
            let mut last = match bars.first() {
                Some(first) if first.time == window.start => Some(first.close),
                _ => replay_bar_before(self.connection(), symbol, window.start)
                    .map_err(|e| self.api_error(e))?
                    .map(|bar| bar.close),
            };
            let mut next = bars.into_iter().peekable();
            let mut candles = Vec::with_capacity(window.minutes);
            for i in 0..window.minutes {
                let time = window.start + i as i64 * MINUTE_MS;
                let candle = match next.next_if(|bar| bar.time == time) {
                    Some(bar) => bar,
                    None => {
                        let Some(close) = last else {
                            return Err(bad(format!(
                                "OKX has no {symbol} history that far back; pick a later start"
                            )));
                        };
                        Candle {
                            time,
                            open: close,
                            high: close,
                            low: close,
                            close,
                            volume: 0.0,
                            closed: true,
                        }
                    }
                };
                last = Some(candle.close);
                candles.push(candle);
            }
            tapes.push((
                symbol,
                Tape::from_candles(window.start, &candles).map_err(bad)?,
            ));
        }
        Ok(tapes)
    }

    fn replay_failed(&mut self, error: String) {
        self.replay_error = Some(error);
        self.emit(ServerEvent::Replay {
            state: self.replay_state(),
        });
    }

    /// Puts the bars in place, in the built-in pairs' order, and restores the saved cursor.
    fn install_replay(
        &mut self,
        window: Window,
        tapes: Vec<(&'static str, Tape)>,
    ) -> Result<(), ApiError> {
        let mut replay = ReplayMarketData::from_tapes(tapes).map_err(bad)?;
        // Other dates than the loaded ones start after their warm-up; the saved cursor only
        // belongs to the dates it was saved on, as at a restart.
        let switching = self.replay_window.is_some_and(|loaded| loaded != window);
        let cursor = if switching {
            Some(WARMUP_MINUTES as i64)
        } else {
            get_setting::<i64>(self.connection(), "replay.cursor").map_err(|e| self.api_error(e))?
        };
        if let Some(cursor) = cursor {
            replay.seek(cursor);
            set_setting(self.connection(), "replay.cursor", &cursor)
                .map_err(|e| self.api_error(e))?;
        }
        self.last_replay_equity_at = replay.now();
        *lock(&self.replay) = replay;
        self.replay_window = Some(window);
        self.replay_error = None;
        self.emit(ServerEvent::Replay {
            state: self.replay_state(),
        });
        self.invalidate(&[Topic::Accounts, Topic::Positions]);
        Ok(())
    }

    /// The saved bars as stretches of time, with what each takes on disk.
    pub fn replay_data(&self) -> Result<ReplayDataView, ApiError> {
        let symbols = lock(&self.replay).symbols();
        let coverage = replay_coverage(self.connection()).map_err(|e| self.api_error(e))?;
        // Stretches where any pair is saved, merged.
        let mut stretches: Vec<Span> = Vec::new();
        for (_, (s, e)) in &coverage {
            match stretches.last_mut() {
                Some(last) if *s <= last.1 => last.1 = last.1.max(*e),
                _ => stretches.push((*s, *e)),
            }
        }
        let loaded = self.replay_window.filter(|_| self.replay_loaded());
        let mut saved = Vec::new();
        for (start, end) in stretches {
            let complete = symbols.iter().all(|symbol| {
                coverage
                    .iter()
                    .any(|(s, (cs, ce))| s == symbol && *cs <= start && *ce >= end)
            });
            let bars = count_replay_bars(self.connection(), (start, end))
                .map_err(|e| self.api_error(e))?;
            saved.push(SavedRange {
                start,
                end,
                complete,
                bytes: bars * BYTES_PER_BAR,
                in_use: loaded.is_some_and(|w| w.span().0 < end && w.span().1 > start),
            });
        }
        let chosen = self.chosen_window()?;
        Ok(ReplayDataView {
            history: self.replay_history,
            loaded: loaded.map(Window::range),
            chosen: chosen.map(Window::range),
            chosen_ready: match chosen {
                Some(window) => self.missing(window)?.is_empty(),
                None => false,
            },
            saved,
            latest: last_midnight(),
            max_days: MAX_RANGE_DAYS,
            warmup_days: WARMUP_DAYS,
            bytes_per_bar: BYTES_PER_BAR,
        })
    }

    /// Deletes every pair's saved bars between `start` and `end`. The loaded replay's bars and
    /// a running download's are kept.
    pub fn delete_replay_data(&mut self, start: i64, end: i64) -> Result<(), ApiError> {
        if !self.replay_history {
            return Err(bad(OFFLINE));
        }
        if start >= end {
            return Err(bad("Nothing to delete: the range ends before it starts"));
        }
        // Bars and coverage are kept per minute; a partial minute would leave them out of step.
        if start % MINUTE_MS != 0 || end % MINUTE_MS != 0 {
            return Err(bad(
                "Delete whole minutes: start and end must fall on a minute",
            ));
        }
        if self.replay_download.is_some() {
            return Err(ApiError::conflict(
                "Wait for the replay download to finish first",
            ));
        }
        if let Some(window) = self.replay_window.filter(|_| self.replay_loaded()) {
            if window.span().0 < end && window.span().1 > start {
                return Err(bad(
                    "The loaded replay uses these bars. Load other dates first.",
                ));
            }
        }
        delete_replay_span(self.connection(), (start, end)).map_err(|e| self.api_error(e))
    }
}

#[cfg(test)]
mod tests {
    use super::gaps;

    #[test]
    fn gaps_are_what_the_coverage_leaves_out() {
        assert_eq!(gaps((0, 10), &[]), [(0, 10)]);
        assert_eq!(gaps((0, 10), &[(2, 4), (6, 8)]), [(0, 2), (4, 6), (8, 10)]);
        assert_eq!(gaps((0, 10), &[(-5, 3), (3, 12)]), []);
        assert_eq!(gaps((5, 10), &[(0, 6), (20, 30)]), [(6, 10)]);
    }
}
