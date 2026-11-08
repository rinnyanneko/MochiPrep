//! When the replay tape moves: `setPlaying`, `setSpeed` and `onReplayTimer` from
//! `apps/server/src/runtime.ts`. The runtime owns one clock and runs the batches it asks for.

use std::time::Duration;

use tokio::time::{interval_at, Instant, Interval, MissedTickBehavior};

/// Node's REPLAY_BATCH_MS: how often a playing tape earns step credit.
pub const REPLAY_BATCH_MS: u64 = 100;

/// Tape-minutes per second when the owner never chose a speed.
pub const DEFAULT_SPEED: u32 = 20;

/// Playing, speed and step credit. `playing` lives in memory only, so a restart is paused.
pub struct ReplayClock {
    playing: bool,
    speed: u32,
    /// Fractional steps earned but not yet run. Never reset on pause, as in Node.
    credit: f64,
    /// A batch (timer or manual) is running.
    in_flight: bool,
    /// Ticks every 100 ms while playing; `None` while paused, like Node's cleared interval.
    timer: Option<Interval>,
}

impl ReplayClock {
    pub fn new(speed: u32) -> Self {
        ReplayClock {
            playing: false,
            speed,
            credit: 0.0,
            in_flight: false,
            timer: None,
        }
    }

    pub fn playing(&self) -> bool {
        self.playing
    }

    pub fn speed(&self) -> u32 {
        self.speed
    }

    pub fn set_speed(&mut self, speed: u32) {
        self.speed = speed;
    }

    /// Starts or stops the timer. Returns whether anything changed, since Node only emits the
    /// replay state on a change. Must run inside a tokio runtime.
    pub fn set_playing(&mut self, playing: bool) -> bool {
        if playing == self.playing {
            return false;
        }
        self.playing = playing;
        self.timer = if playing {
            let period = Duration::from_millis(REPLAY_BATCH_MS);
            // The first tick is one period away, like setInterval's.
            let mut timer = interval_at(Instant::now() + period, period);
            // A late tick must not be followed by a burst of catch-up ticks: Node's timer
            // never fires twice to make up for a busy event loop.
            timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
            Some(timer)
        } else {
            None
        };
        true
    }

    /// Waits for the next tick. Never returns while paused.
    pub async fn next_tick(&mut self) {
        match &mut self.timer {
            Some(timer) => {
                timer.tick().await;
            }
            None => std::future::pending::<()>().await,
        }
    }

    /// One timer tick: earns `speed × 0.1` steps and returns the whole steps to run now, if any.
    /// While a batch is in flight nothing is earned: "the tape waits for agents", so a slow
    /// model pauses market time rather than missing bars.
    pub fn on_tick(&mut self) -> Option<u32> {
        if self.in_flight {
            return None;
        }
        // Same operations and order as `this.speed * REPLAY_BATCH_MS / 1000`, so the floating
        // point credit matches Node's bit for bit.
        self.credit += f64::from(self.speed) * REPLAY_BATCH_MS as f64 / 1000.0;
        let steps = self.credit.floor();
        if steps < 1.0 {
            return None;
        }
        self.credit -= steps;
        Some(steps as u32)
    }

    /// Marks a batch as running. Batches are serialised, so at most one is ever in flight.
    pub fn begin_batch(&mut self) {
        self.in_flight = true;
    }

    pub fn end_batch(&mut self) {
        self.in_flight = false;
    }
}
