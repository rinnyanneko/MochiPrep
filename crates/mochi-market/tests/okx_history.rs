//! Downloading the replay's recorded bars from OKX's history-candles endpoint.

mod fake_okx;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::Query;
use axum::routing::get;
use axum::{Json, Router};
use fake_okx::{FakeOkx, REST, WS};
use mochi_market::okx_public::OkxPublicFeed;
use serde_json::{json, Value};

const START: i64 = 1_767_225_600_000;
const MINUTE: i64 = 60_000;

/// History-candles as OKX answers it: newest first, at most `limit` bars older than `after`,
/// over bars from `first` up to (not including) `end`, skipping the minutes in `gaps`.
fn history(first: i64, end: i64, gaps: &'static [i64]) -> Router {
    Router::new().route(
        "/api/v5/market/history-candles",
        get(move |Query(q): Query<HashMap<String, String>>| async move {
            let after: i64 = q["after"].parse().unwrap();
            let limit: usize = q["limit"].parse().unwrap();
            let mut rows = Vec::new();
            let mut t = after.min(end) - MINUTE;
            while t >= first && rows.len() < limit {
                if !gaps.contains(&t) {
                    let price = format!("{}", 100 + (t - START) / MINUTE);
                    rows.push(json!([
                        t.to_string(),
                        price,
                        price,
                        price,
                        price,
                        "1",
                        "2",
                        "3",
                        "1"
                    ]));
                }
                t -= MINUTE;
            }
            Json(json!({ "code": "0", "msg": "", "data": rows }))
        }),
    )
}

fn feed(rest: Router) -> (OkxPublicFeed, std::sync::Arc<FakeOkx>) {
    let (fake, _sockets) = FakeOkx::new(rest);
    (OkxPublicFeed::with_transport(REST, WS, fake.clone()), fake)
}

#[tokio::test(start_paused = true)]
async fn pages_back_over_the_window_and_fills_a_minute_without_trades() {
    let minutes = 250;
    let (feed, fake) = feed(history(
        START - 10 * MINUTE,
        START + 400 * MINUTE,
        &[START + 5 * MINUTE],
    ));
    let seen = AtomicUsize::new(0);
    let progress = |n: usize| {
        seen.fetch_max(n, Ordering::SeqCst);
    };
    let tape = feed
        .history_tape("BTCUSDT", START, minutes, &progress)
        .await
        .unwrap();
    assert_eq!(tape.len(), minutes);
    assert_eq!(tape.start(), START);
    assert_eq!(tape.close()[0], 100.0);
    assert_eq!(tape.close()[249], 349.0);
    // The minute OKX has no bar for repeats the last close, with no volume.
    assert_eq!(tape.close()[5], tape.close()[4]);
    assert_eq!(tape.volume()[5], 0.0);
    assert_eq!(tape.volume()[6], 2.0);
    assert_eq!(seen.load(Ordering::SeqCst), minutes);
    // Three pages of 100 cover 250 minutes, starting from the window's end.
    let gets = fake.gets_matching("history-candles");
    assert_eq!(gets.len(), 3);
    assert!(
        gets[0]
            .1
            .contains(&format!("after={}", START + 250 * MINUTE)),
        "{}",
        gets[0].1
    );
    assert!(gets[0].1.contains("instId=BTC-USDT-SWAP") && gets[0].1.contains("bar=1m"));
}

#[tokio::test(start_paused = true)]
async fn a_window_before_the_pairs_first_bar_is_refused() {
    let (feed, _) = feed(history(START + 30 * MINUTE, START + 400 * MINUTE, &[]));
    let error = feed
        .history_tape("BTCUSDT", START, 120, &|_| {})
        .await
        .unwrap_err();
    assert!(error.contains("no one-minute history"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn an_okx_error_is_reported() {
    let rest = Router::new().route(
        "/api/v5/market/history-candles",
        get(|| async {
            Json::<Value>(json!({ "code": "50011", "msg": "Too Many Requests", "data": [] }))
        }),
    );
    let (feed, _) = feed(rest);
    let error = feed
        .history_tape("BTCUSDT", START, 120, &|_| {})
        .await
        .unwrap_err();
    assert_eq!(error, "OKX error 50011: Too Many Requests");
}
