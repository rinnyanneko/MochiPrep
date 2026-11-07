use mochi_core::paper::{PaperStore, StoreFailure};
use mochi_domain::{EquityPoint, Fill, LedgerEvent, Order, PaperAccount, Position};
use mochi_store::{open_in_memory, SqlitePaperStore};
use rusqlite::types::Value as SqlValue;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

fn golden() -> Value {
    let path = format!("{}/../golden/store-rows.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn list<T: DeserializeOwned>(value: &Value) -> Vec<T> {
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
    let update: EquityPoint = serde_json::from_value(inputs["equityUpdate"].clone()).unwrap();
    store.upsert_equity_point(&update).unwrap();
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

#[test]
fn writes_the_same_rows_as_node() {
    let g = golden();
    let store = new_store();
    fill_store(&store, &g["inputs"]);
    for table in [
        "accounts",
        "orders",
        "fills",
        "positions",
        "ledger_events",
        "equity_history",
    ] {
        assert_eq!(
            Value::Array(raw_rows(&store, table)),
            g["rows"][table],
            "table {table}"
        );
    }
}

#[test]
fn reads_back_what_node_reads() {
    let g = golden();
    let store = new_store();
    fill_store(&store, &g["inputs"]);
    let r = &g["reads"];
    let account = "acc-1";
    assert_eq!(js(&store.get_account(account).unwrap()), r["getAccount"]);
    assert_eq!(
        js(&store.get_account("acc-2").unwrap()),
        r["getAccountMinimal"]
    );
    assert_eq!(
        js(&store.list_open_orders(account).unwrap()),
        r["listOpenOrders"]
    );
    assert_eq!(
        js(&store.get_open_position(account, "ETHUSDT").unwrap()),
        r["getOpenPosition"]
    );
    assert_eq!(
        js(&store.list_ledger(account, None).unwrap()),
        r["listLedger"]
    );
    assert_eq!(
        js(&store.list_equity(account, 10).unwrap()),
        r["listEquity"]
    );
}

#[derive(Debug)]
struct TestError(String);
impl From<StoreFailure> for TestError {
    fn from(f: StoreFailure) -> Self {
        TestError(f.0)
    }
}

fn account(store: &SqlitePaperStore, id: &str) {
    let g = golden();
    let mut a: PaperAccount = list(&g["inputs"]["accounts"]).remove(0);
    a.id = id.to_string();
    store.insert_account(&a).unwrap();
}

fn account_count(store: &SqlitePaperStore) -> usize {
    store.list_accounts().unwrap().len()
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
    assert_eq!(account_count(&store), 0);
    assert!(store.connection().is_autocommit());

    let ok: Result<(), TestError> = store.transaction(|| {
        account(&store, "a1");
        Ok(())
    });
    assert!(ok.is_ok());
    assert_eq!(account_count(&store), 1);
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
    let ids: Vec<String> = store
        .list_accounts()
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert_eq!(ids, vec!["outer"]);
    assert!(store.connection().is_autocommit());
}
