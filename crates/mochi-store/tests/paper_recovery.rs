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
use mochi_domain::{
    DataSource, LedgerEventType, Liquidity, OrderRequest, OrderSide, OrderStatus, OrderType,
    Protection, Quote,
};
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
        let stop = Protection {
            stop_loss: Some("95".into()),
            take_profit: None,
            trailing_stop: None,
        };
        engine.set_protection(&id, "BTCUSDT", &stop).unwrap();
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
    let open_orders = engine.open_orders(&account_id).unwrap();
    let stop_id = open_orders
        .iter()
        .find(|o| o.order_type == OrderType::StopMarket)
        .unwrap()
        .id
        .clone();
    let ledger_before = engine.store().list_ledger(&account_id, None).unwrap().len();
    events.lock().unwrap().clear();
    let crash = quote("BTCUSDT", "94");
    quotes.lock().unwrap().insert(key("BTCUSDT"), crash.clone());
    engine.on_quote(&crash).unwrap();
    let filled: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Fill { order, .. } => Some(order.id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(filled, [stop_id.as_str()]);
    assert!(engine.position_views(&account_id).unwrap().is_empty());
    // The ETH limit at 1 is untouched by the BTC stop.
    let still_open = engine.open_orders(&account_id).unwrap();
    assert_eq!(still_open.len(), 1);
    assert_eq!(still_open[0].client_order_id, "pending");
    assert_eq!(still_open[0].status, OrderStatus::Open);
    // Entry at 100, the stop sells 1 at the bid of 94.
    let ledger = engine.store().list_ledger(&account_id, None).unwrap();
    assert!(ledger.len() > ledger_before);
    assert!(ledger[ledger_before..].iter().any(|e| {
        e.event_type == LedgerEventType::RealizedPnl
            && e.order_id.as_deref() == Some(stop_id.as_str())
            && e.amount == "-6"
    }));
    let account = engine.store().get_account(&account_id).unwrap().unwrap();
    assert!(audit_ledger(&account, &ledger).unwrap().is_empty());
}

/// Mirrors "trailing extreme survives a restart" in `apps/server/test/recovery.test.ts`: a fresh
/// engine on the same file resumes from the stored extreme instead of restarting at the next last.
#[test]
fn trailing_extreme_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let current: Arc<Mutex<Quote>> = Arc::new(Mutex::new(
        serde_json::from_value(json!({
            "source": "REPLAY", "symbol": "BTCUSDT", "bid": "99.9", "ask": "100.1", "last": "100",
            "mark": "100", "markEstimated": false, "fundingRate": "0", "nextFundingTime": null,
            "ts": 1_000,
        }))
        .unwrap(),
    ));
    let at = |ts: i64, last: &str| -> Quote {
        let mut q = current.lock().unwrap().clone();
        (q.bid, q.ask, q.last, q.mark, q.ts) =
            (last.into(), last.into(), last.into(), last.into(), ts);
        q
    };
    let ids = Arc::new(AtomicU64::new(0));
    // Every lookup and the clock read the one current quote, as the Node test's closures do.
    let open = |path: &Path| {
        let (q, c, ids) = (current.clone(), current.clone(), ids.clone());
        PaperExecutionEngine::new(PaperEngineDeps {
            store: SqlitePaperStore::new(open_database(path).unwrap()),
            instruments: Instruments::with_defaults(),
            get_quote: Box::new(move |_, _| Some(q.lock().unwrap().clone())),
            now: Box::new(move |_| c.lock().unwrap().ts),
            new_id: Box::new(move |prefix| {
                format!("{prefix}_{}", ids.fetch_add(1, Ordering::Relaxed) + 1)
            }),
            on_event: Box::new(|_| {}),
        })
    };
    let trailing_of = |engine: &PaperExecutionEngine<SqlitePaperStore>, id: &str| {
        let views = engine.position_views(id).unwrap();
        let view = views[0].trailing_stop.clone().unwrap();
        (view.extreme.unwrap(), view.trigger_price.unwrap())
    };
    let expected = ("110".to_string(), "108.9".to_string());

    let account_id = {
        let mut first = open(&path);
        let account = first
            .create_account(CreateAccountArgs {
                name: "trail".into(),
                source: DataSource::Replay,
                initial_balance: None,
                is_demo: None,
                config: serde_json::from_value(json!({
                    "slippageBps": "0", "impactBpsPerMillion": "0"
                }))
                .unwrap(),
            })
            .unwrap();
        first
            .place_order(request(json!({
                "accountId": account.id, "clientOrderId": "open", "symbol": "BTCUSDT",
                "side": "BUY", "type": "MARKET", "quantity": "0.01", "leverage": 3,
                "trailingStop": { "kind": "RATIO", "callback": "0.01", "activationPrice": null },
            })))
            .unwrap();
        let quote = at(2_000, "110");
        *current.lock().unwrap() = quote.clone();
        first.on_quote(&quote).unwrap();
        assert_eq!(trailing_of(&first, &account.id), expected);
        account.id
        // The engine, and with it the connection, drops here.
    };

    let mut second = open(&path);
    let quote = at(3_000, "109");
    *current.lock().unwrap() = quote.clone();
    second.on_quote(&quote).unwrap();
    assert_eq!(trailing_of(&second, &account_id), expected);
    let quote = at(4_000, "108.89");
    *current.lock().unwrap() = quote.clone();
    second.on_quote(&quote).unwrap();
    assert!(second.position_views(&account_id).unwrap().is_empty());
    // Filled at that quote's bid, floored to BTCUSDT's 0.1 tick.
    let fill = &second.store().list_fills(&account_id, 10).unwrap()[0];
    assert_eq!(
        (fill.side, fill.price.as_str(), fill.liquidity),
        (OrderSide::Sell, "108.8", Liquidity::Taker)
    );
    let account = second.store().get_account(&account_id).unwrap().unwrap();
    let ledger = second.store().list_ledger(&account_id, None).unwrap();
    assert!(audit_ledger(&account, &ledger).unwrap().is_empty());
}
