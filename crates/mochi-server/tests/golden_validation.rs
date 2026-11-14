//! Request validation against `crates/golden/validation.json`, which `apps/server/golden/validation.ts`
//! records by sending each case through Fastify with the real Zod schemas. The web app shows the
//! error text verbatim, so status, message and parsed value must all match exactly.

use std::path::PathBuf;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use mochi_domain::{OrderRequest, ServerEvent};
use mochi_server::validate::{
    self, AddInstrumentInput, CandleQuery, ClosePositionInput, CreateAccountInput, DecisionQuery,
    LimitQuery, NewsQuery, NewsSourceInput, NewsSourceToggle, ProtectionInput, ReplayControl,
    Schema, SourceQuery, VaultChangeInput, VaultCreateInput, VaultPasswordInput, VaultResetInput,
};
use mochi_server::ApiError;
use serde_json::{json, Value};

fn golden() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../golden/validation.json");
    let text = std::fs::read_to_string(&path).expect("crates/golden/validation.json exists");
    serde_json::from_str(&text).expect("validation.json is JSON")
}

fn cases<'a>(golden: &'a Value, section: &str) -> &'a Vec<Value> {
    golden[section].as_array().expect("section is an array")
}

fn schema(name: &str) -> Schema {
    match name {
        "CreateAccount" => Schema::CreateAccount,
        "PlaceOrder" => Schema::PlaceOrder,
        "ClosePosition" => Schema::ClosePosition,
        "Protection" => Schema::Protection,
        "AddInstrument" => Schema::AddInstrument,
        "NewsSourceInput" => Schema::NewsSourceInput,
        "NewsSourceToggle" => Schema::NewsSourceToggle,
        "ReplayControl" => Schema::ReplayControl,
        "CandleQuery" => Schema::CandleQuery,
        "SourceQuery" => Schema::SourceQuery,
        "LimitQuery" => Schema::LimitQuery,
        "DecisionQuery" => Schema::DecisionQuery,
        "NewsQuery" => Schema::NewsQuery,
        "VaultCreate" => Schema::VaultCreate,
        "VaultPassword" => Schema::VaultPassword,
        "VaultChange" => Schema::VaultChange,
        "VaultReset" => Schema::VaultReset,
        other => panic!("unknown schema {other}"),
    }
}

/// What the client receives: the parsed value on success, `{error}` otherwise.
fn answer(result: Result<Value, ApiError>) -> (u64, Value) {
    match result {
        Ok(value) => (200, value),
        Err(error) => (
            u64::from(error.status().as_u16()),
            json!({ "error": error.message() }),
        ),
    }
}

/// Serialised, so key order counts too (Fastify writes keys in the order Zod produced them).
fn assert_answer(context: &str, case: &Value, (status, body): (u64, Value)) {
    assert_eq!(
        status,
        case["status"].as_u64().unwrap(),
        "status for {context}"
    );
    assert_eq!(
        serde_json::to_string(&body).unwrap(),
        serde_json::to_string(&case["response"]).unwrap(),
        "answer for {context}"
    );
}

fn headers(case: &Value) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in case["headers"].as_object().unwrap() {
        // Node reads header bytes as latin1, so the recorded text maps back to the same bytes.
        let bytes: Vec<u8> = value.as_str().unwrap().chars().map(|c| c as u8).collect();
        map.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_bytes(&bytes).unwrap(),
        );
    }
    map
}

fn json_headers() -> HeaderMap {
    let mut map = HeaderMap::new();
    map.insert("content-type", HeaderValue::from_static("application/json"));
    map
}

/// The typed parse must accept every input the schema accepts.
fn typed_parse_succeeds(name: &str, value: &Value) {
    let ok = match schema(name) {
        Schema::CreateAccount => {
            validate::parse_body::<CreateAccountInput>(Schema::CreateAccount, value).is_ok()
        }
        Schema::PlaceOrder => {
            validate::parse_body::<OrderRequest>(Schema::PlaceOrder, value).is_ok()
        }
        Schema::ClosePosition => {
            validate::parse_body::<ClosePositionInput>(Schema::ClosePosition, value).is_ok()
        }
        Schema::Protection => {
            validate::parse_body::<ProtectionInput>(Schema::Protection, value).is_ok()
        }
        Schema::AddInstrument => {
            validate::parse_body::<AddInstrumentInput>(Schema::AddInstrument, value).is_ok()
        }
        Schema::NewsSourceInput => {
            validate::parse_body::<NewsSourceInput>(Schema::NewsSourceInput, value).is_ok()
        }
        Schema::NewsSourceToggle => {
            validate::parse_body::<NewsSourceToggle>(Schema::NewsSourceToggle, value).is_ok()
        }
        Schema::ReplayControl => {
            validate::parse_body::<ReplayControl>(Schema::ReplayControl, value).is_ok()
        }
        Schema::VaultCreate => {
            validate::parse_body::<VaultCreateInput>(Schema::VaultCreate, value).is_ok()
        }
        Schema::VaultPassword => {
            validate::parse_body::<VaultPasswordInput>(Schema::VaultPassword, value).is_ok()
        }
        Schema::VaultChange => {
            validate::parse_body::<VaultChangeInput>(Schema::VaultChange, value).is_ok()
        }
        Schema::VaultReset => {
            validate::parse_body::<VaultResetInput>(Schema::VaultReset, value).is_ok()
        }
        _ => unreachable!(),
    };
    assert!(ok, "typed parse of {name} {value}");
}

#[test]
fn bodies_match_zod() {
    let golden = golden();
    let bodies = cases(&golden, "bodies");
    assert!(bodies.len() > 150);
    for case in bodies {
        let name = case["schema"].as_str().unwrap();
        let body = case["body"].as_str().unwrap();
        let context = format!("{name} {body}");
        let result = validate::read_body(&json_headers(), body.as_bytes())
            .and_then(|value| validate::check(schema(name), value.as_ref()));
        assert_answer(&context, case, answer(result));
        if case["status"] == 200 {
            let value = validate::read_body(&json_headers(), body.as_bytes())
                .unwrap()
                .unwrap();
            typed_parse_succeeds(name, &value);
        }
    }
}

#[test]
fn body_parsing_matches_fastify() {
    let golden = golden();
    for case in cases(&golden, "parser") {
        let route = case["route"].as_str().unwrap();
        let body = case["body"].as_str().unwrap_or("");
        let context = format!("{} {route} {} {body:?}", case["method"], case["headers"]);
        let result = validate::read_body(&headers(case), body.as_bytes()).and_then(|value| {
            if route == "none" {
                Ok(json!({ "ok": true }))
            } else {
                validate::check(schema(route), value.as_ref())
            }
        });
        assert_answer(&context, case, answer(result));
    }
}

#[test]
fn queries_match_zod() {
    let golden = golden();
    for case in cases(&golden, "queries") {
        let name = case["schema"].as_str().unwrap();
        let query = case["query"].as_str().unwrap();
        let object = validate::query_object(query);
        let result = validate::check(schema(name), Some(&object));
        assert_answer(&format!("{name} ?{query}"), case, answer(result));
        if case["status"] == 200 {
            let ok = match schema(name) {
                Schema::CandleQuery => {
                    validate::parse_query::<CandleQuery>(Schema::CandleQuery, query).is_ok()
                }
                Schema::SourceQuery => {
                    validate::parse_query::<SourceQuery>(Schema::SourceQuery, query).is_ok()
                }
                Schema::LimitQuery => {
                    validate::parse_query::<LimitQuery>(Schema::LimitQuery, query).is_ok()
                }
                Schema::DecisionQuery => {
                    validate::parse_query::<DecisionQuery>(Schema::DecisionQuery, query).is_ok()
                }
                Schema::NewsQuery => {
                    validate::parse_query::<NewsQuery>(Schema::NewsQuery, query).is_ok()
                }
                _ => unreachable!(),
            };
            assert!(ok, "typed parse of {name} ?{query}");
        }
    }
}

#[test]
fn symbol_params_match_zod() {
    let golden = golden();
    for case in cases(&golden, "symbols") {
        let param = case["param"].as_str().unwrap();
        let result = validate::parse_symbol(param).map(|symbol| json!({ "symbol": symbol }));
        assert_answer(param, case, answer(result));
    }
}

#[test]
fn server_events_serialise_as_node_does() {
    let golden = golden();
    for case in cases(&golden, "events") {
        let text = case.as_str().unwrap();
        let event: ServerEvent = serde_json::from_str(text).expect(text);
        assert_eq!(serde_json::to_string(&event).unwrap(), text);
    }
}

#[test]
fn protection_tells_absent_trailing_stop_from_null() {
    let absent: ProtectionInput = validate::parse_body(
        Schema::Protection,
        &json!({ "stopLoss": null, "takeProfit": null }),
    )
    .unwrap();
    assert_eq!(absent.trailing_stop, None);
    let removed: ProtectionInput = validate::parse_body(
        Schema::Protection,
        &json!({ "stopLoss": null, "takeProfit": null, "trailingStop": null }),
    )
    .unwrap();
    assert_eq!(removed.trailing_stop, Some(None));
}

/// The defaults Zod fills in must reach the typed structs the routes use, not only the JSON.
#[test]
fn typed_inputs_carry_zod_defaults() {
    let account: CreateAccountInput = validate::parse_body(
        Schema::CreateAccount,
        &json!({ "name": "  Main ", "source": "LIVE" }),
    )
    .unwrap();
    assert_eq!(
        account,
        CreateAccountInput {
            name: "Main".to_string(),
            source: mochi_domain::DataSource::Live,
            initial_balance: "10000".to_string(),
        }
    );
    let candles: CandleQuery = validate::parse_query(
        Schema::CandleQuery,
        "source=REPLAY&symbol=BTCUSDT&timeframe=15m",
    )
    .unwrap();
    assert_eq!(
        candles,
        CandleQuery {
            source: mochi_domain::DataSource::Replay,
            symbol: "BTCUSDT".to_string(),
            timeframe: mochi_domain::Timeframe::M15,
            limit: 300,
        }
    );
    let limit: LimitQuery = validate::parse_query(Schema::LimitQuery, "").unwrap();
    assert_eq!(limit, LimitQuery { limit: 200 });
    let limit: LimitQuery = validate::parse_query(Schema::LimitQuery, "limit=%2012%20").unwrap();
    assert_eq!(limit, LimitQuery { limit: 12 });
    let decisions: DecisionQuery = validate::parse_query(Schema::DecisionQuery, "").unwrap();
    assert_eq!(
        decisions,
        DecisionQuery {
            limit: 200,
            agent_id: None
        }
    );
    let decisions: DecisionQuery =
        validate::parse_query(Schema::DecisionQuery, "agentId=a1&limit=5").unwrap();
    assert_eq!(
        decisions,
        DecisionQuery {
            limit: 5,
            agent_id: Some("a1".to_string())
        }
    );
}

#[test]
fn place_order_reads_null_and_absent_alike() {
    let base = json!({
        "accountId": "acc-1", "clientOrderId": "cli-1", "symbol": "BTCUSDT", "side": "BUY",
        "type": "LIMIT", "quantity": "1", "leverage": 5,
    });
    let mut with_nulls = base.clone();
    for key in ["price", "stopLoss", "takeProfit", "trailingStop"] {
        with_nulls[key] = Value::Null;
    }
    let a: OrderRequest = validate::parse_body(Schema::PlaceOrder, &base).unwrap();
    let b: OrderRequest = validate::parse_body(Schema::PlaceOrder, &with_nulls).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.price, None);
    assert_eq!(a.leverage, 5);
}

#[test]
fn chunked_body_without_content_type_is_refused() {
    let mut map = HeaderMap::new();
    map.insert("transfer-encoding", HeaderValue::from_static("chunked"));
    let error = validate::read_body(&map, b"").unwrap_err();
    assert_eq!(error.status().as_u16(), 415);
    assert_eq!(error.message(), "Unsupported Media Type");
}
