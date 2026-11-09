//! `/api/news` and `/api/news/sources`: Node's shapes and texts, the sources setting both
//! backends share, and the headline cache going stale when the sources change. Feeds come from
//! an in-memory fake; nothing touches the network.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use mochi_domain::{ServerEvent, Topic};
use mochi_market::news::{FeedReply, FeedTransport, NewsService};
use mochi_market::okx_public::BoxFuture;
use mochi_server::config::{config_from, Config};
use mochi_server::{app, AppState, RuntimeHandle, RuntimeOptions};
use serde_json::{json, Value};
use tokio::time::timeout;
use tower::ServiceExt;

const HOST: &str = "localhost:8787";
const NOW: i64 = 1_794_031_200_000; // 2026-11-07T06:00:00Z
const COINDESK: &str = "https://www.coindesk.com/arc/outboundfeeds/rss";

/// Feeds by exact URL; anything else fails.
#[derive(Default)]
struct Feeds {
    bodies: Mutex<HashMap<String, String>>,
    asked: Mutex<Vec<String>>,
}

impl FeedTransport for Feeds {
    fn get(&self, url: String) -> BoxFuture<'static, Result<FeedReply, String>> {
        self.asked.lock().unwrap().push(url.clone());
        let body = self.bodies.lock().unwrap().get(&url).cloned();
        Box::pin(async move {
            let body = body.ok_or_else(|| "fetch failed".to_string())?;
            Ok(FeedReply {
                status: 200,
                location: None,
                body: body.into_bytes(),
                truncated: false,
            })
        })
    }
}

/// An item published `hours_ago` (0..=6) hours before NOW, or long ago for anything more.
fn item(title: &str, hours_ago: i64) -> String {
    let date = if hours_ago <= 6 {
        format!("2026-11-07T{:02}:00:00Z", 6 - hours_ago)
    } else {
        "2026-11-05T12:00:00Z".to_string()
    };
    format!(
        "<item><title>{title}</title><link>https://x.test/{}</link><published>{date}</published></item>",
        title.len()
    )
}

struct Setup {
    state: AppState,
    feeds: Arc<Feeds>,
}

fn config() -> Config {
    let mut config = config_from(|_| None);
    config.data_dir = std::path::PathBuf::from(".");
    config
}

fn setup() -> Setup {
    let feeds = Arc::new(Feeds::default());
    let clock = Arc::new(AtomicI64::new(NOW));
    let mut options = RuntimeOptions::new(mochi_store::open_in_memory().unwrap());
    options.replay_minutes = Some(mochi_market::tape::WARMUP_MINUTES + 30);
    options.news = NewsService::with_transport(
        feeds.clone(),
        Arc::new(move || clock.load(Ordering::SeqCst)),
    );
    let state = AppState::new(Arc::new(config()), RuntimeHandle::start(options).unwrap());
    Setup { state, feeds }
}

impl Setup {
    async fn send(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("host", HOST)
            .header("authorization", format!("Bearer {}", self.state.token));
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let request = builder
            .body(Body::from(body.unwrap_or("").to_string()))
            .unwrap();
        let response = app(self.state.clone()).oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn ok(&self, method: &str, path: &str, body: Option<&str>) -> Value {
        let (status, value) = self.send(method, path, body).await;
        assert_eq!(status, 200, "{method} {path}: {value}");
        value
    }

    async fn refused(&self, method: &str, path: &str, body: Option<&str>) -> String {
        let (status, value) = self.send(method, path, body).await;
        assert_eq!(status, 400, "{method} {path}: {value}");
        value["error"].as_str().unwrap().to_string()
    }

    fn serve(&self, url: &str, body: &str) {
        self.feeds
            .bodies
            .lock()
            .unwrap()
            .insert(url.to_string(), body.to_string());
    }

    fn asked(&self) -> usize {
        self.feeds.asked.lock().unwrap().len()
    }
}

#[tokio::test]
async fn sources_list_built_ins_then_added_feeds() {
    let s = setup();
    let sources = s.ok("GET", "/api/news/sources", None).await;
    assert_eq!(
        sources,
        json!([
            { "id": "coindesk", "name": "CoinDesk", "url": COINDESK, "enabled": true, "builtIn": true },
            { "id": "cointelegraph", "name": "Cointelegraph", "url": "https://cointelegraph.com/rss", "enabled": true, "builtIn": true },
            { "id": "decrypt", "name": "Decrypt", "url": "https://decrypt.co/feed", "enabled": true, "builtIn": true },
            { "id": "theblock", "name": "The Block", "url": "https://www.theblock.co/rss.xml", "enabled": true, "builtIn": true },
            { "id": "bitcoinmagazine", "name": "Bitcoin Magazine", "url": "https://bitcoinmagazine.com/feed", "enabled": false, "builtIn": true },
            { "id": "cryptoslate", "name": "CryptoSlate", "url": "https://cryptoslate.com/feed/", "enabled": false, "builtIn": true },
        ])
    );

    let added = s
        .ok(
            "POST",
            "/api/news/sources",
            Some(r#"{"name":"  My feed ","url":"https://example.com/rss"}"#),
        )
        .await;
    let id = added["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("news_") && id.len() == 5 + 36, "{id}");
    assert_eq!(
        added,
        json!({ "id": id, "name": "My feed", "url": "https://example.com/rss", "enabled": true, "builtIn": false })
    );
    let sources = s.ok("GET", "/api/news/sources", None).await;
    assert_eq!(sources.as_array().unwrap().len(), 7);
    assert_eq!(sources[6], added);

    // Stored as Node stores it: the whole list, built-ins included.
    let saved: String = s
        .state
        .runtime
        .call(|runtime| {
            runtime
                .connection()
                .query_row(
                    "SELECT value FROM settings WHERE key='news.sources'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        })
        .await
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&saved).unwrap(), sources);
}

#[tokio::test]
async fn add_refuses_with_nodes_texts() {
    let s = setup();
    let post = |body: &'static str| (body, "POST", "/api/news/sources");
    for (body, expected) in [
        (
            r#"{"name":"A","url":"http://example.com/rss"}"#,
            "Feeds must use https",
        ),
        (
            r#"{"name":"A","url":"https://localhost/rss"}"#,
            "Feeds must be on a public host",
        ),
        (
            r#"{"name":"A","url":"https://169.254.169.254/latest"}"#,
            "Feeds must be on a public host",
        ),
        (
            r#"{"name":"A","url":"https://router/rss"}"#,
            "Feeds must be on a public host",
        ),
        (
            r#"{"name":"A","url":"https://cointelegraph.com/rss"}"#,
            "This feed is already added",
        ),
        (
            r#"{"name":"A"}"#,
            "url: Invalid input: expected string, received undefined",
        ),
        (
            r#"{"name":" ","url":"https://example.com/rss"}"#,
            "name: Too small: expected string to have >=1 characters",
        ),
        (
            r#"{"name":"A","url":"https://example.com/rss","x":1}"#,
            "input: Unrecognized key: \"x\"",
        ),
    ] {
        let (body, method, path) = post(body);
        assert_eq!(
            s.refused(method, path, Some(body)).await,
            expected,
            "{body}"
        );
    }
    assert_eq!(
        s.ok("GET", "/api/news/sources", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        6
    );
}

#[tokio::test]
async fn toggle_and_remove() {
    let s = setup();
    assert_eq!(
        s.ok(
            "PUT",
            "/api/news/sources/coindesk",
            Some(r#"{"enabled":false}"#)
        )
        .await,
        json!({ "ok": true })
    );
    let sources = s.ok("GET", "/api/news/sources", None).await;
    assert_eq!(sources[0]["enabled"], false);
    assert_eq!(
        s.refused("PUT", "/api/news/sources/nope", Some(r#"{"enabled":true}"#))
            .await,
        "Unknown news source"
    );
    // The body is read before the id.
    assert_eq!(
        s.refused(
            "PUT",
            "/api/news/sources/nope",
            Some(r#"{"enabled":"yes"}"#)
        )
        .await,
        "enabled: Invalid input: expected boolean, received string"
    );
    assert_eq!(
        s.refused("DELETE", "/api/news/sources/coindesk", None)
            .await,
        "Built-in sources can be turned off, not removed"
    );
    assert_eq!(
        s.refused("DELETE", "/api/news/sources/nope", None).await,
        "Unknown news source"
    );
    let added = s
        .ok(
            "POST",
            "/api/news/sources",
            Some(r#"{"name":"Mine","url":"https://example.com/rss"}"#),
        )
        .await;
    let path = format!("/api/news/sources/{}", added["id"].as_str().unwrap());
    assert_eq!(
        s.ok("PUT", &path, Some(r#"{"enabled":false}"#)).await,
        json!({ "ok": true })
    );
    assert_eq!(
        s.ok("GET", "/api/news/sources", None).await[6]["enabled"],
        false
    );
    assert_eq!(s.ok("DELETE", &path, None).await, json!({ "ok": true }));
    let sources = s.ok("GET", "/api/news/sources", None).await;
    assert_eq!(sources.as_array().unwrap().len(), 6);
    // The built-in stays off.
    assert_eq!(sources[0]["enabled"], false);
}

#[tokio::test]
async fn news_reads_enabled_feeds_and_reports_failures() {
    let s = setup();
    s.serve(
        COINDESK,
        &format!(
            "<rss>{}{}{}</rss>",
            item("Bitcoin rallies", 1),
            item("Markets calm", 2),
            item("Old", 30)
        ),
    );
    let news = s.ok("GET", "/api/news?symbol=BTCUSDT", None).await;
    assert_eq!(
        news,
        json!({
            "items": [
                { "id": "CoinDesk:Bitcoin rallies", "source": "CoinDesk", "title": "Bitcoin rallies", "url": "https://x.test/15", "publishedAt": NOW - 3_600_000 },
                { "id": "CoinDesk:Markets calm", "source": "CoinDesk", "title": "Markets calm", "url": "https://x.test/12", "publishedAt": NOW - 2 * 3_600_000 },
            ],
            "error": "Could not read: Cointelegraph, Decrypt, The Block",
        })
    );
    // The four enabled built-ins, once: the next read is cached.
    assert_eq!(s.asked(), 4);
    s.ok("GET", "/api/news?symbol=ETHUSDT", None).await;
    assert_eq!(s.asked(), 4);
    assert_eq!(
        s.refused("GET", "/api/news?symbol=btc", None).await,
        "symbol: expected a pair like DOGEUSDT"
    );
}

#[tokio::test]
async fn changing_sources_refetches_and_tells_the_web_app() {
    let s = setup();
    s.serve(
        COINDESK,
        &format!("<rss>{}</rss>", item("From CoinDesk", 1)),
    );
    s.ok("GET", "/api/news?symbol=BTCUSDT", None).await;
    assert_eq!(s.asked(), 4);

    let mut events = s.state.runtime.subscribe();
    for id in ["coindesk", "cointelegraph", "decrypt", "theblock"] {
        let path = format!("/api/news/sources/{id}");
        s.ok("PUT", &path, Some(r#"{"enabled":false}"#)).await;
    }
    let topics = timeout(Duration::from_secs(5), async {
        loop {
            if let ServerEvent::Invalidate { topics } = events.recv().await.unwrap() {
                return topics;
            }
        }
    })
    .await
    .unwrap();
    assert!(topics.contains(&Topic::News));

    // Every source off: a fresh read, which fetches nothing and empties the cache.
    let news = s.ok("GET", "/api/news?symbol=BTCUSDT", None).await;
    assert_eq!(news, json!({ "items": [], "error": null }));
    assert_eq!(s.asked(), 4);
}
