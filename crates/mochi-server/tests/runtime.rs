//! The runtime actor and the replay clock, against the behaviour of `apps/server/src/runtime.ts`.
//! Timing tests run on tokio's paused clock, which jumps straight to the next timer whenever
//! every task is idle, so they are exact and fast.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mochi_core::paper::CreateAccountArgs;
use mochi_domain::{
    default_instrument, DataSource, FeedState, FeedStatus, OrderRequest, Quote, ServerEvent,
    ToastLevel, Topic, VaultStatus,
};
use mochi_market::tape::WARMUP_MINUTES;
use mochi_server::runtime::{LiveFeed, ReplayClock, RuntimeHandle, RuntimeOptions};
use mochi_store::{get_setting, open_database, open_in_memory, save_instrument, set_setting};
use serde_json::json;
use tokio::sync::broadcast;
use tokio::time::{sleep, timeout, Instant};

const MINUTE: i64 = 60_000;

fn start(db: rusqlite::Connection, minutes: Option<usize>) -> RuntimeHandle {
    let mut options = RuntimeOptions::new(db);
    options.replay_minutes = minutes;
    RuntimeHandle::start(options).unwrap()
}

fn replay_account(name: &str, source: DataSource) -> CreateAccountArgs {
    CreateAccountArgs {
        name: name.into(),
        source,
        initial_balance: None,
        is_demo: None,
        config: None,
    }
}

async fn cursor(rt: &RuntimeHandle) -> Option<i64> {
    rt.call(|rt| get_setting::<i64>(rt.connection(), "replay.cursor").unwrap())
        .await
        .unwrap()
}

/// Every event already sent, without waiting for more.
fn drain(events: &mut broadcast::Receiver<ServerEvent>) -> Vec<ServerEvent> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        out.push(event);
    }
    out
}

#[tokio::test(start_paused = true)]
async fn replay_batch_steps_floor_of_credit() {
    let rt = start(open_in_memory().unwrap(), None);
    let mut events = rt.subscribe();
    let begin = rt.replay_state().await.unwrap();
    assert_eq!(begin.speed, 20);
    assert!(!begin.playing);
    let begin_now = begin.now.unwrap();

    // Speed 20 is 20 tape-minutes a second: 2 steps on every 100 ms tick.
    rt.set_playing(true).await.unwrap();
    sleep(Duration::from_millis(150)).await;
    assert_eq!(
        rt.replay_state().await.unwrap().now,
        Some(begin_now + 2 * MINUTE)
    );
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        rt.replay_state().await.unwrap().now,
        Some(begin_now + 4 * MINUTE)
    );

    // Each batch emits the last step's quotes once, one per default symbol, then the state.
    let sent = drain(&mut events);
    assert!(matches!(&sent[0], ServerEvent::Replay { state } if state.playing));
    let batch: Vec<&ServerEvent> = sent[1..5].iter().collect();
    for (event, symbol) in batch.iter().zip(["BTCUSDT", "ETHUSDT", "SOLUSDT"]) {
        match event {
            ServerEvent::Quote { quote } => {
                assert_eq!(quote.symbol, symbol);
                assert_eq!(quote.ts, begin_now + 2 * MINUTE);
            }
            other => panic!("expected a quote, got {other:?}"),
        }
    }
    assert!(
        matches!(batch[3], ServerEvent::Replay { state } if state.now == Some(begin_now + 2 * MINUTE))
    );
    let quotes = sent
        .iter()
        .filter(|event| matches!(event, ServerEvent::Quote { .. }))
        .count();
    assert_eq!(quotes, 6, "two batches of three quotes");

    // Speed 5 earns half a step per tick, so a step lands on every second tick.
    rt.set_speed(5).await.unwrap();
    let before = rt.replay_state().await.unwrap().now.unwrap();
    sleep(Duration::from_millis(100)).await;
    assert_eq!(rt.replay_state().await.unwrap().now, Some(before));
    sleep(Duration::from_millis(100)).await;
    assert_eq!(rt.replay_state().await.unwrap().now, Some(before + MINUTE));

    // Pausing stops the timer.
    rt.set_playing(false).await.unwrap();
    let paused = rt.replay_state().await.unwrap().now;
    sleep(Duration::from_secs(1)).await;
    assert_eq!(rt.replay_state().await.unwrap().now, paused);
    rt.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn no_credit_while_a_batch_is_in_flight() {
    let mut clock = ReplayClock::new(20);
    clock.set_playing(true);
    assert_eq!(clock.on_tick(), Some(2));

    // "The tape waits for agents": ticks during a batch earn nothing.
    clock.begin_batch();
    assert_eq!(clock.on_tick(), None);
    assert_eq!(clock.on_tick(), None);
    assert_eq!(clock.on_tick(), None);
    clock.end_batch();
    assert_eq!(clock.on_tick(), Some(2), "no credit was saved up meanwhile");

    // Fractions carry over between ticks, and a manual batch blocks the timer the same way.
    clock.set_speed(3);
    assert_eq!(clock.on_tick(), None);
    assert_eq!(clock.on_tick(), None);
    clock.begin_batch();
    assert_eq!(clock.on_tick(), None);
    clock.end_batch();
    // 0.3 + 0.3 + 0.3 is 0.8999... in floating point, as in JS.
    assert_eq!(clock.on_tick(), None);
    assert_eq!(clock.on_tick(), Some(1));
}

#[tokio::test(start_paused = true)]
async fn end_of_tape_stops_playing_and_persists_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mochiprep.db");
    // Four playable steps: the cursor starts at the warmup and the last index is warmup + 4.
    let minutes = Some(WARMUP_MINUTES + 5);
    let last = (WARMUP_MINUTES + 4) as i64;

    let rt = start(open_database(&path).unwrap(), minutes);
    let mut events = rt.subscribe();
    rt.set_playing(true).await.unwrap();
    sleep(Duration::from_millis(350)).await;

    let state = rt.replay_state().await.unwrap();
    assert!(state.at_end);
    assert!(!state.playing);
    assert_eq!(state.now, state.end_time);
    assert_eq!(state.progress, 1.0);
    assert_eq!(cursor(&rt).await, Some(last));
    let stopped = drain(&mut events)
        .into_iter()
        .filter_map(|event| match event {
            ServerEvent::Replay { state } => Some(state),
            _ => None,
        })
        .collect::<Vec<_>>();
    // Playing, the first batch, the stop at the end, then the last batch's state.
    let flags: Vec<(bool, bool)> = stopped.iter().map(|s| (s.playing, s.at_end)).collect();
    assert_eq!(
        flags,
        vec![(true, false), (true, false), (false, true), (false, true)]
    );
    rt.shutdown().await;

    // A restart at the end stays at the end, not playing.
    let rt = start(open_database(&path).unwrap(), minutes);
    let state = rt.replay_state().await.unwrap();
    assert!(state.at_end);
    assert!(!state.playing);
    assert_eq!(state.now, state.end_time);

    // Pressing play at the end stops again on the next tick without moving.
    rt.set_playing(true).await.unwrap();
    sleep(Duration::from_millis(150)).await;
    let state = rt.replay_state().await.unwrap();
    assert!(!state.playing);
    assert!(state.at_end);
    assert_eq!(cursor(&rt).await, Some(last));
    rt.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn restart_restores_cursor_and_speed_not_playing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mochiprep.db");
    let minutes = Some(WARMUP_MINUTES + 100);

    let rt = start(open_database(&path).unwrap(), minutes);
    let first = rt.replay_state().await.unwrap();
    assert_eq!(first.now, first.start_time);
    rt.step_replay(7).await.unwrap();
    rt.set_speed(40).await.unwrap();
    rt.set_playing(true).await.unwrap();
    rt.set_playing(false).await.unwrap();
    rt.set_playing(true).await.unwrap();
    let before = rt.replay_state().await.unwrap();
    rt.shutdown().await;

    let rt = start(open_database(&path).unwrap(), minutes);
    let after = rt.replay_state().await.unwrap();
    assert!(!after.playing, "playing lives in memory only");
    assert_eq!(after.speed, 40);
    assert_eq!(after.now, before.now);
    assert_eq!(after.now, Some(first.now.unwrap() + 7 * MINUTE));
    rt.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn invalidate_coalesces_within_200ms() {
    let rt = start(open_in_memory().unwrap(), None);
    let mut events = rt.subscribe();
    let begin = Instant::now();
    rt.invalidate(&[Topic::Accounts]).await.unwrap();
    sleep(Duration::from_millis(50)).await;
    // Repeats collapse, and the timer is not re-armed by later calls.
    rt.invalidate(&[Topic::Orders, Topic::Accounts])
        .await
        .unwrap();

    let event = events.recv().await.unwrap();
    assert_eq!(begin.elapsed(), Duration::from_millis(200));
    assert_eq!(
        event,
        ServerEvent::Invalidate {
            topics: vec![Topic::Accounts, Topic::Orders]
        }
    );
    assert!(
        timeout(Duration::from_millis(500), events.recv())
            .await
            .is_err(),
        "one event for the whole window"
    );

    // The next call opens a new window.
    let again = Instant::now();
    rt.invalidate(&[Topic::Instruments]).await.unwrap();
    let event = events.recv().await.unwrap();
    assert_eq!(again.elapsed(), Duration::from_millis(200));
    assert_eq!(
        event,
        ServerEvent::Invalidate {
            topics: vec![Topic::Instruments]
        }
    );
    rt.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn equity_recorded_every_15_tape_minutes() {
    let rt = start(open_in_memory().unwrap(), Some(WARMUP_MINUTES + 100));
    let start_now = rt.replay_state().await.unwrap().now.unwrap();
    let (replay_id, live_id) = rt
        .call(|rt| {
            let engine = rt.engine_mut();
            let replay = engine
                .create_account(replay_account("replay", DataSource::Replay))
                .unwrap();
            let live = engine
                .create_account(replay_account("live", DataSource::Live))
                .unwrap();
            (replay.id, live.id)
        })
        .await
        .unwrap();
    let equity = |id: String| {
        let rt = rt.clone();
        async move {
            rt.call(move |rt| {
                let store = rt.engine().store();
                mochi_core::paper::PaperStore::list_equity(store, &id, 100)
                    .unwrap()
                    .iter()
                    .map(|point| point.ts)
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap()
        }
    };
    // Creating the account seeds one point.
    assert_eq!(equity(replay_id.clone()).await, vec![start_now]);

    rt.step_replay(14).await.unwrap();
    assert_eq!(equity(replay_id.clone()).await, vec![start_now]);
    rt.step_replay(1).await.unwrap();
    assert_eq!(
        equity(replay_id.clone()).await,
        vec![start_now, start_now + 15 * MINUTE]
    );
    // Within one batch too, and only replay accounts.
    rt.step_replay(31).await.unwrap();
    assert_eq!(
        equity(replay_id.clone()).await,
        vec![
            start_now,
            start_now + 15 * MINUTE,
            start_now + 30 * MINUTE,
            start_now + 45 * MINUTE
        ]
    );
    assert_eq!(equity(live_id).await.len(), 1);
    rt.shutdown().await;
}

#[tokio::test]
async fn settings_round_trip_json() {
    let rt = start(open_in_memory().unwrap(), None);
    rt.set_speed(120).await.unwrap();
    let stored = rt
        .call(|rt| {
            rt.connection()
                .query_row(
                    "SELECT value FROM settings WHERE key='replay.speed'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .unwrap()
        })
        .await
        .unwrap();
    assert_eq!(stored, "120");
    rt.step_replay(3).await.unwrap();
    assert_eq!(cursor(&rt).await, Some((WARMUP_MINUTES + 3) as i64));
    rt.shutdown().await;
}

/// A live feed that records what it was asked to track and serves fixed quotes.
#[derive(Default)]
struct RecordingFeed {
    tracked: Mutex<Vec<String>>,
    quotes: Mutex<HashMap<String, Quote>>,
}

impl LiveFeed for RecordingFeed {
    fn track(&self, symbol: &str) {
        self.tracked.lock().unwrap().push(symbol.to_string());
    }
    fn untrack(&self, symbol: &str) {
        self.tracked.lock().unwrap().retain(|s| s != symbol);
    }
    fn get_quote(&self, symbol: &str) -> Option<Quote> {
        self.quotes.lock().unwrap().get(symbol).cloned()
    }
    fn status(&self) -> FeedStatus {
        FeedStatus {
            source: DataSource::Live,
            provider: "Recording".into(),
            state: FeedState::Connected,
            last_message_at: Some(1),
            detail: None,
        }
    }
}

fn doge() -> mochi_domain::InstrumentSpecification {
    let mut doge = default_instrument("SOLUSDT").unwrap();
    doge.symbol = "DOGEUSDT".into();
    doge.base_currency = "DOGE".into();
    doge
}

#[tokio::test]
async fn instruments_rows_register_in_engine_registry() {
    let db = open_in_memory().unwrap();
    save_instrument(&db, &doge()).unwrap();
    let feed = Arc::new(RecordingFeed::default());
    let mut options = RuntimeOptions::new(db);
    options.live = feed.clone();
    let rt = RuntimeHandle::start(options).unwrap();

    assert_eq!(*feed.tracked.lock().unwrap(), vec!["DOGEUSDT".to_string()]);
    let in_engine = rt
        .call(|rt| rt.engine().instruments().get("DOGEUSDT").cloned())
        .await
        .unwrap();
    assert_eq!(in_engine, Some(doge()));

    // `/api/state` lists the built-ins first, then added pairs, like Node's INSTRUMENTS object.
    let state = serde_json::to_value(rt.state().await.unwrap()).unwrap();
    let symbols: Vec<&String> = state["instruments"].as_object().unwrap().keys().collect();
    assert_eq!(symbols, ["BTCUSDT", "ETHUSDT", "SOLUSDT", "DOGEUSDT"]);
    assert_eq!(
        state["instruments"]["DOGEUSDT"]["baseCurrency"],
        json!("DOGE")
    );
    rt.shutdown().await;
}

#[tokio::test]
async fn state_reports_demo_vault_feeds_and_replay() {
    let db = open_in_memory().unwrap();
    let rt = start(db, None);
    let state = serde_json::to_value(rt.state().await.unwrap()).unwrap();
    let keys: Vec<&String> = state.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["demoActive", "vault", "replay", "feeds", "instruments"]
    );
    assert_eq!(state["demoActive"], json!(false));
    assert_eq!(state["vault"], json!("OFF"));
    assert_eq!(state["replay"]["loaded"], json!(true));
    assert_eq!(state["replay"]["playing"], json!(false));
    assert_eq!(state["replay"]["speed"], json!(20));
    assert_eq!(state["replay"]["progress"], json!(0));
    let replay_now = state["replay"]["now"].clone();
    assert_eq!(
        state["feeds"],
        json!([
            { "source": "REPLAY", "provider": "Synthetic replay", "state": "OFFLINE_REPLAY",
              "lastMessageAt": replay_now },
            { "source": "LIVE", "provider": "OKX public", "state": "DISCONNECTED",
              "lastMessageAt": null },
        ])
    );

    rt.call(|rt| {
        set_setting(rt.connection(), "demo.active", &true).unwrap();
        rt.connection()
            .execute(
                "INSERT INTO vault_keys (id,kind,params,created_at) VALUES ('password','password','{}',1)",
                [],
            )
            .unwrap();
    })
    .await
    .unwrap();
    let state = rt.state().await.unwrap();
    assert!(state.demo_active);
    assert_eq!(state.vault, VaultStatus::Locked);
    rt.shutdown().await;
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn live_quote(price: &str) -> Quote {
    serde_json::from_value(json!({
        "source": "LIVE", "symbol": "SOLUSDT", "bid": price, "ask": price, "last": price,
        "mark": price, "markEstimated": false, "fundingRate": null, "nextFundingTime": null,
        "ts": unix_ms(),
    }))
    .unwrap()
}

#[tokio::test]
async fn live_quotes_reach_the_engine_and_liquidation_toasts() {
    let feed = Arc::new(RecordingFeed::default());
    feed.quotes
        .lock()
        .unwrap()
        .insert("SOLUSDT".into(), live_quote("100"));
    let (quotes, live_quotes) = tokio::sync::mpsc::channel(16);
    let mut options = RuntimeOptions::new(open_in_memory().unwrap());
    options.live = feed.clone();
    options.live_quotes = Some(live_quotes);
    let rt = RuntimeHandle::start(options).unwrap();
    let mut events = rt.subscribe();

    let account_id = rt
        .call(|rt| {
            let engine = rt.engine_mut();
            let account = engine
                .create_account(replay_account("Livewire", DataSource::Live))
                .unwrap();
            let request: OrderRequest = serde_json::from_value(json!({
                "accountId": account.id, "symbol": "SOLUSDT", "side": "BUY", "type": "MARKET",
                "quantity": "1", "leverage": 50, "clientOrderId": "open",
            }))
            .unwrap();
            engine.place_order(request).unwrap();
            account.id
        })
        .await
        .unwrap();
    // Ids are `<prefix>_<uuid v4>`, as Node's randomUUID gives.
    let uuid = account_id.strip_prefix("acct_").unwrap();
    assert_eq!(uuid.len(), 36);
    assert_eq!(uuid.as_bytes()[14], b'4');

    let crash = live_quote("50");
    quotes.send(crash.clone()).await.unwrap();

    let toast = ServerEvent::Toast {
        level: ToastLevel::Error,
        message: "Livewire: LONG SOLUSDT was liquidated (simulated)".into(),
    };
    // Read until an invalidate arrives after the toast; the order placed above may have sent one.
    let mut seen = Vec::new();
    let topics = loop {
        let event = timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        if let ServerEvent::Invalidate { topics } = &event {
            if seen.contains(&toast) {
                break topics.clone();
            }
        }
        seen.push(event);
    };
    // The toast comes first, then the quote goes out to subscribers.
    let at = |wanted: &ServerEvent| seen.iter().position(|e| e == wanted).unwrap();
    assert!(at(&toast) < at(&ServerEvent::Quote { quote: crash }));
    assert_eq!(
        topics,
        vec![
            Topic::Accounts,
            Topic::Orders,
            Topic::Positions,
            Topic::Arena
        ]
    );
    let open = rt
        .call(move |rt| {
            mochi_core::paper::PaperStore::list_open_positions(rt.engine().store(), &account_id)
                .unwrap()
                .len()
        })
        .await
        .unwrap();
    assert_eq!(open, 0);
    rt.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn live_tick_invalidates_arena_every_60s() {
    let rt = start(open_in_memory().unwrap(), None);
    let mut events = rt.subscribe();
    let begin = Instant::now();
    // The first live tick runs after 1 s and records equity at once (Node's lastLiveEquityAt = 0).
    let event = events.recv().await.unwrap();
    assert_eq!(
        event,
        ServerEvent::Invalidate {
            topics: vec![Topic::Arena]
        }
    );
    assert_eq!(begin.elapsed(), Duration::from_millis(1_200));
    let event = events.recv().await.unwrap();
    assert_eq!(
        event,
        ServerEvent::Invalidate {
            topics: vec![Topic::Arena]
        }
    );
    assert_eq!(begin.elapsed(), Duration::from_millis(61_200));
    rt.shutdown().await;
}

#[tokio::test]
async fn calls_after_shutdown_fail_cleanly() {
    let rt = start(open_in_memory().unwrap(), None);
    let other = rt.clone();
    rt.shutdown().await;
    let error = other.replay_state().await.unwrap_err();
    assert_eq!(error.status().as_u16(), 503);
}

#[tokio::test]
async fn a_tape_without_playable_bars_does_not_start() {
    let mut options = RuntimeOptions::new(open_in_memory().unwrap());
    options.replay_minutes = Some(WARMUP_MINUTES);
    let error = RuntimeHandle::start(options).err().unwrap();
    assert!(error.to_string().contains("needs more than"), "{error}");
}
