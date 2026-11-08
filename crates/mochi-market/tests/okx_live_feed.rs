//! The OKX public feed's connection behaviour: subscribe, ticker quotes, the 20 s ping, the 30 s
//! silence limit, reconnect backoff, REST polling while the socket is down, and the caches.
//!
//! All but the first test run on tokio's paused clock against the in-memory fake in
//! `fake_okx`, so every time below is exact. The first runs over real loopback sockets, against a
//! local axum REST server and a local WebSocket server.

mod fake_okx;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::http::Uri;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use fake_okx::{accept, FakeOkx, REST, WS};
use futures_util::{SinkExt, StreamExt};
use mochi_domain::{FeedState, FeedStatus, Quote, Timeframe};
use mochi_market::okx_public::{backoff, OkxPublicFeed};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Instant};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// OKX REST: every ticker, mark and funding request answers with fixed values, `last` = 200, so
/// a quote that came from REST is easy to tell from a socket one.
fn okx_rest() -> Router {
    Router::new().fallback(|uri: Uri| async move {
        let path = uri.path();
        let data = if path.ends_with("/market/ticker") {
            json!([{ "bidPx": "199.9", "askPx": "200.1", "last": "200", "ts": "1767225600000" }])
        } else if path.ends_with("/mark-price") {
            json!([{ "markPx": "200" }])
        } else if path.ends_with("/funding-rate") {
            json!([{ "fundingRate": "0.0001", "fundingTime": "1767254400000" }])
        } else if path.ends_with("/market/candles") {
            json!([["1767225600000", "1", "2", "0.5", "1.5", "10", "11", "12", "1"]])
        } else if path.ends_with("/public/instruments") {
            json!([{
                "instId": "DOGE-USDT-SWAP", "settleCcy": "USDT", "ctType": "linear", "state": "live",
                "ruleType": "normal", "instCategory": "1", "ctValCcy": "DOGE", "ctVal": "1000",
                "lotSz": "0.01", "minSz": "0.01", "tickSz": "0.00001", "lever": "75"
            }])
        } else {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        };
        axum::Json(json!({ "code": "0", "msg": "", "data": data })).into_response()
    })
}

fn ticker(inst_id: &str, last: &str) -> String {
    json!({
        "arg": { "channel": "tickers", "instId": inst_id },
        "data": [{ "instId": inst_id, "bidPx": last, "askPx": last, "last": last, "ts": "1767225600000" }]
    })
    .to_string()
}

fn started_feed() -> (
    OkxPublicFeed,
    Arc<FakeOkx>,
    mpsc::UnboundedReceiver<DuplexStream>,
    mpsc::Receiver<Quote>,
    mpsc::UnboundedReceiver<FeedStatus>,
) {
    let (fake, sockets) = FakeOkx::new(okx_rest());
    let feed = OkxPublicFeed::with_transport(REST, WS, fake.clone());
    let quotes = feed.quotes();
    let statuses = feed.statuses();
    feed.start();
    (feed, fake, sockets, quotes, statuses)
}

/// The next text frame the client sent.
async fn next_text<S: AsyncRead + AsyncWrite + Unpin>(server: &mut WebSocketStream<S>) -> String {
    loop {
        match server.next().await.expect("socket open").expect("frame") {
            Message::Text(text) => return text.to_string(),
            _ => continue,
        }
    }
}

fn drain<T>(channel: &mut mpsc::Receiver<T>) -> Vec<T> {
    let mut out = Vec::new();
    while let Ok(item) = channel.try_recv() {
        out.push(item);
    }
    out
}

fn drain_statuses(
    channel: &mut mpsc::UnboundedReceiver<FeedStatus>,
) -> Vec<(FeedState, Option<String>)> {
    let mut out = Vec::new();
    while let Ok(status) = channel.try_recv() {
        out.push((status.state, status.detail));
    }
    out
}

/// Seconds since `t0`, for readable assertions.
fn secs(t0: Instant, times: &[Instant]) -> Vec<f64> {
    times.iter().map(|t| (*t - t0).as_secs_f64()).collect()
}

fn ticker_gets(fake: &FakeOkx, t0: Instant) -> Vec<f64> {
    let times: Vec<Instant> = fake
        .gets_matching("/market/ticker?instId=BTC-USDT-SWAP")
        .into_iter()
        .map(|(at, _)| at)
        .collect();
    secs(t0, &times)
}

/// Lets every task that is ready run, without moving the paused clock.
async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn connects_subscribes_and_emits_ticker_quotes() {
    // Real sockets: a local REST server and a local WebSocket server.
    let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_base = format!("http://{}", rest.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(rest, okx_rest()).await.unwrap() });
    let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}/ws/v5/public", ws.local_addr().unwrap());

    let feed = OkxPublicFeed::new(&rest_base, &ws_url);
    let mut quotes = feed.quotes();
    feed.track("DOGEUSDT");
    feed.start();

    let (stream, _) = timeout(Duration::from_secs(10), ws.accept())
        .await
        .expect("timed out waiting for the dial")
        .unwrap();
    let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
    let subscribe: Value = serde_json::from_str(&next_text(&mut server).await).unwrap();
    assert_eq!(subscribe["op"], "subscribe");
    let args = subscribe["args"].as_array().unwrap();
    // Three channels for each of the built-ins and the pair tracked before start.
    assert_eq!(args.len(), 12);
    assert_eq!(
        args[0],
        json!({ "channel": "tickers", "instId": "BTC-USDT-SWAP" })
    );
    assert_eq!(
        args[1],
        json!({ "channel": "mark-price", "instId": "BTC-USDT-SWAP" })
    );
    assert_eq!(
        args[2],
        json!({ "channel": "funding-rate", "instId": "BTC-USDT-SWAP" })
    );
    assert_eq!(
        args[9],
        json!({ "channel": "tickers", "instId": "DOGE-USDT-SWAP" })
    );
    assert_eq!(feed.status().state, FeedState::Connected);

    // The start-up REST poll's quotes (last 200) may arrive first; wait for the socket's one.
    server
        .send(Message::text(
            json!({ "arg": { "channel": "mark-price", "instId": "ETH-USDT-SWAP" }, "data": [{ "markPx": "3000.5" }] })
                .to_string(),
        ))
        .await
        .unwrap();
    server
        .send(Message::text(ticker("ETH-USDT-SWAP", "3000")))
        .await
        .unwrap();
    let quote = loop {
        let quote = timeout(Duration::from_secs(10), quotes.recv())
            .await
            .expect("timed out waiting for a quote")
            .unwrap();
        if quote.last == "3000" {
            break quote;
        }
    };
    assert_eq!(quote.symbol, "ETHUSDT");
    assert_eq!(quote.mark, "3000.5");
    assert!(!quote.mark_estimated);
    assert_eq!(feed.get_quote("ETHUSDT"), Some(quote));
    // Mark price and funding messages alone never make a quote.
    server
        .send(Message::text(
            json!({ "arg": { "channel": "mark-price", "instId": "ETH-USDT-SWAP" }, "data": [{ "markPx": "3001" }] })
                .to_string(),
        ))
        .await
        .unwrap();
    server
        .send(Message::text(ticker("SOL-USDT-SWAP", "150")))
        .await
        .unwrap();
    let next = loop {
        let quote = timeout(Duration::from_secs(10), quotes.recv())
            .await
            .expect("timed out waiting for a quote")
            .unwrap();
        if quote.last != "200" {
            break quote;
        }
    };
    assert_eq!(
        (next.symbol.as_str(), next.last.as_str()),
        ("SOLUSDT", "150")
    );
    feed.stop();
    assert_eq!(feed.status().state, FeedState::Disconnected);
    assert_eq!(feed.status().detail.as_deref(), Some("Feed stopped"));
}

#[tokio::test(start_paused = true)]
async fn silence_30s_degrades_polls_rest_and_reconnects() {
    let t0 = Instant::now();
    let (feed, fake, mut sockets, mut quotes, mut statuses) = started_feed();

    // Open at 2.5 s, so the 20 s pings (22.5, 42.5) never land on a 5 s poll tick.
    sleep(Duration::from_millis(2500)).await;
    let mut server = accept(&mut sockets).await;
    next_text(&mut server).await;
    server
        .send(Message::text(ticker("BTC-USDT-SWAP", "100")))
        .await
        .unwrap();
    settle().await;
    let opened = drain(&mut quotes);
    assert_eq!(opened.last().map(|q| q.last.as_str()), Some("100"));
    assert_eq!(
        drain_statuses(&mut statuses),
        [(FeedState::Connecting, None), (FeedState::Connected, None)]
    );

    // From here the server reads but never answers. The ping at 22.5 s goes out; at 42.5 s the
    // socket has been silent 40 s, past the 30 s limit, and the feed closes it.
    let reader = tokio::spawn(async move {
        let mut frames = Vec::new();
        while let Some(Ok(frame)) = server.next().await {
            frames.push((Instant::now(), frame));
        }
        frames
    });
    sleep_until_secs(t0, 42.4).await;
    assert_eq!(feed.status().state, FeedState::Connected);
    sleep_until_secs(t0, 42.6).await;
    let status = feed.status();
    assert_eq!(status.state, FeedState::Degraded);
    assert_eq!(
        status.detail.as_deref(),
        Some("Connection closed; reconnecting, REST polling meanwhile")
    );
    let frames = reader.await.unwrap();
    let pings: Vec<f64> = frames
        .iter()
        .filter(|(_, frame)| matches!(frame, Message::Text(t) if t.as_str() == "ping"))
        .map(|(at, _)| (*at - t0).as_secs_f64())
        .collect();
    assert_eq!(pings, [22.5]);

    // The reconnect dials 1 s later; the test lets it hang in the handshake until 58.5 s.
    // Meanwhile REST polls every 5 s, and its quotes keep coming.
    sleep_until_secs(t0, 58.5).await;
    assert_eq!(secs(t0, &fake.dial_times()), [0.0, 43.5]);
    assert_eq!(feed.status().state, FeedState::Connecting);
    assert_eq!(ticker_gets(&fake, t0), [0.0, 45.0, 50.0, 55.0]);
    let degraded = drain(&mut quotes);
    let from_rest = degraded
        .iter()
        .filter(|q| q.symbol == "BTCUSDT" && q.last == "200");
    assert_eq!(from_rest.count(), 3);

    // Connected again: quotes from the socket, and REST polling stops.
    let mut server = accept(&mut sockets).await;
    let subscribe = next_text(&mut server).await;
    assert!(subscribe.contains("BTC-USDT-SWAP"));
    server
        .send(Message::text(ticker("BTC-USDT-SWAP", "101")))
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        drain(&mut quotes).last().map(|q| q.last.clone()),
        Some("101".into())
    );
    assert_eq!(feed.status().state, FeedState::Connected);
    assert_eq!(feed.status().detail, None);
    assert_eq!(
        drain_statuses(&mut statuses),
        [
            (
                FeedState::Degraded,
                Some("Connection closed; reconnecting, REST polling meanwhile".into())
            ),
            (FeedState::Connecting, None),
            (FeedState::Connected, None),
        ]
    );
    sleep_until_secs(t0, 75.0).await;
    assert_eq!(ticker_gets(&fake, t0), [0.0, 45.0, 50.0, 55.0]);
    feed.stop();
}

async fn sleep_until_secs(t0: Instant, secs: f64) {
    tokio::time::sleep_until(t0 + Duration::from_secs_f64(secs)).await;
    settle().await;
}

#[tokio::test(start_paused = true)]
async fn backoff_caps_at_30s() {
    assert_eq!(
        (0..8).map(|n| backoff(n).as_secs()).collect::<Vec<_>>(),
        [1, 2, 4, 8, 16, 30, 30, 30]
    );
    let t0 = Instant::now();
    let (fake, mut sockets) = FakeOkx::new(okx_rest());
    fake.refuse.store(true, Ordering::SeqCst);
    let feed = OkxPublicFeed::with_transport(REST, WS, fake.clone());
    feed.start();

    sleep_until_secs(t0, 0.5).await;
    let status = feed.status();
    assert_eq!(status.state, FeedState::Degraded);
    assert_eq!(
        status.detail.as_deref(),
        Some("WebSocket error; reconnecting, REST polling meanwhile")
    );
    // Refused every time: 1, 2, 4, 8, 16, then 30 s apart.
    sleep_until_secs(t0, 125.0).await;
    let dials = secs(t0, &fake.dial_times());
    assert_eq!(dials, [0.0, 1.0, 3.0, 7.0, 15.0, 31.0, 61.0, 91.0, 121.0]);

    // A successful open resets the count: the next drop reconnects after 1 s again.
    fake.refuse.store(false, Ordering::SeqCst);
    sleep_until_secs(t0, 151.0).await;
    let server = accept(&mut sockets).await;
    settle().await;
    assert_eq!(feed.status().state, FeedState::Connected);
    drop(server);
    sleep_until_secs(t0, 152.5).await;
    assert_eq!(secs(t0, &fake.dial_times()).last(), Some(&152.0));
    feed.stop();
}

/// A feed whose every dial is refused, so it sits in backoff and REST polls every 5 s.
async fn refused_feed_in_backoff(t0: Instant) -> (OkxPublicFeed, Arc<FakeOkx>) {
    let (fake, _sockets) = FakeOkx::new(okx_rest());
    fake.refuse.store(true, Ordering::SeqCst);
    let feed = OkxPublicFeed::with_transport(REST, WS, fake.clone());
    feed.start();
    // Dials at 0, 1 and 3 s; at 5.5 s the next one (7 s) is pending and one poll timer tick
    // (5 s) has fired.
    sleep_until_secs(t0, 5.5).await;
    assert_eq!(secs(t0, &fake.dial_times()), [0.0, 1.0, 3.0]);
    assert_eq!(ticker_gets(&fake, t0), [0.0, 5.0]);
    (feed, fake)
}

#[tokio::test(start_paused = true)]
async fn stop_during_backoff_dials_and_polls_no_more() {
    let t0 = Instant::now();
    let (feed, fake) = refused_feed_in_backoff(t0).await;
    feed.stop();
    sleep_until_secs(t0, 65.5).await;
    assert_eq!(fake.dial_times().len(), 3);
    // Two polls of three requests for each of the three built-in pairs.
    assert_eq!(fake.gets.lock().unwrap().len(), 18);
    assert_eq!(feed.status().state, FeedState::Disconnected);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_feed_dials_and_polls_no_more() {
    let t0 = Instant::now();
    let (feed, fake) = refused_feed_in_backoff(t0).await;
    drop(feed);
    sleep_until_secs(t0, 65.5).await;
    assert_eq!(fake.dial_times().len(), 3);
    // Two polls of three requests for each of the three built-in pairs.
    assert_eq!(fake.gets.lock().unwrap().len(), 18);
}

#[tokio::test(start_paused = true)]
async fn rest_poll_only_while_not_connected() {
    let t0 = Instant::now();
    let (feed, fake, mut sockets, _quotes, _statuses) = started_feed();
    let mut server = accept(&mut sockets).await;
    // Answers every ping, so the socket never goes silent.
    let pong = tokio::spawn(async move {
        while let Some(Ok(frame)) = server.next().await {
            if matches!(&frame, Message::Text(t) if t.as_str() == "ping") {
                server.send(Message::text("pong")).await.unwrap();
            }
        }
    });
    // Connected for a minute: only the start-up poll.
    sleep_until_secs(t0, 61.0).await;
    assert_eq!(feed.status().state, FeedState::Connected);
    assert_eq!(ticker_gets(&fake, t0), [0.0]);

    // The server goes away; polls every 5 s while the reconnect hangs in its handshake.
    pong.abort();
    sleep_until_secs(t0, 77.0).await;
    assert_ne!(feed.status().state, FeedState::Connected);
    assert_eq!(ticker_gets(&fake, t0), [0.0, 65.0, 70.0, 75.0]);
    feed.stop();
}

#[tokio::test(start_paused = true)]
async fn track_after_start_subscribes_and_polls() {
    let t0 = Instant::now();
    let (feed, fake, mut sockets, mut quotes, _statuses) = started_feed();
    let mut server = accept(&mut sockets).await;
    let first = next_text(&mut server).await;
    assert!(!first.contains("DOGE"));
    sleep_until_secs(t0, 1.0).await;
    assert!(fake.gets_matching("DOGE").is_empty());

    feed.track("DOGEUSDT");
    let subscribe: Value = serde_json::from_str(&next_text(&mut server).await).unwrap();
    assert_eq!(
        subscribe,
        json!({ "op": "subscribe", "args": [
            { "channel": "tickers", "instId": "DOGE-USDT-SWAP" },
            { "channel": "mark-price", "instId": "DOGE-USDT-SWAP" },
            { "channel": "funding-rate", "instId": "DOGE-USDT-SWAP" },
        ]})
    );
    settle().await;
    // A new pair polls REST at once, even while connected, so it has a quote without waiting.
    let doge: Vec<String> = fake
        .gets_matching("DOGE")
        .into_iter()
        .map(|(_, u)| u)
        .collect();
    assert_eq!(
        doge,
        [
            "https://okx.test/api/v5/market/ticker?instId=DOGE-USDT-SWAP",
            "https://okx.test/api/v5/public/mark-price?instType=SWAP&instId=DOGE-USDT-SWAP",
            "https://okx.test/api/v5/public/funding-rate?instId=DOGE-USDT-SWAP",
        ]
    );
    assert_eq!(
        feed.get_quote("DOGEUSDT").map(|q| q.last),
        Some("200".into())
    );
    // Tracking it again changes nothing.
    feed.track("DOGEUSDT");
    settle().await;
    assert_eq!(fake.gets_matching("DOGE").len(), 3);

    server
        .send(Message::text(ticker("DOGE-USDT-SWAP", "0.3")))
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        drain(&mut quotes).last().map(|q| q.last.clone()),
        Some("0.3".into())
    );

    feed.untrack("DOGEUSDT");
    let unsubscribe: Value = serde_json::from_str(&next_text(&mut server).await).unwrap();
    assert_eq!(unsubscribe["op"], "unsubscribe");
    assert_eq!(unsubscribe["args"][0]["instId"], "DOGE-USDT-SWAP");
    assert_eq!(feed.get_quote("DOGEUSDT"), None);
    server
        .send(Message::text(ticker("DOGE-USDT-SWAP", "0.4")))
        .await
        .unwrap();
    settle().await;
    assert!(drain(&mut quotes).is_empty());
    assert_eq!(feed.get_quote("DOGEUSDT"), None);
    feed.stop();
}

#[tokio::test(start_paused = true)]
async fn candle_cache_3s() {
    let (fake, _sockets) = FakeOkx::new(okx_rest());
    let feed = OkxPublicFeed::with_transport(REST, WS, fake.clone());
    let hits = || fake.gets_matching("/market/candles").len();

    let first = feed
        .get_candles("BTCUSDT", Timeframe::H1, 1000)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(
        fake.gets_matching("/market/candles")[0].1,
        "https://okx.test/api/v5/market/candles?instId=BTC-USDT-SWAP&bar=1H&limit=300"
    );
    sleep(Duration::from_millis(2999)).await;
    assert_eq!(
        feed.get_candles("BTCUSDT", Timeframe::H1, 1000)
            .await
            .unwrap(),
        first
    );
    assert_eq!(hits(), 1);
    // Another limit is another cache entry, though OKX is asked for the same 300.
    feed.get_candles("BTCUSDT", Timeframe::H1, 300)
        .await
        .unwrap();
    assert_eq!(hits(), 2);
    sleep(Duration::from_millis(1)).await;
    feed.get_candles("BTCUSDT", Timeframe::H1, 1000)
        .await
        .unwrap();
    assert_eq!(hits(), 3);

    // The instrument list: 5 minutes, unless asked for fresh.
    let list_hits = || fake.gets_matching("/public/instruments").len();
    let specs = feed.list_instruments(false).await.unwrap();
    assert_eq!(specs[0].symbol, "DOGEUSDT");
    feed.list_instruments(false).await.unwrap();
    assert_eq!(list_hits(), 1);
    feed.list_instruments(true).await.unwrap();
    assert_eq!(list_hits(), 2);
    sleep(Duration::from_secs(299)).await;
    feed.list_instruments(false).await.unwrap();
    assert_eq!(list_hits(), 2);
    sleep(Duration::from_secs(1)).await;
    feed.list_instruments(false).await.unwrap();
    assert_eq!(list_hits(), 3);
}

#[tokio::test(start_paused = true)]
async fn rest_failures_and_timeouts_have_nodes_texts() {
    let (fake, _sockets) = FakeOkx::new(
        Router::new()
            .route(
                "/api/v5/market/candles",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(9)).await;
                    "late"
                }),
            )
            .route(
                "/api/v5/public/instruments",
                get(|| async {
                    axum::Json(json!({ "code": "50011", "msg": "Too Many Requests", "data": [] }))
                }),
            ),
    );
    let feed = OkxPublicFeed::with_transport(REST, WS, fake);
    let started = Instant::now();
    let error = feed
        .get_candles("BTCUSDT", Timeframe::M1, 5)
        .await
        .unwrap_err();
    assert_eq!(error, "The operation was aborted due to timeout");
    assert_eq!(started.elapsed(), Duration::from_secs(8));
    let error = feed.list_instruments(true).await.unwrap_err();
    assert_eq!(error, "OKX error 50011: Too Many Requests");
}
