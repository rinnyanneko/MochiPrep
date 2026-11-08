//! Fan-out of `ServerEvent`s to every subscriber (the SSE streams), with invalidations
//! coalesced as Node's `invalidate` does (runtime.ts:210-218).

use std::collections::HashMap;
use std::time::Duration;

use mochi_domain::{Quote, ServerEvent, Topic};
use tokio::sync::broadcast;
use tokio::time::Instant;

/// Node's INVALIDATE_MS: topics marked dirty within this window go out as one event.
pub const INVALIDATE_MS: u64 = 200;

/// Node's QUOTE_BROADCAST_MS: the least time between two live quotes sent for one symbol.
pub const QUOTE_BROADCAST_MS: u64 = 250;

/// Events buffered per subscriber. A subscriber that falls this far behind loses the oldest
/// ones (`RecvError::Lagged`) instead of holding up the runtime; the SSE route decides what a
/// lagging client gets.
const CHANNEL_CAPACITY: usize = 1024;

pub struct EventHub {
    sender: broadcast::Sender<ServerEvent>,
    /// Topics to send, in the order first marked (a JS Set keeps insertion order).
    dirty: Vec<Topic>,
    /// When the pending invalidate goes out; `None` when nothing is pending.
    flush_at: Option<Instant>,
    /// When each symbol's last live quote went out.
    quote_sent_at: HashMap<String, Instant>,
}

impl EventHub {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(CHANNEL_CAPACITY);
        EventHub {
            sender,
            dirty: Vec::new(),
            flush_at: None,
            quote_sent_at: HashMap::new(),
        }
    }

    /// A sender handle, so subscribing never has to go through the runtime task.
    pub fn sender(&self) -> broadcast::Sender<ServerEvent> {
        self.sender.clone()
    }

    pub fn emit(&self, event: ServerEvent) {
        // No subscribers is not an error: nobody has the page open.
        let _ = self.sender.send(event);
    }

    /// A live quote for subscribers; the engine has already seen it. At most one per symbol
    /// every 250 ms goes out, as in Node: OKX sends tickers far faster than a screen needs.
    /// Throttled here, before the fan-out, so every client sees the same quotes. The first
    /// quote for a symbol always goes out (Node's `?? 0`).
    pub fn emit_live_quote(&mut self, quote: Quote) {
        let now = Instant::now();
        let throttle = Duration::from_millis(QUOTE_BROADCAST_MS);
        if let Some(sent_at) = self.quote_sent_at.get(&quote.symbol) {
            if now - *sent_at < throttle {
                return;
            }
        }
        self.quote_sent_at.insert(quote.symbol.clone(), now);
        self.emit(ServerEvent::Quote { quote });
    }

    /// Marks topics dirty. The first call arms one 200 ms timer; later calls only add topics.
    pub fn invalidate(&mut self, topics: &[Topic]) {
        for topic in topics {
            if !self.dirty.contains(topic) {
                self.dirty.push(*topic);
            }
        }
        if self.flush_at.is_none() {
            self.flush_at = Some(Instant::now() + Duration::from_millis(INVALIDATE_MS));
        }
    }

    pub fn flush_at(&self) -> Option<Instant> {
        self.flush_at
    }

    /// Sends the pending invalidate, if it has any topics, and disarms the timer.
    pub fn flush_invalidate(&mut self) {
        self.flush_at = None;
        let topics = std::mem::take(&mut self.dirty);
        if !topics.is_empty() {
            self.emit(ServerEvent::Invalidate { topics });
        }
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}
