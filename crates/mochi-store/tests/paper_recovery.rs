//! Mirrors the paper part of `apps/server/test/recovery.test.ts`: reopening a file database must
//! give back the same account, positions, orders and ledger, and the engine must keep working.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mochi_core::ledger::audit_ledger;
use mochi_core::paper::{
    CreateAccountArgs, EngineEvent, PaperEngineDeps, PaperExecutionEngine, PaperStore,
};
use mochi_domain::registry::Instruments;
use mochi_domain::{DataSource, OrderRequest, OrderStatus, Quote};
use mochi_store::{open_database, SqlitePaperStore};
use serde_json::{json, Value};

type Quotes = Arc<Mutex<HashMap<String, Quote>>>;

/// Quotes are keyed by the source's Debug name, as the engine's `get_quote` closure looks them up.
fn key(symbol: &str) -> String {
    format!("{:?}:{symbol}", DataSource::Replay)
}

fn quote(symbol: &str, price: &str) -> Quote {
    serde_json::from_value(json!({
        "source": "REPLAY", "symbol": symbol, "bid": price, "ask": price, "last": price,
        "mark": price, "markEstimated": false, "fundingRate": "0", "nextFundingTime": null,
        "ts": 3_600_000,
    }))
    .unwrap()
}

fn request(value: Value) -> OrderRequest {
    serde_json::from_value(value).unwrap()
}

/// Opens the file database and builds an engine over it. The id counter is shared by the caller,
/// so ids keep counting up after a reopen instead of colliding with stored rows.
fn open_engine(
    path: &Path,
    quotes: &Quotes,
    ids: &Arc<AtomicU64>,
    events: &Arc<Mutex<Vec<EngineEvent>>>,
) -> PaperExecutionEngine<SqlitePaperStore> {
    let (q, ids, e) = (quotes.clone(), ids.clone(), events.clone());
    PaperExecutionEngine::new(PaperEngineDeps {
        store: SqlitePaperStore::new(open_database(path).unwrap()),
        instruments: Instruments::with_defaults(),
        get_quote: Box::new(move |source, symbol| {
            q.lock()
                .unwrap()
                .get(&format!("{source:?}:{symbol}"))
                .cloned()
        }),
        now: Box::new(|_| 3_600_000),
        new_id: Box::new(move |prefix| {
            format!("{prefix}_{}", ids.fetch_add(1, Ordering::Relaxed) + 1)
        }),
        on_event: Box::new(move |event| e.lock().unwrap().push(event)),
    })
}

/// Everything the engine reads back for one account, as JSON so the comparison is exact.
fn state(engine: &PaperExecutionEngine<SqlitePaperStore>, account_id: &str) -> Value {
    let store = engine.store();
    let account = store.get_account(account_id).unwrap().unwrap();
    let ledger = store.list_ledger(account_id, None).unwrap();
    assert!(audit_ledger(&account, &ledger).unwrap().is_empty());
    json!({
        "account": account,
        "balance": engine.balance_of(account_id).unwrap(),
        "positions": engine.position_views(account_id).unwrap(),
        "openOrders": engine.open_orders(account_id).unwrap(),
        "orders": store.list_orders(account_id, 100).unwrap(),
        "fills": store.list_fills(account_id, 100).unwrap(),
        "ledger": ledger,
    })
}

#[test]
fn reopened_database_restores_engine_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let quotes: Quotes = Arc::default();
    let ids = Arc::new(AtomicU64::new(0));
    let events: Arc<Mutex<Vec<EngineEvent>>> = Arc::default();
    for (symbol, price) in [("BTCUSDT", "100"), ("ETHUSDT", "50")] {
        quotes
            .lock()
            .unwrap()
            .insert(key(symbol), quote(symbol, price));
    }

    let (account_id, before) = {
        let mut engine = open_engine(&path, &quotes, &ids, &events);
        let account = engine
            .create_account(CreateAccountArgs {
                name: "persistent".into(),
                source: DataSource::Replay,
                initial_balance: Some("10000".into()),
                is_demo: None,
                config: serde_json::from_value(json!({
                    "slippageBps": "0", "impactBpsPerMillion": "0"
                }))
                .unwrap(),
            })
            .unwrap();
        let id = account.id.clone();
        // An open position with a stop below it, plus a resting limit that cannot fill yet.
        let opened = engine
            .place_order(request(json!({
                "accountId": id, "symbol": "BTCUSDT", "side": "BUY", "type": "MARKET",
                "quantity": "1", "leverage": 5, "clientOrderId": "open",
            })))
            .unwrap();
        assert_eq!(opened.order.status, OrderStatus::Filled);
        engine
            .set_protection(&id, "BTCUSDT", Some("95"), None)
            .unwrap();
        engine
            .place_order(request(json!({
                "accountId": id, "symbol": "ETHUSDT", "side": "BUY", "type": "LIMIT",
                "quantity": "1", "price": "1", "leverage": 3, "clientOrderId": "pending",
            })))
            .unwrap();
        let before = state(&engine, &id);
        assert_eq!(before["positions"].as_array().unwrap().len(), 1);
        assert_eq!(before["openOrders"].as_array().unwrap().len(), 2);
        (id, before)
        // The engine, and with it the connection, drops here.
    };

    let mut engine = open_engine(&path, &quotes, &ids, &events);
    assert_eq!(state(&engine, &account_id), before);

    // The stop survived the reopen and still fires on a quote through it.
    events.lock().unwrap().clear();
    let crash = quote("BTCUSDT", "94");
    quotes.lock().unwrap().insert(key("BTCUSDT"), crash.clone());
    engine.on_quote(&crash).unwrap();
    assert!(events
        .lock()
        .unwrap()
        .iter()
        .any(|e| matches!(e, EngineEvent::Fill { .. })));
    assert!(engine.position_views(&account_id).unwrap().is_empty());
    let account = engine.store().get_account(&account_id).unwrap().unwrap();
    let ledger = engine.store().list_ledger(&account_id, None).unwrap();
    assert!(audit_ledger(&account, &ledger).unwrap().is_empty());
}
