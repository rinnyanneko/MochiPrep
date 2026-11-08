//! The live market feed as the runtime sees it. Node's runtime holds an `OkxMarketData`; this
//! port does not have the OKX public adapter yet, so the server starts with [`OfflineFeed`],
//! which never connects and reports the state Node reports before `start()`.

use mochi_domain::{DataSource, FeedState, FeedStatus, Quote};

/// The calls the runtime makes on a live feed. Quotes the feed receives reach the runtime
/// through the `live_quotes` channel in `RuntimeOptions`, not through this trait.
pub trait LiveFeed: Send + Sync {
    /// Subscribes to a pair the owner added. The built-in pairs are the feed's own business.
    fn track(&self, symbol: &str);
    fn untrack(&self, symbol: &str);
    /// The latest quote, or `None` until one has arrived.
    fn get_quote(&self, symbol: &str) -> Option<Quote>;
    fn status(&self) -> FeedStatus;
}

/// A feed with no connection: no quotes, ever.
pub struct OfflineFeed;

/// The name Node's OKX adapter reports, so `/api/state` reads the same before it connects.
pub const OKX_PUBLIC: &str = "OKX public";

impl LiveFeed for OfflineFeed {
    fn track(&self, _symbol: &str) {}

    fn untrack(&self, _symbol: &str) {}

    fn get_quote(&self, _symbol: &str) -> Option<Quote> {
        None
    }

    /// `OkxMarketData.status()` before `start()`: disconnected, no message yet, no detail.
    fn status(&self) -> FeedStatus {
        FeedStatus {
            source: DataSource::Live,
            provider: OKX_PUBLIC.to_string(),
            state: FeedState::Disconnected,
            last_message_at: None,
            detail: None,
        }
    }
}
