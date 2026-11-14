//! The signed OKX REST client, a port of `packages/exchanges/src/okx-client.ts`: the same requests
//! byte for byte, the same reading of every answer, and the same error texts.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::ids::{iso_string, sign, to_inst_id};

pub const OKX_LIVE: &str = "https://www.okx.com";
pub const OKX_DEMO: &str = "https://openapi.okx.com";
pub const TIMEOUT_MS: u64 = 8_000;
/// Community-documented meaning: order does not exist. Anything else is treated as unknown.
pub const ORDER_NOT_FOUND: &str = "51603";
/// What fetch's `AbortSignal.timeout` rejects with.
const TIMEOUT_TEXT: &str = "The operation was aborted due to timeout";

/// The two OKX account kinds. Demo requests carry `x-simulated-trading: 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Venue {
    OkxDemo,
    OkxLive,
}

impl Venue {
    pub fn rest_base(self) -> &'static str {
        match self {
            Venue::OkxDemo => OKX_DEMO,
            Venue::OkxLive => OKX_LIVE,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Venue::OkxDemo => "OKX_DEMO",
            Venue::OkxLive => "OKX_LIVE",
        }
    }
}

/// An OKX API key. The values never show in `Debug`, and every copy is wiped when dropped.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Credentials {
    api_key: String,
    secret_key: String,
    passphrase: String,
}

impl Credentials {
    pub fn new(
        api_key: impl Into<String>,
        secret_key: impl Into<String>,
        passphrase: impl Into<String>,
    ) -> Self {
        Credentials {
            api_key: api_key.into(),
            secret_key: secret_key.into(),
            passphrase: passphrase.into(),
        }
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn secret_key(&self) -> &str {
        &self.secret_key
    }

    pub fn passphrase(&self) -> &str {
        &self.passphrase
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials { .. }")
    }
}

/// Why a call failed. The `Display` texts are Node's error messages.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OkxError {
    /// OKX answered and refused. Safe to treat as "did not happen".
    #[error("OKX {code}: {message}")]
    Rejection { code: String, message: String },
    /// No usable answer. The request may or may not have taken effect, so never blindly retry a
    /// write.
    #[error("{0}")]
    Unknown(String),
    /// Node's plain `Error`s: a request refused before sending, or an answer the client could not
    /// read at all (Node's `TypeError`). The engine treats these as neither of the above.
    #[error("{0}")]
    Invalid(String),
}

/// One OKX result row, field order kept, exactly as Node's `Record<string, string>` holds it.
pub type Row = Map<String, Value>;

/// `getContractValue`'s answer; an absent field is the empty string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractValue {
    pub ct_val: String,
    pub lot_sz: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TdMode {
    #[default]
    Isolated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrdType {
    Market,
    Limit,
}

/// One protective leg attached to an entry order. Fields are in the order the engine writes them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachAlgoOrd {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_algo_cl_ord_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sl_trigger_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sl_ord_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tp_trigger_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tp_ord_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_spread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_px: Option<String>,
}

/// `POST /trade/order`. Sizes and prices are decimal strings, as Node sends them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceOrderBody {
    pub inst_id: String,
    pub td_mode: TdMode,
    pub side: Side,
    pub ord_type: OrdType,
    pub sz: String,
    pub cl_ord_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_algo_ords: Option<Vec<AttachAlgoOrd>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlgoOrdType {
    Conditional,
    MoveOrderStop,
}

/// The algo kinds the pending list is read for. `oco` is read only: MochiPrep never places one,
/// but the OKX app does for a TP/SL pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingAlgoType {
    Conditional,
    MoveOrderStop,
    Oco,
}

impl PendingAlgoType {
    pub fn as_str(self) -> &'static str {
        match self {
            PendingAlgoType::Conditional => "conditional",
            PendingAlgoType::MoveOrderStop => "move_order_stop",
            PendingAlgoType::Oco => "oco",
        }
    }
}

/// `POST /trade/order-algo`, fields in the order the engine's `algoBody` writes them. Net mode
/// is required at connect, so there is no `posSide`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlgoOrderBody {
    pub inst_id: String,
    pub td_mode: TdMode,
    pub side: Side,
    pub algo_cl_ord_id: String,
    pub sz: String,
    /// Always true: a protective algo only ever closes.
    pub reduce_only: bool,
    pub ord_type: AlgoOrdType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sl_trigger_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sl_ord_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tp_trigger_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tp_ord_px: Option<String>,
    /// Exactly one of `callback_ratio` / `callback_spread` for `move_order_stop`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_spread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_px: Option<String>,
}

/// One entry of `POST /trade/cancel-algos`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelAlgo {
    pub inst_id: String,
    pub algo_id: String,
}

// ---- transport ------------------------------------------------------------------------------

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A signed request. The key, passphrase and signature headers are wiped on drop and never shown
/// by `Debug`.
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    /// Absent for an empty body, as Node sends none.
    pub body: Option<String>,
}

impl HttpRequest {
    /// A header by name, ignoring case as `Headers.get` does.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| *name).collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &names)
            .field("body", &self.body)
            .finish()
    }
}

impl Drop for HttpRequest {
    fn drop(&mut self) {
        for (_, value) in &mut self.headers {
            value.zeroize();
        }
    }
}

pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// How the client reaches OKX. An error is the text a failed `fetch` gives in Node.
pub trait Transport: Send + Sync {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpReply, String>>;
}

/// The real network: rustls over the Mozilla root store, no proxy, 8 s per request.
pub struct NetTransport {
    client: reqwest::Client,
}

impl NetTransport {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .use_preconfigured_tls(tls_config())
            // Node's fetch ignores proxy variables; so does this.
            .no_proxy()
            .build()
            .expect("an HTTP client with a preconfigured TLS config always builds");
        NetTransport { client }
    }
}

impl Default for NetTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for NetTransport {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpReply, String>> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .expect("the client only sends GET and POST");
        let mut builder = self.client.request(method, request.url.as_str());
        for (name, value) in &request.headers {
            builder = builder.header(*name, value.as_str());
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }
        drop(request);
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(TIMEOUT_MS);
            // Node's undici says only "fetch failed", and so does this: the cause can carry the
            // URL or a header, and the text must never carry a credential.
            let response = match tokio::time::timeout_at(deadline, builder.send()).await {
                Err(_) => return Err(TIMEOUT_TEXT.to_string()),
                Ok(Err(_)) => return Err("fetch failed".to_string()),
                Ok(Ok(response)) => response,
            };
            let status = response.status().as_u16();
            // Node reads the body under the same 8 s signal. A body that fails or times out makes
            // `response.json()` throw, which the client reports as an answer without JSON; an
            // empty body reads the same way here.
            let body = match tokio::time::timeout_at(deadline, response.bytes()).await {
                Ok(Ok(bytes)) => bytes.to_vec(),
                _ => Vec::new(),
            };
            Ok(HttpReply { status, body })
        })
    }
}

/// rustls with the ring provider and the Mozilla roots.
fn tls_config() -> rustls::ClientConfig {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

// ---- the client -----------------------------------------------------------------------------

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn unix_ms() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

pub struct OkxClient {
    credentials: Credentials,
    venue: Venue,
    base: String,
    transport: Arc<dyn Transport>,
    clock: Clock,
}

impl fmt::Debug for OkxClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OkxClient")
            .field("venue", &self.venue)
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl OkxClient {
    pub fn new(credentials: Credentials, venue: Venue, transport: Arc<dyn Transport>) -> Self {
        OkxClient {
            credentials,
            venue,
            base: venue.rest_base().to_string(),
            transport,
            clock: Arc::new(unix_ms),
        }
    }

    /// Node's `baseUrl` option: tests point the client at a fake host.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// The clock that stamps and so signs each request, in Unix milliseconds. Tests fix it.
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    pub fn venue(&self) -> Venue {
        self.venue
    }

    async fn request(
        &self,
        method: &'static str,
        path: &str,
        payload: Option<String>,
    ) -> Result<Vec<Value>, OkxError> {
        let body = payload.unwrap_or_default();
        let timestamp = iso_string((self.clock)());
        let signature = sign(
            self.credentials.secret_key(),
            &timestamp,
            method,
            path,
            &body,
        );
        let mut headers = vec![
            ("content-type", "application/json".to_string()),
            ("OK-ACCESS-KEY", self.credentials.api_key().to_string()),
            ("OK-ACCESS-SIGN", signature),
            ("OK-ACCESS-TIMESTAMP", timestamp),
            (
                "OK-ACCESS-PASSPHRASE",
                self.credentials.passphrase().to_string(),
            ),
        ];
        if self.venue == Venue::OkxDemo {
            headers.push(("x-simulated-trading", "1".to_string()));
        }
        let request = HttpRequest {
            method,
            url: format!("{}{path}", self.base),
            headers,
            body: (!body.is_empty()).then_some(body),
        };
        let reply = self
            .transport
            .send(request)
            .await
            .map_err(|message| OkxError::Unknown(format!("No answer from OKX: {message}")))?;
        read_answer(reply)
    }

    pub async fn get_config(&self) -> Result<Row, OkxError> {
        let data = self.request("GET", "/api/v5/account/config", None).await?;
        first_row(&data).ok_or_else(|| OkxError::Rejection {
            code: "config".into(),
            message: "No account configuration returned".into(),
        })
    }

    pub async fn get_contract_value(&self, symbol: &str) -> Result<ContractValue, OkxError> {
        let path = format!(
            "/api/v5/public/instruments?instType=SWAP&instId={}",
            to_inst_id(symbol)
        );
        let data = self.request("GET", &path, None).await?;
        let instrument = nullish_row(&data);
        let field = |key: &str| {
            instrument
                .as_ref()
                .and_then(|row| nullish_text(row.get(key)))
                .unwrap_or_default()
        };
        Ok(ContractValue {
            ct_val: field("ctVal"),
            lot_sz: field("lotSz"),
        })
    }

    pub async fn get_usdt_balance(&self) -> Result<Option<Row>, OkxError> {
        let data = self
            .request("GET", "/api/v5/account/balance?ccy=USDT", None)
            .await?;
        let details = nullish_row(&data).and_then(|account| match account.get("details") {
            Some(Value::Array(details)) => Some(details.clone()),
            _ => None,
        });
        Ok(details.and_then(|details| {
            details.into_iter().find_map(|detail| match detail {
                Value::Object(row) if row.get("ccy") == Some(&json!("USDT")) => Some(row),
                _ => None,
            })
        }))
    }

    pub async fn get_positions(&self) -> Result<Vec<Row>, OkxError> {
        let data = self
            .request("GET", "/api/v5/account/positions?instType=SWAP", None)
            .await?;
        rows(data)
    }

    pub async fn set_leverage(&self, inst_id: &str, lever: u32) -> Result<(), OkxError> {
        let body = json!({ "instId": inst_id, "lever": lever.to_string(), "mgnMode": "isolated" });
        self.request(
            "POST",
            "/api/v5/account/set-leverage",
            Some(body.to_string()),
        )
        .await?;
        Ok(())
    }

    /// Returns the ordId. A missing row is unknown: never resend, look it up by clOrdId.
    pub async fn place_order(&self, body: &PlaceOrderBody) -> Result<String, OkxError> {
        let data = self
            .request("POST", "/api/v5/trade/order", Some(to_json(body)))
            .await?;
        let row = first_row(&data).ok_or_else(|| {
            OkxError::Unknown("OKX acknowledged the order without a result row".into())
        })?;
        if row.get("sCode") != Some(&json!("0")) {
            return Err(OkxError::Rejection {
                code: nullish_text(row.get("sCode")).unwrap_or_else(|| "order".into()),
                message: nullish_text(row.get("sMsg")).unwrap_or_else(|| "Order refused".into()),
            });
        }
        Ok(nullish_text(row.get("ordId")).unwrap_or_default())
    }

    /// Returns the algoId. A missing row is unknown: never resend, look it up by algoClOrdId.
    pub async fn place_algo(&self, body: &AlgoOrderBody) -> Result<String, OkxError> {
        // Caught here rather than by OKX, so an engine mistake never becomes a signed real-money
        // request.
        if body.ord_type == AlgoOrdType::MoveOrderStop
            && body.callback_ratio.is_none() == body.callback_spread.is_none()
        {
            return Err(OkxError::Invalid(
                "A trailing stop needs exactly one of callbackRatio or callbackSpread".into(),
            ));
        }
        let data = self
            .request("POST", "/api/v5/trade/order-algo", Some(to_json(body)))
            .await?;
        let row = first_row(&data).ok_or_else(|| {
            OkxError::Unknown("OKX acknowledged the algo order without a result row".into())
        })?;
        if row.get("sCode") != Some(&json!("0")) {
            return Err(OkxError::Rejection {
                code: nullish_text(row.get("sCode")).unwrap_or_else(|| "algo".into()),
                message: nullish_text(row.get("sMsg"))
                    .unwrap_or_else(|| "Algo order refused".into()),
            });
        }
        Ok(nullish_text(row.get("algoId")).unwrap_or_default())
    }

    pub async fn cancel_algos(&self, items: &[CancelAlgo]) -> Result<(), OkxError> {
        if items.len() > 10 {
            return Err(OkxError::Invalid(
                "OKX cancels at most 10 algo orders per call".into(),
            ));
        }
        if items.is_empty() {
            return Ok(());
        }
        let data = self
            .request("POST", "/api/v5/trade/cancel-algos", Some(to_json(&items)))
            .await?;
        // A row without sCode is not a refusal; a present one, even null, must be "0".
        let bad = rows(data)?
            .into_iter()
            .find(|row| row.contains_key("sCode") && row.get("sCode") != Some(&json!("0")));
        match bad {
            Some(row) => Err(OkxError::Rejection {
                code: nullish_text(row.get("sCode")).unwrap_or_else(|| "algo".into()),
                message: nullish_text(row.get("sMsg"))
                    .unwrap_or_else(|| "Algo cancel refused".into()),
            }),
            None => Ok(()),
        }
    }

    pub async fn get_pending_algos(&self, ord_type: PendingAlgoType) -> Result<Vec<Row>, OkxError> {
        let path = format!(
            "/api/v5/trade/orders-algo-pending?ordType={}&instType=SWAP",
            ord_type.as_str()
        );
        rows(self.request("GET", &path, None).await?)
    }

    /// `None` only on an empty answer. The not-found code is unconfirmed, so every rejection is
    /// returned: an unknown answer must never be read as "the algo is absent".
    pub async fn get_algo(&self, algo_cl_ord_id: &str) -> Result<Option<Row>, OkxError> {
        let path = format!("/api/v5/trade/order-algo?algoClOrdId={algo_cl_ord_id}");
        Ok(nullish_row(&self.request("GET", &path, None).await?))
    }

    /// The result row is not inspected: the next lookup says what happened.
    pub async fn cancel_order(&self, inst_id: &str, cl_ord_id: &str) -> Result<(), OkxError> {
        let body = json!({ "instId": inst_id, "clOrdId": cl_ord_id });
        self.request("POST", "/api/v5/trade/cancel-order", Some(body.to_string()))
            .await?;
        Ok(())
    }

    /// `None` only when OKX positively says the order does not exist.
    pub async fn get_order(&self, inst_id: &str, cl_ord_id: &str) -> Result<Option<Row>, OkxError> {
        let path = format!("/api/v5/trade/order?instId={inst_id}&clOrdId={cl_ord_id}");
        match self.request("GET", &path, None).await {
            Ok(data) => Ok(nullish_row(&data)),
            Err(OkxError::Rejection { code, .. }) if code == ORDER_NOT_FOUND => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).expect("request bodies are plain JSON")
}

/// Node's reading of an answer, in its order: JSON first, then the status, then the code.
fn read_answer(reply: HttpReply) -> Result<Vec<Value>, OkxError> {
    let status = reply.status;
    // `response.json()` decodes UTF-8 leniently and drops a byte-order mark.
    let text = String::from_utf8_lossy(&reply.body);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let Ok(json) = serde_json::from_str::<Value>(text) else {
        return Err(OkxError::Unknown(format!(
            "OKX returned HTTP {status} without JSON"
        )));
    };
    if status >= 500 {
        return Err(OkxError::Unknown(format!("OKX returned HTTP {status}")));
    }
    if json.is_null() {
        // Node reads `json.code` on null and throws this TypeError.
        return Err(OkxError::Invalid(
            "Cannot read properties of null (reading 'code')".into(),
        ));
    }
    if json.get("code") == Some(&json!("0")) {
        return match json.get("data") {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(data)) => Ok(data.clone()),
            // Node fails later with a TypeError whose text depends on the caller.
            Some(_) => Err(OkxError::Invalid(
                "OKX returned data that is not a list".into(),
            )),
        };
    }
    // On batch-style writes the useful reason sits on the first row.
    let row = json
        .get("data")
        .and_then(Value::as_array)
        .and_then(|data| data.first());
    let row_field = |key: &str| row.and_then(|row| truthy_text(row.get(key)));
    Err(OkxError::Rejection {
        code: row_field("sCode")
            .or_else(|| truthy_text(json.get("code")))
            .unwrap_or_else(|| status.to_string()),
        message: row_field("sMsg")
            .or_else(|| truthy_text(json.get("msg")))
            .unwrap_or_else(|| "Request refused".into()),
    })
}

/// A JS value as `String(value)`, when it is one OKX could send as a field.
fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// `value || fallback`: empty strings, zero and false fall through.
pub(crate) fn truthy_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let truthy = match value {
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::Bool(flag) => *flag,
        _ => false,
    };
    if truthy {
        text_of(value)
    } else {
        None
    }
}

/// `value ?? fallback`: only absent and null fall through.
pub(crate) fn nullish_text(value: Option<&Value>) -> Option<String> {
    value.and_then(text_of)
}

fn as_row(value: &Value) -> Row {
    match value {
        Value::Object(row) => row.clone(),
        // A row that is a bare value has none of the fields any caller reads.
        _ => Row::new(),
    }
}

/// `const [row] = data; if (!row) ...`: the first row unless it is falsy.
fn first_row(data: &[Value]) -> Option<Row> {
    let first = data.first()?;
    let falsy = match first {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(text) => text.is_empty(),
        _ => false,
    };
    (!falsy).then(|| as_row(first))
}

/// `const [row] = data; row ?? null`.
fn nullish_row(data: &[Value]) -> Option<Row> {
    match data.first() {
        None | Some(Value::Null) => None,
        Some(first) => Some(as_row(first)),
    }
}

/// A list of rows. Node hands non-object rows on and fails later with a TypeError; failing here is
/// the same outcome for every caller.
fn rows(data: Vec<Value>) -> Result<Vec<Row>, OkxError> {
    data.into_iter()
        .map(|value| match value {
            Value::Object(row) => Ok(row),
            _ => Err(OkxError::Invalid(
                "OKX returned a row that is not an object".into(),
            )),
        })
        .collect()
}
