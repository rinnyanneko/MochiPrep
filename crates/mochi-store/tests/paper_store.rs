use std::panic::{catch_unwind, AssertUnwindSafe};

use mochi_core::paper::{PaperStore, StoreFailure};
use mochi_domain::{DataSource, EquityPoint, Fill, LedgerEvent, Order, PaperAccount, Position};
use mochi_store::{open_in_memory, SqlitePaperStore};
use rusqlite::types::Value as SqlValue;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

const TABLES: [&str; 6] = [
    "accounts",
    "orders",
    "fills",
    "positions",
    "ledger_events",
    "equity_history",
];

fn golden() -> Value {
    let path = format!("{}/../golden/store-rows.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn list<T: DeserializeOwned>(value: &Value) -> Vec<T> {
    serde_json::from_value(value.clone()).unwrap()
}

fn one<T: DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}

/// Everything the generator inserted through Node's store, inserted the same way here.
fn fill_store(store: &SqlitePaperStore, inputs: &Value) {
    for a in list::<PaperAccount>(&inputs["accounts"]) {
        store.insert_account(&a).unwrap();
    }
    for o in list::<Order>(&inputs["orders"]) {
        store.insert_order(&o).unwrap();
    }
    for f in list::<Fill>(&inputs["fills"]) {
        store.insert_fill(&f).unwrap();
    }
    for p in list::<Position>(&inputs["positions"]) {
        store.insert_position(&p).unwrap();
    }
    // Node's input has no seq (the database assigns it); the Rust type needs one, and it is ignored.
    let mut ledger = inputs["ledger"].clone();
    for event in ledger.as_array_mut().unwrap() {
        event["seq"] = json!(0);
    }
    for e in list::<LedgerEvent>(&ledger) {
        store.append_ledger(&e).unwrap();
    }
    for p in list::<EquityPoint>(&inputs["equity"]) {
        store.upsert_equity_point(&p).unwrap();
    }
    store
        .upsert_equity_point(&one(&inputs["equityUpdate"]))
        .unwrap();
}

fn js<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

fn new_store() -> SqlitePaperStore {
    SqlitePaperStore::new(open_in_memory().unwrap())
}

/// Raw rows with SQLite's own types kept, so TEXT '1' and INTEGER 1 differ.
fn raw_rows(store: &SqlitePaperStore, table: &str) -> Vec<Value> {
    let conn = store.connection();
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .unwrap();
    let names: Vec<String> = stmt.column_names().iter().map(|n| n.to_string()).collect();
    let rows = stmt
        .query_map([], |r| {
            let mut row = serde_json::Map::new();
            for (i, name) in names.iter().enumerate() {
                let v = match r.get::<_, SqlValue>(i)? {
                    SqlValue::Null => Value::Null,
                    SqlValue::Integer(n) => json!(n),
                    SqlValue::Real(x) => json!(x),
                    SqlValue::Text(s) => json!(s),
                    SqlValue::Blob(_) => panic!("unexpected blob in {table}.{name}"),
                };
                row.insert(name.clone(), v);
            }
            Ok(Value::Object(row))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn assert_tables(store: &SqlitePaperStore, expected: &Value, label: &str) {
    for table in TABLES {
        assert_eq!(
            Value::Array(raw_rows(store, table)),
            expected[table],
            "{label}: table {table}"
        );
    }
}

/// `saveAccountProfile` writes the name, initial balance and live limits only, the limits in
/// `JSON.stringify`'s text for JS numbers (`1e-7`, `0.30000000000000004`, `1e+21`).
#[test]
fn save_account_profile_writes_node_bytes() {
    let g = golden();
    let store = new_store();
    for account in g["inputs"]["accounts"].as_array().unwrap() {
        store.insert_account(&one(account)).unwrap();
    }
    for profile in g["profile"]["saves"].as_array().unwrap() {
        store.save_account_profile(&one(profile)).unwrap();
    }
    assert_eq!(
        Value::Array(raw_rows(&store, "accounts")),
        g["profile"]["accounts"]
    );
}

#[test]
fn writes_the_same_rows_as_node() {
    let g = golden();
    let store = new_store();
    fill_store(&store, &g["inputs"]);
    assert_tables(&store, &g["rows"], "after inserts");
}

#[test]
fn reads_back_what_node_reads() {
    let g = golden();
    let store = new_store();
    fill_store(&store, &g["inputs"]);
    let live = DataSource::Live;
    let replay = DataSource::Replay;
    let a = "acc-1";
    let actual = [
        ("getAccount", js(&store.get_account(a).unwrap())),
        ("getAccountReplay", js(&store.get_account("acc-2").unwrap())),
        ("listAccounts", js(&store.list_accounts().unwrap())),
        ("getOrder", js(&store.get_order("ord-3").unwrap())),
        (
            "getOrderByClientId",
            js(&store.get_order_by_client_id(a, "cli-4").unwrap()),
        ),
        ("listOpenOrders", js(&store.list_open_orders(a).unwrap())),
        (
            "listOpenOrdersForMarketLive",
            js(&store.list_open_orders_for_market(live, "BTCUSDT").unwrap()),
        ),
        (
            "listOpenOrdersForMarketReplay",
            js(&store
                .list_open_orders_for_market(replay, "BTCUSDT")
                .unwrap()),
        ),
        ("listOrdersLimit3", js(&store.list_orders(a, 3).unwrap())),
        ("listOrdersAll", js(&store.list_orders(a, 100).unwrap())),
        (
            "listFillsByOrder",
            js(&store.list_fills_by_order("ord-1").unwrap()),
        ),
        ("listFillsLimit3", js(&store.list_fills(a, 3).unwrap())),
        ("listFillsAll", js(&store.list_fills(a, 100).unwrap())),
        (
            "getOpenPosition",
            js(&store.get_open_position(a, "ETHUSDT").unwrap()),
        ),
        (
            "listOpenPositions",
            js(&store.list_open_positions(a).unwrap()),
        ),
        (
            "listOpenPositionsForMarketLive",
            js(&store
                .list_open_positions_for_market(live, "ETHUSDT")
                .unwrap()),
        ),
        (
            "listOpenPositionsForMarketReplay",
            js(&store
                .list_open_positions_for_market(replay, "ETHUSDT")
                .unwrap()),
        ),
        (
            "listClosedPositionsLimit2",
            js(&store.list_closed_positions(a, 2).unwrap()),
        ),
        (
            "listClosedPositionsAll",
            js(&store.list_closed_positions(a, 100).unwrap()),
        ),
        ("listLedger", js(&store.list_ledger(a, None).unwrap())),
        (
            "listLedgerSince",
            js(&store
                .list_ledger(a, Some(g["inputs"]["ledger"][1]["ts"].as_i64().unwrap()))
                .unwrap()),
        ),
        ("listEquityLimit3", js(&store.list_equity(a, 3).unwrap())),
        ("listEquityAll", js(&store.list_equity(a, 100).unwrap())),
    ];
    // Every key in the fixture must be checked, so a new Node read cannot go unnoticed.
    assert_eq!(actual.len(), g["reads"].as_object().unwrap().len());
    for (name, value) in actual {
        assert_eq!(value, g["reads"][name], "read {name}");
    }
}

#[test]
fn saves_write_only_their_columns() {
    let g = golden();
    let store = new_store();
    fill_store(&store, &g["inputs"]);
    let saves = &g["inputs"]["saves"];
    store.save_account(&one(&saves["account"])).unwrap();
    store.save_order(&one(&saves["order"])).unwrap();
    store.save_position(&one(&saves["position"])).unwrap();

    let after = &g["afterSaves"];
    assert_tables(&store, &after["rows"], "after saves");
    let a = "acc-1";
    assert_eq!(
        js(&store.get_account(a).unwrap()),
        after["reads"]["getAccount"]
    );
    assert_eq!(
        js(&store.get_order("ord-2").unwrap()),
        after["reads"]["getOrder"]
    );
    assert_eq!(
        js(&store.get_open_position(a, "ETHUSDT").unwrap()),
        after["reads"]["getOpenPosition"]
    );
    assert_eq!(
        js(&store.list_closed_positions(a, 100).unwrap()),
        after["reads"]["listClosedPositionsAll"]
    );
}

#[test]
fn delete_account_data_matches_node() {
    let g = golden();
    let store = new_store();
    fill_store(&store, &g["inputs"]);
    let saves = &g["inputs"]["saves"];
    store.save_account(&one(&saves["account"])).unwrap();
    store.save_order(&one(&saves["order"])).unwrap();
    store.save_position(&one(&saves["position"])).unwrap();

    store.delete_account_data("acc-2", true).unwrap();
    assert_tables(&store, &g["afterKeepingRow"], "keeping the account row");
    store.delete_account_data("acc-1", false).unwrap();
    assert_tables(&store, &g["afterDeletingRow"], "deleting the account row");
}

#[derive(Debug)]
struct TestError(String);
impl From<StoreFailure> for TestError {
    fn from(f: StoreFailure) -> Self {
        TestError(f.message)
    }
}

fn account(store: &SqlitePaperStore, id: &str) {
    let g = golden();
    let mut a: PaperAccount = list(&g["inputs"]["accounts"]).remove(0);
    a.id = id.to_string();
    store.insert_account(&a).unwrap();
}

fn account_ids(store: &SqlitePaperStore) -> Vec<String> {
    store
        .list_accounts()
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect()
}

#[test]
fn transaction_rolls_back_on_error() {
    let store = new_store();
    let result: Result<(), TestError> = store.transaction(|| {
        account(&store, "a1");
        Err(TestError("boom".into()))
    });
    // The closure's own error comes back, not a storage one.
    assert_eq!(result.unwrap_err().0, "boom");
    assert!(account_ids(&store).is_empty());
    assert!(store.connection().is_autocommit());

    let ok: Result<(), TestError> = store.transaction(|| {
        account(&store, "a1");
        Ok(())
    });
    assert!(ok.is_ok());
    assert_eq!(account_ids(&store), vec!["a1"]);
}

#[test]
fn nested_transaction_rolls_back_only_inner() {
    let store = new_store();
    let result: Result<(), TestError> = store.transaction(|| {
        account(&store, "outer");
        let inner: Result<(), TestError> = store.transaction(|| {
            account(&store, "inner");
            Err(TestError("inner fails".into()))
        });
        assert!(inner.is_err());
        Ok(())
    });
    assert!(result.is_ok());
    assert_eq!(account_ids(&store), vec!["outer"]);
    assert!(store.connection().is_autocommit());
}

/// Afterwards a plain transaction must commit durably, which proves depth was restored and no
/// transaction was left open.
fn assert_usable(store: &SqlitePaperStore) {
    assert!(store.connection().is_autocommit());
    let ok: Result<(), TestError> = store.transaction(|| {
        account(store, "after");
        Ok(())
    });
    assert!(ok.is_ok());
    assert!(store.connection().is_autocommit());
    assert!(account_ids(store).contains(&"after".to_string()));
}

#[test]
fn panic_in_transaction_rolls_back() {
    let store = new_store();
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _: Result<(), TestError> = store.transaction(|| {
            account(&store, "partial");
            panic!("closure panics");
        });
    }));
    assert!(caught.is_err());
    assert!(store.connection().is_autocommit());
    assert!(account_ids(&store).is_empty());
    assert_usable(&store);
    assert_eq!(account_ids(&store), vec!["after"]);
}

#[test]
fn panic_in_nested_transaction_rolls_back_only_inner() {
    let store = new_store();
    let result: Result<(), TestError> = store.transaction(|| {
        account(&store, "outer");
        let caught = catch_unwind(AssertUnwindSafe(|| {
            let _: Result<(), TestError> = store.transaction(|| {
                account(&store, "inner");
                panic!("inner panics");
            });
        }));
        assert!(caught.is_err());
        // The outer transaction is still open and still usable.
        assert!(!store.connection().is_autocommit());
        account(&store, "outer2");
        Ok(())
    });
    assert!(result.is_ok());
    assert_eq!(account_ids(&store), vec!["outer", "outer2"]);
    assert_usable(&store);
}

#[test]
fn failed_commit_rolls_back_and_leaves_store_usable() {
    let store = new_store();
    // A deferred foreign key is only checked at COMMIT: the child row insert succeeds, the
    // commit then fails, and the failure must not leave the transaction open.
    store
        .connection()
        .execute_batch(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY);
             CREATE TABLE child (p INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);",
        )
        .unwrap();
    let result: Result<(), TestError> = store.transaction(|| {
        account(&store, "doomed");
        store
            .connection()
            .execute("INSERT INTO child (p) VALUES (99)", [])
            .map_err(|e| TestError(e.to_string()))?;
        Ok(())
    });
    assert!(result.is_err());
    assert!(store.connection().is_autocommit());
    assert!(account_ids(&store).is_empty());
    assert_usable(&store);
}
