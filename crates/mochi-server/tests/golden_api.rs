//! The paper, market, replay, state and instrument routes, and the read-only GETs of the later
//! plans' areas, against `crates/golden/api-script.json`: a session
//! `apps/server/golden/api-script.ts` recorded from the Node app over a database seeded with an
//! agent, a provider, decisions and exchange accounts. The same requests go through the Rust app,
//! which must answer with the same status and the same body bytes, once random ids and
//! wall-clock times are normalised the same way on both sides.
//!
//! The tests after it cover what the script cannot reach through HTTP alone: an account that
//! agents use, and exchange accounts, which this backend cannot trade yet.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use mochi_domain::InstrumentSpecification;
use mochi_server::config::{config_from, Config};
use mochi_server::{app, AppState, RuntimeHandle, RuntimeOptions};
use regex::{Captures, Regex};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};
use tower::ServiceExt;

const HOST: &str = "localhost:8787";

fn fixture() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../golden/api-script.json");
    let text = std::fs::read_to_string(&path).expect("crates/golden/api-script.json exists");
    serde_json::from_str(&text).expect("api-script.json is JSON")
}

/// `ApiNormaliser` in `apps/server/golden/api-script.ts`, line for line. Its parameters come
/// from the fixture, which records what Node normalised.
struct Normaliser {
    random_id: Regex,
    named_id: Regex,
    time_field: Regex,
    wall_clock_after: i64,
    wall_clock: String,
    names: HashMap<String, String>,
    ids: HashMap<String, String>,
    counts: HashMap<String, u32>,
}

impl Normaliser {
    fn new(normalised: &Value) -> Self {
        let words = |list: &Value| {
            list.as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect::<Vec<_>>()
                .join("|")
        };
        let prefixes = words(&normalised["ids"]["prefixes"]);
        let fields = words(&normalised["wallClock"]["fields"]);
        let uuid = "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}";
        Normaliser {
            random_id: Regex::new(&format!(r"\b({prefixes})_{uuid}\b")).unwrap(),
            named_id: Regex::new(&format!(r"\b({prefixes})_\d+\b")).unwrap(),
            time_field: Regex::new(&format!(r#""({fields})":(\d+)"#)).unwrap(),
            wall_clock_after: normalised["wallClock"]["after"].as_i64().unwrap(),
            wall_clock: normalised["wallClock"]["as"].as_str().unwrap().to_string(),
            names: HashMap::new(),
            ids: HashMap::new(),
            counts: HashMap::new(),
        }
    }

    fn normalise(&mut self, text: &str) -> String {
        let random_id = self.random_id.clone();
        let named = random_id.replace_all(text, |caps: &Captures| {
            let id = caps[0].to_string();
            if let Some(name) = self.names.get(&id) {
                return name.clone();
            }
            let prefix = caps[1].to_string();
            let n = self.counts.entry(prefix.clone()).or_insert(0);
            *n += 1;
            let name = format!("{prefix}_{n}");
            self.names.insert(id.clone(), name.clone());
            self.ids.insert(name.clone(), id);
            name
        });
        self.time_field
            .replace_all(&named, |caps: &Captures| {
                let value: i64 = caps[2].parse().unwrap();
                if value > self.wall_clock_after {
                    format!(r#""{}":"{}""#, &caps[1], self.wall_clock)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned()
    }

    fn resolve(&self, text: &str) -> String {
        self.named_id
            .replace_all(text, |caps: &Captures| {
                self.ids
                    .get(&caps[0])
                    .cloned()
                    .unwrap_or_else(|| caps[0].to_string())
            })
            .into_owned()
    }
}

fn config() -> Config {
    let mut config = config_from(|_| None);
    config.data_dir = PathBuf::from(".");
    config
}

/// A JSON value as Node's `node:sqlite` read it back, for inserting the same row here.
fn sql_value(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Number(n) => match n.as_i64() {
            Some(n) => SqlValue::Integer(n),
            None => SqlValue::Real(n.as_f64().unwrap()),
        },
        Value::String(text) => SqlValue::Text(text.clone()),
        other => panic!("node:sqlite never returns {other}"),
    }
}

/// The rows Node's script wrote before its server started (the tables of later plans), inserted
/// as they were read back. The columns' affinity stores each value as Node's insert did.
fn insert_seed(db: &rusqlite::Connection, seed: &Value) {
    for table in seed.as_array().unwrap() {
        let name = table["table"].as_str().unwrap();
        for row in table["rows"].as_array().unwrap() {
            let row = row.as_object().unwrap();
            let columns: Vec<&str> = row.keys().map(String::as_str).collect();
            let sql = format!(
                "INSERT INTO {name} ({}) VALUES ({})",
                columns.join(","),
                vec!["?"; columns.len()].join(",")
            );
            db.execute(
                &sql,
                rusqlite::params_from_iter(row.values().map(sql_value)),
            )
            .unwrap();
        }
    }
}

/// The runtime Node's script ran against: a fresh database with the fixture's added pairs and
/// seeded rows, the same tape length, and no live feed. Starting it seeds the mock provider and
/// pauses the seeded agent, as Node's constructor did.
fn state(fixture: &Value) -> AppState {
    let db = mochi_store::open_in_memory().unwrap();
    for spec in fixture["addedInstruments"].as_array().unwrap() {
        let spec: InstrumentSpecification = serde_json::from_value(spec.clone()).unwrap();
        mochi_store::save_instrument(&db, &spec).unwrap();
    }
    insert_seed(&db, &fixture["seed"]);
    let mut options = RuntimeOptions::new(db);
    options.replay_minutes = Some(fixture["replayMinutes"].as_u64().unwrap() as usize);
    AppState::new(Arc::new(config()), RuntimeHandle::start(options).unwrap())
}

fn request(state: &AppState, normaliser: &Normaliser, step: &Value) -> Request<Body> {
    let path = normaliser.resolve(step["path"].as_str().unwrap());
    let mut builder = Request::builder()
        .method(step["method"].as_str().unwrap())
        .uri(path)
        .header("host", HOST)
        .header("authorization", format!("Bearer {}", state.token));
    // Exactly the body headers Node saw, content-length included.
    for (name, value) in step["headers"].as_object().unwrap() {
        builder = builder.header(name.as_str(), value.as_str().unwrap());
    }
    let body = match (&step["body"], &step["repeat"]) {
        (Value::String(body), _) => normaliser.resolve(body),
        (_, Value::Object(repeat)) => repeat["char"]
            .as_str()
            .unwrap()
            .repeat(repeat["length"].as_u64().unwrap() as usize),
        _ => String::new(),
    };
    builder.body(Body::from(body)).unwrap()
}

#[tokio::test]
async fn the_recorded_node_session_replays_byte_for_byte() {
    let fixture = fixture();
    let state = state(&fixture);
    let mut normaliser = Normaliser::new(&fixture["normalised"]);
    let steps = fixture["steps"].as_array().unwrap();
    assert!(steps.len() > 100);
    for (i, step) in steps.iter().enumerate() {
        let context = format!("step {i}: {} {}", step["method"], step["path"]);
        let response = app(state.clone())
            .oneshot(request(&state, &normaliser, step))
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = normaliser.normalise(std::str::from_utf8(&bytes).unwrap());
        assert_eq!(
            (status, text.as_str()),
            (
                step["status"].as_u64().unwrap() as u16,
                step["response"].as_str().unwrap()
            ),
            "{context}"
        );
    }
    state.runtime.shutdown().await;
}

// ---- paths the Node script cannot reach ------------------------------------------------------

/// Sends one request with the session token; returns the status and the parsed body.
async fn send(state: &AppState, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", HOST)
        .header("authorization", format!("Bearer {}", state.token));
    let body = match body {
        Some(body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = app(state.clone())
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn short_tape_state() -> AppState {
    let mut options = RuntimeOptions::new(mochi_store::open_in_memory().unwrap());
    options.replay_minutes = Some(mochi_market::tape::WARMUP_MINUTES + 100);
    AppState::new(Arc::new(config()), RuntimeHandle::start(options).unwrap())
}

/// Runs one statement on the runtime's database, with `?1` bound to `account_id`.
async fn sql(state: &AppState, statement: &'static str, account_id: &str) {
    let account_id = account_id.to_string();
    state
        .runtime
        .call(move |runtime| {
            runtime
                .connection()
                .execute(statement, rusqlite::params_from_iter([account_id]))
                .unwrap()
        })
        .await
        .unwrap();
}

async fn new_account(state: &AppState) -> String {
    let (status, account) = send(
        state,
        "POST",
        "/api/accounts",
        Some(json!({ "name": "Main", "source": "REPLAY" })),
    )
    .await;
    assert_eq!(status, 200);
    account["id"].as_str().unwrap().to_string()
}

fn not_available() -> (u16, Value) {
    (
        501,
        json!({ "error": "Not available in the Rust backend yet" }),
    )
}

#[tokio::test]
async fn an_account_that_agents_use_cannot_be_deleted() {
    let state = short_tape_state();
    let id = new_account(&state).await;
    // The runtime seeded the 'mock' provider when it started.
    sql(
        &state,
        "INSERT INTO agents (id,name,provider_id,model,symbol,timeframe,strategy,account_id,risk,
         interval_sec,status,running,created_at)
         VALUES ('agent_1','A','mock','m','BTCUSDT','15m','',?1,'{}',900,'IDLE',0,1)",
        &id,
    )
    .await;
    let answer = send(&state, "DELETE", &format!("/api/accounts/{id}"), None).await;
    assert_eq!(
        answer,
        (
            400,
            json!({ "error": "Delete the agents using this account first" })
        )
    );
    let (status, _) = send(&state, "GET", &format!("/api/accounts/{id}/orders"), None).await;
    assert_eq!(status, 200, "the account is still there");
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn exchange_accounts_read_but_do_not_trade_yet() {
    let state = short_tape_state();
    let id = new_account(&state).await;
    let order = |client: &str, kind: &str, price: Value| {
        json!({
            "accountId": id, "clientOrderId": client, "symbol": "BTCUSDT", "side": "BUY",
            "type": kind, "quantity": "0.01", "price": price, "leverage": 5,
        })
    };
    // An open position and a resting order, then the account becomes an exchange account.
    let (status, filled) = send(
        &state,
        "POST",
        "/api/orders",
        Some(order("a", "MARKET", Value::Null)),
    )
    .await;
    assert_eq!(
        (status, filled["order"]["status"].as_str()),
        (200, Some("FILLED"))
    );
    let (_, resting) = send(
        &state,
        "POST",
        "/api/orders",
        Some(order("b", "LIMIT", json!("1000"))),
    )
    .await;
    assert_eq!(resting["order"]["status"], "OPEN");
    // 15 tape-minutes: one equity point while it is still a paper account.
    send(&state, "POST", "/api/replay", Some(json!({ "steps": 15 }))).await;
    sql(
        &state,
        "UPDATE accounts SET venue='OKX_DEMO' WHERE id=?1",
        &id,
    )
    .await;

    let resting_id = resting["order"]["id"].as_str().unwrap();
    for (method, path, body) in [
        (
            "POST",
            "/api/orders".to_string(),
            Some(order("c", "MARKET", Value::Null)),
        ),
        ("DELETE", format!("/api/orders/{resting_id}"), None),
        (
            "POST",
            format!("/api/positions/{id}/BTCUSDT/close"),
            Some(json!({ "fraction": 1, "clientOrderId": "d" })),
        ),
        (
            "PUT",
            format!("/api/positions/{id}/BTCUSDT/protection"),
            Some(json!({ "stopLoss": null, "takeProfit": null })),
        ),
        ("DELETE", format!("/api/accounts/{id}"), None),
    ] {
        let answer = send(&state, method, &path, body).await;
        assert_eq!(answer, not_available(), "{method} {path}");
    }

    // Node's read models for a disconnected exchange account: no positions, no local audit,
    // and the balance of its last equity point.
    let (_, positions) = send(
        &state,
        "GET",
        &format!("/api/accounts/{id}/positions"),
        None,
    )
    .await;
    assert_eq!(positions, json!([]));
    let (_, audit) = send(&state, "GET", &format!("/api/accounts/{id}/audit"), None).await;
    assert_eq!(
        audit,
        json!({ "consistent": true, "issues": [], "notApplicable": true })
    );
    let (_, equity) = send(&state, "GET", &format!("/api/accounts/{id}/equity"), None).await;
    let point = equity.as_array().unwrap().last().unwrap().clone();
    let (_, accounts) = send(&state, "GET", "/api/accounts", None).await;
    let balance = &accounts[0]["balance"];
    assert_eq!(balance["equity"], point["equity"]);
    assert_eq!(balance["walletBalance"], point["wallet"]);
    assert_eq!(balance["unrealizedPnl"], point["unrealized"]);
    assert_eq!(balance["availableBalance"], "0");
    assert_eq!(balance["usedMargin"], "0");
    // The engine's own refusal, as in Node.
    let answer = send(&state, "POST", &format!("/api/accounts/{id}/reset"), None).await;
    assert_eq!(
        answer,
        (400, json!({ "error": "Exchange accounts cannot be reset" }))
    );
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn an_exchange_account_without_equity_history_shows_its_initial_balance() {
    let state = short_tape_state();
    let id = new_account(&state).await;
    sql(
        &state,
        "UPDATE accounts SET venue='OKX_LIVE' WHERE id=?1",
        &id,
    )
    .await;
    sql(
        &state,
        "DELETE FROM equity_history WHERE account_id=?1",
        &id,
    )
    .await;
    let (_, accounts) = send(&state, "GET", "/api/accounts", None).await;
    assert_eq!(
        accounts[0]["balance"],
        json!({
            "accountId": id, "initialBalance": "10000", "walletBalance": "10000",
            "availableBalance": "0", "usedMargin": "0", "orderMargin": "0", "equity": "10000",
            "realizedPnl": "0", "unrealizedPnl": "0", "feesPaid": "0", "fundingPaid": "0",
        })
    );
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn live_candles_wait_for_the_okx_adapter() {
    let state = short_tape_state();
    let answer = send(
        &state,
        "GET",
        "/api/market/candles?source=LIVE&symbol=BTCUSDT&timeframe=1m",
        None,
    )
    .await;
    assert_eq!(answer, not_available());
    state.runtime.shutdown().await;
}
