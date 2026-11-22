//! The live market feed as the runtime sees it. Node's runtime holds an `OkxMarketData`; here
//! that is [`OkxPublicFeed`] from `mochi-market`. [`OfflineFeed`] never connects: tests use it,
//! and so does the server with `MOCHIPREP_OFFLINE=1`.

use futures_util::future::BoxFuture;
use mochi_domain::{
    Candle, DataSource, FeedState, FeedStatus, InstrumentSpecification, Quote, Timeframe,
};
use mochi_market::okx_public::OkxPublicFeed;
use mochi_market::tape::Tape;

pub use mochi_market::okx_public::PROVIDER as OKX_PUBLIC;

/// Node's text for a feed that has no instrument list.
const CANNOT_LIST: &str = "This feed cannot list instruments";
/// The same for candles, which every Node feed has; only a test's own feed lacks them.
const NO_CANDLES: &str = "This feed has no candles";
/// What the offline feed answers instead of OKX data.
pub const OFFLINE_TEXT: &str = "Live OKX market data is turned off (MOCHIPREP_OFFLINE=1)";

/// A failed feed request: the text a route answers with (a 400, as in Node).
pub type FeedResult<'a, T> = BoxFuture<'a, Result<T, String>>;

/// The calls the runtime makes on a live feed. Quotes and status changes the feed receives
/// reach the runtime through the `live_quotes` and `live_statuses` channels in `RuntimeOptions`,
/// not through this trait.
/// Why an offline feed has no replay history.
const NO_HISTORY: &str = "Replay history needs the OKX connection";

pub trait LiveFeed: Send + Sync {
    /// Node's `live.start()`, called once the runtime is built.
    fn start(&self) {}
    fn stop(&self) {}
    /// Subscribes to a pair the owner added. The built-in pairs are the feed's own business.
    fn track(&self, symbol: &str);
    fn untrack(&self, symbol: &str);
    /// The latest quote, or `None` until one has arrived.
    fn get_quote(&self, symbol: &str) -> Option<Quote>;
    fn status(&self) -> FeedStatus;
    /// Oldest first.
    fn candles<'a>(
        &'a self,
        _symbol: &'a str,
        _timeframe: Timeframe,
        _limit: usize,
    ) -> FeedResult<'a, Vec<Candle>> {
        Box::pin(async { Err(NO_CANDLES.to_string()) })
    }
    /// Every pair this feed could trade. `fresh` skips any cache, for a spec that will be saved.
    fn list_instruments(&self, _fresh: bool) -> FeedResult<'_, Vec<InstrumentSpecification>> {
        Box::pin(async { Err(CANNOT_LIST.to_string()) })
    }
    /// Recorded one-minute bars of `symbol` from `start` for `minutes` minutes, for the replay.
    /// `progress` hears how many minutes have arrived.
    fn history_tape<'a>(
        &'a self,
        _symbol: &'a str,
        _start: i64,
        _minutes: usize,
        _progress: &'a (dyn Fn(usize) + Send + Sync),
    ) -> FeedResult<'a, Tape> {
        Box::pin(async { Err(NO_HISTORY.to_string()) })
    }
}

/// A feed with no connection: no quotes, no candles, no instrument list.
pub struct OfflineFeed;

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

    fn candles<'a>(
        &'a self,
        _symbol: &'a str,
        _timeframe: Timeframe,
        _limit: usize,
    ) -> FeedResult<'a, Vec<Candle>> {
        Box::pin(async { Err(OFFLINE_TEXT.to_string()) })
    }

    fn list_instruments(&self, _fresh: bool) -> FeedResult<'_, Vec<InstrumentSpecification>> {
        Box::pin(async { Err(OFFLINE_TEXT.to_string()) })
    }
}

impl LiveFeed for OkxPublicFeed {
    fn start(&self) {
        OkxPublicFeed::start(self);
    }

    fn stop(&self) {
        OkxPublicFeed::stop(self);
    }

    fn track(&self, symbol: &str) {
        OkxPublicFeed::track(self, symbol);
    }

    fn untrack(&self, symbol: &str) {
        OkxPublicFeed::untrack(self, symbol);
    }

    fn get_quote(&self, symbol: &str) -> Option<Quote> {
        OkxPublicFeed::get_quote(self, symbol)
    }

    fn status(&self) -> FeedStatus {
        OkxPublicFeed::status(self)
    }

    fn candles<'a>(
        &'a self,
        symbol: &'a str,
        timeframe: Timeframe,
        limit: usize,
    ) -> FeedResult<'a, Vec<Candle>> {
        Box::pin(self.get_candles(symbol, timeframe, limit))
    }

    fn list_instruments(&self, fresh: bool) -> FeedResult<'_, Vec<InstrumentSpecification>> {
        Box::pin(OkxPublicFeed::list_instruments(self, fresh))
    }

    fn history_tape<'a>(
        &'a self,
        symbol: &'a str,
        start: i64,
        minutes: usize,
        progress: &'a (dyn Fn(usize) + Send + Sync),
    ) -> FeedResult<'a, Tape> {
        Box::pin(OkxPublicFeed::history_tape(
            self, symbol, start, minutes, progress,
        ))
    }
}
