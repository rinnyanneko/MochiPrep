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
    /// Applies the next order before returning OKX's ambiguous timeout code.
    pub timeout_next_response: bool,
    pub fail_lookup_after_timeout: bool,
    /// Fails the first positions read after the next reduce-only order fills.
    pub fail_positions_after_reduce: bool,
    fail_next_positions: bool,
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
    /// Additional rows returned alongside the position managed by mock order fills.
    pub other_positions: Vec<Row>,
    pub recent_fills: Vec<Row>,
    /// `account/positions-history` rows, in any order; served newest `uTime` first.
    pub positions_history: Vec<Row>,
    pub execution_history: Vec<Row>,
    /// Makes `account/positions-history` fail.
    pub history_down: bool,
    /// Answers this many more `account/positions-history` requests, then fails them.
    pub history_answers_left: Option<usize>,
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
    /// Loses the next algo placement before applying it.
    pub drop_next_algo_request: bool,
    pub reject_next_algo: Option<Refusal>,
    /// Refuses every algo placement this matches.
    pub reject_algo_if: Option<AlgoPredicate>,
    /// Refuses every algo cancel, leaving the algos live.
    pub cancel_fails: bool,
    /// Makes algo lookups (pending list and single lookup) fail.
    pub algo_lookup_down: bool,
    /// Refuses algo detail lookups while the pending lists remain available.
    pub algo_lookup_error_code: Option<String>,
    /// Limit orders rest, live and unfilled, until [`FakeState::fill_resting`]; market orders
    /// still fill at once.
    pub rest_limit_orders: bool,
    /// Resting limit orders, as sent, oldest first.
    resting: Vec<Row>,
    /// Requests to hold at the transport, each once; see [`FakeOkx::hold_next`].
    holds: Vec<Arc<Hold>>,
    algo_seq: u64,
    trade_seq: u64,
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
            timeout_next_response: false,
            fail_lookup_after_timeout: false,
            fail_positions_after_reduce: false,
            fail_next_positions: false,
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
            other_positions: Vec::new(),
            recent_fills: Vec::new(),
            positions_history: Vec::new(),
            execution_history: Vec::new(),
            history_down: false,
            history_answers_left: None,
            simulated_headers: Vec::new(),
            calls: Vec::new(),
            algos: IndexMap::new(),
            algo_placed: Vec::new(),
            canceled_algo_ids: Vec::new(),
            lose_next_algo_response: false,
            drop_next_algo_request: false,
            reject_next_algo: None,
            reject_algo_if: None,
            cancel_fails: false,
            algo_lookup_down: false,
            algo_lookup_error_code: None,
            rest_limit_orders: false,
            resting: Vec::new(),
            holds: Vec::new(),
            algo_seq: 0,
            trade_seq: 0,
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

    /// Simulate canceling the remaining quantity after an execution, including OKX's rule
    /// that Split TP attachments only appear after a complete fill.
    pub fn cancel_resting_after_partial_fill(&mut self, contracts: &str) {
        let mut entry = self.resting.remove(0);
        entry.insert("sz".into(), json!(contracts));
        if entry
            .get("attachAlgoOrds")
            .and_then(Value::as_array)
            .is_some_and(|legs| legs.len() > 1)
        {
            entry.remove("attachAlgoOrds");
        }
        fill(self, &entry);
        let key = text(entry.get("clOrdId")).unwrap();
        self.orders
            .get_mut(&key)
            .unwrap()
            .insert("state".into(), json!("canceled"));
    }

    /// Fills every resting limit order, oldest first, as the market reaching their prices would.
    pub fn fill_resting(&mut self) {
        for order in std::mem::take(&mut self.resting) {
            fill(self, &order);
        }
    }
}

/// One request held at the transport until the test releases it, so a test can act while the
/// request is out, deterministically. The fake answers it from its state at release time.
pub struct Hold {
    method: &'static str,
    path_suffix: String,
    reached: tokio::sync::Notify,
    released: tokio::sync::Notify,
}

impl Hold {
    /// Resolves once the held request has arrived.
    pub async fn reached(&self) {
        self.reached.notified().await;
    }

    /// Lets the held request through.
    pub fn release(&self) {
        self.released.notify_one();
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

    /// Holds the next `method` request whose path ends with `path_suffix` until
    /// [`Hold::release`]. Later matching requests pass, unless held again.
    pub fn hold_next(&self, method: &'static str, path_suffix: &str) -> Arc<Hold> {
        let hold = Arc::new(Hold {
            method,
            path_suffix: path_suffix.to_string(),
            reached: tokio::sync::Notify::new(),
            released: tokio::sync::Notify::new(),
        });
        self.state().holds.push(hold.clone());
        hold
    }

    /// The hold this request meets, taken so it holds only once.
    fn take_hold(&self, request: &HttpRequest) -> Option<Arc<Hold>> {
        let path = url::Url::parse(&request.url).ok()?.path().to_string();
        let mut state = self.state();
        let at = state
            .holds
            .iter()
            .position(|h| h.method == request.method && path.ends_with(&h.path_suffix))?;
        Some(state.holds.remove(at))
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
            // (ctVal, lotSz): the built-in pairs as `default_instruments` has them, plus PEPE as
            // an added pair. Connect checks every built-in pair against this answer.
            let table = match inst_id.as_str() {
                "BTC-USDT-SWAP" => Some(("0.01", "0.01")),
                "ETH-USDT-SWAP" => Some(("0.1", "0.01")),
                "BNB-USDT-SWAP" => Some(("0.01", "1")),
                "XRP-USDT-SWAP" => Some(("100", "0.01")),
                "SOL-USDT-SWAP" => Some(("1", "0.01")),
                "TRX-USDT-SWAP" => Some(("1000", "0.01")),
                "HYPE-USDT-SWAP" => Some(("0.1", "1")),
                "ZEC-USDT-SWAP" => Some(("0.01", "1")),
                "DOGE-USDT-SWAP" => Some(("1000", "0.01")),
                "LINK-USDT-SWAP" => Some(("1", "0.1")),
                "PEPE-USDT-SWAP" => Some(("10000000", "0.1")),
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
                .or_else(|| table.map(|(ct_val, _)| ct_val.to_string()));
            let mut row = Row::new();
            // JSON.stringify drops an undefined ctVal.
            if let Some(ct_val) = ct_val {
                row.insert("ctVal".into(), json!(ct_val));
            }
            row.insert(
                "lotSz".into(),
                json!(table.map_or("0.01", |(_, lot_sz)| lot_sz)),
            );
            return Ok(ok(json!([row])));
        }
        if ends("/account/balance") {
            return Ok(ok(json!([{ "details": [{
                "ccy": "USDT", "eq": state.equity, "availEq": state.equity, "upl": "0",
            }] }])));
        }
        if ends("/trade/fills") {
            let inst_id = param("instId");
            let rows: Vec<_> = state
                .recent_fills
                .iter()
                .rev()
                .filter(|row| text(row.get("instId")) == inst_id)
                .take(crate::client::RECENT_FILLS_LIMIT)
                .collect();
            return Ok(ok(json!(rows)));
        }
        if ends("/trade/fills-history") {
            let bill = |row: &Row| text(row.get("billId")).and_then(|id| id.parse::<u64>().ok());
            let after = param("after").and_then(|id| id.parse::<u64>().ok());
            let limit = param("limit")
                .and_then(|limit| limit.parse::<usize>().ok())
                .unwrap_or(100);
            let mut rows: Vec<_> = state
                .execution_history
                .iter()
                .filter(|row| after.is_none_or(|after| bill(row).is_some_and(|id| id < after)))
                .collect();
            rows.sort_by_key(|row| std::cmp::Reverse(bill(row)));
            rows.truncate(limit);
            return Ok(ok(json!(rows)));
        }
        if ends("/account/positions-history") {
            let exhausted = match &mut state.history_answers_left {
                Some(0) => true,
                Some(left) => {
                    *left -= 1;
                    false
                }
                None => false,
            };
            if state.history_down || exhausted {
                return Ok(answer(json!([]), "50001", "Systems are busy"));
            }
            let u_time = |row: &Row| number(row.get("uTime"));
            let after = param("after").and_then(|after| Dec::parse(&after).ok());
            let limit = param("limit")
                .and_then(|limit| limit.parse::<usize>().ok())
                .unwrap_or(100);
            let mut rows: Vec<Row> = state
                .positions_history
                .iter()
                .filter(|row| after.as_ref().is_none_or(|after| u_time(row).lt(after)))
                .cloned()
                .collect();
            rows.sort_by(|a, b| u_time(b).cmp_value(&u_time(a)));
            rows.truncate(limit);
            return Ok(ok(json!(rows)));
        }
        if ends("/account/positions") {
            if std::mem::take(&mut state.fail_next_positions) {
                return Ok(answer(json!([]), "50001", "Systems are busy"));
            }
            let positions: Vec<_> = state
                .other_positions
                .iter()
                .chain(state.position.iter())
                .collect();
            return Ok(ok(json!(positions)));
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
            if let Some(code) = &state.algo_lookup_error_code {
                return Ok(answer(json!([]), code, "Algo lookup refused"));
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
        if let Some(hold) = self.0.take_hold(&request) {
            let fake = self.0.clone();
            return Box::pin(async move {
                hold.reached.notify_one();
                hold.released.notified().await;
                fake.handle(&request)
            });
        }
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
        .filter(|(key, _)| key.as_str() != "attachAlgoClOrdId")
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
    if std::mem::take(&mut state.drop_next_algo_request) {
        return Err("fetch failed".into());
    }
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
        .filter(|(key, _)| !matches!(key.as_str(), "tdMode" | "reduceOnly" | "ordType"))
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
    if let Some(legs) = order.get("attachAlgoOrds").and_then(Value::as_array) {
        let trailing = legs
            .iter()
            .any(|l| l.get("callbackRatio").is_some() || l.get("callbackSpread").is_some());
        let fixed = legs.iter().any(|l| l.get("slTriggerPx").is_some());
        if trailing && fixed {
            return Ok(answer(
                json!([refusal_row(
                    &Refusal::new(
                        "54091",
                        "You can only attach one SL when placing a trailing stop order."
                    ),
                    "ordId"
                )]),
                "1",
                "",
            ));
        }
        if legs.len() > 1
            && legs
                .iter()
                .any(|l| l.get("tpTriggerPx").is_some() && l.get("sz").is_none())
        {
            return Ok(answer(
                json!([refusal_row(
                    &Refusal::new(
                        "51089",
                        "The size of the TP order among split TPs attached cannot be empty"
                    ),
                    "ordId"
                )]),
                "1",
                "",
            ));
        }
    }
    // As OKX does, so an engine that resends a client ID or reduces a position it does not hold
    // fails a fake test instead of building a state real OKX would never reach.
    // OKX only checks a client ID against pending orders; one that filled or was canceled can
    // be reused, and then executes again.
    let cl_ord_text = text(order.get("clOrdId")).unwrap_or_default();
    let pending = state.orders.get(&cl_ord_text).is_some_and(|existing| {
        matches!(
            existing.get("state").and_then(Value::as_str),
            Some("live" | "partially_filled")
        )
    });
    if !cl_ord_text.is_empty() && pending {
        return Ok(refused("51016", "Duplicated clOrdId"));
    }
    if order.get("reduceOnly") == Some(&json!(true)) && reducible(state, &order).is_zero() {
        return Ok(refused(
            "51169",
            "Order failed because you don't have any positions in this direction for this contract.",
        ));
    }
    let mut order = object(&order);
    let ord_id = state.placed.len().to_string();
    order.insert("ordId".into(), json!(ord_id));
    let cl_ord_id = order.get("clOrdId").cloned().unwrap_or(Value::Null);
    if state.rest_limit_orders && order.get("ordType") == Some(&json!("limit")) {
        let mut live = Row::new();
        live.insert("ordId".into(), json!(ord_id));
        live.insert("clOrdId".into(), cl_ord_id.clone());
        live.insert("state".into(), json!("live"));
        live.insert("accFillSz".into(), json!("0"));
        live.insert("avgPx".into(), json!(""));
        live.insert("fee".into(), json!("0"));
        live.insert("pnl".into(), json!("0"));
        state
            .orders
            .insert(text(Some(&cl_ord_id)).unwrap_or_default(), live);
        state.resting.push(order);
    } else {
        fill(state, &order);
        if order.get("reduceOnly") == Some(&json!(true))
            && std::mem::take(&mut state.fail_positions_after_reduce)
        {
            state.fail_next_positions = true;
        }
    }
    if std::mem::take(&mut state.timeout_next_response) {
        if std::mem::take(&mut state.fail_lookup_after_timeout) {
            state.lookup_error_code = Some("50001".into());
        }
        let mut reply = answer(json!([]), "50004", "API endpoint request timeout");
        reply.status = 400;
        return Ok(reply);
    }
    if state.lose_next_response {
        state.lose_next_response = false;
        return Err("socket hang up".into());
    }
    Ok(ok(
        json!([{ "ordId": ord_id, "clOrdId": cl_ord_id, "sCode": "0", "sMsg": "" }]),
    ))
}

fn refused(code: &str, message: &str) -> HttpReply {
    answer(
        json!([refusal_row(&Refusal::new(code, message), "ordId")]),
        "1",
        "",
    )
}

/// How much of the held position `order` can reduce: zero when nothing is held on its pair, or
/// when the position is on the order's side.
fn reducible(state: &FakeState, order: &Value) -> Dec {
    let buy = order.get("side") == Some(&json!("buy"));
    let held = number(
        state
            .position
            .as_ref()
            .filter(|position| position.get("instId") == order.get("instId"))
            .and_then(|position| position.get("pos")),
    );
    if held.is_negative() == buy {
        if held.is_negative() {
            held.neg()
        } else {
            held
        }
    } else {
        Dec::from_js_number(0.0)
    }
}

/// Fills `order` (its body plus the `ordId` given on arrival) in full at [`PRICE`]. A reduce-only
/// order fills at most the position it reduces, and is canceled when there is none left.
fn fill(state: &mut FakeState, order: &Row) {
    let ord_id = order.get("ordId").cloned().unwrap_or(Value::Null);
    let cl_ord_id = order.get("clOrdId").cloned().unwrap_or(Value::Null);
    let buy = order.get("side") == Some(&json!("buy"));
    let mut sz = order.get("sz").cloned().unwrap_or(Value::Null);
    if order.get("reduceOnly") == Some(&json!(true)) {
        let available = reducible(state, &Value::Object(order.clone()));
        if available.is_zero() {
            let mut canceled = Row::new();
            canceled.insert("ordId".into(), ord_id);
            canceled.insert("clOrdId".into(), cl_ord_id.clone());
            canceled.insert("state".into(), json!("canceled"));
            canceled.insert("accFillSz".into(), json!("0"));
            canceled.insert("avgPx".into(), json!(""));
            canceled.insert("fee".into(), json!("0"));
            canceled.insert("pnl".into(), json!("0"));
            state
                .orders
                .insert(text(Some(&cl_ord_id)).unwrap_or_default(), canceled);
            return;
        }
        if number(Some(&sz)).gt(&available) {
            sz = json!(available.to_fixed());
        }
    }
    let mut filled = Row::new();
    state.trade_seq += 1;
    let trade_id = state.trade_seq.to_string();
    filled.insert("tradeId".into(), json!(trade_id));
    filled.insert("ordId".into(), ord_id.clone());
    filled.insert("clOrdId".into(), cl_ord_id.clone());
    filled.insert("state".into(), json!("filled"));
    filled.insert("fillTime".into(), json!("1800000000000"));
    filled.insert("fillSz".into(), sz.clone());
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
    let subtype = match (
        buy,
        held.is_zero() || held.is_negative() == signed.is_negative(),
    ) {
        (true, true) => "3",
        (false, true) => "4",
        (false, false) => "5",
        (true, false) => "6",
    };
    state.recent_fills.push(object(&json!({
        "instId": order.get("instId"), "ordId": ord_id, "tradeId": trade_id,
        "clOrdId": cl_ord_id,
        "fillSz": sz, "fillTime": "1800000000000", "ts": "1800000000001",
        "posSide": "net", "subType": subtype,
    })));
    let size = held.add(&signed);
    state.position = if size.is_zero() {
        None
    } else {
        Some(object(&json!({
            "instId": order.get("instId"), "pos": size.to_fixed(), "avgPx": PRICE, "markPx": PRICE,
            "upl": "0", "lever": "3", "liqPx": "50000", "margin": "10", "mgnMode": "isolated", "posSide": "net", "tradeId": trade_id, "uTime": "1800000000000", "cTime": "1800000000000",
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
}
