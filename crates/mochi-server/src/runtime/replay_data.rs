//! The replay's bars. A runtime built with [`super::RuntimeOptions::with_live_feed`] replays
//! recorded OKX history: nothing is downloaded or held in memory until the replay is first
//! used, then OKX's one-minute bars of every built-in pair are fetched once, saved in the
//! database, and read from there on later loads. Every other runtime replays the bundled
//! synthetic tape from the start.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use mochi_domain::{Candle, ServerEvent, Topic};
use mochi_market::replay::ReplayMarketData;
use mochi_market::tape::{Tape, TAPE_MINUTES};
use mochi_store::{get_setting, load_replay_bars, save_replay_bars, set_setting};
use serde::{Deserialize, Serialize};

use super::{lock, unix_ms, Command, Runtime};
use crate::error::ApiError;

const DAY_MS: i64 = 86_400_000;
const WINDOW_KEY: &str = "replay.window";
/// Said by everything that needs the replay's bars before they are loaded.
pub const NOT_LOADED: &str = "The replay data is not loaded yet. Open the replay to download it.";

/// The minutes the recorded replay covers: fixed when first downloaded, so every agent and
/// every later load sees the same bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Window {
    start: i64,
    minutes: usize,
}

/// A download in progress: how many of `total` minutes, over every pair, have arrived.
pub(super) struct Download {
    done: Arc<AtomicUsize>,
    total: usize,
}

impl Download {
    pub(super) fn fraction(&self) -> f64 {
        (self.done.load(Ordering::Relaxed) as f64 / self.total.max(1) as f64).clamp(0.0, 1.0)
    }
}

impl Runtime {
    pub(super) fn replay_loaded(&self) -> bool {
        lock(&self.replay).is_loaded()
    }

    /// Refuses work that needs the replay's bars before they are loaded.
    pub(super) fn assert_replay_loaded(&self) -> Result<(), ApiError> {
        if self.replay_loaded() {
            Ok(())
        } else {
            Err(ApiError::bad_request(NOT_LOADED))
        }
    }

    /// Loads the replay's bars: from the database when every pair is saved there, else from
    /// OKX in the background. Does nothing while loaded or loading.
    pub fn load_replay(&mut self) -> Result<(), ApiError> {
        if self.replay_loaded() || self.replay_download.is_some() {
            return Ok(());
        }
        self.replay_error = None;
        let window = match get_setting::<Window>(self.connection(), WINDOW_KEY)
            .map_err(|e| self.api_error(e))?
        {
            Some(window) => window,
            // The window ends at the last UTC midnight: every minute in it is history on OKX.
            None => Window {
                start: unix_ms().div_euclid(DAY_MS) * DAY_MS - TAPE_MINUTES as i64 * 60_000,
                minutes: TAPE_MINUTES,
            },
        };
        let symbols = lock(&self.replay).symbols();
        let mut saved = Vec::new();
        let mut missing = Vec::new();
        for symbol in symbols {
            match load_replay_bars(self.connection(), symbol, window.start, window.minutes)
                .map_err(|e| self.api_error(e))?
            {
                Some(bars) => saved.push((symbol, tape(window, &bars)?)),
                None => missing.push(symbol),
            }
        }
        if missing.is_empty() {
            return self.install_replay(saved);
        }
        set_setting(self.connection(), WINDOW_KEY, &window).map_err(|e| self.api_error(e))?;
        let done = Arc::new(AtomicUsize::new(saved.len() * window.minutes));
        self.replay_download = Some(Download {
            done: done.clone(),
            total: (saved.len() + missing.len()) * window.minutes,
        });
        self.emit(ServerEvent::Replay {
            state: self.replay_state(),
        });
        let live = self.live.clone();
        self.tasks.push(Box::pin(async move {
            let mut fetched = Vec::new();
            let mut error = None;
            for symbol in missing {
                let before = done.load(Ordering::Relaxed);
                let progress = |minutes: usize| done.store(before + minutes, Ordering::Relaxed);
                match live
                    .history_tape(symbol, window.start, window.minutes, &progress)
                    .await
                {
                    Ok(tape) => fetched.push((symbol, tape)),
                    Err(e) => {
                        error = Some(format!("Could not download the {symbol} replay data: {e}"));
                        break;
                    }
                }
            }
            Box::new(move |r: &mut Runtime| r.finish_download(saved, fetched, error)) as Command
        }));
        Ok(())
    }

    /// Saves what arrived, so a failed download resumes where it stopped, then installs the
    /// bars or reports why not.
    fn finish_download(
        &mut self,
        mut saved: Vec<(&'static str, Tape)>,
        fetched: Vec<(&'static str, Tape)>,
        error: Option<String>,
    ) {
        self.replay_download = None;
        for (symbol, tape) in fetched {
            let bars: Vec<Candle> = (0..tape.len()).map(|i| tape.candle(i)).collect();
            if let Err(e) = save_replay_bars(self.connection(), symbol, &bars) {
                let e = self.api_error(e);
                return self.replay_failed(e.message().to_string());
            }
            saved.push((symbol, tape));
        }
        if let Some(error) = error {
            return self.replay_failed(self.redact(&error));
        }
        if let Err(e) = self.install_replay(saved) {
            self.replay_failed(e.message().to_string());
        }
    }

    fn replay_failed(&mut self, error: String) {
        self.replay_error = Some(error);
        self.emit(ServerEvent::Replay {
            state: self.replay_state(),
        });
    }

    /// Puts the bars in place, in the built-in pairs' order, and restores the saved cursor.
    fn install_replay(&mut self, mut tapes: Vec<(&'static str, Tape)>) -> Result<(), ApiError> {
        let order = lock(&self.replay).symbols();
        tapes.sort_by_key(|(symbol, _)| order.iter().position(|s| s == symbol));
        let mut replay = ReplayMarketData::from_tapes(tapes).map_err(ApiError::bad_request)?;
        if let Some(cursor) =
            get_setting::<i64>(self.connection(), "replay.cursor").map_err(|e| self.api_error(e))?
        {
            replay.seek(cursor);
        }
        self.last_replay_equity_at = replay.now();
        *lock(&self.replay) = replay;
        self.replay_error = None;
        self.emit(ServerEvent::Replay {
            state: self.replay_state(),
        });
        self.invalidate(&[Topic::Accounts, Topic::Positions]);
        Ok(())
    }
}

fn tape(window: Window, bars: &[Candle]) -> Result<Tape, ApiError> {
    Tape::from_candles(window.start, bars).map_err(ApiError::bad_request)
}
