//! Public OKX USDT-swap market data: a port of `OkxMarketData` (packages/market-data/src/okx.ts).
//! No API key, ever: every request is an anonymous public one. WebSocket first, REST polling
//! while it is down.
//!
//! The feed runs two tasks once started: the connection loop (connect, subscribe, read, ping,
//! reconnect with backoff) and the 5 s REST poll timer, which only polls while the socket is not
//! CONNECTED. Quotes and status changes go out on the channels from [`OkxPublicFeed::quotes`]
//! and [`OkxPublicFeed::statuses`]; quotes are only ever built from ticker data.
//!
//! The network sits behind [`Transport`], so tests can run the whole feed against an in-process
//! fake OKX under tokio's paused clock without waiting on a real socket.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::join_all;
use futures_util::{SinkExt, StreamExt};
use mochi_domain::jsnum::{js_number_string, js_string_to_number};
use mochi_domain::{
    Candle, DataSource, FeedState, FeedStatus, InstrumentSpecification, Quote, Timeframe,
    DEFAULT_SYMBOLS,
};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval_at, sleep, timeout, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{client_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream};

use crate::instrument_parse::parse_okx_instruments;

pub const REST_BASE: &str = "https://www.okx.com";
pub const WS_URL: &str = "wss://ws.okx.com/ws/v5/public";
/// The name `/api/state` shows for this feed.
pub const PROVIDER: &str = "OKX public";

/// A text "ping" keeps OKX from closing an idle socket; it answers "pong".
pub const PING_EVERY: Duration = Duration::from_secs(20);
/// No message for this long means the socket is dead even if it never closed.
pub const SILENCE_LIMIT: Duration = Duration::from_secs(30);
pub const REST_POLL_EVERY: Duration = Duration::from_secs(5);
pub const CANDLE_CACHE: Duration = Duration::from_secs(3);
pub const INSTRUMENT_CACHE: Duration = Duration::from_secs(300);
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Node's `AbortSignal.timeout(8_000)` on every REST request, body included.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
/// OKX's own page size for candles; the route allows 1000 but the feed returns at most this.
pub const MAX_CANDLES: usize = 300;
/// Quotes waiting for the runtime. When it falls this far behind the feed waits for it rather
/// than dropping quotes the paper engine needs.
const QUOTE_QUEUE: usize = 1024;

/// What fetch's `AbortSignal.timeout` rejects with.
const TIMEOUT_TEXT: &str = "The operation was aborted due to timeout";

/// OKX's bar names, which differ from MochiPrep's for hours.
pub fn bar(timeframe: Timeframe) -> &'static str {
    match timeframe {
        Timeframe::M1 => "1m",
        Timeframe::M5 => "5m",
        Timeframe::M15 => "15m",
        Timeframe::H1 => "1H",
        Timeframe::H4 => "4H",
    }
}

/// MochiPrep's name for a timeframe, as the TS cache key writes it.
fn timeframe_name(timeframe: Timeframe) -> &'static str {
    match timeframe {
        Timeframe::M1 => "1m",
        Timeframe::M5 => "5m",
        Timeframe::M15 => "15m",
        Timeframe::H1 => "1h",
        Timeframe::H4 => "4h",
    }
}

/// `BTCUSDT` -> `BTC-USDT-SWAP`.
pub fn to_inst_id(symbol: &str) -> String {
    format!(
        "{}-USDT-SWAP",
        symbol.strip_suffix("USDT").unwrap_or(symbol)
    )
}

/// The wait before reconnect attempt `attempt` (from 0): 1, 2, 4, 8, 16, then 30 s.
pub fn backoff(attempt: u32) -> Duration {
    let ms = 1000u64.saturating_mul(2u64.saturating_pow(attempt));
    Duration::from_millis(ms).min(MAX_BACKOFF)
}

// ---- parsing --------------------------------------------------------------------------------

/// JS `Number(value)` for a JSON value OKX sent; absent is NaN, as `Number(undefined)`.
fn js_number(value: Option<&Value>) -> f64 {
    match value {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => js_string_to_number(s),
        Some(_) => f64::NAN,
    }
}

/// A timestamp as a whole number of ms, or `None` where JS would hold NaN or a fraction.
fn whole_ms(x: f64) -> Option<i64> {
    (x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15).then_some(x as i64)
}

/// `parseOkxCandles`: OKX rows are newest first, `[ts,o,h,l,c,vol,volCcy,volCcyQuote,confirm]`.
/// Volume is index 6, the size in base currency; index 5 would be contracts. A row without a
/// usable time is refused rather than given one.
pub fn parse_okx_candles(rows: &[Value]) -> Result<Vec<Candle>, String> {
    let mut candles = Vec::with_capacity(rows.len());
    for row in rows {
        let cell = |i: usize| row.get(i);
        let time = whole_ms(js_number(cell(0)))
            .ok_or_else(|| "OKX sent a candle without a valid time".to_string())?;
        candles.push(Candle {
            time,
            open: js_number(cell(1)),
            high: js_number(cell(2)),
            low: js_number(cell(3)),
            close: js_number(cell(4)),
            volume: js_number(cell(6)),
            closed: cell(8).and_then(Value::as_str) == Some("1"),
        });
    }
    // Stable, like Array.prototype.sort.
    candles.sort_by_key(|candle| candle.time);
    Ok(candles)
}

/// What is known about one symbol so far, from any channel or poll. Absent fields are
/// `undefined` in Node; a present empty string is kept, as Node keeps it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Partial {
    pub bid: Option<String>,
    pub ask: Option<String>,
    pub last: Option<String>,
    pub mark: Option<String>,
    pub funding_rate: Option<String>,
    /// `Number(fundingTime)`, NaN included.
    pub next_funding_time: Option<f64>,
    /// `Number(ts)`, NaN included.
    pub ts: Option<f64>,
}

/// JS `Number.prototype.toString` for any double, NaN and infinities included.
fn js_to_string(x: f64) -> String {
    if x.is_nan() {
        "NaN".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        js_number_string(x)
    }
}

/// `buildQuote`: a quote once bid, ask, last and a timestamp are all known. Without a mark price
/// the mark is the JS-formatted bid/ask mid, flagged `markEstimated`.
pub fn build_quote(symbol: &str, p: &Partial) -> Option<Quote> {
    let text = |v: &Option<String>| v.as_ref().filter(|t| !t.is_empty()).cloned();
    let (bid, ask, last) = (text(&p.bid)?, text(&p.ask)?, text(&p.last)?);
    // `!p.ts`: 0 and NaN count as missing. A fraction has no `i64` form; OKX never sends one.
    let ts = p.ts.filter(|ts| *ts != 0.0).and_then(whole_ms)?;
    let mid = (js_string_to_number(&bid) + js_string_to_number(&ask)) / 2.0;
    Some(Quote {
        source: DataSource::Live,
        symbol: symbol.to_string(),
        mark: p.mark.clone().unwrap_or_else(|| js_to_string(mid)),
        mark_estimated: p.mark.as_deref().is_none_or(str::is_empty),
        bid,
        ask,
        last,
        funding_rate: p.funding_rate.clone(),
        // JSON.stringify writes NaN as null.
        next_funding_time: p.next_funding_time.and_then(whole_ms),
        ts,
    })
}

/// A text field of an OKX row. OKX sends every field as a string; anything else counts as absent.
fn text_field(row: &Value, key: &str) -> Option<String> {
    row.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `applyTicker`'s update: the ticker fields replace what was known; mark and funding stay.
fn with_ticker(current: Partial, row: &Value) -> Partial {
    Partial {
        bid: text_field(row, "bidPx"),
        ask: text_field(row, "askPx"),
        last: text_field(row, "last"),
        ts: Some(js_number(row.get("ts"))),
        ..current
    }
}

/// `${value}` in a JS template, for the parts of an OKX error envelope.
fn js_template(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => js_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::Object(_)) => "[object Object]".to_string(),
        Some(other) => other.to_string(),
    }
}

/// The three subscriptions per symbol.
fn channels(symbol: &str) -> Vec<Value> {
    let inst_id = to_inst_id(symbol);
    ["tickers", "mark-price", "funding-rate"]
        .iter()
        .map(|channel| json!({ "channel": channel, "instId": inst_id }))
        .collect()
}

fn unix_ms() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

// ---- transport ------------------------------------------------------------------------------

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A byte stream the WebSocket runs over: a TCP connection, or an in-memory pipe in tests.
pub trait Connection: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Connection for T {}

pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// How the feed reaches OKX. Errors are the text a failed `fetch` gives in Node.
pub trait Transport: Send + Sync {
    /// An anonymous GET: no credentials, no cookies.
    fn get(&self, url: String) -> BoxFuture<'static, Result<HttpReply, String>>;
    /// A connection to the host and port of `url`, over which the feed opens the WebSocket.
    fn dial(&self, url: String) -> BoxFuture<'static, io::Result<Box<dyn Connection>>>;
}

/// The real network, with rustls over the Mozilla root store.
pub struct NetTransport {
    client: reqwest::Client,
}

impl NetTransport {
    pub fn new(tls: Arc<rustls::ClientConfig>) -> Self {
        let client = reqwest::Client::builder()
            .use_preconfigured_tls((*tls).clone())
            // Node's fetch ignores proxy variables; so does this.
            .no_proxy()
            .build()
            .expect("an HTTP client with a preconfigured TLS config always builds");
        NetTransport { client }
    }
}

impl Transport for NetTransport {
    fn get(&self, url: String) -> BoxFuture<'static, Result<HttpReply, String>> {
        let request = self.client.get(url);
        Box::pin(async move {
            // Node's undici says only "fetch failed", and so does this: the cause can include the
            // URL, which is harmless here, but the text the web app shows stays the same.
            let response = request
                .send()
                .await
                .map_err(|_| "fetch failed".to_string())?;
            let status = response.status().as_u16();
            let body = response
                .bytes()
                .await
                .map_err(|_| "fetch failed".to_string())?;
            Ok(HttpReply {
                status,
                body: body.to_vec(),
            })
        })
    }

    fn dial(&self, url: String) -> BoxFuture<'static, io::Result<Box<dyn Connection>>> {
        Box::pin(async move {
            let url = url::Url::parse(&url)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            let host = url
                .host_str()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no host"))?
                .to_string();
            let port = url
                .port_or_known_default()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no port"))?;
            let stream = TcpStream::connect((host.as_str(), port)).await?;
            stream.set_nodelay(true)?;
            Ok(Box::new(stream) as Box<dyn Connection>)
        })
    }
}

/// rustls with the ring provider and the Mozilla roots, for both REST and the WebSocket.
pub fn tls_config() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    Arc::new(config)
}

// ---- the feed -------------------------------------------------------------------------------

type Socket = WebSocketStream<MaybeTlsStream<Box<dyn Connection>>>;

struct State {
    /// Tracked symbols in insertion order, like Node's Set.
    symbols: Vec<String>,
    partials: HashMap<String, Partial>,
    candle_cache: HashMap<String, (Instant, Vec<Candle>)>,
    instrument_cache: Option<(Instant, Vec<InstrumentSpecification>)>,
    running: bool,
    connection: FeedState,
    detail: Option<String>,
    /// Wall clock, for `status()`.
    last_message_at: Option<i64>,
    /// Monotonic, for the silence check.
    last_message: Option<Instant>,
    /// Reconnect attempts since the last successful open.
    attempts: u32,
    /// Text frames for the open socket. `None` while no socket is open.
    outbox: Option<mpsc::UnboundedSender<String>>,
    tasks: Vec<JoinHandle<()>>,
    quote_tx: Option<mpsc::Sender<Quote>>,
    status_tx: Option<mpsc::UnboundedSender<FeedStatus>>,
}

struct Shared {
    rest_base: String,
    ws_url: String,
    transport: Arc<dyn Transport>,
    tls: Arc<rustls::ClientConfig>,
    state: Mutex<State>,
}

/// The data behind a poisoned lock is still consistent: no update panics halfway.
fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Public OKX market data. Dropping it stops the feed.
pub struct OkxPublicFeed {
    shared: Arc<Shared>,
}

impl OkxPublicFeed {
    /// The real network. `rest_base` and `ws_url` are normally [`REST_BASE`] and [`WS_URL`].
    pub fn new(rest_base: &str, ws_url: &str) -> Self {
        let tls = tls_config();
        let transport = Arc::new(NetTransport::new(tls.clone()));
        Self::build(rest_base, ws_url, transport, tls)
    }

    /// Any transport; tests use an in-memory one.
    pub fn with_transport(rest_base: &str, ws_url: &str, transport: Arc<dyn Transport>) -> Self {
        Self::build(rest_base, ws_url, transport, tls_config())
    }

    fn build(
        rest_base: &str,
        ws_url: &str,
        transport: Arc<dyn Transport>,
        tls: Arc<rustls::ClientConfig>,
    ) -> Self {
        let state = State {
            symbols: DEFAULT_SYMBOLS.iter().map(|s| s.to_string()).collect(),
            partials: HashMap::new(),
            candle_cache: HashMap::new(),
            instrument_cache: None,
            running: false,
            connection: FeedState::Disconnected,
            detail: None,
            last_message_at: None,
            last_message: None,
            attempts: 0,
            outbox: None,
            tasks: Vec::new(),
            quote_tx: None,
            status_tx: None,
        };
        OkxPublicFeed {
            shared: Arc::new(Shared {
                rest_base: rest_base.to_string(),
                ws_url: ws_url.to_string(),
                transport,
                tls,
                state: Mutex::new(state),
            }),
        }
    }

    /// Every quote from now on, in arrival order (Node's `onQuote`). A second call replaces the
    /// first receiver, which then ends.
    pub fn quotes(&self) -> mpsc::Receiver<Quote> {
        let (tx, rx) = mpsc::channel(QUOTE_QUEUE);
        lock(&self.shared.state).quote_tx = Some(tx);
        rx
    }

    /// Every status change from now on (Node's `onStatus`). Unbounded: changes are rare, and
    /// each one matters to the web app's feed badge.
    pub fn statuses(&self) -> mpsc::UnboundedReceiver<FeedStatus> {
        let (tx, rx) = mpsc::unbounded_channel();
        lock(&self.shared.state).status_tx = Some(tx);
        rx
    }

    /// Connects, polls REST once, and starts the 5 s poll timer. Must run inside tokio.
    pub fn start(&self) {
        let shared = &self.shared;
        {
            let mut state = lock(&shared.state);
            if state.running {
                return;
            }
            state.running = true;
            set_connection(&mut state, FeedState::Connecting, None);
        }
        let connection = tokio::spawn(run_connection(shared.clone()));
        let poller = tokio::spawn(run_poll_timer(shared.clone()));
        lock(&shared.state).tasks = vec![connection, poller];
        tokio::spawn(poll_rest(shared.clone()));
    }

    pub fn stop(&self) {
        let mut state = lock(&self.shared.state);
        state.running = false;
        for task in state.tasks.drain(..) {
            task.abort();
        }
        // Dropping the outbox and the aborted task's socket closes the connection.
        state.outbox = None;
        set_connection(
            &mut state,
            FeedState::Disconnected,
            Some("Feed stopped".into()),
        );
    }

    pub fn track(&self, symbol: &str) {
        let running = {
            let mut state = lock(&self.shared.state);
            if state.symbols.iter().any(|s| s == symbol) {
                return;
            }
            state.symbols.push(symbol.to_string());
            send_op(&state, "subscribe", symbol);
            state.running
        };
        if running {
            tokio::spawn(poll_rest(self.shared.clone()));
        }
    }

    pub fn untrack(&self, symbol: &str) {
        let mut state = lock(&self.shared.state);
        send_op(&state, "unsubscribe", symbol);
        state.symbols.retain(|s| s != symbol);
        state.partials.remove(symbol);
    }

    /// The latest quote, or `None` until a complete ticker has arrived.
    pub fn get_quote(&self, symbol: &str) -> Option<Quote> {
        let state = lock(&self.shared.state);
        state
            .partials
            .get(symbol)
            .and_then(|partial| build_quote(symbol, partial))
    }

    pub fn status(&self) -> FeedStatus {
        status_of(&lock(&self.shared.state))
    }

    /// Everything that arrives on the socket lands here (Node's `handleMessage`). Returns the
    /// quote it makes, if any; only ticker messages make one.
    pub fn handle_message(&self, raw: &str) -> Option<Quote> {
        handle_message(&self.shared, raw)
    }

    /// One REST poll of every tracked symbol, as the timer runs it.
    pub async fn poll_rest(&self) {
        poll_rest(self.shared.clone()).await;
    }

    /// Oldest first, at most [`MAX_CANDLES`], cached for 3 s per symbol, timeframe and limit.
    pub async fn get_candles(
        &self,
        symbol: &str,
        timeframe: Timeframe,
        limit: usize,
    ) -> Result<Vec<Candle>, String> {
        let key = format!("{symbol}:{}:{limit}", timeframe_name(timeframe));
        if let Some((at, candles)) = lock(&self.shared.state).candle_cache.get(&key) {
            if at.elapsed() < CANDLE_CACHE {
                return Ok(candles.clone());
            }
        }
        let path = format!(
            "/api/v5/market/candles?instId={}&bar={}&limit={}",
            to_inst_id(symbol),
            bar(timeframe),
            limit.min(MAX_CANDLES)
        );
        let rows = get(&self.shared, &path).await?;
        let candles = parse_okx_candles(&rows)?;
        lock(&self.shared.state)
            .candle_cache
            .insert(key, (Instant::now(), candles.clone()));
        Ok(candles)
    }

    /// Everything MochiPrep could trade on OKX, cached for 5 minutes. Pass `fresh` when the
    /// result decides a real spec.
    pub async fn list_instruments(
        &self,
        fresh: bool,
    ) -> Result<Vec<InstrumentSpecification>, String> {
        if !fresh {
            if let Some((at, specs)) = &lock(&self.shared.state).instrument_cache {
                if at.elapsed() < INSTRUMENT_CACHE {
                    return Ok(specs.clone());
                }
            }
        }
        let rows = get(&self.shared, "/api/v5/public/instruments?instType=SWAP").await?;
        let specs = parse_okx_instruments(&rows).map_err(|e| e.to_string())?;
        lock(&self.shared.state).instrument_cache = Some((Instant::now(), specs.clone()));
        Ok(specs)
    }
}

impl Drop for OkxPublicFeed {
    fn drop(&mut self) {
        for task in lock(&self.shared.state).tasks.drain(..) {
            task.abort();
        }
    }
}

fn status_of(state: &State) -> FeedStatus {
    FeedStatus {
        source: DataSource::Live,
        provider: PROVIDER.to_string(),
        state: state.connection,
        last_message_at: state.last_message_at,
        detail: state.detail.clone(),
    }
}

fn set_connection(state: &mut State, connection: FeedState, detail: Option<String>) {
    state.connection = connection;
    state.detail = detail;
    if let Some(tx) = &state.status_tx {
        // A closed receiver means nobody listens any more, which is fine.
        let _ = tx.send(status_of(state));
    }
}

fn note_message(state: &mut State) {
    state.last_message_at = Some(unix_ms());
    state.last_message = Some(Instant::now());
}

/// Sends a (un)subscribe for one symbol if a socket is open; otherwise the next open's
/// subscribe covers every tracked symbol.
fn send_op(state: &State, op: &str, symbol: &str) {
    if let Some(outbox) = &state.outbox {
        let _ = outbox.send(json!({ "op": op, "args": channels(symbol) }).to_string());
    }
}

fn handle_message(shared: &Shared, raw: &str) -> Option<Quote> {
    let mut state = lock(&shared.state);
    note_message(&mut state);
    if raw == "pong" {
        return None;
    }
    let message: Value = serde_json::from_str(raw).ok()?;
    let arg = message.get("arg")?;
    let inst_id = arg.get("instId").and_then(Value::as_str)?;
    let symbol = state
        .symbols
        .iter()
        .find(|s| to_inst_id(s) == inst_id)?
        .clone();
    let row = message.get("data")?.get(0).filter(|row| row.is_object())?;
    let current = state.partials.get(&symbol).cloned().unwrap_or_default();
    match arg.get("channel").and_then(Value::as_str) {
        Some("tickers") => {
            let next = with_ticker(current, row);
            let quote = build_quote(&symbol, &next);
            state.partials.insert(symbol, next);
            quote
        }
        Some("mark-price") => {
            let next = Partial {
                mark: text_field(row, "markPx"),
                ..current
            };
            state.partials.insert(symbol, next);
            None
        }
        Some("funding-rate") => {
            let next = Partial {
                funding_rate: text_field(row, "fundingRate"),
                next_funding_time: Some(js_number(row.get("fundingTime"))),
                ..current
            };
            state.partials.insert(symbol, next);
            None
        }
        _ => None,
    }
}

/// Hands a quote to whoever listens, waiting if the queue is full.
async fn emit_quote(shared: &Shared, quote: Quote) {
    let tx = lock(&shared.state).quote_tx.clone();
    if let Some(tx) = tx {
        let _ = tx.send(quote).await;
    }
}

/// `get<T>(path)`: the `data` of OKX's `{code,msg,data}` envelope, within 8 s.
async fn get(shared: &Shared, path: &str) -> Result<Vec<Value>, String> {
    let url = format!("{}{path}", shared.rest_base);
    let reply = match timeout(HTTP_TIMEOUT, shared.transport.get(url)).await {
        Err(_) => return Err(TIMEOUT_TEXT.to_string()),
        Ok(reply) => reply?,
    };
    if !(200..300).contains(&reply.status) {
        return Err(format!("OKX HTTP {}", reply.status));
    }
    let body: Value = serde_json::from_slice(&reply.body)
        .map_err(|_| "OKX sent a response that is not JSON".to_string())?;
    if body.get("code").and_then(Value::as_str) != Some("0") {
        return Err(format!(
            "OKX error {}: {}",
            js_template(body.get("code")),
            js_template(body.get("msg"))
        ));
    }
    match body.get("data") {
        Some(Value::Array(rows)) => Ok(rows.clone()),
        _ => Err("OKX sent no data".to_string()),
    }
}

/// `pollRest`: ticker, mark price and funding for every tracked symbol, all at once. A symbol
/// whose three requests do not all succeed is skipped until the next poll.
async fn poll_rest(shared: Arc<Shared>) {
    let symbols = lock(&shared.state).symbols.clone();
    let polls = symbols.into_iter().map(|symbol| {
        let shared = shared.clone();
        async move {
            let inst_id = to_inst_id(&symbol);
            let ticker_path = format!("/api/v5/market/ticker?instId={inst_id}");
            let mark_path = format!("/api/v5/public/mark-price?instType=SWAP&instId={inst_id}");
            let funding_path = format!("/api/v5/public/funding-rate?instId={inst_id}");
            let ticker = get(&shared, &ticker_path);
            let mark = get(&shared, &mark_path);
            let funding = get(&shared, &funding_path);
            let (Ok(ticker), Ok(mark), Ok(funding)) = tokio::join!(ticker, mark, funding) else {
                return;
            };
            let quote = {
                let mut state = lock(&shared.state);
                // A pair removed while the poll was in flight stays removed.
                if !state.symbols.contains(&symbol) {
                    return;
                }
                note_message(&mut state);
                // Mark and funding are replaced, even by nothing, as Node's spread does.
                let current = Partial {
                    mark: mark.first().and_then(|row| text_field(row, "markPx")),
                    funding_rate: funding
                        .first()
                        .and_then(|row| text_field(row, "fundingRate")),
                    next_funding_time: funding.first().map(|row| js_number(row.get("fundingTime"))),
                    ..state.partials.get(&symbol).cloned().unwrap_or_default()
                };
                // Without a ticker row nothing is kept: Node only stores the merged state when it
                // applies a ticker.
                let Some(row) = ticker.first() else {
                    return;
                };
                let next = with_ticker(current, row);
                let quote = build_quote(&symbol, &next);
                state.partials.insert(symbol, next);
                quote
            };
            if let Some(quote) = quote {
                emit_quote(&shared, quote).await;
            }
        }
    });
    join_all(polls).await;
}

/// Every 5 s, a REST poll while the socket is not CONNECTED. Polls are not awaited, as Node's
/// `void this.pollRest()`: a slow one does not delay the next.
async fn run_poll_timer(shared: Arc<Shared>) {
    let mut timer = interval_at(Instant::now() + REST_POLL_EVERY, REST_POLL_EVERY);
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        timer.tick().await;
        if lock(&shared.state).connection != FeedState::Connected {
            tokio::spawn(poll_rest(shared.clone()));
        }
    }
}

/// Connect, serve the socket until it ends, wait out the backoff, and again, until stopped.
async fn run_connection(shared: Arc<Shared>) {
    loop {
        let reason = match open_socket(&shared).await {
            Ok(socket) => serve_socket(&shared, socket).await,
            Err(()) => "WebSocket error".to_string(),
        };
        let delay = {
            let mut state = lock(&shared.state);
            if !state.running {
                return;
            }
            set_connection(
                &mut state,
                FeedState::Degraded,
                Some(format!("{reason}; reconnecting, REST polling meanwhile")),
            );
            let delay = backoff(state.attempts);
            state.attempts += 1;
            delay
        };
        sleep(delay).await;
        set_connection(&mut lock(&shared.state), FeedState::Connecting, None);
    }
}

async fn open_socket(shared: &Shared) -> Result<Socket, ()> {
    let stream = shared
        .transport
        .dial(shared.ws_url.clone())
        .await
        .map_err(|_| ())?;
    let connector = Connector::Rustls(shared.tls.clone());
    let (socket, _response) =
        client_async_tls_with_config(shared.ws_url.as_str(), stream, None, Some(connector))
            .await
            .map_err(|_| ())?;
    Ok(socket)
}

/// Runs an open socket until it closes, fails or falls silent. Returns why it ended.
async fn serve_socket(shared: &Shared, mut socket: Socket) -> String {
    let (outbox, mut frames) = mpsc::unbounded_channel();
    {
        // Under one lock: a pair tracked from here on is subscribed through the outbox, and
        // every pair tracked before is in this first subscribe.
        let mut state = lock(&shared.state);
        state.attempts = 0;
        note_message(&mut state);
        let args: Vec<Value> = state.symbols.iter().flat_map(|s| channels(s)).collect();
        let _ = outbox.send(json!({ "op": "subscribe", "args": args }).to_string());
        state.outbox = Some(outbox.clone());
        set_connection(&mut state, FeedState::Connected, None);
    }
    let mut ping = interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    ping.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let reason = loop {
        tokio::select! {
            message = socket.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if let Some(quote) = handle_message(shared, text.as_str()) {
                        emit_quote(shared, quote).await;
                    }
                }
                // Node sees a binary message as a message too (and fails to parse it).
                Some(Ok(Message::Binary(_))) => note_message(&mut lock(&shared.state)),
                // Protocol pings are answered by tungstenite; Node never sees them either.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(_))) | None => break "Connection closed",
                Some(Err(_)) => break "WebSocket error",
            },
            Some(frame) = frames.recv() => {
                if socket.send(Message::text(frame)).await.is_err() {
                    break "WebSocket error";
                }
            }
            _ = ping.tick() => {
                let silent = lock(&shared.state)
                    .last_message
                    .is_some_and(|at| at.elapsed() > SILENCE_LIMIT);
                if silent {
                    // A dead peer may never take the close frame; do not wait on it for long.
                    let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                    break "Connection closed";
                }
                if socket.send(Message::text("ping")).await.is_err() {
                    break "WebSocket error";
                }
            }
        }
    };
    let mut state = lock(&shared.state);
    if state
        .outbox
        .as_ref()
        .is_some_and(|o| o.same_channel(&outbox))
    {
        state.outbox = None;
    }
    reason.to_string()
}
