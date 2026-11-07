//! Replays `crates/golden/paper-scenarios.json` (written by `apps/server/golden/paper-scenarios.ts`
//! against the Node engine) through the Rust paper engine, and compares every op's result, error,
//! events and raw rows.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use mochi_core::ledger::audit_ledger;
use mochi_core::paper::{
    CreateAccountArgs, EngineEvent, PaperEngineDeps, PaperExecutionEngine, PaperStore,
};
use mochi_domain::registry::Instruments;
use mochi_domain::{DataSource, OrderRequest, Quote};
use mochi_store::{open_in_memory, SqlitePaperStore};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};

const TABLES: [&str; 6] = [
    "accounts",
    "orders",
    "fills",
    "positions",
    "ledger_events",
    "equity_history",
];

fn golden() -> Vec<Value> {
    let path = format!(
        "{}/../golden/paper-scenarios.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn parse<T: serde::de::DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}

/// A result as JSON; `Some` because TS records only defined results.
fn to_json<T: serde::Serialize>(value: &T) -> Option<Value> {
    Some(serde_json::to_value(value).unwrap())
}

fn source_key(source: DataSource, symbol: &str) -> String {
    format!("{source:?}:{symbol}")
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

fn snapshot(store: &SqlitePaperStore) -> Value {
    let mut rows = serde_json::Map::new();
    for table in TABLES {
        rows.insert(table.to_string(), Value::Array(raw_rows(store, table)));
    }
    Value::Object(rows)
}

/// The JSON path of the first place two values differ, with both sides, or None when equal.
fn first_difference(path: &str, expected: &Value, actual: &Value) -> Option<String> {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            let mut keys: Vec<&String> = e.keys().chain(a.keys()).collect();
            keys.sort();
            keys.dedup();
            keys.into_iter().find_map(|k| {
                let missing = Value::String("<missing>".to_string());
                first_difference(
                    &format!("{path}.{k}"),
                    e.get(k).unwrap_or(&missing),
                    a.get(k).unwrap_or(&missing),
                )
            })
        }
        (Value::Array(e), Value::Array(a)) if e.len() == a.len() => e
            .iter()
            .zip(a)
            .enumerate()
            .find_map(|(i, (e, a))| first_difference(&format!("{path}[{i}]"), e, a)),
        _ if expected == actual => None,
        _ => Some(format!("{path}: expected {expected}, got {actual}")),
    }
}

/// The engine plus the quote book, clock and event log the ops drive.
struct Harness {
    engine: PaperExecutionEngine<SqlitePaperStore>,
    quotes: Rc<RefCell<HashMap<String, Quote>>>,
    clock: Rc<RefCell<HashMap<String, i64>>>,
    events: Rc<RefCell<Vec<EngineEvent>>>,
}

impl Harness {
    fn new() -> Self {
        let quotes: Rc<RefCell<HashMap<String, Quote>>> = Rc::default();
        let clock: Rc<RefCell<HashMap<String, i64>>> = Rc::default();
        let events: Rc<RefCell<Vec<EngineEvent>>> = Rc::default();
        let counter = Rc::new(Cell::new(0u64));
        let (q, c, e) = (quotes.clone(), clock.clone(), events.clone());
        let engine = PaperExecutionEngine::new(PaperEngineDeps {
            store: SqlitePaperStore::new(open_in_memory().unwrap()),
            instruments: Instruments::with_defaults(),
            get_quote: Box::new(move |source, symbol| {
                q.borrow().get(&source_key(source, symbol)).cloned()
            }),
            now: Box::new(move |source| {
                c.borrow().get(&format!("{source:?}")).copied().unwrap_or(0)
            }),
            new_id: Box::new(move |prefix| {
                counter.set(counter.get() + 1);
                format!("{prefix}_{}", counter.get())
            }),
            on_event: Box::new(move |event| e.borrow_mut().push(event)),
        });
        Harness {
            engine,
            quotes,
            clock,
            events,
        }
    }

    /// Runs one op: Ok(None) for TS `undefined`, Err with the thrown message.
    fn apply(&mut self, op: &Value) -> Result<Option<Value>, String> {
        let text = |key: &str| op[key].as_str().unwrap().to_string();
        let engine = &mut self.engine;
        let outcome = match op["op"].as_str().unwrap() {
            "quote" => {
                assert_eq!(op["feed"], json!(false), "onQuote is not ported yet");
                let quote: Quote = parse(&op["quote"]);
                let key = source_key(quote.source, &quote.symbol);
                self.quotes.borrow_mut().insert(key, quote);
                Ok(None)
            }
            "clearQuote" => {
                let source: DataSource = parse(&op["source"]);
                let key = source_key(source, &text("symbol"));
                self.quotes.borrow_mut().remove(&key);
                Ok(None)
            }
            "now" => {
                let source: DataSource = parse(&op["source"]);
                let ts = op["ts"].as_i64().unwrap();
                self.clock.borrow_mut().insert(format!("{source:?}"), ts);
                Ok(None)
            }
            "createAccount" => engine
                .create_account(parse::<CreateAccountArgs>(&op["args"]))
                .map(|a| to_json(&a)),
            "resetAccount" => engine
                .reset_account(&text("accountId"))
                .map(|a| to_json(&a)),
            "placeOrder" => engine
                .place_order(parse::<OrderRequest>(&op["request"]))
                .map(|r| to_json(&r)),
            "cancelOrder" => engine.cancel_order(&text("orderId")).map(|()| None),
            "closePosition" => engine
                .close_position(
                    &text("accountId"),
                    &text("symbol"),
                    op["fraction"].as_f64().unwrap(),
                    &text("clientOrderId"),
                )
                .map(|r| to_json(&r)),
            "setProtection" => engine
                .set_protection(
                    &text("accountId"),
                    &text("symbol"),
                    op["stopLoss"].as_str(),
                    op["takeProfit"].as_str(),
                )
                .map(|()| None),
            other => panic!("op {other} is not ported yet"),
        };
        outcome.map_err(|e| e.to_string())
    }
}

#[test]
fn scenarios_match_typescript() {
    let scenarios = golden();
    assert!(!scenarios.is_empty());
    for scenario in &scenarios {
        let name = scenario["name"].as_str().unwrap();
        let ops = scenario["ops"].as_array().unwrap();
        let steps = scenario["steps"].as_array().unwrap();
        assert_eq!(ops.len(), steps.len(), "{name}: ops and steps differ");
        let mut harness = Harness::new();
        for (i, (op, step)) in ops.iter().zip(steps).enumerate() {
            harness.events.borrow_mut().clear();
            let outcome = harness.apply(op);
            let events = serde_json::to_value(&*harness.events.borrow()).unwrap();
            let (result, error) = match outcome {
                Ok(result) => (result, None),
                Err(message) => (None, Some(message)),
            };
            let mut actual = serde_json::Map::new();
            if let Some(result) = result {
                actual.insert("result".to_string(), result);
            }
            if let Some(error) = error {
                actual.insert("error".to_string(), Value::String(error));
            }
            actual.insert("events".to_string(), events);
            if step.get("rows").is_some() {
                actual.insert("rows".to_string(), snapshot(harness.engine.store()));
            }
            if let Some(diff) = first_difference("", step, &Value::Object(actual)) {
                panic!("{name}: op {i} ({}): {diff}", op["op"]);
            }
        }

        let store = harness.engine.store();
        let mut audit = serde_json::Map::new();
        for account in store.list_accounts().unwrap() {
            let ledger = store.list_ledger(&account.id, None).unwrap();
            let issues = audit_ledger(&account, &ledger).unwrap();
            assert!(
                issues.is_empty(),
                "{name}: audit of {}: {issues:?}",
                account.id
            );
            audit.insert(account.id.clone(), json!(issues));
        }
        if let Some(diff) = first_difference("audit", &scenario["audit"], &Value::Object(audit)) {
            panic!("{name}: {diff}");
        }
    }
}
