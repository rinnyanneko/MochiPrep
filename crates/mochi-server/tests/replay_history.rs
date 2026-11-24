//! A replay of recorded OKX history over dates the owner picks: nothing is loaded until then,
//! only missing minutes are downloaded, overlapping ranges reuse saved bars, saved ranges can be
//! deleted, and a restart loads the picked dates from the database.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mochi_domain::{Candle, FeedStatus, Quote};
use mochi_server::runtime::live_feed::FeedResult;
use mochi_server::runtime::{LiveFeed, OfflineFeed, ReplayRange, NOT_LOADED};
use mochi_server::{RuntimeHandle, RuntimeOptions};

const PAIRS: [&str; 2] = ["BTCUSDT", "ETHUSDT"];
const DAY: i64 = 86_400_000;
const MINUTE: i64 = 60_000;
/// The hour fetched before a window's first minute.
const LEAD: u64 = 60;

/// Serves a bar for every minute but the ones in `quiet`, priced by its time, and records
/// what was asked for.
#[derive(Default)]
struct History {
    asked: Mutex<Vec<(String, i64, usize)>>,
    fail: bool,
    quiet: Vec<i64>,
    /// When set, each request waits for a permit, so a test can act during a download.
    hold: Option<Arc<tokio::sync::Semaphore>>,
}

impl LiveFeed for History {
    fn track(&self, _symbol: &str) {}
    fn untrack(&self, _symbol: &str) {}
    fn get_quote(&self, _symbol: &str) -> Option<Quote> {
        None
    }
    fn status(&self) -> FeedStatus {
        OfflineFeed.status()
    }
    fn history_bars<'a>(
        &'a self,
        symbol: &'a str,
        start: i64,
        minutes: usize,
        progress: &'a (dyn Fn(usize) + Send + Sync),
    ) -> FeedResult<'a, Vec<Candle>> {
        self.asked
            .lock()
            .unwrap()
            .push((symbol.to_string(), start, minutes));
        Box::pin(async move {
            if let Some(hold) = &self.hold {
                hold.acquire().await.unwrap().forget();
            }
            if self.fail {
                return Err("offline".to_string());
            }
            let bars = (0..minutes as i64)
                .map(|i| start + i * MINUTE)
                .filter(|t| !self.quiet.contains(t))
                .map(|time| {
                    let price = 100.0 + (time / MINUTE % 1000) as f64;
                    Candle {
                        time,
                        open: price,
                        high: price + 1.0,
                        low: price - 1.0,
                        close: price,
                        volume: 2.0,
                        closed: true,
                    }
                })
                .collect();
            progress(minutes);
            Ok(bars)
        })
    }
}

fn start(path: &std::path::Path, feed: Arc<History>) -> RuntimeHandle {
    let mut options = RuntimeOptions::new(mochi_store::open_database(path).unwrap());
    options.builtin_symbols = PAIRS.to_vec();
    options.replay_history = true;
    options.live = feed;
    RuntimeHandle::start(options).unwrap()
}

/// A range of `days` days ending `back` days before the last UTC midnight.
fn range(back: i64, days: i64) -> ReplayRange {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let to = now.div_euclid(DAY) * DAY - back * DAY;
    ReplayRange {
        from: to - days * DAY,
        to,
    }
}

async fn settle(rt: &RuntimeHandle) -> mochi_domain::ReplayState {
    for _ in 0..500 {
        let state = rt.replay_state().await.unwrap();
        if state.downloading.is_none() {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the download never finished");
}

#[tokio::test]
async fn picked_dates_download_once_overlaps_are_reused_and_saved_ranges_can_be_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mochiprep.db");
    let feed = Arc::new(History::default());
    let rt = start(&path, feed.clone());

    // Nothing is fetched or held until dates are picked.
    assert!(!rt.replay_state().await.unwrap().loaded);
    let refused = rt.call(|r| r.load_replay(None)).await.unwrap().unwrap_err();
    assert_eq!(refused.message(), "Choose the replay's dates first");
    let refused = rt
        .call(|r| r.assert_replay_loaded())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(refused.message(), NOT_LOADED);

    // Two days, with four days of warm-up in front, and an hour before that for the close a
    // quiet first minute would repeat: six days and an hour a pair to fetch.
    let first = range(3, 2);
    let plan = rt
        .call(move |r| r.plan_replay(first))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plan.pairs, 2);
    assert_eq!(plan.missing_minutes, 2 * (6 * 1440 + LEAD));
    assert_eq!(plan.requests, 2 * 87); // 8700 minutes a pair, 100 a request
    assert!(plan.seconds > 0);
    assert_eq!(plan.download_bytes, plan.missing_minutes * 57);
    assert_eq!(plan.saved_bytes, 0);
    assert_eq!(plan.span.from, first.from - 4 * DAY);

    rt.call(move |r| r.load_replay(Some(first)))
        .await
        .unwrap()
        .unwrap();
    let state = settle(&rt).await;
    assert!(state.loaded, "{:?}", state.load_error);
    assert_eq!(state.end_time, Some(first.to));
    assert_eq!(feed.asked.lock().unwrap().len(), 2);
    let quotes = rt
        .call(|r| r.quotes(mochi_domain::DataSource::Replay))
        .await
        .unwrap();
    assert_eq!(quotes.len(), 2);

    let data = rt.call(|r| r.replay_data()).await.unwrap().unwrap();
    assert_eq!(data.saved.len(), 1);
    assert!(data.saved[0].complete && data.saved[0].in_use);
    assert_eq!(
        data.saved[0].start,
        first.from - 4 * DAY - LEAD as i64 * MINUTE
    );
    assert_eq!(data.saved[0].end, first.to);
    assert_eq!(data.saved[0].bytes, 2 * (6 * 1440 + LEAD) * 57);

    // One day later: five of its six days are saved, so only the last day is fetched.
    let later = range(2, 2);
    let plan = rt
        .call(move |r| r.plan_replay(later))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plan.missing_minutes, 2 * 1440);
    assert_eq!(plan.saved_bytes, 2 * 5 * 1440 * 57);
    rt.call(move |r| r.load_replay(Some(later)))
        .await
        .unwrap()
        .unwrap();
    assert!(settle(&rt).await.loaded);
    let asked = feed.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 4);
    assert_eq!(asked[2], ("BTCUSDT".to_string(), first.to, 1440));
    // The two downloads merged into one stretch.
    let data = rt.call(|r| r.replay_data()).await.unwrap().unwrap();
    assert_eq!(data.saved.len(), 1);
    assert_eq!(data.saved[0].end, later.to);

    // The loaded replay's bars stay; the first day, outside it, can go.
    let span = (data.saved[0].start, later.to);
    let refused = rt
        .call(move |r| r.delete_replay_data(span.0, span.1))
        .await
        .unwrap()
        .unwrap_err();
    assert!(refused.message().contains("loaded replay uses"));
    let first_day = data.saved[0].start;
    rt.call(move |r| r.delete_replay_data(first_day, first_day + DAY))
        .await
        .unwrap()
        .unwrap();
    let data = rt.call(|r| r.replay_data()).await.unwrap().unwrap();
    assert_eq!(data.saved[0].start, first_day + DAY);
    assert!(data.chosen_ready);
    rt.shutdown().await;

    // A restart with no connection loads the picked dates from the database.
    let offline = Arc::new(History {
        fail: true,
        ..Default::default()
    });
    let rt = start(&path, offline.clone());
    assert!(!rt.replay_state().await.unwrap().loaded);
    rt.call(|r| r.load_replay(None)).await.unwrap().unwrap();
    let again = rt.replay_state().await.unwrap();
    assert!(again.loaded);
    assert_eq!(again.end_time, Some(later.to));
    assert!(offline.asked.lock().unwrap().is_empty());
    rt.shutdown().await;
}

#[tokio::test]
async fn a_minute_without_a_trade_repeats_the_last_close() {
    let dir = tempfile::tempdir().unwrap();
    let r = range(3, 1);
    let quiet_minute = r.from - 4 * DAY + 10 * MINUTE;
    // The window's very first minute is quiet too, on a database with nothing before it: the
    // hour fetched ahead of the window gives the close it repeats.
    let first_minute = r.from - 4 * DAY;
    let feed = Arc::new(History {
        quiet: vec![quiet_minute, first_minute],
        ..Default::default()
    });
    let rt = start(&dir.path().join("mochiprep.db"), feed);
    rt.call(move |x| x.load_replay(Some(r)))
        .await
        .unwrap()
        .unwrap();
    assert!(settle(&rt).await.loaded);
    let candles = rt
        .call(|x| x.replay_candles("BTCUSDT", mochi_domain::Timeframe::M1, 100_000))
        .await
        .unwrap();
    let quiet = candles.iter().find(|c| c.time == quiet_minute).unwrap();
    let before = candles
        .iter()
        .find(|c| c.time == quiet_minute - MINUTE)
        .unwrap();
    assert_eq!(quiet.close, before.close);
    assert_eq!(quiet.volume, 0.0);
    let first = candles.iter().find(|c| c.time == first_minute).unwrap();
    assert_eq!(first.volume, 0.0);
    assert_eq!(
        first.close,
        100.0 + ((first_minute - MINUTE) / MINUTE % 1000) as f64
    );
    rt.shutdown().await;
}

/// The hour before a window is not part of its dates, so it can be deleted while the window
/// stays saved. A window whose first minute had no trade then has no close to repeat, so the
/// hour is fetched again rather than the saved window becoming impossible to load.
#[tokio::test]
async fn a_deleted_lead_in_is_fetched_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mochiprep.db");
    let r = range(3, 1);
    let first_minute = r.from - 4 * DAY;
    let quiet = || {
        Arc::new(History {
            quiet: vec![first_minute],
            ..Default::default()
        })
    };
    let rt = start(&path, quiet());
    rt.call(move |x| x.load_replay(Some(r)))
        .await
        .unwrap()
        .unwrap();
    assert!(settle(&rt).await.loaded);
    let lead = first_minute - LEAD as i64 * MINUTE;
    rt.call(move |x| x.delete_replay_data(lead, first_minute))
        .await
        .unwrap()
        .unwrap();
    rt.shutdown().await;

    let feed = quiet();
    let rt = start(&path, feed.clone());
    let plan = rt.call(move |x| x.plan_replay(r)).await.unwrap().unwrap();
    assert_eq!(plan.missing_minutes, PAIRS.len() as u64 * LEAD);
    rt.call(|x| x.load_replay(None)).await.unwrap().unwrap();
    let state = settle(&rt).await;
    assert!(state.loaded, "{:?}", state.load_error);
    let asked = feed.asked.lock().unwrap().clone();
    assert_eq!(asked[0], ("BTCUSDT".to_string(), lead, LEAD as usize));
    rt.shutdown().await;
}

/// Loading other dates pauses the replay while their bars download. Playing or stepping the old
/// bars meanwhile would save a cursor the new dates then start from, so neither is admitted,
/// even when it reaches the runtime after the route's own check.
#[tokio::test]
async fn nothing_plays_or_steps_during_a_download() {
    let dir = tempfile::tempdir().unwrap();
    let hold = Arc::new(tokio::sync::Semaphore::new(PAIRS.len()));
    let feed = Arc::new(History {
        hold: Some(hold.clone()),
        ..Default::default()
    });
    let rt = start(&dir.path().join("mochiprep.db"), feed);
    let first = range(3, 1);
    rt.call(move |x| x.load_replay(Some(first)))
        .await
        .unwrap()
        .unwrap();
    assert!(settle(&rt).await.loaded);

    let later = range(1, 1);
    let playing = rt
        .call(move |x| {
            x.load_replay(Some(later)).unwrap();
            x.set_playing(true);
            x.replay_state().playing
        })
        .await
        .unwrap();
    assert!(!playing);
    let before = rt.replay_state().await.unwrap();
    assert!(before.downloading.is_some());
    let refused = rt.step_replay(1).await.unwrap_err();
    assert_eq!(refused.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(rt.replay_state().await.unwrap().now, before.now);

    hold.add_permits(PAIRS.len());
    let state = settle(&rt).await;
    assert!(state.loaded);
    assert_eq!(state.end_time, Some(later.to));
    rt.step_replay(1).await.unwrap();
    rt.shutdown().await;
}

#[tokio::test]
async fn bad_dates_are_refused_and_a_failed_download_can_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mochiprep.db");
    let rt = start(
        &path,
        Arc::new(History {
            fail: true,
            ..Default::default()
        }),
    );
    let ahead = range(-1, 1);
    let long = range(1, 91);
    let odd = ReplayRange {
        from: range(3, 1).from + 1,
        to: range(3, 1).to,
    };
    for (bad, message) in [
        (ahead, "must end by the last UTC midnight"),
        (long, "at most 90 days"),
        (odd, "whole UTC days"),
    ] {
        let error = rt
            .call(move |r| r.plan_replay(bad))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.message().contains(message), "{}", error.message());
    }
    let r = range(3, 1);
    rt.call(move |x| x.load_replay(Some(r)))
        .await
        .unwrap()
        .unwrap();
    let state = settle(&rt).await;
    assert!(!state.loaded);
    assert_eq!(
        state.load_error.as_deref(),
        Some("Could not download the BTCUSDT replay data: offline")
    );
    rt.shutdown().await;

    let rt = start(&path, Arc::new(History::default()));
    rt.call(|x| x.load_replay(None)).await.unwrap().unwrap();
    assert!(settle(&rt).await.loaded);
    rt.shutdown().await;
}

async fn post(
    state: &mochi_server::AppState,
    path: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost:8787")
        .header("authorization", format!("Bearer {}", state.token))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = mochi_server::app(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn the_routes_estimate_load_list_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let rt = start(
        &dir.path().join("mochiprep.db"),
        Arc::new(History::default()),
    );
    let state = mochi_server::AppState::new(
        Arc::new(mochi_server::config::config_from(|_| None)),
        rt.clone(),
    );
    let r = range(3, 1);
    let body = serde_json::json!({ "from": r.from, "to": r.to });
    let (status, plan) = post(&state, "/api/replay/estimate", body.clone()).await;
    assert_eq!(status, 200, "{plan}");
    assert_eq!(plan["missingMinutes"], 2 * (5 * 1440 + LEAD));
    assert_eq!(plan["range"]["from"], r.from);
    let (status, _) = post(
        &state,
        "/api/replay/estimate",
        serde_json::json!({ "from": r.from }),
    )
    .await;
    assert_eq!(status, 400);
    let (status, replay) = post(&state, "/api/replay/load", body).await;
    assert_eq!(status, 200, "{replay}");
    assert!(settle(&rt).await.loaded);
    let span = serde_json::json!({ "from": r.from - 4 * DAY, "to": r.to });
    let (status, refused) = post(&state, "/api/replay/data/delete", span).await;
    assert_eq!(status, 400, "{refused}");
    let part = serde_json::json!({ "from": r.from - 10 * DAY, "to": r.from - 10 * DAY + 1 });
    let (status, refused) = post(&state, "/api/replay/data/delete", part).await;
    assert_eq!(status, 400);
    assert!(refused["error"].as_str().unwrap().contains("whole minutes"));
    rt.shutdown().await;
}
