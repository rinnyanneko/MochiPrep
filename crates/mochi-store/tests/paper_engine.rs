//! Engine properties the scenario fixture cannot show: it is `Send`, and instruments registered
//! after construction are tradable.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mochi_core::paper::{CreateAccountArgs, PaperEngineDeps, PaperExecutionEngine};
use mochi_domain::registry::Instruments;
use mochi_domain::{DataSource, OrderRequest, OrderStatus, Quote};
use mochi_store::{open_in_memory, SqlitePaperStore};
use serde_json::json;

// The runtime holds the engine across async tasks, so this must keep compiling.
#[test]
fn engine_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<PaperExecutionEngine<SqlitePaperStore>>();
}

fn quote(symbol: &str, price: &str) -> Quote {
    serde_json::from_value(json!({
        "source": "REPLAY", "symbol": symbol, "bid": price, "ask": price, "last": price,
        "mark": price, "markEstimated": false, "fundingRate": "0", "nextFundingTime": null,
        "ts": 3_600_000,
    }))
    .unwrap()
}

#[test]
fn trades_an_instrument_registered_after_construction() {
    let quotes: Arc<Mutex<Vec<Quote>>> = Arc::default();
    let ids = AtomicU64::new(0);
    let q = quotes.clone();
    let mut engine = PaperExecutionEngine::new(PaperEngineDeps {
        store: SqlitePaperStore::new(open_in_memory().unwrap()),
        instruments: Instruments::with_defaults(),
        get_quote: Box::new(move |source, symbol| {
            q.lock()
                .unwrap()
                .iter()
                .find(|quote| quote.source == source && quote.symbol == symbol)
                .cloned()
        }),
        now: Box::new(|_| 3_600_000),
        new_id: Box::new(move |prefix| {
            format!("{prefix}_{}", ids.fetch_add(1, Ordering::Relaxed) + 1)
        }),
        on_event: Box::new(|_| {}),
    });
    let account = engine
        .create_account(CreateAccountArgs {
            name: "registry".into(),
            source: DataSource::Replay,
            initial_balance: Some("10000".into()),
            is_demo: None,
            config: Default::default(),
        })
        .unwrap();
    quotes.lock().unwrap().push(quote("DOGEUSDT", "0.2"));
    let order = |client_order_id: &str| -> OrderRequest {
        serde_json::from_value(json!({
            "accountId": account.id, "symbol": "DOGEUSDT", "side": "BUY", "type": "MARKET",
            "quantity": "100", "leverage": 5, "clientOrderId": client_order_id,
        }))
        .unwrap()
    };

    let error = engine.place_order(order("before")).unwrap_err();
    assert_eq!(error.to_string(), "Unknown instrument DOGEUSDT");

    let mut doge = engine.instruments().get("SOLUSDT").unwrap().clone();
    doge.symbol = "DOGEUSDT".into();
    doge.base_currency = "DOGE".into();
    engine.instruments_mut().register(doge);

    let placed = engine.place_order(order("after")).unwrap();
    assert_eq!(placed.order.status, OrderStatus::Filled);
    let positions = engine.position_views(&account.id).unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].position.symbol, "DOGEUSDT");
}

// A zero interval never advances the funding clock; the engine must refuse it, not spin.
#[test]
fn refuses_a_zero_funding_interval_instead_of_looping() {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let quotes: Arc<Mutex<Vec<Quote>>> = Arc::default();
        let ids = AtomicU64::new(0);
        let q = quotes.clone();
        let mut engine = PaperExecutionEngine::new(PaperEngineDeps {
            store: SqlitePaperStore::new(open_in_memory().unwrap()),
            instruments: Instruments::with_defaults(),
            get_quote: Box::new(move |source, symbol| {
                q.lock()
                    .unwrap()
                    .iter()
                    .find(|quote| quote.source == source && quote.symbol == symbol)
                    .cloned()
            }),
            now: Box::new(|_| 3_600_000),
            new_id: Box::new(move |prefix| {
                format!("{prefix}_{}", ids.fetch_add(1, Ordering::Relaxed) + 1)
            }),
            on_event: Box::new(|_| {}),
        });
        let account = engine
            .create_account(CreateAccountArgs {
                name: "funding".into(),
                source: DataSource::Replay,
                initial_balance: Some("10000".into()),
                is_demo: None,
                config: Default::default(),
            })
            .unwrap();
        quotes.lock().unwrap().push(quote("SOLUSDT", "100"));
        let order = |client_order_id: &str| -> OrderRequest {
            serde_json::from_value(json!({
                "accountId": account.id, "symbol": "SOLUSDT", "side": "BUY", "type": "MARKET",
                "quantity": "1", "leverage": 5, "clientOrderId": client_order_id,
            }))
            .unwrap()
        };
        engine.place_order(order("open")).unwrap();

        let mut sol = engine.instruments().get("SOLUSDT").unwrap().clone();
        sol.funding_interval_hours = 0;
        engine.instruments_mut().register(sol);
        let refused = "Instrument SOLUSDT has no funding interval";

        // An open position must not make the quote loop.
        let mut later = quote("SOLUSDT", "100");
        later.ts = 4 * 3_600_000;
        assert_eq!(engine.on_quote(&later).unwrap_err().to_string(), refused);

        // Nor may a new position open with a funding time that never advances.
        let fresh = engine
            .create_account(CreateAccountArgs {
                name: "fresh".into(),
                source: DataSource::Replay,
                initial_balance: Some("10000".into()),
                is_demo: None,
                config: Default::default(),
            })
            .unwrap();
        let open: OrderRequest = serde_json::from_value(json!({
            "accountId": fresh.id, "symbol": "SOLUSDT", "side": "BUY", "type": "MARKET",
            "quantity": "1", "leverage": 5, "clientOrderId": "fresh",
        }))
        .unwrap();
        assert_eq!(engine.place_order(open).unwrap_err().to_string(), refused);
        assert!(engine.position_views(&fresh.id).unwrap().is_empty());
        done.send(()).unwrap();
    });
    finished
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("engine looped or panicked on a zero funding interval");
}
