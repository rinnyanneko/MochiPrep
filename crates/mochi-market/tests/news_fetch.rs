//! Fetching feeds: every redirect hop re-validated, the redirect cap, the size cap, the 8 s
//! timeout, and the service's cache. Fully offline: the service tests run against an in-memory
//! fake that answers by exact URL, and the real transport is tested against a loopback server.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use mochi_domain::NewsSource;
use mochi_market::news::{
    fetch_source, FeedReply, FeedTransport, NetFeedTransport, NewsService, ACCEPT, MAX_BODY_BYTES,
    MAX_FEED_CHARS, REFRESH_MS, USER_AGENT,
};
use mochi_market::okx_public::BoxFuture;
use tokio::net::TcpListener;
use tokio::time::Instant;

const NOW: i64 = 1_794_031_200_000; // 2026-11-07T06:00:00Z
const HOUR: i64 = 3_600_000;

#[derive(Clone)]
struct Route {
    status: u16,
    location: Option<String>,
    body: String,
    delay: Duration,
}

/// Feeds by exact URL. Anything else fails as a network error does.
#[derive(Default)]
struct FakeFeeds {
    routes: Mutex<HashMap<String, Route>>,
    asked: Mutex<Vec<String>>,
}

impl FakeFeeds {
    fn feed(&self, url: &str, body: &str) {
        self.set(url, 200, None, body);
    }
    fn redirect(&self, url: &str, location: &str) {
        self.set(url, 301, Some(location), "");
    }
    fn set(&self, url: &str, status: u16, location: Option<&str>, body: &str) {
        let route = Route {
            status,
            location: location.map(str::to_string),
            body: body.to_string(),
            delay: Duration::ZERO,
        };
        self.routes.lock().unwrap().insert(url.to_string(), route);
    }
    fn slow(&self, url: &str, delay: Duration) {
        if let Some(route) = self.routes.lock().unwrap().get_mut(url) {
            route.delay = delay;
        }
    }
    fn unset(&self, url: &str) {
        self.routes.lock().unwrap().remove(url);
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl FeedTransport for FakeFeeds {
    fn get(&self, url: String) -> BoxFuture<'static, Result<FeedReply, String>> {
        self.asked.lock().unwrap().push(url.clone());
        let route = self.routes.lock().unwrap().get(&url).cloned();
        Box::pin(async move {
            let route = route.ok_or_else(|| "fetch failed".to_string())?;
            tokio::time::sleep(route.delay).await;
            Ok(FeedReply {
                status: route.status,
                location: route.location,
                body: route.body.into_bytes(),
                truncated: false,
            })
        })
    }
}

fn source(id: &str) -> NewsSource {
    NewsSource {
        id: id.to_string(),
        name: id.to_string(),
        url: format!("https://{id}.test/feed"),
        enabled: true,
        built_in: false,
    }
}

fn feed(title: &str) -> String {
    format!(
        "<rss><item><title>{title}</title><link>https://x.test</link><pubDate>Sat, 07 Nov 2026 05:00:00 GMT</pubDate></item></rss>"
    )
}

struct Setup {
    feeds: Arc<FakeFeeds>,
    clock: Arc<AtomicI64>,
    service: NewsService,
}

fn setup() -> Setup {
    let feeds = Arc::new(FakeFeeds::default());
    let clock = Arc::new(AtomicI64::new(NOW));
    let now = {
        let clock = clock.clone();
        Arc::new(move || clock.load(Ordering::SeqCst))
    };
    let service = NewsService::with_transport(feeds.clone(), now);
    Setup {
        feeds,
        clock,
        service,
    }
}

impl Setup {
    async fn read(&self, sources: &[NewsSource]) -> (Vec<String>, Option<String>) {
        let result = self.service.for_symbol(sources.to_vec(), "BTC", 12).await;
        let titles = result.items.into_iter().map(|item| item.title).collect();
        (titles, result.error)
    }
    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn redirect_to_http_refused() {
    let s = setup();
    s.feeds.redirect("https://a.test/feed", "http://a.test/rss");
    s.feeds.feed("http://a.test/rss", &feed("Never"));
    let error = fetch_source(s.feeds.as_ref(), &source("a"))
        .await
        .unwrap_err();
    assert_eq!(error, "Feeds must use https: http://a.test/rss");
    assert_eq!(s.feeds.asked(), ["https://a.test/feed"]);

    let (titles, error) = s.read(&[source("a")]).await;
    assert!(titles.is_empty());
    assert_eq!(error.as_deref(), Some("Could not read: a"));
    // The first URL is checked as much as any hop.
    let mut plain = source("b");
    plain.url = "http://b.test/feed".into();
    let error = fetch_source(s.feeds.as_ref(), &plain).await.unwrap_err();
    assert_eq!(error, "Feeds must use https: http://b.test/feed");
    assert!(!s.feeds.asked().contains(&"http://b.test/feed".to_string()));
}

#[tokio::test]
async fn redirect_to_private_host_refused() {
    for (target, refused) in [
        (
            "https://169.254.169.254/latest",
            "https://169.254.169.254/latest",
        ),
        ("https://localhost/x", "https://localhost/x"),
        ("https://10.0.0.1/x", "https://10.0.0.1/x"),
        ("https://[::1]/x", "https://[::1]/x"),
        ("https://0x7f.1/x", "https://127.0.0.1/x"),
        ("//router/x", "https://router/x"),
    ] {
        let s = setup();
        s.feeds.redirect("https://a.test/feed", target);
        s.feeds.feed(refused, &feed("Never"));
        let error = fetch_source(s.feeds.as_ref(), &source("a"))
            .await
            .unwrap_err();
        assert_eq!(error, format!("Feeds must be on a public host: {refused}"));
        assert_eq!(s.feeds.asked(), ["https://a.test/feed"], "{target}");
    }
}

#[tokio::test]
async fn redirect_hops_resolve_relative_locations() {
    let s = setup();
    s.feeds.redirect("https://a.test/feed", "/rss");
    s.feeds.redirect("https://a.test/rss", "../moved/?x=1");
    s.feeds
        .redirect("https://a.test/moved/?x=1", "https://b.test/final");
    s.feeds.feed("https://b.test/final", &feed("Moved"));
    let (titles, error) = s.read(&[source("a")]).await;
    assert_eq!(titles, ["Moved"]);
    assert_eq!(error, None);
    assert_eq!(
        s.feeds.asked(),
        [
            "https://a.test/feed",
            "https://a.test/rss",
            "https://a.test/moved/?x=1",
            "https://b.test/final"
        ]
    );
}

#[tokio::test]
async fn fourth_redirect_refused() {
    let s = setup();
    for hop in 0..4 {
        s.feeds.redirect(
            &format!("https://a.test/{hop}"),
            &format!("https://a.test/{}", hop + 1),
        );
    }
    s.feeds.feed("https://a.test/4", &feed("Too far"));
    s.feeds.feed("https://a.test/3", &feed("Three hops"));
    let mut start = source("a");

    // Three redirects are followed.
    start.url = "https://a.test/1".into();
    let items = fetch_source(s.feeds.as_ref(), &start).await.unwrap();
    assert_eq!(items[0].title, "Three hops");

    // A fourth is not, and is never fetched.
    s.feeds.redirect("https://a.test/3", "https://a.test/4");
    s.feeds.asked.lock().unwrap().clear();
    start.url = "https://a.test/0".into();
    let error = fetch_source(s.feeds.as_ref(), &start).await.unwrap_err();
    assert_eq!(error, "Too many redirects");
    assert_eq!(s.feeds.asked().len(), 4);
    assert!(!s.feeds.asked().contains(&"https://a.test/4".to_string()));
}

#[tokio::test]
async fn redirect_without_location_is_a_failed_status() {
    let s = setup();
    s.feeds.set("https://a.test/feed", 301, None, "");
    s.feeds.set("https://b.test/feed", 302, Some(""), "");
    s.feeds.set("https://c.test/feed", 500, None, "");
    for (id, status) in [("a", 301), ("b", 302), ("c", 500)] {
        let error = fetch_source(s.feeds.as_ref(), &source(id))
            .await
            .unwrap_err();
        assert_eq!(error, format!("HTTP {status}"));
    }
}

#[tokio::test]
async fn too_large_refused() {
    let s = setup();
    let wrap = |filler: &str| format!("{}{filler}", feed("Big"));
    let base = wrap("").encode_utf16().count();
    // Characters as JS counts them: UTF-16 units of the decoded text, not bytes.
    s.feeds.feed(
        "https://a.test/feed",
        &wrap(&"x".repeat(MAX_FEED_CHARS - base + 1)),
    );
    s.feeds.feed(
        "https://b.test/feed",
        &wrap(&"x".repeat(MAX_FEED_CHARS - base)),
    );
    // 3 bytes and 1 unit each: well over 3 MB, still under the limit.
    s.feeds.feed(
        "https://c.test/feed",
        &wrap(&"\u{20AC}".repeat(MAX_FEED_CHARS - base)),
    );
    // 4 bytes and 2 units each.
    s.feeds.feed(
        "https://d.test/feed",
        &wrap(&"\u{1F600}".repeat((MAX_FEED_CHARS - base) / 2 + 1)),
    );
    let feeds = s.feeds.clone();
    let read = |id: &str| {
        let (feeds, source) = (feeds.clone(), source(id));
        async move { fetch_source(feeds.as_ref(), &source).await }
    };
    assert_eq!(read("a").await.unwrap_err(), "feed too large");
    assert_eq!(read("b").await.unwrap()[0].title, "Big");
    assert_eq!(read("c").await.unwrap()[0].title, "Big");
    assert_eq!(read("d").await.unwrap_err(), "feed too large");

    let (titles, error) = s.read(&[source("a"), source("b")]).await;
    assert_eq!(titles, ["Big"]);
    assert_eq!(error.as_deref(), Some("Could not read: a"));
}

#[tokio::test]
async fn all_sources_fail_keeps_cache() {
    let s = setup();
    s.feeds.feed("https://a.test/feed", &feed("Cached A"));
    s.feeds.feed("https://b.test/feed", &feed("Cached B"));
    let sources = [source("a"), source("b")];
    assert_eq!(
        s.read(&sources).await,
        (vec!["Cached A".into(), "Cached B".into()], None)
    );

    // Every source down: the last good headlines stay, and the error names them all.
    s.feeds.unset("https://a.test/feed");
    s.feeds.set("https://b.test/feed", 503, None, "");
    s.advance(REFRESH_MS);
    let (titles, error) = s.read(&sources).await;
    assert_eq!(titles, ["Cached A", "Cached B"]);
    assert_eq!(error.as_deref(), Some("Could not read: a, b"));
    assert_eq!(s.feeds.asked().len(), 4);

    // One back: only what it has now.
    s.feeds.feed("https://a.test/feed", &feed("Fresh A"));
    s.advance(REFRESH_MS);
    assert_eq!(
        s.read(&sources).await,
        (vec!["Fresh A".into()], Some("Could not read: b".into()))
    );

    // Every source turned off: the cache empties, since nothing failed.
    let off: Vec<NewsSource> = sources
        .iter()
        .cloned()
        .map(|mut s| {
            s.enabled = false;
            s
        })
        .collect();
    s.advance(REFRESH_MS);
    assert_eq!(s.read(&off).await, (vec![], None));
}

#[tokio::test]
async fn sources_change_invalidates_cache() {
    let s = setup();
    s.feeds.feed("https://a.test/feed", &feed("From A"));
    s.feeds.feed("https://b.test/feed", &feed("From B"));
    assert_eq!(s.read(&[source("a")]).await.0, ["From A"]);

    // Within 5 minutes the cache answers, whatever the sources are now.
    s.advance(REFRESH_MS - 1);
    assert_eq!(s.read(&[source("b")]).await.0, ["From A"]);
    assert_eq!(s.feeds.asked().len(), 1);

    s.service.invalidate();
    assert_eq!(s.read(&[source("b")]).await.0, ["From B"]);
    assert_eq!(s.feeds.asked().len(), 2);

    // And after 5 minutes on its own.
    s.advance(REFRESH_MS);
    s.read(&[source("b")]).await;
    assert_eq!(s.feeds.asked().len(), 3);
}

#[tokio::test]
async fn concurrent_readers_share_one_refresh() {
    let s = setup();
    s.feeds.feed("https://a.test/feed", &feed("One"));
    let sources = vec![source("a")];
    let (first, second) = tokio::join!(
        s.service.for_symbol(sources.clone(), "BTC", 12),
        s.service.for_symbol(sources.clone(), "ETH", 12),
    );
    assert_eq!(first.items.len(), 1);
    assert_eq!(second.items.len(), 1);
    assert_eq!(s.feeds.asked().len(), 1);
}

#[tokio::test]
async fn only_enabled_sources_are_read_and_old_items_dropped() {
    let s = setup();
    s.feeds.feed("https://a.test/feed", &feed("One"));
    let mut off = source("off");
    off.enabled = false;
    let (titles, _) = s.read(&[source("a"), off]).await;
    assert_eq!(titles, ["One"]);
    assert_eq!(s.feeds.asked(), ["https://a.test/feed"]);
    // The cache keeps the item; selection drops it once it is more than a day old (it was
    // published an hour before NOW).
    s.advance(23 * HOUR);
    assert_eq!(s.read(&[source("a")]).await.0, ["One"]);
    s.advance(1);
    s.service.invalidate();
    s.feeds.unset("https://a.test/feed");
    assert_eq!(s.read(&[source("a")]).await.0, Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn each_hop_times_out_after_8s() {
    let s = setup();
    s.feeds
        .redirect("https://a.test/feed", "https://a.test/rss");
    s.feeds.feed("https://a.test/rss", &feed("Slow"));
    s.feeds
        .slow("https://a.test/feed", Duration::from_millis(7_900));
    s.feeds
        .slow("https://a.test/rss", Duration::from_millis(7_900));
    let started = Instant::now();
    assert_eq!(
        fetch_source(s.feeds.as_ref(), &source("a")).await.unwrap()[0].title,
        "Slow"
    );
    assert_eq!(started.elapsed(), Duration::from_millis(15_800));

    s.feeds.slow("https://a.test/rss", Duration::from_secs(9));
    let started = Instant::now();
    let error = fetch_source(s.feeds.as_ref(), &source("a"))
        .await
        .unwrap_err();
    assert_eq!(error, "The operation was aborted due to timeout");
    assert_eq!(started.elapsed(), Duration::from_millis(15_900));
}

// ---- the real transport, over loopback --------------------------------------------------------

async fn serve(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

#[tokio::test]
async fn net_transport_sends_headers_and_never_follows_redirects() {
    let seen: Arc<Mutex<Vec<HeaderMap>>> = Arc::default();
    let router = Router::new()
        .route(
            "/feed",
            get({
                let seen = seen.clone();
                move |headers: HeaderMap| async move {
                    seen.lock().unwrap().push(headers);
                    (
                        StatusCode::MOVED_PERMANENTLY,
                        [("location", "/next")],
                        "moved",
                    )
                        .into_response()
                }
            }),
        )
        .route("/next", get(|| async { "next" }))
        .route("/big", get(|| async { "x".repeat(MAX_BODY_BYTES + 10) }))
        .route("/fine", get(|| async { "x".repeat(MAX_BODY_BYTES) }));
    let base = serve(router).await;
    let transport = NetFeedTransport::new();

    let reply = transport.get(format!("{base}/feed")).await.unwrap();
    assert_eq!(reply.status, 301);
    assert_eq!(reply.location.as_deref(), Some("/next"));
    // A redirect's body is never read.
    assert!(reply.body.is_empty());
    let headers = seen.lock().unwrap()[0].clone();
    assert_eq!(headers["user-agent"], USER_AGENT);
    assert_eq!(headers["accept"], ACCEPT);
    assert!(headers.get("cookie").is_none());
    assert!(headers.get("authorization").is_none());

    let big = transport.get(format!("{base}/big")).await.unwrap();
    assert!(big.truncated);
    assert_eq!(big.body.len(), MAX_BODY_BYTES);
    let fine = transport.get(format!("{base}/fine")).await.unwrap();
    assert!(!fine.truncated);
    assert_eq!(fine.body.len(), MAX_BODY_BYTES);

    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = format!("http://{}/feed", closed.local_addr().unwrap());
    drop(closed);
    assert_eq!(transport.get(dead).await.unwrap_err(), "fetch failed");
}

#[tokio::test]
async fn a_truncated_body_is_too_large() {
    struct Truncated;
    impl FeedTransport for Truncated {
        fn get(&self, _url: String) -> BoxFuture<'static, Result<FeedReply, String>> {
            Box::pin(async {
                Ok(FeedReply {
                    status: 200,
                    location: None,
                    body: b"<item>".to_vec(),
                    truncated: true,
                })
            })
        }
    }
    let error = fetch_source(&Truncated, &source("a")).await.unwrap_err();
    assert_eq!(error, "feed too large");
}
