//! A small in-memory OKX for tests, a port of the `FakeOkx` in `apps/server/test/okx-live.test.ts`:
//! it verifies the key and every signature, fills market orders at once at [`PRICE`], and exposes
//! the same fault knobs and recorders, snake_cased. Never used outside tests.

use std::sync::{Arc, Mutex, MutexGuard};

use indexmap::IndexMap;
use mochi_decimal::Dec;
use serde_json::{json, Value};

use crate::client::{BoxFuture, Credentials, HttpReply, HttpRequest, Row, Transport};
use crate::ids::sign;

pub const API_KEY: &str = "okx-key-abcdef123456";
pub const SECRET_KEY: &str = "okx-secret-abcdef123456";
pub const PASSPHRASE: &str = "okx-pass-abcdef123456";
/// Every fill, mark and average price.
pub const PRICE: &str = "75000";
/// `String(PRICE * 0.99)`: the trigger the fake reports for a trailing stop.
const TRAIL_TRIGGER: &str = "74250";

/// The credentials the fake accepts.
pub fn credentials() -> Credentials {
    Credentials::new(API_KEY, SECRET_KEY, PASSPHRASE)
}

/// An OKX refusal for the `reject_*` knobs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub s_code: String,
    pub s_msg: String,
}

impl Refusal {
    pub fn new(s_code: impl Into<String>, s_msg: impl Into<String>) -> Self {
        Refusal {
            s_code: s_code.into(),
            s_msg: s_msg.into(),
        }
    }
}

pub type AlgoPredicate = Box<dyn Fn(&Value) -> bool + Send>;

/// The fake's knobs and recorders, named as in Node.
pub struct FakeState {
    pub perm: String,
    pub pos_mode: String,
    pub acct_lv: String,
    /// Account equity, reported as `eq` and `availEq`.
    pub equity: String,
    /// Every request fails as an unreachable host does.
    pub down: bool,
    /// Applies the next order on the exchange, then loses the response.
    pub lose_next_response: bool,
    /// Loses the next order before it reaches the exchange.
    pub drop_next_request: bool,
    pub reject_next: Option<Refusal>,
    /// Same refusal, but the way OKX reports it with an overall success code.
    pub reject_row_next: Option<Refusal>,
    /// Makes order lookups fail with a code that does not mean "not found".
    pub lookup_error_code: Option<String>,
    pub ct_val_override: Option<String>,
    pub pepe_ct_val_override: Option<String>,
    pub leverage_calls: Vec<Value>,
    pub placed: Vec<Value>,
    /// Orders by clOrdId.
    pub orders: IndexMap<String, Row>,
    pub position: Option<Row>,
    pub simulated_headers: Vec<Option<String>>,
    /// Every signed request as `METHOD pathname`, in arrival order.
    pub calls: Vec<String>,
    /// Algo orders by algoClOrdId (or algoId when it has none), whether attached on entry, placed
    /// through the API or in the app.
    pub algos: IndexMap<String, Row>,
    pub algo_placed: Vec<Value>,
    pub canceled_algo_ids: Vec<String>,
    /// Applies the next algo order, then loses the response.
    pub lose_next_algo_response: bool,
    pub reject_next_algo: Option<Refusal>,
    /// Refuses every algo placement this matches.
    pub reject_algo_if: Option<AlgoPredicate>,
    /// Refuses every algo cancel, leaving the algos live.
    pub cancel_fails: bool,
    /// Makes algo lookups (pending list and single lookup) fail.
    pub algo_lookup_down: bool,
    algo_seq: u64,
}

impl Default for FakeState {
    fn default() -> Self {
        FakeState {
            perm: "read_only,trade".into(),
            pos_mode: "net_mode".into(),
            acct_lv: "2".into(),
            equity: "1000".into(),
            down: false,
            lose_next_response: false,
            drop_next_request: false,
            reject_next: None,
            reject_row_next: None,
            lookup_error_code: None,
            ct_val_override: None,
            pepe_ct_val_override: None,
            leverage_calls: Vec::new(),
            placed: Vec::new(),
            orders: IndexMap::new(),
            position: None,
            simulated_headers: Vec::new(),
            calls: Vec::new(),
            algos: IndexMap::new(),
            algo_placed: Vec::new(),
            canceled_algo_ids: Vec::new(),
            lose_next_algo_response: false,
            reject_next_algo: None,
            reject_algo_if: None,
            cancel_fails: false,
            algo_lookup_down: false,
            algo_seq: 0,
        }
    }
}

impl FakeState {
    /// An algo order that lives on OKX, as if placed in the OKX app. Fields override the defaults
    /// in place, as a JS object spread does.
    pub fn add_algo(&mut self, fields: Row) -> Row {
        self.algo_seq += 1;
        let mut algo = Row::new();
        algo.insert("algoId".into(), json!(format!("A{}", self.algo_seq)));
        algo.insert("algoClOrdId".into(), json!(""));
        algo.insert("instId".into(), json!("BTC-USDT-SWAP"));
        algo.insert("state".into(), json!("live"));
        algo.insert("side".into(), json!("sell"));
        algo.insert("sz".into(), json!("0.1"));
        for (key, value) in fields {
            algo.insert(key, value);
        }
        let key = match text(algo.get("algoClOrdId")) {
            Some(id) if !id.is_empty() => id,
            _ => text(algo.get("algoId")).unwrap_or_default(),
        };
        self.algos.insert(key, algo.clone());
        algo
    }
}

/// The fake exchange. Share it as an `Arc`; inspect and set knobs through [`FakeOkx::state`].
#[derive(Default)]
pub struct FakeOkx {
    state: Mutex<FakeState>,
}

impl FakeOkx {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeOkx::default())
    }

    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// See [`FakeState::add_algo`].
    pub fn add_algo(&self, fields: Row) -> Row {
        self.state().add_algo(fields)
    }

    /// A transport that plugs into `OkxClient::new`.
    pub fn transport(self: &Arc<Self>) -> Arc<dyn Transport> {
        Arc::new(FakeTransport(self.clone()))
    }

    fn handle(&self, request: &HttpRequest) -> Result<HttpReply, String> {
        let url = url::Url::parse(&request.url).map_err(|error| error.to_string())?;
        let pathname = url.path().to_string();
        let path = match url.query() {
            Some(query) if !query.is_empty() => format!("{pathname}?{query}"),
            _ => pathname.clone(),
        };
        let param = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        let body = request.body.clone().unwrap_or_default();
        let method = request.method;
        let mut state = self.state();
        if state.down {
            return Err("fetch failed".into());
        }
        if request.header("OK-ACCESS-KEY") != Some(API_KEY)
            || request.header("OK-ACCESS-PASSPHRASE") != Some(PASSPHRASE)
        {
            return Ok(answer(json!([]), "50119", "bad key"));
        }
        let timestamp = request.header("OK-ACCESS-TIMESTAMP").unwrap_or("null");
        let expected = sign(SECRET_KEY, timestamp, method, &path, &body);
        if request.header("OK-ACCESS-SIGN") != Some(expected.as_str()) {
            return Ok(answer(json!([]), "50113", "bad signature"));
        }
        let simulated = request.header("x-simulated-trading").map(str::to_string);
        state.simulated_headers.push(simulated);
        state.calls.push(format!("{method} {pathname}"));

        let ends = |suffix: &str| pathname.ends_with(suffix);
        if ends("/account/config") {
            return Ok(ok(json!([{
                "uid": "42", "perm": state.perm, "posMode": state.pos_mode, "acctLv": state.acct_lv,
            }])));
        }
        if ends("/public/instruments") {
            let inst_id = param("instId").unwrap_or_default();
            let table = match inst_id.as_str() {
                "BTC-USDT-SWAP" => Some("0.01"),
                "ETH-USDT-SWAP" => Some("0.1"),
                "SOL-USDT-SWAP" => Some("1"),
                "PEPE-USDT-SWAP" => Some("10000000"),
                _ => None,
            };
            let is_pepe = inst_id.starts_with("PEPE");
            let ct_val = state
                .ct_val_override
                .clone()
                .or_else(|| {
                    is_pepe
                        .then(|| state.pepe_ct_val_override.clone())
                        .flatten()
                })
                .or_else(|| table.map(str::to_string));
            let mut row = Row::new();
            // JSON.stringify drops an undefined ctVal.
            if let Some(ct_val) = ct_val {
                row.insert("ctVal".into(), json!(ct_val));
            }
            row.insert("lotSz".into(), json!(if is_pepe { "0.1" } else { "0.01" }));
            return Ok(ok(json!([row])));
        }
        if ends("/account/balance") {
            return Ok(ok(json!([{ "details": [{
                "ccy": "USDT", "eq": state.equity, "availEq": state.equity, "upl": "0",
            }] }])));
        }
        if ends("/account/positions") {
            let positions = match &state.position {
                Some(position) => json!([position]),
                None => json!([]),
            };
            return Ok(ok(positions));
        }
        if ends("/account/set-leverage") {
            state.leverage_calls.push(parse(&body)?);
            return Ok(ok(json!([{}])));
        }
        if ends("/trade/order") && method == "GET" {
            if let Some(code) = state.lookup_error_code.clone() {
                return Ok(answer(json!([]), &code, "Systems are busy"));
            }
            let order = param("clOrdId").and_then(|id| state.orders.get(&id).cloned());
            return Ok(match order {
                Some(order) => ok(json!([order])),
                None => answer(json!([]), "51603", "Order does not exist"),
            });
        }
        if ends("/trade/order") {
            return place(&mut state, parse(&body)?);
        }
        if ends("/trade/orders-algo-pending") {
            if state.algo_lookup_down {
                return Ok(answer(json!([]), "50001", "Systems are busy"));
            }
            let ord_type = param("ordType");
            let live: Vec<&Row> = state
                .algos
                .values()
                .filter(|algo| {
                    algo.get("state") == Some(&json!("live"))
                        && text(algo.get("ordType")) == ord_type
                })
                .collect();
            return Ok(ok(json!(live)));
        }
        if ends("/trade/order-algo") && method == "GET" {
            if state.algo_lookup_down {
                return Ok(answer(json!([]), "50001", "Systems are busy"));
            }
            let algo = param("algoClOrdId").and_then(|id| state.algos.get(&id).cloned());
            return Ok(ok(match algo {
                Some(algo) => json!([algo]),
                None => json!([]),
            }));
        }
        if ends("/trade/order-algo") {
            return place_algo(&mut state, parse(&body)?);
        }
        if ends("/trade/cancel-algos") {
            let items = parse(&body)?;
            let ids: Vec<String> = items
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| text(item.get("algoId")).unwrap_or_default())
                        .collect()
                })
                .unwrap_or_default();
            if state.cancel_fails {
                let rows: Vec<Value> = ids
                    .iter()
                    .map(|id| json!({ "algoId": id, "sCode": "51400", "sMsg": "Cancellation failed" }))
                    .collect();
                return Ok(ok(json!(rows)));
            }
            for id in &ids {
                state.canceled_algo_ids.push(id.clone());
                if let Some(algo) = state
                    .algos
                    .values_mut()
                    .find(|algo| text(algo.get("algoId")).as_deref() == Some(id.as_str()))
                {
                    algo.insert("state".into(), json!("canceled"));
                }
            }
            let rows: Vec<Value> = ids
                .iter()
                .map(|id| json!({ "algoId": id, "sCode": "0", "sMsg": "" }))
                .collect();
            return Ok(ok(json!(rows)));
        }
        Ok(answer(json!([]), "404", &format!("Unhandled {path}")))
    }
}

struct FakeTransport(Arc<FakeOkx>);

impl Transport for FakeTransport {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpReply, String>> {
        let reply = self.0.handle(&request);
        Box::pin(async move { reply })
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn parse(body: &str) -> Result<Value, String> {
    serde_json::from_str(body).map_err(|error| error.to_string())
}

fn answer(data: Value, code: &str, msg: &str) -> HttpReply {
    HttpReply {
        status: 200,
        body: json!({ "code": code, "msg": msg, "data": data })
            .to_string()
            .into_bytes(),
    }
}

fn ok(data: Value) -> HttpReply {
    answer(data, "0", "")
}

fn object(value: &Value) -> Row {
    value.as_object().cloned().unwrap_or_default()
}

/// A refusal row: `{ ...row, <id>: '' }`.
fn refusal_row(refusal: &Refusal, id_key: &str) -> Value {
    json!({ "sCode": refusal.s_code, "sMsg": refusal.s_msg, id_key: "" })
}

/// An attached element as the algo it becomes on the fill.
fn algo_fields(element: &Row) -> Row {
    let trailing = element.contains_key("callbackRatio") || element.contains_key("callbackSpread");
    let mut fields: Row = element
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "attachAlgoClOrdId" | "slOrdPx" | "tpOrdPx"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    fields.insert(
        "ordType".into(),
        json!(if trailing {
            "move_order_stop"
        } else {
            "conditional"
        }),
    );
    if trailing {
        fields.insert("moveTriggerPx".into(), json!(TRAIL_TRIGGER));
    }
    fields
}

fn place_algo(state: &mut FakeState, body: Value) -> Result<HttpReply, String> {
    state.algo_placed.push(body.clone());
    let refused = state.reject_next_algo.is_some()
        || state
            .reject_algo_if
            .as_ref()
            .is_some_and(|matches| matches(&body));
    if refused {
        let refusal = state
            .reject_next_algo
            .take()
            .unwrap_or_else(|| Refusal::new("51000", "Parameter error"));
        return Ok(ok(json!([refusal_row(&refusal, "algoId")])));
    }
    let body = object(&body);
    let ord_type = body.get("ordType").cloned().unwrap_or(Value::Null);
    let mut fields: Row = body
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "tdMode" | "reduceOnly" | "slOrdPx" | "tpOrdPx" | "ordType"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let trailing = ord_type == json!("move_order_stop");
    fields.insert("ordType".into(), ord_type);
    if trailing {
        fields.insert("moveTriggerPx".into(), json!(TRAIL_TRIGGER));
    }
    let algo = state.add_algo(fields);
    if state.lose_next_algo_response {
        state.lose_next_algo_response = false;
        return Err("socket hang up".into());
    }
    Ok(ok(json!([{
        "algoId": algo.get("algoId"), "algoClOrdId": algo.get("algoClOrdId"), "sCode": "0", "sMsg": "",
    }])))
}

/// `Number(text)`, as exact decimals; anything unreadable counts as zero, as an empty string does.
fn number(value: Option<&Value>) -> Dec {
    text(value)
        .and_then(|text| Dec::parse(text.trim()).ok())
        .unwrap_or_else(|| Dec::from_js_number(0.0))
}

fn place(state: &mut FakeState, order: Value) -> Result<HttpReply, String> {
    if state.drop_next_request {
        state.drop_next_request = false;
        return Err("socket hang up".into());
    }
    state.placed.push(order.clone());
    if let Some(refusal) = state.reject_next.take() {
        return Ok(answer(json!([refusal_row(&refusal, "ordId")]), "1", ""));
    }
    if let Some(refusal) = state.reject_row_next.take() {
        return Ok(ok(json!([refusal_row(&refusal, "ordId")])));
    }
    let order = object(&order);
    let ord_id = state.placed.len().to_string();
    let cl_ord_id = order.get("clOrdId").cloned().unwrap_or(Value::Null);
    let sz = order.get("sz").cloned().unwrap_or(Value::Null);
    let buy = order.get("side") == Some(&json!("buy"));
    let mut filled = Row::new();
    filled.insert("ordId".into(), json!(ord_id));
    filled.insert("clOrdId".into(), cl_ord_id.clone());
    filled.insert("state".into(), json!("filled"));
    filled.insert("accFillSz".into(), sz.clone());
    filled.insert("avgPx".into(), json!(PRICE));
    filled.insert("fee".into(), json!("-0.05"));
    filled.insert("pnl".into(), json!("0"));
    state
        .orders
        .insert(text(Some(&cl_ord_id)).unwrap_or_default(), filled);

    let signed = if buy {
        number(Some(&sz))
    } else {
        number(Some(&sz)).neg()
    };
    let held = number(
        state
            .position
            .as_ref()
            .and_then(|position| position.get("pos")),
    );
    let size = held.add(&signed);
    state.position = if size.is_zero() {
        None
    } else {
        Some(object(&json!({
            "instId": order.get("instId"), "pos": size.to_fixed(), "avgPx": PRICE, "markPx": PRICE,
            "upl": "0", "lever": "3", "liqPx": "50000", "margin": "10", "mgnMode": "isolated",
        })))
    };
    // Attached algos go live on the fill. They are left alone when the position closes, so the
    // engine's own cleanup is what is tested.
    let attached = order
        .get("attachAlgoOrds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for element in attached {
        let element = object(&element);
        let mut fields = algo_fields(&element);
        fields.insert(
            "algoClOrdId".into(),
            element
                .get("attachAlgoClOrdId")
                .cloned()
                .unwrap_or(json!("")),
        );
        fields.insert(
            "instId".into(),
            order.get("instId").cloned().unwrap_or(Value::Null),
        );
        fields.insert("side".into(), json!(if buy { "sell" } else { "buy" }));
        fields.insert("sz".into(), sz.clone());
        state.add_algo(fields);
    }
    if state.lose_next_response {
        state.lose_next_response = false;
        return Err("socket hang up".into());
    }
    Ok(ok(
        json!([{ "ordId": ord_id, "clOrdId": cl_ord_id, "sCode": "0", "sMsg": "" }]),
    ))
}
