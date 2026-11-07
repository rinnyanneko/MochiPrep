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
