//! News sources, as Node's runtime keeps them (`runtime.ts` "news"): the built-ins come from
//! code with only `enabled` overlaid from the `news.sources` setting, followed by the owner's own
//! feeds from the same setting. Headlines themselves live in [`NewsService`]'s memory cache and
//! are fetched outside the runtime task.

use mochi_domain::{NewsSource, Topic};
use mochi_market::news::{default_news_sources, validate_feed_url, FeedReply, FeedTransport};
use mochi_market::okx_public::BoxFuture;
use mochi_store::{get_setting, set_setting};
use serde_json::Value;

use super::Runtime;
use crate::error::ApiError;

pub use mochi_market::news::NewsService;

/// The setting both backends keep the sources in.
const SOURCES_KEY: &str = "news.sources";

/// What every feed answers while `MOCHIPREP_OFFLINE=1`. Never shown: a failed feed is reported
/// by name only.
pub const OFFLINE_NEWS_TEXT: &str = "News feeds are turned off (MOCHIPREP_OFFLINE=1)";

/// A transport that never connects, for tests and `MOCHIPREP_OFFLINE=1`.
pub struct OfflineNews;

impl FeedTransport for OfflineNews {
    fn get(&self, _url: String) -> BoxFuture<'static, Result<FeedReply, String>> {
        Box::pin(async { Err(OFFLINE_NEWS_TEXT.to_string()) })
    }
}

/// JS truthiness of a saved field.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

impl Runtime {
    pub fn news(&self) -> NewsService {
        self.news.clone()
    }

    /// Node's `newsSources`: built-ins first, in code order, then the owner's feeds in the
    /// order added.
    pub fn news_sources(&self) -> Result<Vec<NewsSource>, ApiError> {
        let saved = get_setting::<Vec<Value>>(self.connection(), SOURCES_KEY)
            .map_err(|e| self.api_error(e))?
            .unwrap_or_default();
        let mut sources: Vec<NewsSource> = default_news_sources()
            .into_iter()
            .map(|mut source| {
                // `saved.find((s) => s.id === source.id)?.enabled ?? source.enabled`
                let enabled = saved
                    .iter()
                    .find(|s| s.get("id").and_then(Value::as_str) == Some(source.id.as_str()))
                    .and_then(|s| s.get("enabled"))
                    .and_then(Value::as_bool);
                source.enabled = enabled.unwrap_or(source.enabled);
                source
            })
            .collect();
        for entry in saved.iter().filter(|s| !truthy(s.get("builtIn"))) {
            let source: NewsSource =
                serde_json::from_value(entry.clone()).map_err(|e| self.api_error(e))?;
            sources.push(source);
        }
        Ok(sources)
    }

    /// Node's `saveNewsSources`: stored, the headline cache marked stale, the web app told.
    fn save_news_sources(&mut self, sources: &[NewsSource]) -> Result<(), ApiError> {
        set_setting(self.connection(), SOURCES_KEY, sources).map_err(|e| self.api_error(e))?;
        self.news.invalidate();
        self.invalidate(&[Topic::News]);
        Ok(())
    }

    /// `INSTRUMENTS[symbol]?.baseCurrency ?? symbol.replace(/USDT$/, '')`.
    pub fn news_base_currency(&self, symbol: &str) -> String {
        self.instruments()
            .iter()
            .find(|spec| spec.symbol == symbol)
            .map(|spec| spec.base_currency.clone())
            .unwrap_or_else(|| symbol.strip_suffix("USDT").unwrap_or(symbol).to_string())
    }

    /// Node's `addNewsSource`, refusals in its order. `name` is already trimmed by validation.
    pub fn add_news_source(&mut self, name: String, url: String) -> Result<NewsSource, ApiError> {
        if let Some(problem) = validate_feed_url(&url) {
            return Err(ApiError::bad_request(problem));
        }
        let mut sources = self.news_sources()?;
        if sources.iter().any(|s| s.url == url) {
            return Err(ApiError::bad_request("This feed is already added"));
        }
        let source = NewsSource {
            id: format!("news_{}", uuid::Uuid::new_v4()),
            name,
            url,
            enabled: true,
            built_in: false,
        };
        sources.push(source.clone());
        self.save_news_sources(&sources)?;
        Ok(source)
    }

    pub fn set_news_source_enabled(&mut self, id: &str, enabled: bool) -> Result<(), ApiError> {
        let mut sources = self.news_sources()?;
        let Some(source) = sources.iter_mut().find(|s| s.id == id) else {
            return Err(ApiError::bad_request("Unknown news source"));
        };
        source.enabled = enabled;
        self.save_news_sources(&sources)
    }

    pub fn remove_news_source(&mut self, id: &str) -> Result<(), ApiError> {
        let mut sources = self.news_sources()?;
        let Some(source) = sources.iter().find(|s| s.id == id) else {
            return Err(ApiError::bad_request("Unknown news source"));
        };
        if source.built_in {
            return Err(ApiError::bad_request(
                "Built-in sources can be turned off, not removed",
            ));
        }
        sources.retain(|s| s.id != id);
        self.save_news_sources(&sources)
    }
}
