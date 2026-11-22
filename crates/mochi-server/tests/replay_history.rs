//! A replay of recorded OKX history: nothing is loaded until the replay is used, the bars come
//! from the feed once, and later loads read them from the database.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mochi_domain::{FeedStatus, Quote};
use mochi_market::tape::{generate_tape, Tape};
use mochi_server::runtime::live_feed::FeedResult;
use mochi_server::runtime::{LiveFeed, OfflineFeed};
use mochi_server::{RuntimeHandle, RuntimeOptions};

const PAIRS: [&str; 2] = ["BTCUSDT", "ETHUSDT"];

/// Serves the synthetic tape's bars as history, moved to the asked start, and counts requests.
#[derive(Default)]
struct History {
    asked: AtomicUsize,
    fail: bool,
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
    fn history_tape<'a>(
        &'a self,
        symbol: &'a str,
        start: i64,
        minutes: usize,
        progress: &'a (dyn Fn(usize) + Send + Sync),
    ) -> FeedResult<'a, Tape> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.fail {
                return Err("offline".to_string());
            }
            let synthetic = generate_tape(symbol, minutes).unwrap();
            let bars: Vec<_> = (0..minutes)
                .map(|i| {
                    let mut bar = synthetic.candle(i);
                    bar.time = start + i as i64 * 60_000;
                    bar
                })
                .collect();
            progress(minutes);
            Tape::from_candles(start, &bars)
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

async fn wait_loaded(rt: &RuntimeHandle) -> mochi_domain::ReplayState {
    for _ in 0..200 {
        let state = rt.replay_state().await.unwrap();
        if state.loaded || state.load_error.is_some() {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the replay never loaded");
}

#[tokio::test]
async fn the_replay_downloads_once_when_first_used_and_loads_from_the_database_after() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mochiprep.db");
    let feed = Arc::new(History::default());
    let rt = start(&path, feed.clone());

    // Nothing is fetched or held until the replay is used.
    let state = rt.replay_state().await.unwrap();
    assert!(!state.loaded);
    assert_eq!(feed.asked.load(Ordering::SeqCst), 0);
    rt.call(|r| {
        r.engine_mut()
            .create_account(mochi_core::paper::CreateAccountArgs {
                name: "replay".into(),
                source: mochi_domain::DataSource::Replay,
                initial_balance: None,
                is_demo: None,
                config: None,
            })
            .unwrap();
    })
    .await
    .unwrap();
    let account = rt
        .call(|r| {
            let error = r.assert_replay_loaded().unwrap_err();
            error.message().to_string()
        })
        .await
        .unwrap();
    assert_eq!(account, mochi_server::runtime::NOT_LOADED);
    let refused = rt
        .call(|r| r.assert_tradable("BTCUSDT", mochi_domain::DataSource::Replay))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(refused.message(), mochi_server::runtime::NOT_LOADED);

    rt.call(|r| r.load_replay()).await.unwrap().unwrap();
    let state = wait_loaded(&rt).await;
    assert!(state.loaded, "{:?}", state.load_error);
    assert_eq!(state.downloading, None);
    assert_eq!(feed.asked.load(Ordering::SeqCst), PAIRS.len());
    // The window ends at a UTC midnight, so the bars are history on OKX.
    assert_eq!(state.end_time.unwrap() % 86_400_000, 0);
    // Every pair has a quote from its recorded bars.
    let quotes = rt
        .call(|r| r.quotes(mochi_domain::DataSource::Replay))
        .await
        .unwrap();
    assert_eq!(quotes.len(), PAIRS.len());
    assert!(rt
        .call(|r| r.assert_tradable("ETHUSDT", mochi_domain::DataSource::Replay))
        .await
        .unwrap()
        .is_ok());
    rt.shutdown().await;

    // A restart with no connection loads the same bars from the database.
    let offline = Arc::new(History {
        fail: true,
        ..Default::default()
    });
    let rt = start(&path, offline.clone());
    assert!(!rt.replay_state().await.unwrap().loaded);
    rt.call(|r| r.load_replay()).await.unwrap().unwrap();
    let again = rt.replay_state().await.unwrap();
    assert!(again.loaded);
    assert_eq!(again.end_time, state.end_time);
    assert_eq!(offline.asked.load(Ordering::SeqCst), 0);
    rt.shutdown().await;
}

#[tokio::test]
async fn a_failed_download_says_why_and_can_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mochiprep.db");
    let rt = start(
        &path,
        Arc::new(History {
            fail: true,
            ..Default::default()
        }),
    );
    rt.call(|r| r.load_replay()).await.unwrap().unwrap();
    let state = wait_loaded(&rt).await;
    assert!(!state.loaded);
    assert_eq!(
        state.load_error.as_deref(),
        Some("Could not download the BTCUSDT replay data: offline")
    );
    rt.shutdown().await;

    let rt = start(&path, Arc::new(History::default()));
    rt.call(|r| r.load_replay()).await.unwrap().unwrap();
    assert!(wait_loaded(&rt).await.loaded);
    rt.shutdown().await;
}
