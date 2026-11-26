//! OKX in the runtime, ported from `apps/server/test/okx-live.test.ts`: connecting through every
//! check, arming, the sync loop, agents on exchange accounts, restart, and the OKX half of the
//! master password. Everything runs against the in-process fake OKX on a real database file, so
//! the file itself can be searched for credentials.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use mochi_ai::{AiProvider, AiRequest, AiResponse, CompletionFuture, ProviderKind};
use mochi_core::paper::{CreateAccountArgs, PaperStore};
use mochi_domain::{
    default_risk_settings, Candle, DataSource, FeedState, FeedStatus, OrderRequest, PaperAccount,
    Quote, ServerEvent, Timeframe, ToastLevel, Topic, VaultStatus,
};
use mochi_okx::fake::{FakeOkx, API_KEY, PASSPHRASE, SECRET_KEY};
use mochi_server::config::{config_from, Config};
use mochi_server::runtime::agents::CycleTrigger;
use mochi_server::runtime::{live_feed::FeedResult, LiveFeed, OkxOptions, ARM_PHRASE};
use mochi_server::validate::{self, AgentInput, OkxConnectInput, Schema};
use mochi_server::{app, ApiError, AppState, RuntimeHandle, RuntimeOptions};
use mochi_vault::KdfParams;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::broadcast;
use tower::ServiceExt;

const HOST: &str = "localhost:8787";
const CREDS: [&str; 3] = [API_KEY, SECRET_KEY, PASSPHRASE];
const PASSWORD: &str = "mochi-master-password";
const FAST: KdfParams = KdfParams {
    n: 1024,
    r: 8,
    p: 1,
};

fn wall_clock() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Node's `FakeFeed`: bid 74999, ask 75001, last 75000, stamped now, and 60 closed 15m bars.
struct Feed;

impl LiveFeed for Feed {
    fn track(&self, _: &str) {}
    fn untrack(&self, _: &str) {}
    fn get_quote(&self, symbol: &str) -> Option<Quote> {
        Some(Quote {
            source: DataSource::Live,
            symbol: symbol.into(),
            bid: "74999".into(),
            ask: "75001".into(),
            last: "75000".into(),
            mark: "75000".into(),
            mark_estimated: false,
            funding_rate: None,
            next_funding_time: None,
            ts: wall_clock(),
        })
    }
    fn status(&self) -> FeedStatus {
        FeedStatus {
            source: DataSource::Live,
            provider: "fixture".into(),
            state: FeedState::Connected,
            last_message_at: None,
            detail: None,
        }
    }
    fn candles<'a>(&'a self, _: &'a str, _: Timeframe, _: usize) -> FeedResult<'a, Vec<Candle>> {
        let size = 900_000;
        let last = (wall_clock() / size) * size - size;
        let bars = (0..60)
            .map(|i| Candle {
                time: last - (59 - i) * size,
                open: 75_000.0,
                high: 75_050.0,
                low: 74_950.0,
                close: 75_000.0,
                volume: 1.0,
                closed: true,
            })
            .collect();
        Box::pin(async move { Ok(bars) })
    }
}

/// Node's `longDecision`, recording what the model is shown. The answer can be swapped.
struct Model {
    seen: Arc<Mutex<Vec<String>>>,
    answer: Arc<Mutex<String>>,
}

impl AiProvider for Model {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Openai
    }
    fn complete<'a>(&'a self, request: &'a AiRequest) -> CompletionFuture<'a> {
        let shown = format!(
            "{}\n{}\n{}\n{}",
            request.model,
            request.system,
            request.user,
            serde_json::to_string(&request.context).unwrap()
        );
        self.seen.lock().unwrap().push(shown);
        let text = self.answer.lock().unwrap().clone();
        Box::pin(async move {
            Ok(AiResponse {
                text,
                input_tokens: Some(1.0),
                output_tokens: Some(1.0),
            })
        })
    }
}

fn decision(action: &str, stop_loss: f64, take_profit: Option<f64>) -> String {
    json!({
        "symbol": "BTCUSDT", "action": action, "orderType": "MARKET", "entryPrice": null,
        "stopLoss": stop_loss, "takeProfit": take_profit, "reason": "test", "confidence": 0.7,
        "expiresAt": null,
    })
    .to_string()
}

fn long() -> String {
    // PRICE * 0.99 and PRICE * 1.02.
    decision("LONG", 74_250.0, Some(76_500.0))
}

struct World {
    _dir: TempDir,
    path: PathBuf,
    okx: Arc<FakeOkx>,
    seen: Arc<Mutex<Vec<String>>>,
    answer: Arc<Mutex<String>>,
    rt: RuntimeHandle,
    state: AppState,
}

fn config() -> Config {
    let mut config = config_from(|_| None);
    config.data_dir = PathBuf::from(".");
    config
}

fn boot(
    path: &Path,
    okx: &Arc<FakeOkx>,
    seen: &Arc<Mutex<Vec<String>>>,
    answer: &Arc<Mutex<String>>,
) -> RuntimeHandle {
    let mut options = RuntimeOptions::new(mochi_store::open_database(path).unwrap());
    options.replay_minutes = Some(mochi_market::tape::WARMUP_MINUTES + 30);
    options.live = Arc::new(Feed);
    options.kdf = FAST;
    let (seen, answer) = (seen.clone(), answer.clone());
    options.provider_factory = Some(Arc::new(move |_, _| {
        Ok(Arc::new(Model {
            seen: seen.clone(),
            answer: answer.clone(),
        }))
    }));
    options.okx = OkxOptions {
        transport: Some(okx.transport()),
        base_url: Some("https://okx.invalid".into()),
        ..OkxOptions::default()
    };
    RuntimeHandle::start(options).unwrap()
}

impl World {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        let okx = FakeOkx::new();
        let seen = Arc::default();
        let answer = Arc::new(Mutex::new(long()));
        let rt = boot(&path, &okx, &seen, &answer);
        rt.call(|r| {
            r.connection()
                .execute_batch(
                    "INSERT INTO providers (id,name,kind,base_url,default_model,input_price,
                       output_price,extra_headers,created_at)
                     VALUES ('remote','r','OPENAI',NULL,'m',NULL,NULL,'{}',1)",
                )
                .unwrap();
            r.set_key("remote", "ai-key").unwrap();
        })
        .await
        .unwrap();
        let state = AppState::new(Arc::new(config()), rt.clone());
        World {
            _dir: dir,
            path,
            okx,
            seen,
            answer,
            rt,
            state,
        }
    }

    /// Node's `runtime.stop(); db.close(); boot()`.
    async fn restart(&mut self) {
        self.rt.shutdown().await;
        self.rt = boot(&self.path, &self.okx, &self.seen, &self.answer);
        self.state = AppState::new(Arc::new(config()), self.rt.clone());
    }

    fn file_holds(&self, value: &str) -> bool {
        let wal = PathBuf::from(format!("{}-wal", self.path.display()));
        [self.path.clone(), wal].iter().any(|file| {
            std::fs::read(file).is_ok_and(|bytes| {
                bytes
                    .windows(value.len())
                    .any(|window| window == value.as_bytes())
            })
        })
    }

    async fn connect_with(
        &self,
        environment: &str,
        remember: bool,
    ) -> Result<PaperAccount, ApiError> {
        let input: OkxConnectInput = validate::parse_body(
            Schema::OkxConnect,
            &json!({
                "apiKey": API_KEY, "secretKey": SECRET_KEY, "passphrase": PASSPHRASE,
                "name": "OKX", "environment": environment, "remember": remember,
            }),
        )
        .unwrap();
        self.rt.connect_okx(input, None).await
    }

    async fn connect(&self, environment: &str) -> PaperAccount {
        self.connect_with(environment, false).await.unwrap()
    }

    async fn statuses(&self) -> Vec<Value> {
        self.rt
            .call(|r| serde_json::to_value(r.exchange_statuses().unwrap()).unwrap())
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    async fn account_count(&self) -> usize {
        self.rt
            .call(|r| r.engine().store().list_accounts().unwrap().len())
            .await
            .unwrap()
    }

    async fn arm(&self, id: &str, phrase: &str) -> Result<(), ApiError> {
        let (id, phrase) = (id.to_string(), phrase.to_string());
        self.rt.call(move |r| r.arm(&id, &phrase)).await.unwrap()
    }

    async fn place(&self, account_id: &str) -> Value {
        let request: OrderRequest = serde_json::from_value(json!({
            "accountId": account_id, "clientOrderId": uuid::Uuid::new_v4().to_string(),
            "symbol": "BTCUSDT", "side": "BUY", "type": "MARKET", "quantity": "0.001",
            "leverage": 3, "stopLoss": "74000",
        }))
        .unwrap();
        let engine = self.rt.execution(account_id).await.unwrap();
        serde_json::to_value(engine.place_order(request).await.unwrap()).unwrap()
    }

    async fn agent_on(&self, account_id: &str, mode: &str) -> String {
        let input: AgentInput = validate::parse_body(
            Schema::AgentInput,
            &json!({
                "name": "auto", "providerId": "remote", "model": "m", "symbols": ["BTCUSDT"],
                "timeframe": "15m", "strategy": "", "accountId": account_id,
                "risk": default_risk_settings(), "intervalSec": 60, "executionMode": mode,
            }),
        )
        .unwrap();
        self.rt
            .call(move |r| r.create_agent(input).unwrap().id)
            .await
            .unwrap()
    }

    async fn agent(&self, id: &str) -> mochi_store::rows::Agent {
        let id = id.to_string();
        self.rt
            .call(move |r| r.require_agent(&id).unwrap())
            .await
            .unwrap()
    }

    async fn can_run(&self, id: &str) -> Result<(), ApiError> {
        let id = id.to_string();
        self.rt
            .call(move |r| {
                let agent = r.require_agent(&id)?;
                r.assert_can_run(&agent)
            })
            .await
            .unwrap()
    }

    async fn cycle(&self, id: &str, trigger: CycleTrigger) -> Value {
        self.rt.run_cycle(id, trigger).await.unwrap().unwrap()
    }

    async fn can_execute(&self, record: &Value) -> Result<(), ApiError> {
        let record = record.clone();
        self.rt
            .call(move |r| r.assert_can_execute_signal(&record))
            .await
            .unwrap()
    }

    async fn pending_signals(&self) -> usize {
        self.rt
            .call(|r| {
                mochi_store::rows::list_pending_signals(r.connection())
                    .unwrap()
                    .len()
            })
            .await
            .unwrap()
    }

    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("host", HOST)
            .header("authorization", format!("Bearer {}", self.state.token));
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let response = app(self.state.clone())
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn placed(&self) -> usize {
        self.okx.state().placed.len()
    }
}

fn toasts(events: &mut broadcast::Receiver<ServerEvent>) -> Vec<(ToastLevel, String)> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let ServerEvent::Toast { level, message } = event {
            out.push((level, message));
        }
    }
    out
}

fn contains(error: &ApiError, text: &str) -> bool {
    error.message().contains(text)
}

// ---- connecting -------------------------------------------------------------------------------

/// A change to the fake's knobs.
type Patch = fn(&mut mochi_okx::fake::FakeState);

#[tokio::test]
async fn refuses_each_unsafe_key_and_creates_no_account() {
    let cases: [(&str, Patch, &str); 4] = [
        (
            "a key that can withdraw",
            |s| s.perm = "read_only,withdraw,trade".into(),
            "This API key has withdrawal permission. Create a key without it.",
        ),
        (
            "a read-only key",
            |s| s.perm = "read_only".into(),
            "This API key has no trade permission.",
        ),
        (
            "hedge mode",
            |s| s.pos_mode = "long_short_mode".into(),
            "Set OKX position mode to one-way (net) first. MochiPrep does not support hedge mode.",
        ),
        (
            "a spot-mode account",
            |s| s.acct_lv = "1".into(),
            "This OKX account is in Spot mode, which cannot trade perpetuals. Switch it to Futures mode.",
        ),
    ];
    for (label, patch, message) in cases {
        let w = World::new().await;
        patch(&mut w.okx.state());
        let error = w.connect_with("OKX_LIVE", false).await.unwrap_err();
        assert_eq!(error.status().as_u16(), 400, "{label}");
        assert_eq!(error.message(), message, "{label}");
        assert_eq!(w.account_count().await, 0, "{label}");
        w.rt.shutdown().await;
    }
}

#[tokio::test]
async fn refuses_to_size_orders_if_okx_contract_specs_changed() {
    let w = World::new().await;
    w.okx.state().ct_val_override = Some("0.001".into());
    let error = w.connect_with("OKX_LIVE", false).await.unwrap_err();
    assert_eq!(
        error.message(),
        "OKX contract specs for BTCUSDT changed. Refusing to size orders from stale values."
    );
    assert_eq!(w.account_count().await, 0);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn refuses_to_connect_if_a_saved_pair_changed_on_okx() {
    let w = World::new().await;
    let pepe = mochi_market::instrument_parse::parse_okx_instrument(&json!({
        "instId": "PEPE-USDT-SWAP", "settleCcy": "USDT", "ctType": "linear", "state": "live",
        "ruleType": "normal", "instCategory": "1", "ctValCcy": "PEPE", "ctVal": "10000000",
        "lotSz": "0.1", "minSz": "0.1", "tickSz": "0.0000000001", "lever": "50",
    }))
    .unwrap()
    .unwrap();
    w.rt.call(move |r| r.add_instrument(pepe).unwrap())
        .await
        .unwrap();
    w.okx.state().pepe_ct_val_override = Some("20000000".into());
    let error = w.connect_with("OKX_LIVE", false).await.unwrap_err();
    assert!(contains(&error, "contract specs for PEPEUSDT"), "{error:?}");
    w.rt.shutdown().await;
}

/// Node's `removeInstrument` reads `positionsOf`, so a pair held on OKX cannot be removed.
#[tokio::test]
async fn a_pair_held_on_okx_cannot_be_removed() {
    let w = World::new().await;
    let pepe = mochi_market::instrument_parse::parse_okx_instrument(&json!({
        "instId": "PEPE-USDT-SWAP", "settleCcy": "USDT", "ctType": "linear", "state": "live",
        "ruleType": "normal", "instCategory": "1", "ctValCcy": "PEPE", "ctVal": "10000000",
        "lotSz": "0.1", "minSz": "0.1", "tickSz": "0.0000000001", "lever": "50",
    }))
    .unwrap()
    .unwrap();
    w.rt.call(move |r| r.add_instrument(pepe).unwrap())
        .await
        .unwrap();
    w.connect("OKX_LIVE").await;
    w.okx.state().position = Some(
        json!({
            "instId": "PEPE-USDT-SWAP", "pos": "1", "avgPx": "0.000001", "markPx": "0.000001",
            "upl": "0", "lever": "3", "liqPx": "0.0000005", "margin": "10", "mgnMode": "isolated",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    w.rt.sync_exchanges().await.unwrap();
    let (status, body) = w.send("DELETE", "/api/instruments/PEPEUSDT", None).await;
    assert_eq!(
        (status, body),
        (400, json!({"error": "OKX has an open PEPEUSDT position"}))
    );
    w.okx.state().position = None;
    w.rt.sync_exchanges().await.unwrap();
    let (status, _) = w.send("DELETE", "/api/instruments/PEPEUSDT", None).await;
    assert_eq!(status, 200);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn signs_every_request_and_marks_only_demo_requests_as_simulated() {
    let w = World::new().await;
    w.connect("OKX_LIVE").await;
    assert!(w.okx.state().simulated_headers.iter().all(Option::is_none));
    w.okx.state().simulated_headers.clear();
    w.connect("OKX_DEMO").await;
    let headers = w.okx.state().simulated_headers.clone();
    assert!(!headers.is_empty());
    assert!(headers.iter().all(|h| h.as_deref() == Some("1")));
    w.rt.shutdown().await;
}

#[tokio::test]
async fn never_exposes_credentials_through_status() {
    let w = World::new().await;
    w.connect("OKX_LIVE").await;
    let accounts =
        w.rt.call(|r| serde_json::to_string(&r.engine().store().list_accounts().unwrap()).unwrap())
            .await
            .unwrap();
    let (_, status) = w.send("GET", "/api/exchange/status", None).await;
    let text = format!("{}{accounts}{status}", json!(w.statuses().await));
    for value in CREDS {
        assert!(!text.contains(value));
    }
    w.rt.shutdown().await;
}

#[tokio::test]
async fn a_connect_writes_the_account_and_its_status_in_node_order() {
    let w = World::new().await;
    let (status, view) = w
        .send(
            "POST",
            "/api/exchange/okx/connect",
            Some(json!({
                "apiKey": format!(" {API_KEY} "), "secretKey": SECRET_KEY, "passphrase": PASSPHRASE,
                "name": " Main ", "environment": "OKX_LIVE",
            })),
        )
        .await;
    assert_eq!(status, 200, "{view}");
    let keys: Vec<&String> = view.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "id",
            "name",
            "source",
            "venue",
            "exchangeUid",
            "liveLimits",
            "isDemo",
            "initialBalance",
            "walletBalance",
            "usedMargin",
            "realizedPnl",
            "feesPaid",
            "fundingPaid",
            "config",
            "createdAt",
            "balance"
        ]
    );
    assert_eq!(view["name"], "Main");
    assert_eq!(view["source"], "LIVE");
    assert_eq!(view["venue"], "OKX_LIVE");
    assert_eq!(view["exchangeUid"], "42");
    // The first balance OKX reports becomes the initial balance.
    assert_eq!(view["initialBalance"], "1000");
    assert_eq!(
        view["liveLimits"],
        json!({"maxLeverage":3,"maxRiskPerTradePct":0.5,"maxDailyLossPct":2,"maxOpenPositions":2,
               "requireStopLoss":true,"maxQuoteAgeSec":10,"maxNotional":100})
    );
    let id = view["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("acct_"));
    let statuses = w.statuses().await;
    let keys: Vec<&String> = statuses[0].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "accountId",
            "name",
            "venue",
            "connected",
            "saved",
            "armed",
            "healthy",
            "lastSyncAt",
            "error",
            "limits"
        ]
    );
    assert_eq!(statuses[0]["connected"], true);
    assert_eq!(statuses[0]["armed"], false);
    assert_eq!(statuses[0]["healthy"], true);
    assert_eq!(statuses[0]["saved"], false);
    assert!(statuses[0]["lastSyncAt"].is_i64());

    // A reconnect of the same OKX account keeps its row and name, and takes the new limits.
    let (status, again) = w
        .send(
            "POST",
            "/api/exchange/okx/connect",
            Some(json!({
                "apiKey": API_KEY, "secretKey": SECRET_KEY, "passphrase": PASSPHRASE,
                "name": "Other", "environment": "OKX_LIVE",
                "limits": {"maxLeverage":2.5,"maxRiskPerTradePct":1e-7,"maxDailyLossPct":0.30000000000000004,
                           "maxOpenPositions":1,"requireStopLoss":false,"maxQuoteAgeSec":10,"maxNotional":50},
            })),
        )
        .await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["id"], id.as_str());
    assert_eq!(again["name"], "Main");
    let stored: String =
        w.rt.call(move |r| {
            r.connection()
                .query_row("SELECT live_limits FROM accounts WHERE id=?", [id], |row| {
                    row.get(0)
                })
                .unwrap()
        })
        .await
        .unwrap();
    assert_eq!(
        stored,
        r#"{"maxLeverage":2.5,"maxRiskPerTradePct":1e-7,"maxDailyLossPct":0.30000000000000004,"maxOpenPositions":1,"requireStopLoss":false,"maxQuoteAgeSec":10,"maxNotional":50}"#
    );
    w.rt.shutdown().await;
}

#[tokio::test]
async fn the_exchange_routes_check_the_account_and_the_phrase() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    let arm = format!("/api/exchange/okx/{}/arm", account.id);
    let (status, body) = w
        .send(
            "POST",
            "/api/exchange/okx/nope/arm",
            Some(json!({"confirmation": ARM_PHRASE})),
        )
        .await;
    assert_eq!((status, body), (404, json!({"error": "Unknown account"})));
    let (status, body) = w
        .send("POST", &arm, Some(json!({"confirmation": "yes"})))
        .await;
    assert_eq!(
        (status, body),
        (400, json!({"error": "Type exactly: TRADE REAL MONEY"}))
    );
    let (status, body) = w
        .send("POST", &arm, Some(json!({"confirmation": ARM_PHRASE})))
        .await;
    assert_eq!((status, body), (200, json!({"ok": true})));
    assert_eq!(w.statuses().await[0]["armed"], true);
    let disarm = format!("/api/exchange/okx/{}/disarm", account.id);
    let (status, body) = w.send("POST", &disarm, None).await;
    assert_eq!((status, body), (200, json!({"ok": true})));
    assert_eq!(w.statuses().await[0]["armed"], false);
    let disconnect = format!("/api/exchange/okx/{}/disconnect", account.id);
    let (status, body) = w.send("POST", &disconnect, None).await;
    assert_eq!((status, body), (200, json!({"ok": true})));
    assert_eq!(w.statuses().await[0]["connected"], false);
    let (status, body) = w
        .send("POST", &arm, Some(json!({"confirmation": ARM_PHRASE})))
        .await;
    assert_eq!(
        (status, body),
        (400, json!({"error": "Connect this account first"}))
    );
    let (status, body) = w
        .send("POST", "/api/exchange/okx/nope/disconnect", None)
        .await;
    assert_eq!((status, body), (404, json!({"error": "Unknown account"})));
    w.rt.shutdown().await;
}

#[tokio::test]
async fn an_exchange_account_without_a_session_says_it_is_not_connected() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.rt.call({
        let id = account.id.clone();
        move |r| r.disconnect_exchange(&id).unwrap()
    })
    .await
    .unwrap();
    let (status, body) = w
        .send(
            "POST",
            "/api/orders",
            Some(json!({
                "accountId": account.id, "clientOrderId": "x", "symbol": "BTCUSDT", "side": "BUY",
                "type": "MARKET", "quantity": "0.001", "leverage": 3, "stopLoss": "74000",
            })),
        )
        .await;
    assert_eq!(
        (status, body),
        (
            400,
            json!({"error": "OKX is not connected to OKX. Enter its API key in Settings."})
        )
    );
    // Positions of a disconnected exchange account are empty, as in Node.
    let (status, body) = w
        .send(
            "GET",
            &format!("/api/accounts/{}/positions", account.id),
            None,
        )
        .await;
    assert_eq!((status, body), (200, json!([])));
    w.rt.shutdown().await;
}

#[tokio::test]
async fn deleting_an_exchange_account_disconnects_it_first() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    let (status, body) = w
        .send("DELETE", &format!("/api/accounts/{}", account.id), None)
        .await;
    assert_eq!((status, body), (200, json!({"ok": true})));
    assert_eq!(w.account_count().await, 0);
    assert!(w.statuses().await.is_empty());
    let connected =
        w.rt.call(move |r| r.execution_target(&account.id).is_ok())
            .await
            .unwrap();
    assert!(!connected);
    w.rt.shutdown().await;
}

// ---- real-money safety ------------------------------------------------------------------------

#[tokio::test]
async fn sends_nothing_until_armed_and_only_the_exact_phrase_arms() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    let refused = w.place(&account.id).await;
    assert_eq!(refused["order"]["status"], "REJECTED");
    assert!(refused["order"]["rejectReason"]
        .as_str()
        .unwrap()
        .contains("NOT_ARMED"));
    assert_eq!(w.placed(), 0);
    let error = w.arm(&account.id, "yes").await.unwrap_err();
    assert_eq!(error.message(), "Type exactly: TRADE REAL MONEY");
    w.arm(&account.id, &format!("{ARM_PHRASE} "))
        .await
        .unwrap_err();
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    assert_eq!(w.place(&account.id).await["order"]["status"], "FILLED");
    w.rt.shutdown().await;
}

#[tokio::test]
async fn cannot_arm_until_sync_succeeds() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.okx.state().down = true;
    w.rt.sync_exchanges().await.unwrap();
    let error = w.arm(&account.id, ARM_PHRASE).await.unwrap_err();
    assert_eq!(error.message(), "Cannot arm until sync with OKX succeeds");
    let error = w.arm("nope", ARM_PHRASE).await.unwrap_err();
    assert_eq!(error.message(), "Connect this account first");
    w.rt.shutdown().await;
}

#[tokio::test]
async fn demo_accounts_start_armed_and_disarm_leaves_them_armed() {
    let w = World::new().await;
    let demo = w.connect("OKX_DEMO").await;
    assert_eq!(w.statuses().await[0]["armed"], true);
    w.rt.call(|r| r.disarm(None)).await.unwrap();
    assert_eq!(w.statuses().await[0]["armed"], true);
    assert_eq!(w.place(&demo.id).await["order"]["status"], "FILLED");
    w.rt.shutdown().await;
}

#[tokio::test]
async fn api_protection_route_on_an_okx_account_goes_through_the_armed_gate() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.place(&account.id).await; // long, stop at 74000
    w.okx.state().calls.clear();
    w.rt.call(|r| r.disarm(None)).await.unwrap();
    let path = format!("/api/positions/{}/BTCUSDT/protection", account.id);
    let (status, refused) = w
        .send(
            "PUT",
            &path,
            Some(json!({"stopLoss": "73000", "takeProfit": null})),
        )
        .await;
    assert_eq!(status, 400);
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("NOT_ARMED: this change increases risk"));
    assert!(!w.okx.state().calls.iter().any(|c| c.starts_with("POST")));
    let (status, tightened) = w
        .send(
            "PUT",
            &path,
            Some(json!({"stopLoss": "74500", "takeProfit": null})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(tightened["ok"], true);
    assert_eq!(tightened["pending"], false);
    assert_eq!(
        tightened["after"],
        json!({"stopLoss": "74500", "takeProfit": null, "trailingStop": null})
    );
    assert_eq!(w.okx.state().algo_placed.len(), 1);
    w.rt.shutdown().await;
}

/// Node's error handler answers a `ProtectionChangeError` as any other thrown error: 400 with
/// its message, secrets removed.
#[tokio::test]
async fn a_part_way_protection_refusal_through_the_route_is_a_400_with_its_reason() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.place(&account.id).await;
    w.okx.state().reject_algo_if = Some(Box::new(|body| body["ordType"] == "move_order_stop"));
    let (status, body) = w
        .send(
            "PUT",
            &format!("/api/positions/{}/BTCUSDT/protection", account.id),
            Some(json!({"stopLoss": "74500", "takeProfit": null,
                        "trailingStop": {"kind": "RATIO", "callback": "0.01", "activationPrice": null}})),
        )
        .await;
    assert_eq!(
        (status, body),
        (400, json!({"error": "OKX 51000: Parameter error"}))
    );
    w.rt.shutdown().await;
}

/// Legs OKX already accepted change what clients should show, even though the call failed.
#[tokio::test]
async fn a_part_way_protection_refusal_still_invalidates_orders_and_positions() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.place(&account.id).await;
    w.okx.state().reject_algo_if = Some(Box::new(|body| body["ordType"] == "move_order_stop"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut events = w.rt.subscribe();
    let (status, _) = w
        .send(
            "PUT",
            &format!("/api/positions/{}/BTCUSDT/protection", account.id),
            Some(json!({"stopLoss": "74500", "takeProfit": null,
                        "trailingStop": {"kind": "RATIO", "callback": "0.01", "activationPrice": null}})),
        )
        .await;
    assert_eq!(status, 400);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut topics = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let ServerEvent::Invalidate { topics: t } = event {
            topics.extend(t);
        }
    }
    assert!(topics.contains(&Topic::Orders), "{topics:?}");
    assert!(topics.contains(&Topic::Positions), "{topics:?}");
    w.rt.shutdown().await;
}

// ---- AI agents --------------------------------------------------------------------------------

#[tokio::test]
async fn trades_automatically_on_okx_demo_without_arming() {
    let w = World::new().await;
    let account = w.connect("OKX_DEMO").await;
    let agent = w.agent_on(&account.id, "AUTO").await;
    w.can_run(&agent).await.unwrap();
    let id = agent.clone();
    w.rt.call(move |r| r.start_agent(&id).unwrap())
        .await
        .unwrap();
    let record = w.cycle(&agent, CycleTrigger::Schedule).await;
    assert_eq!(record["outcome"], "EXECUTED", "{record}");
    assert_eq!(record["venue"], "OKX_DEMO");
    assert_eq!(w.placed(), 1);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn trades_automatically_with_real_money_once_armed_sized_inside_the_account_ceiling() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let agent = w.agent_on(&account.id, "AUTO").await;
    let id = agent.clone();
    w.rt.call(move |r| r.start_agent(&id).unwrap())
        .await
        .unwrap();
    let record = w.cycle(&agent, CycleTrigger::Schedule).await;
    assert_eq!(record["outcome"], "EXECUTED", "{record}");
    assert_eq!(record["venue"], "OKX_LIVE");
    let notional: f64 = record["risk"]["sized"]["notional"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(notional <= 100.0);
    assert_eq!(
        w.okx.state().placed[0]["attachAlgoOrds"][0]["slTriggerPx"],
        "74250"
    );
    w.rt.shutdown().await;
}

#[tokio::test]
async fn an_unarmed_auto_agent_run_once_is_refused_by_the_engine_and_can_still_exit() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    let agent = w.agent_on(&account.id, "AUTO").await;
    let record = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(record["outcome"], "ORDER_REJECTED", "{record}");
    assert!(record["outcomeDetail"]
        .as_str()
        .unwrap()
        .contains("NOT_ARMED"));
    assert_eq!(w.placed(), 0);
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.place(&account.id).await;
    w.rt.call(|r| r.disarm(None)).await.unwrap();
    {
        let mut okx = w.okx.state();
        let position = okx.position.as_mut().unwrap();
        position.insert("pos".into(), json!("0.4"));
        let mut cross = position.clone();
        cross.insert("pos".into(), json!("0.2"));
        cross.insert("mgnMode".into(), json!("cross"));
        okx.other_positions.push(cross);
    }
    w.rt.sync_exchanges().await.unwrap();
    // An exit needs no arming.
    *w.answer.lock().unwrap() = decision("CLOSE", 74_000.0, None);
    let placed = w.placed();
    let record = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(record["outcome"], "EXECUTED", "{record}");
    assert_eq!(w.placed(), placed + 1);
    {
        let okx = w.okx.state();
        assert_eq!(okx.placed.last().unwrap()["sz"], "0.4");
        assert_eq!(okx.placed.last().unwrap()["tdMode"], "isolated");
        assert_eq!(okx.other_positions[0]["pos"], "0.2");
    }
    w.rt.shutdown().await;
}

#[tokio::test]
async fn still_trades_automatically_on_a_paper_account() {
    let w = World::new().await;
    let account =
        w.rt.call(|r| {
            r.engine_mut()
                .create_account(CreateAccountArgs {
                    name: "paper".into(),
                    source: DataSource::Live,
                    initial_balance: None,
                    is_demo: None,
                    config: None,
                })
                .unwrap()
        })
        .await
        .unwrap();
    let agent = w.agent_on(&account.id, "AUTO").await;
    let id = agent.clone();
    w.rt.call(move |r| r.start_agent(&id).unwrap())
        .await
        .unwrap();
    let record = w.cycle(&agent, CycleTrigger::Schedule).await;
    assert_eq!(record["outcome"], "EXECUTED", "{record}");
    assert_eq!(record["venue"], "PAPER");
    assert_eq!(w.placed(), 0);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn a_suggestion_can_run_unarmed_because_it_places_nothing() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    let agent = w.agent_on(&account.id, "SIGNAL").await;
    w.can_run(&agent).await.unwrap();
    let record = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(record["outcome"], "SIGNAL", "{record}");
    assert_eq!(w.placed(), 0);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn an_old_close_suggestion_expires_after_the_position_flips() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.place(&account.id).await;
    *w.answer.lock().unwrap() = decision("CLOSE", 74000.0, None);
    let agent = w.agent_on(&account.id, "SIGNAL").await;
    let record = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(record["outcome"], "SIGNAL", "{record}");
    {
        let mut okx = w.okx.state();
        let position = okx.position.as_mut().unwrap();
        position.insert("pos".into(), json!("-0.1"));
        position.insert("tradeId".into(), json!("10"));
    }
    w.rt.sync_exchanges().await.unwrap();
    let placed = w.placed();
    let done =
        w.rt.execute_signal(record["id"].as_str().unwrap())
            .await
            .unwrap();
    assert_eq!(done["outcome"], "SIGNAL_EXPIRED", "{done}");
    assert_eq!(w.placed(), placed);
    assert_eq!(w.okx.state().position.as_ref().unwrap()["pos"], "-0.1");

    let current = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(current["outcome"], "SIGNAL", "{current}");
    let done =
        w.rt.execute_signal(current["id"].as_str().unwrap())
            .await
            .unwrap();
    assert_eq!(done["outcome"], "EXECUTED", "{done}");
    assert!(w.okx.state().position.is_none());
    w.rt.shutdown().await;
}

#[tokio::test]
async fn a_protect_suggestion_expires_for_a_replacement_with_the_same_stop() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.place(&account.id).await;
    *w.answer.lock().unwrap() = decision("PROTECT", 74_500.0, None);
    let agent = w.agent_on(&account.id, "SIGNAL").await;
    let suggestion = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(suggestion["outcome"], "SIGNAL", "{suggestion}");
    assert_eq!(suggestion["protectionBefore"]["stopLoss"], "74000");

    w.okx
        .state()
        .position
        .as_mut()
        .unwrap()
        .insert("tradeId".into(), json!("10"));
    w.rt.sync_exchanges().await.unwrap();
    let engine = w.rt.execution(&account.id).await.unwrap();
    let protection = engine
        .protection_of(account.id.clone(), "BTCUSDT".into())
        .await
        .unwrap();
    assert_eq!(protection.stop_loss.as_deref(), Some("74000"));
    let placed = w.okx.state().algo_placed.len();
    let done =
        w.rt.execute_signal(suggestion["id"].as_str().unwrap())
            .await
            .unwrap();
    assert_eq!(done["outcome"], "SIGNAL_EXPIRED", "{done}");
    assert_eq!(w.okx.state().algo_placed.len(), placed);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn a_close_suggestion_cannot_close_a_remote_replacement_hidden_by_the_cache() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.place(&account.id).await;
    *w.answer.lock().unwrap() = decision("CLOSE", 74000.0, None);
    let agent = w.agent_on(&account.id, "SIGNAL").await;
    let record = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(record["outcome"], "SIGNAL", "{record}");
    w.okx
        .state()
        .position
        .as_mut()
        .unwrap()
        .insert("tradeId".into(), json!("10"));
    let placed = w.placed();
    let done =
        w.rt.execute_signal(record["id"].as_str().unwrap())
            .await
            .unwrap();
    assert_eq!(done["outcome"], "ORDER_REJECTED", "{done}");
    assert_eq!(w.placed(), placed);
    assert!(w.okx.state().position.is_some());
    w.rt.shutdown().await;
}

#[tokio::test]
async fn refuses_to_accept_while_unarmed_and_keeps_the_suggestion_for_later() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    let agent = w.agent_on(&account.id, "SIGNAL").await;
    let record = w.cycle(&agent, CycleTrigger::RunOnce).await;
    let error = w.can_execute(&record).await.unwrap_err();
    assert_eq!(
        error.message(),
        "OKX is not armed. Arm it in Settings first."
    );
    assert_eq!(w.pending_signals().await, 1);
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.can_execute(&record).await.unwrap();
    let done =
        w.rt.execute_signal(record["id"].as_str().unwrap())
            .await
            .unwrap();
    assert_eq!(done["outcome"], "EXECUTED", "{done}");
    assert_eq!(w.placed(), 1);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn the_api_refuses_to_accept_while_unarmed_without_using_up_the_suggestion() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    let agent = w.agent_on(&account.id, "SIGNAL").await;
    let record = w.cycle(&agent, CycleTrigger::RunOnce).await;
    let path = format!("/api/signals/{}/execute", record["id"].as_str().unwrap());
    let (status, refused) = w.send("POST", &path, None).await;
    assert_eq!(status, 400);
    assert!(refused["error"].as_str().unwrap().contains("not armed"));
    let id = record["id"].as_str().unwrap().to_string();
    let outcome =
        w.rt.call(move |r| {
            mochi_store::rows::get_decision(r.connection(), &id)
                .unwrap()
                .unwrap()["outcome"]
                .clone()
        })
        .await
        .unwrap();
    assert_eq!(outcome, "SIGNAL");
    assert_eq!(w.placed(), 0);
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let (status, accepted) = w.send("POST", &path, None).await;
    assert_eq!(status, 200);
    assert_eq!(accepted["outcome"], "EXECUTED", "{accepted}");
    w.rt.shutdown().await;
}

#[tokio::test]
async fn agent_protection_risks_use_the_isolated_position_even_when_cross_is_first() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.place(&account.id).await;
    let agent = w.agent_on(&account.id, "AUTO").await;
    {
        let mut okx = w.okx.state();
        let isolated = okx.position.as_mut().unwrap();
        isolated.insert("pos".into(), json!("10"));
        let mut cross = isolated.clone();
        cross.insert("pos".into(), json!("0.01"));
        cross.insert("mgnMode".into(), json!("cross"));
        okx.other_positions.push(cross);
    }
    w.rt.sync_exchanges().await.unwrap();
    *w.answer.lock().unwrap() = decision("PROTECT", 73_500.0, None);
    let before = w.okx.state().algo_placed.len();
    let result = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(result["outcome"], "RISK_REJECTED", "{result}");
    assert!(
        result["risk"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["id"] == "protect-risk" && check["passed"] == false),
        "{result}"
    );
    assert_eq!(w.okx.state().algo_placed.len(), before);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn agent_close_risk_records_the_isolated_position_even_when_cross_is_first() {
    let w = World::new().await;
    let account = w.connect("OKX_DEMO").await;
    w.place(&account.id).await;
    {
        let mut okx = w.okx.state();
        let mut cross = okx.position.as_ref().unwrap().clone();
        cross.insert("pos".into(), json!("0.01"));
        cross.insert("mgnMode".into(), json!("cross"));
        okx.other_positions.push(cross);
    }
    w.rt.sync_exchanges().await.unwrap();
    let agent = w.agent_on(&account.id, "AUTO").await;
    *w.answer.lock().unwrap() = decision("CLOSE", 74_000.0, None);

    let result = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(result["outcome"], "EXECUTED", "{result}");
    assert!(w.okx.state().position.is_none());
    let close_check = result["risk"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "has-position")
        .unwrap();
    assert_eq!(close_check["detail"], "LONG 0.001");
    w.rt.shutdown().await;
}

#[tokio::test]
async fn agent_protect_can_remove_an_unsent_take_profit_with_an_unchanged_stop() {
    let w = World::new().await;
    let account = w.connect("OKX_DEMO").await;
    w.okx.state().rest_limit_orders = true;
    let request = serde_json::from_value(json!({
        "accountId": account.id, "clientOrderId": "agent-remove-unsent-target",
        "symbol": "BTCUSDT", "side": "BUY", "type": "LIMIT", "quantity": "0.001",
        "price": "74500", "leverage": 3, "stopLoss": "74000", "takeProfit": "78000",
    }))
    .unwrap();
    let engine = w.rt.execution(&account.id).await.unwrap();
    let entry = engine.place_order(request).await.unwrap().order;
    {
        let mut okx = w.okx.state();
        okx.fill_resting();
        // The entry lookup can report a fill ahead of the position's trade watermark.
        okx.orders
            .get_mut(&mochi_okx::to_cl_ord_id(&entry.client_order_id))
            .unwrap()
            .insert("tradeId".into(), json!("2"));
    }
    w.rt.sync_exchanges().await.unwrap();
    let target_id = format!("{}:deferred:tp", entry.id);
    let target_before = target_id.clone();
    let target =
        w.rt.call(move |r| {
            r.engine()
                .store()
                .get_order(&target_before)
                .unwrap()
                .unwrap()
        })
        .await
        .unwrap();
    assert_eq!(target.status, mochi_domain::OrderStatus::Open);
    assert!(target.exchange_order_id.is_none());
    let agent = w.agent_on(&account.id, "AUTO").await;
    *w.answer.lock().unwrap() = decision("PROTECT", 74_000.0, None);

    let result = w.cycle(&agent, CycleTrigger::RunOnce).await;
    let target =
        w.rt.call(move |r| r.engine().store().get_order(&target_id).unwrap().unwrap())
            .await
            .unwrap();
    assert_eq!(
        target.status,
        mochi_domain::OrderStatus::Canceled,
        "{result}"
    );
    assert_eq!(target.reject_reason.as_deref(), Some("Removed"));
    assert_eq!(result["outcome"], "EXECUTED", "{result}");
    assert_eq!(result["protectionBefore"]["stopLoss"], "74000");
    assert_eq!(result["protectionBefore"], result["protectionAfter"]);
    assert_eq!(
        w.cycle(&agent, CycleTrigger::RunOnce).await["outcome"],
        "HOLD"
    );
    w.rt.shutdown().await;
}

#[tokio::test]
async fn accepts_a_protect_while_unarmed_leaving_the_arming_rule_to_the_protection_gate() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.place(&account.id).await; // long, stop at 74000
    w.rt.call(|r| r.disarm(None)).await.unwrap();
    let agent = w.agent_on(&account.id, "SIGNAL").await;

    // Tighter: lowers risk, so it needs no arming, like an exit.
    *w.answer.lock().unwrap() = decision("PROTECT", 74_500.0, None);
    let tighter = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(tighter["outcome"], "SIGNAL", "{tighter}");
    w.can_execute(&tighter).await.unwrap();
    let placed = w.okx.state().algo_placed.len();
    let done =
        w.rt.execute_signal(tighter["id"].as_str().unwrap())
            .await
            .unwrap();
    assert_eq!(done["outcome"], "EXECUTED", "{done}");
    assert_eq!(done["protectionAfter"]["stopLoss"], "74500");
    assert_eq!(w.okx.state().algo_placed.len(), placed + 1);
    let engine = w.rt.execution(&account.id).await.unwrap();
    let protection = engine
        .protection_of(account.id.clone(), "BTCUSDT".into())
        .await
        .unwrap();
    assert_eq!(protection.stop_loss.as_deref(), Some("74500"));

    // Looser: accepted here, refused by the gate, nothing sent.
    *w.answer.lock().unwrap() = decision("PROTECT", 73_500.0, None);
    let looser = w.cycle(&agent, CycleTrigger::RunOnce).await;
    w.can_execute(&looser).await.unwrap();
    let refused =
        w.rt.execute_signal(looser["id"].as_str().unwrap())
            .await
            .unwrap();
    assert_eq!(refused["outcome"], "ORDER_REJECTED", "{refused}");
    assert!(refused["outcomeDetail"]
        .as_str()
        .unwrap()
        .contains("NOT_ARMED"));
    assert_eq!(w.okx.state().algo_placed.len(), placed + 1);
    let protection = engine
        .protection_of(account.id.clone(), "BTCUSDT".into())
        .await
        .unwrap();
    assert_eq!(protection.stop_loss.as_deref(), Some("74500"));
    w.rt.shutdown().await;
}

/// Node's runner records `error.after` on a part-way refusal, so History shows what is in force.
#[tokio::test]
async fn a_part_way_agent_protection_refusal_records_what_is_in_force() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.place(&account.id).await;
    w.okx.state().reject_algo_if = Some(Box::new(|body| body["ordType"] == "move_order_stop"));
    let agent = w.agent_on(&account.id, "AUTO").await;
    let id = agent.clone();
    w.rt.call(move |r| {
        r.connection()
            .execute("UPDATE agents SET allow_trailing_stop=1 WHERE id=?", [id])
            .unwrap();
    })
    .await
    .unwrap();
    *w.answer.lock().unwrap() = json!({
        "symbol": "BTCUSDT", "action": "PROTECT", "orderType": "MARKET", "entryPrice": null,
        "stopLoss": 74_500, "takeProfit": null, "reason": "test", "confidence": 0.7,
        "expiresAt": null, "trailingStop": {"kind": "RATIO", "callback": 0.01, "activationPrice": null},
    })
    .to_string();
    let record = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(record["outcome"], "ORDER_REJECTED", "{record}");
    assert_eq!(record["outcomeDetail"], "OKX 51000: Parameter error");
    assert_eq!(
        record["protectionAfter"],
        json!({"stopLoss": "74500", "takeProfit": null, "trailingStop": null})
    );
    w.rt.shutdown().await;
}

#[tokio::test]
async fn a_suggestion_accepted_on_a_real_account_still_passes_account_ceilings() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let agent = w.agent_on(&account.id, "SIGNAL").await;
    let record = w.cycle(&agent, CycleTrigger::RunOnce).await;
    let result =
        w.rt.execute_signal(record["id"].as_str().unwrap())
            .await
            .unwrap();
    let notional: f64 = result["risk"]["sized"]["notional"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(notional <= 100.0);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn will_not_schedule_an_agent_on_an_unarmed_real_account() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    let agent = w.agent_on(&account.id, "AUTO").await;
    let error = w.can_run(&agent).await.unwrap_err();
    assert_eq!(
        error.message(),
        "OKX is not armed. Arm it in Settings first."
    );
    w.rt.shutdown().await;
}

#[tokio::test]
async fn refuses_to_start_an_agent_on_an_unarmed_real_account_through_the_api() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    let agent = w.agent_on(&account.id, "AUTO").await;
    let (status, _) = w
        .send("POST", &format!("/api/agents/{agent}/start"), None)
        .await;
    assert_eq!(status, 400);
    assert!(!w.agent(&agent).await.running);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn never_shows_the_exchange_credentials_to_the_model() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let agent = w.agent_on(&account.id, "AUTO").await;
    w.cycle(&agent, CycleTrigger::RunOnce).await;
    let seen = w.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    for value in CREDS {
        assert!(!seen[0].contains(value));
    }
    w.rt.shutdown().await;
}

#[tokio::test]
async fn exchange_secrets_are_redacted_from_errors() {
    let w = World::new().await;
    w.connect("OKX_LIVE").await;
    let text = format!("refused {API_KEY} / {SECRET_KEY} / {PASSPHRASE}");
    let redacted = w.rt.call(move |r| r.redact(&text)).await.unwrap();
    for value in CREDS {
        assert!(!redacted.contains(value), "{redacted}");
    }
    w.rt.shutdown().await;
}

#[tokio::test]
async fn pauses_agents_and_refuses_orders_when_sync_with_okx_is_lost() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let agent = w.agent_on(&account.id, "AUTO").await;
    let id = agent.clone();
    w.rt.call(move |r| r.start_agent(&id).unwrap())
        .await
        .unwrap();
    let mut events = w.rt.subscribe();
    w.okx.state().down = true;
    w.rt.sync_exchanges().await.unwrap();
    let a = w.agent(&agent).await;
    assert!(!a.running);
    assert_eq!(a.status, "PAUSED");
    assert_eq!(a.status_detail.as_deref(), Some("Lost sync with OKX"));
    assert_eq!(
        toasts(&mut events),
        [(
            ToastLevel::Error,
            "OKX: lost sync with OKX. Agents paused. No answer from OKX: fetch failed".to_string()
        )]
    );
    // Only the transition to unhealthy pauses and says so.
    w.rt.sync_exchanges().await.unwrap();
    assert!(toasts(&mut events).is_empty());
    w.okx.state().down = false;
    let result = w.place(&account.id).await;
    assert!(result["order"]["rejectReason"]
        .as_str()
        .unwrap()
        .contains("EXCHANGE_NOT_SYNCED"));
    assert_eq!(w.placed(), 0);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn a_disarmed_real_account_can_still_exit_while_sync_with_okx_is_lost() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    assert_eq!(w.place(&account.id).await["order"]["status"], "FILLED");
    w.rt.sync_exchanges().await.unwrap();
    w.rt.call(|r| r.disarm(None)).await.unwrap();
    // Only the sync's reads fail: OKX is back before the exit, but the engine stays unhealthy
    // until the next sync succeeds.
    w.okx.state().down = true;
    w.rt.sync_exchanges().await.unwrap();
    w.okx.state().down = false;
    assert_eq!(w.statuses().await[0]["healthy"], false);
    assert_eq!(w.statuses().await[0]["armed"], false);
    let placed = w.placed();
    let engine = w.rt.execution(&account.id).await.unwrap();
    let position_id = engine.positions(account.id.clone()).await.unwrap()[0]
        .position
        .id
        .clone();
    let result = engine
        .close_position(
            account.id.clone(),
            "BTCUSDT".into(),
            1.0,
            "exit-1".into(),
            Some(position_id),
        )
        .await
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(w.placed(), placed + 1, "{result}");
    assert_eq!(result["order"]["status"], "FILLED", "{result}");
    assert!(!result["order"]["rejectReason"]
        .as_str()
        .unwrap_or_default()
        .contains("EXCHANGE_NOT_SYNCED"));
    w.rt.shutdown().await;
}

/// The live tick syncs every 5 s: a lost sync pauses agents without anyone calling it.
#[tokio::test(start_paused = true)]
async fn the_live_tick_syncs_every_five_seconds() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let agent = w.agent_on(&account.id, "SIGNAL").await;
    let id = agent.clone();
    w.rt.call(move |r| {
        r.start_agent(&id).unwrap();
        // Far in the future, so the tick does not run a cycle.
        r.connection()
            .execute(
                "UPDATE agents SET next_run_at=? WHERE id=?",
                rusqlite::params![i64::MAX, id],
            )
            .unwrap();
    })
    .await
    .unwrap();
    let mut events = w.rt.subscribe();
    w.okx.state().down = true;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    w.rt.call(|_| ()).await.unwrap();
    assert_eq!(w.agent(&agent).await.status, "PAUSED");
    assert_eq!(toasts(&mut events).len(), 1);
    assert_eq!(w.statuses().await[0]["healthy"], false);
    w.okx.state().down = false;
    // The next sync is 5 s after the last one, not on the next tick.
    tokio::time::sleep(Duration::from_millis(3000)).await;
    w.rt.call(|_| ()).await.unwrap();
    assert_eq!(w.statuses().await[0]["healthy"], false);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    w.rt.call(|_| ()).await.unwrap();
    assert_eq!(w.statuses().await[0]["healthy"], true);
    // Agents are never un-paused by a sync.
    assert_eq!(w.agent(&agent).await.status, "PAUSED");
    w.rt.shutdown().await;
}

#[tokio::test]
async fn stop_all_disarms_real_accounts_through_the_api() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let (status, _) = w.send("POST", "/api/agents/stop-all", None).await;
    assert_eq!(status, 200);
    assert_eq!(w.statuses().await[0]["armed"], false);
    w.rt.shutdown().await;
}

/// Makes every account read fail with a store error, then undoes it.
async fn accounts_unreadable(w: &World, unreadable: bool) {
    let sql = if unreadable {
        "ALTER TABLE accounts RENAME TO accounts_unreadable"
    } else {
        "ALTER TABLE accounts_unreadable RENAME TO accounts"
    };
    w.rt.call(move |r| r.connection().execute_batch(sql).unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn disarm_needs_no_account_read_so_a_store_error_cannot_leave_real_money_armed() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    for wanted in [Some(account.id.clone()), None] {
        w.arm(&account.id, ARM_PHRASE).await.unwrap();
        accounts_unreadable(&w, true).await;
        let probe = account.id.clone();
        let read =
            w.rt.call(move |r| r.engine().store().get_account(&probe).is_err())
                .await
                .unwrap();
        assert!(
            read,
            "the account read must fail for this test to mean anything"
        );
        w.rt.call(move |r| r.disarm(wanted.as_deref()))
            .await
            .unwrap();
        accounts_unreadable(&w, false).await;
        assert_eq!(w.statuses().await[0]["armed"], false);
    }
    w.rt.shutdown().await;
}

#[tokio::test]
async fn stop_all_disarms_real_accounts_even_when_the_account_read_fails() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    accounts_unreadable(&w, true).await;
    let (status, body) = w.send("POST", "/api/agents/stop-all", None).await;
    accounts_unreadable(&w, false).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(w.statuses().await[0]["armed"], false);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn disarm_route_clears_arming_even_when_the_account_read_fails_and_unknown_ids_stay_404() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    accounts_unreadable(&w, true).await;
    let (status, _) = w
        .send(
            "POST",
            &format!("/api/exchange/okx/{}/disarm", account.id),
            None,
        )
        .await;
    accounts_unreadable(&w, false).await;
    assert_ne!(status, 200);
    assert_eq!(w.statuses().await[0]["armed"], false);
    let (status, body) = w.send("POST", "/api/exchange/okx/nope/disarm", None).await;
    assert_eq!((status, body), (404, json!({"error": "Unknown account"})));
    w.rt.shutdown().await;
}

#[tokio::test]
async fn stop_all_disarms_even_when_stopping_agents_fails() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let agent = w.agent_on(&account.id, "AUTO").await;
    let id = agent.clone();
    w.rt.call(move |r| {
        r.start_agent(&id).unwrap();
        r.connection()
            .execute_batch(
                "CREATE TRIGGER refuse_agent_update BEFORE UPDATE ON agents                  BEGIN SELECT RAISE(ABORT, 'refused'); END",
            )
            .unwrap();
    })
    .await
    .unwrap();
    w.rt.call(|r| r.set_playing(true)).await.unwrap();
    let (status, _) = w.send("POST", "/api/agents/stop-all", None).await;
    assert_ne!(status, 200);
    assert_eq!(w.statuses().await[0]["armed"], false);
    // The replay stops too.
    assert!(!w.rt.call(|r| r.replay_state().playing).await.unwrap());
    w.rt.shutdown().await;
}

#[tokio::test]
async fn lost_sync_pauses_every_agent_and_retries_a_failed_pause_without_new_toasts() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let agent = w.agent_on(&account.id, "AUTO").await;
        let id = agent.clone();
        w.rt.call(move |r| r.start_agent(&id).unwrap())
            .await
            .unwrap();
        ids.push(agent);
    }
    // Only the first agent's update is refused; the others must still be paused.
    let first = ids[0].clone();
    w.rt.call(move |r| {
        r.connection()
            .execute_batch(&format!(
                "CREATE TRIGGER refuse_one BEFORE UPDATE ON agents WHEN OLD.id='{first}'                  BEGIN SELECT RAISE(ABORT, 'refused'); END"
            ))
            .unwrap();
    })
    .await
    .unwrap();
    let mut events = w.rt.subscribe();
    w.okx.state().down = true;
    w.rt.sync_exchanges().await.unwrap();
    assert!(w.agent(&ids[0]).await.running);
    assert!(!w.agent(&ids[1]).await.running);
    assert!(!w.agent(&ids[2]).await.running);
    assert_eq!(toasts(&mut events).len(), 1);
    // The store recovers; a later failed sync finishes the pause, quietly.
    w.rt.call(|r| {
        r.connection()
            .execute_batch("DROP TRIGGER refuse_one")
            .unwrap()
    })
    .await
    .unwrap();
    w.rt.sync_exchanges().await.unwrap();
    assert!(!w.agent(&ids[0]).await.running);
    assert_eq!(w.agent(&ids[0]).await.status, "PAUSED");
    assert!(toasts(&mut events).is_empty());
    w.rt.shutdown().await;
}

/// A lost-sync pause that keeps failing to write leaves the agent stored as running. Once sync is
/// back, the live tick must still not run its scheduled cycle, which would trade on an account the
/// owner was told had its agents paused.
#[tokio::test(start_paused = true)]
async fn an_agent_whose_lost_sync_pause_is_still_owed_is_not_scheduled() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let agent = w.agent_on(&account.id, "AUTO").await;
    let id = agent.clone();
    w.rt.call(move |r| {
        r.start_agent(&id).unwrap();
        r.connection()
            .execute(
                "UPDATE agents SET next_run_at=? WHERE id=?",
                rusqlite::params![i64::MAX, id],
            )
            .unwrap();
        // Only the pause is refused; the cycle's own status writes still go through.
        r.connection()
            .execute_batch(
                "CREATE TRIGGER refuse_pause BEFORE UPDATE ON agents WHEN NEW.running=0
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
            )
            .unwrap();
    })
    .await
    .unwrap();
    w.okx.state().down = true;
    w.rt.sync_exchanges().await.unwrap();
    w.okx.state().down = false;
    w.rt.sync_exchanges().await.unwrap();
    assert_eq!(w.statuses().await[0]["healthy"], true);
    assert!(w.agent(&agent).await.running);

    let placed = w.placed();
    let id = agent.clone();
    w.rt.call(move |r| {
        r.connection()
            .execute(
                "UPDATE agents SET next_run_at=0 WHERE id=?",
                rusqlite::params![id],
            )
            .unwrap();
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(3500)).await;
    w.rt.call(|_| ()).await.unwrap();
    assert_eq!(w.placed(), placed);
    w.rt.shutdown().await;
}

/// The owed pause belongs to the account, not the session: connecting the account anew and
/// arming it again does not let the agent that was never paused trade.
#[tokio::test(start_paused = true)]
async fn an_owed_lost_sync_pause_survives_connecting_the_account_anew() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let agent = w.agent_on(&account.id, "AUTO").await;
    let id = agent.clone();
    w.rt.call(move |r| {
        r.start_agent(&id).unwrap();
        r.connection()
            .execute(
                "UPDATE agents SET next_run_at=? WHERE id=?",
                rusqlite::params![i64::MAX, id],
            )
            .unwrap();
        r.connection()
            .execute_batch(
                "CREATE TRIGGER refuse_pause BEFORE UPDATE ON agents WHEN NEW.running=0
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
            )
            .unwrap();
    })
    .await
    .unwrap();
    w.okx.state().down = true;
    w.rt.sync_exchanges().await.unwrap();
    w.okx.state().down = false;
    w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    assert!(w.agent(&agent).await.running);

    let placed = w.placed();
    let id = agent.clone();
    w.rt.call(move |r| {
        r.connection()
            .execute(
                "UPDATE agents SET next_run_at=0 WHERE id=?",
                rusqlite::params![id],
            )
            .unwrap();
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(3500)).await;
    w.rt.call(|_| ()).await.unwrap();
    assert_eq!(w.placed(), placed);
    w.rt.shutdown().await;
}

// ---- restart ----------------------------------------------------------------------------------

#[tokio::test]
async fn forgets_exchange_credentials_and_arming_on_restart_and_never_writes_them_to_disk() {
    let mut w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let agent = w.agent_on(&account.id, "AUTO").await;
    let id = agent.clone();
    w.rt.call(move |r| r.start_agent(&id).unwrap())
        .await
        .unwrap();
    w.place(&account.id).await;
    for value in CREDS {
        assert!(!w.file_holds(value), "{value} on disk");
    }

    w.restart().await;
    let status = &w.statuses().await[0];
    assert_eq!(status["connected"], false);
    assert_eq!(status["armed"], false);
    let id = account.id.clone();
    let error =
        w.rt.call(move |r| r.execution_target(&id).map(|_| ()))
            .await
            .unwrap()
            .unwrap_err();
    assert!(contains(&error, "not connected"));
    let a = w.agent(&agent).await;
    assert!(!a.running);
    assert_eq!(a.status, "PAUSED");
    let error = w.can_run(&agent).await.unwrap_err();
    assert_eq!(error.message(), "OKX is not connected to OKX");

    let reconnected = w.connect("OKX_LIVE").await;
    assert_eq!(reconnected.id, account.id);
    let status = &w.statuses().await[0];
    assert_eq!(status["connected"], true);
    assert_eq!(status["armed"], false);
    w.rt.shutdown().await;
}

// ---- master password --------------------------------------------------------------------------

async fn enable(w: &World) {
    w.rt.enable_vault(PASSWORD.into()).await.unwrap();
}

async fn scrub(w: &World) {
    w.rt.call(|r| mochi_store::scrub(r.connection()).unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn saves_okx_keys_only_on_request_sealed_and_reconnects_them_on_unlock_without_arming() {
    let mut w = World::new().await;
    let error = w.connect_with("OKX_LIVE", true).await.unwrap_err();
    assert_eq!(
        error.message(),
        "Set a master password and unlock it to save OKX keys"
    );
    assert!(w.okx.state().calls.is_empty());
    assert_eq!(w.placed(), 0);

    enable(&w).await;
    let account = w.connect_with("OKX_LIVE", true).await.unwrap();
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let status = &w.statuses().await[0];
    assert_eq!(
        (status["saved"].clone(), status["armed"].clone()),
        (json!(true), json!(true))
    );
    scrub(&w).await;
    for value in CREDS {
        assert!(!w.file_holds(value), "{value} on disk");
    }

    w.restart().await;
    let status = &w.statuses().await[0];
    assert_eq!(
        (status["connected"].clone(), status["saved"].clone()),
        (json!(false), json!(true))
    );
    let outcome = w.rt.unlock(PASSWORD.into()).await.unwrap();
    assert_eq!(outcome.status, VaultStatus::Unlocked);
    assert!(outcome.failed.is_empty());
    let status = &w.statuses().await[0];
    assert_eq!(status["connected"], true);
    assert_eq!(status["armed"], false);
    assert_eq!(status["saved"], true);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn still_refuses_a_saved_key_that_gained_withdrawal_permission() {
    let mut w = World::new().await;
    enable(&w).await;
    w.connect_with("OKX_LIVE", true).await.unwrap();
    w.restart().await;
    w.okx.state().perm = "read_only,withdraw,trade".into();
    let mut events = w.rt.subscribe();
    let (status, body) = w
        .send(
            "POST",
            "/api/vault/unlock",
            Some(json!({"password": PASSWORD})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"status": "UNLOCKED", "failed": ["OKX"]}));
    assert_eq!(w.statuses().await[0]["connected"], false);
    assert!(toasts(&mut events).contains(&(
        ToastLevel::Error,
        "OKX: could not reconnect to OKX. This API key has withdrawal permission. Create a key without it."
            .to_string()
    )));
    w.rt.shutdown().await;
}

#[tokio::test]
async fn unlock_skips_saved_keys_of_accounts_already_connected() {
    let mut w = World::new().await;
    enable(&w).await;
    w.connect_with("OKX_LIVE", true).await.unwrap();
    w.restart().await;
    // Connected by hand while locked, then unlocked: no second connect.
    w.connect("OKX_LIVE").await;
    let configs = |w: &World| {
        w.okx
            .state()
            .calls
            .iter()
            .filter(|c| c.ends_with("/account/config"))
            .count()
    };
    let before = configs(&w);
    let outcome = w.rt.unlock(PASSWORD.into()).await.unwrap();
    assert!(outcome.failed.is_empty());
    assert_eq!(configs(&w), before);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn forgets_saved_okx_keys_on_disconnect_or_when_reconnected_without_saving() {
    let w = World::new().await;
    enable(&w).await;
    let account = w.connect_with("OKX_DEMO", true).await.unwrap();
    w.connect("OKX_DEMO").await;
    assert_eq!(w.statuses().await[0]["saved"], false);
    w.connect_with("OKX_DEMO", true).await.unwrap();
    assert_eq!(w.statuses().await[0]["saved"], true);
    w.rt.call(move |r| r.disconnect_exchange(&account.id).unwrap())
        .await
        .unwrap();
    assert_eq!(w.statuses().await[0]["saved"], false);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn keeps_saved_okx_keys_when_reconnected_by_hand_while_locked() {
    let mut w = World::new().await;
    enable(&w).await;
    w.connect_with("OKX_LIVE", true).await.unwrap();
    w.restart().await;
    assert_eq!(
        w.rt.call(|r| r.vault_status().unwrap()).await.unwrap(),
        VaultStatus::Locked
    );
    w.connect("OKX_LIVE").await;
    let status = &w.statuses().await[0];
    assert_eq!(
        (status["connected"].clone(), status["saved"].clone()),
        (json!(true), json!(true))
    );

    w.restart().await;
    w.rt.unlock(PASSWORD.into()).await.unwrap();
    let status = &w.statuses().await[0];
    assert_eq!(status["connected"], true);
    assert_eq!(status["saved"], true);
    assert_eq!(status["armed"], false);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn turning_it_off_deletes_saved_okx_keys() {
    let mut w = World::new().await;
    enable(&w).await;
    w.connect_with("OKX_DEMO", true).await.unwrap();
    w.rt.disable_vault(PASSWORD.into()).await.unwrap();
    assert_eq!(w.statuses().await[0]["saved"], false);
    w.restart().await;
    assert_eq!(
        w.rt.call(|r| r.vault_status().unwrap()).await.unwrap(),
        VaultStatus::Off
    );
    w.rt.shutdown().await;
}

#[tokio::test]
async fn a_connect_still_out_on_okx_when_the_account_is_disconnected_leaves_it_disconnected() {
    let w = World::new().await;
    enable(&w).await;
    let account = w.connect_with("OKX_DEMO", true).await.unwrap();
    let hold = w.okx.hold_next("GET", "/account/config");
    let (again, ()) = tokio::join!(w.connect_with("OKX_DEMO", true), async {
        hold.reached().await;
        let id = account.id.clone();
        w.rt.call(move |r| r.disconnect_exchange(&id).unwrap())
            .await
            .unwrap();
        hold.release();
    });
    assert!(again
        .unwrap_err()
        .message()
        .contains("disconnected or had its saved key changed while connecting to OKX"));
    let status = &w.statuses().await[0];
    assert_eq!(
        (status["connected"].clone(), status["saved"].clone()),
        (json!(false), json!(false))
    );
    w.rt.shutdown().await;
}

#[tokio::test]
async fn accounts_disconnected_during_the_unlock_reconnect_stay_disconnected_and_forgotten() {
    let mut w = World::new().await;
    enable(&w).await;
    let live = w.connect_with("OKX_LIVE", true).await.unwrap();
    let demo = w.connect_with("OKX_DEMO", true).await.unwrap();
    w.restart().await;
    let mut events = w.rt.subscribe();
    // The first reconnect is held on OKX while the owner disconnects both accounts: the one in
    // flight, and the one still waiting its turn.
    let hold = w.okx.hold_next("GET", "/account/config");
    let (outcome, ()) = tokio::join!(w.rt.unlock(PASSWORD.into()), async {
        hold.reached().await;
        for id in [live.id.clone(), demo.id.clone()] {
            w.rt.call(move |r| r.disconnect_exchange(&id).unwrap())
                .await
                .unwrap();
        }
        hold.release();
    });
    let outcome = outcome.unwrap();
    assert_eq!(outcome.status, VaultStatus::Unlocked);
    assert!(outcome.failed.is_empty());
    for status in w.statuses().await {
        assert_eq!(
            (status["connected"].clone(), status["saved"].clone()),
            (json!(false), json!(false)),
            "{}",
            status["venue"]
        );
    }
    assert!(toasts(&mut events)
        .iter()
        .all(|(_, message)| !message.contains("could not reconnect")));
    w.rt.shutdown().await;
}

/// The owner connects by hand and arms while the unlock reconnect of the same account is held on
/// OKX. The reconnect then finishes, but never replaces the owner's session (which would reset
/// the arming) or brings back the saved key the owner chose to forget.
#[tokio::test]
async fn a_manual_connect_and_arm_during_the_unlock_reconnect_keep_the_owners_session_and_key() {
    for remember in [false, true] {
        let mut w = World::new().await;
        enable(&w).await;
        let live = w.connect_with("OKX_LIVE", true).await.unwrap();
        w.restart().await;
        let mut events = w.rt.subscribe();
        let hold = w.okx.hold_next("GET", "/account/config");
        let (outcome, ()) = tokio::join!(w.rt.unlock(PASSWORD.into()), async {
            hold.reached().await;
            w.connect_with("OKX_LIVE", remember).await.unwrap();
            w.arm(&live.id, ARM_PHRASE).await.unwrap();
            hold.release();
        });
        let outcome = outcome.unwrap();
        assert!(outcome.failed.is_empty(), "remember {remember}");
        let status = &w.statuses().await[0];
        assert_eq!(status["connected"], true, "remember {remember}");
        assert_eq!(status["armed"], true, "remember {remember}");
        assert_eq!(status["saved"], remember, "remember {remember}");
        assert!(toasts(&mut events)
            .iter()
            .all(|(_, message)| !message.contains("could not reconnect")));
        w.rt.shutdown().await;
    }
}

/// A connect refused at registration, because the owner disconnected the account during its
/// first sync, leaves the account's limits as they were.
#[tokio::test]
async fn a_connect_refused_at_registration_leaves_the_accounts_limits_as_they_were() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    let id = account.id.clone();
    w.rt.call(move |r| r.disconnect_exchange(&id).unwrap())
        .await
        .unwrap();
    let limits = |rt: RuntimeHandle| {
        let id = account.id.clone();
        async move {
            rt.call(move |r| {
                r.engine()
                    .store()
                    .get_account(&id)
                    .unwrap()
                    .unwrap()
                    .live_limits
                    .unwrap()
            })
            .await
            .unwrap()
        }
    };
    let before = limits(w.rt.clone()).await;
    let mut input: OkxConnectInput = validate::parse_body(
        Schema::OkxConnect,
        &json!({
            "apiKey": API_KEY, "secretKey": SECRET_KEY, "passphrase": PASSPHRASE,
            "name": "OKX", "environment": "OKX_LIVE", "remember": false,
        }),
    )
    .unwrap();
    input.limits.max_quote_age_sec = before.max_quote_age_sec + 1.0;
    let hold = w.okx.hold_next("GET", "/account/balance");
    let (again, ()) = tokio::join!(w.rt.connect_okx(input, None), async {
        hold.reached().await;
        let id = account.id.clone();
        w.rt.call(move |r| r.disconnect_exchange(&id).unwrap())
            .await
            .unwrap();
        hold.release();
    });
    assert!(again
        .unwrap_err()
        .message()
        .contains("while connecting to OKX"));
    assert_eq!(limits(w.rt.clone()).await, before);
    assert_eq!(w.statuses().await[0]["connected"], false);
    w.rt.shutdown().await;
}

/// A connect asked to save its key under one master password, still out on OKX while the owner
/// resets the vault and sets a new password, saves nothing: the key is never sealed under a vault
/// the box was not ticked for. It still connects.
#[tokio::test]
async fn a_connect_out_on_okx_across_a_vault_reset_and_new_password_saves_no_key() {
    let w = World::new().await;
    enable(&w).await;
    let hold = w.okx.hold_next("GET", "/account/config");
    let (connected, ()) = tokio::join!(w.connect_with("OKX_DEMO", true), async {
        hold.reached().await;
        w.rt.reset_vault(mochi_server::runtime::VAULT_RESET_PHRASE.into())
            .await
            .unwrap();
        w.rt.enable_vault("another password".into()).await.unwrap();
        hold.release();
    });
    assert!(connected
        .unwrap_err()
        .message()
        .contains("the key was not saved"));
    let status = &w.statuses().await[0];
    assert_eq!(
        (status["connected"].clone(), status["saved"].clone()),
        (json!(true), json!(false))
    );
    w.rt.shutdown().await;
}

#[tokio::test]
async fn manual_reconnect_restores_a_deferred_exit_that_had_not_reached_dispatch() {
    reconnect_restores_deferred_exit(false).await;
}

#[tokio::test]
async fn manual_reconnect_retries_restoring_an_unsent_exit_after_a_store_failure() {
    reconnect_restores_deferred_exit(true).await;
}

async fn reconnect_restores_deferred_exit(fail_first: bool) {
    let w = World::new().await;
    w.rt.suspend_periodic_okx_sync_for_test().await.unwrap();
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.okx.state().rest_limit_orders = true;
    let (status, entry) = w
        .send(
            "POST",
            "/api/orders",
            Some(json!({
                "accountId": account.id, "clientOrderId": "reconnect-staged-exit",
                "symbol": "BTCUSDT", "side": "BUY", "type": "LIMIT", "price": "74500",
                "quantity": "0.001", "leverage": 3, "stopLoss": "74000", "takeProfit": "78000",
            })),
        )
        .await;
    assert_eq!(status, 200, "{entry}");
    let id = format!("{}:deferred:tp", entry["order"]["id"].as_str().unwrap());
    w.okx.state().fill_resting();
    let early_reads: Vec<_> = (0..3)
        .map(|_| w.okx.hold_next("GET", "/account/positions"))
        .collect();
    let before_dispatch = w.okx.hold_next("GET", "/account/positions");
    let (old_sync, ()) = tokio::join!(w.rt.sync_exchanges(), async {
        let timeout = Duration::from_secs(2);
        for hold in early_reads {
            tokio::time::timeout(timeout, hold.reached()).await.unwrap();
            hold.release();
        }
        tokio::time::timeout(timeout, before_dispatch.reached())
            .await
            .unwrap();
        let staged_id = id.clone();
        let reason =
            w.rt.call(move |r| {
                r.engine()
                    .store()
                    .get_order(&staged_id)
                    .unwrap()
                    .unwrap()
                    .reject_reason
            })
            .await
            .unwrap()
            .unwrap();
        assert!(reason.contains("submission started"));
        if fail_first {
            let trigger = format!(
                "CREATE TRIGGER refuse_exit_restore BEFORE UPDATE ON orders
                 WHEN OLD.id = '{id}' BEGIN SELECT RAISE(ABORT, 'exit restore refused'); END"
            );
            w.rt.call(move |r| r.connection().execute_batch(&trigger).unwrap())
                .await
                .unwrap();
            // Let the entry's pending invalidation flush before observing this reconnect.
            tokio::time::sleep(Duration::from_millis(400)).await;
            let mut events = w.rt.subscribe();
            let error = w.connect_with("OKX_LIVE", false).await.unwrap_err();
            assert!(
                error.message().contains("exit restore refused"),
                "{error:?}"
            );
            assert!(w.okx.state().algo_placed.is_empty());
            assert_eq!(w.statuses().await[0]["connected"], false);
            tokio::time::timeout(timeout, async {
                loop {
                    if let ServerEvent::Invalidate { topics } = events.recv().await.unwrap() {
                        if topics.contains(&Topic::Exchange) {
                            break;
                        }
                    }
                }
            })
            .await
            .expect("reconnect failure should publish the disconnected state");
            w.rt.call(|r| {
                r.connection()
                    .execute_batch("DROP TRIGGER refuse_exit_restore")
                    .unwrap()
            })
            .await
            .unwrap();
        }
        assert_eq!(w.connect("OKX_LIVE").await.id, account.id);
        assert!(w.okx.state().algo_placed.is_empty());
        w.rt.sync_exchanges().await.unwrap();
        assert_eq!(w.okx.state().algo_placed.len(), 1);
        // The old read now sees a mismatch and attempts to restore its former intent.
        w.okx
            .state()
            .position
            .as_mut()
            .unwrap()
            .insert("tradeId".into(), json!("2"));
        before_dispatch.release();
    });
    old_sync.unwrap();
    assert_eq!(w.okx.state().algo_placed.len(), 1);
    let confirmed =
        w.rt.call(move |r| {
            r.engine()
                .store()
                .get_order(&id)
                .unwrap()
                .unwrap()
                .exchange_order_id
        })
        .await
        .unwrap();
    assert!(confirmed.is_some());
    w.rt.shutdown().await;
}

#[tokio::test]
async fn manual_reconnect_waits_until_registration_to_send_deferred_protection() {
    let w = World::new().await;
    w.rt.suspend_periodic_okx_sync_for_test().await.unwrap();
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.okx.state().rest_limit_orders = true;
    let (status, result) = w
        .send(
            "POST",
            "/api/orders",
            Some(json!({
                "accountId": account.id, "clientOrderId": "reconnect-deferred",
                "symbol": "BTCUSDT", "side": "BUY", "type": "LIMIT", "price": "74500",
                "quantity": "0.001", "leverage": 3, "stopLoss": "74000", "takeProfit": "78000",
            })),
        )
        .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["order"]["status"], "OPEN");
    w.okx.state().fill_resting();
    let old_history = w.okx.hold_next("GET", "/trade/fills");
    let (old_sync, ()) = tokio::join!(w.rt.sync_exchanges(), async {
        tokio::time::timeout(Duration::from_secs(2), old_history.reached())
            .await
            .unwrap();
        let reconnected = w.connect("OKX_LIVE").await;
        assert_eq!(reconnected.id, account.id);
        assert!(w.okx.state().algo_placed.is_empty());
        old_history.release();
    });
    old_sync.unwrap();
    assert!(w.okx.state().algo_placed.is_empty());
    w.rt.sync_exchanges().await.unwrap();
    assert_eq!(w.okx.state().algo_placed.len(), 1);
    w.rt.shutdown().await;
}

/// A sync still out on OKX when the owner connects the account anew must not write what it read
/// over the new session's rows: here its late answer says the entry is still resting, after the
/// new session has recorded it filled.
#[tokio::test]
async fn a_sync_of_a_replaced_session_never_writes_over_the_new_sessions_orders() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.okx.state().rest_limit_orders = true;
    let request: OrderRequest = serde_json::from_value(json!({
        "accountId": account.id, "clientOrderId": uuid::Uuid::new_v4().to_string(),
        "symbol": "BTCUSDT", "side": "BUY", "type": "LIMIT", "price": "74500",
        "quantity": "0.001", "leverage": 3, "stopLoss": "74000",
    }))
    .unwrap();
    let engine = w.rt.execution(&account.id).await.unwrap();
    let placed = engine.place_order(request).await.unwrap();
    assert_eq!(placed.order.status, mochi_domain::OrderStatus::Open);
    let agent = w.agent_on(&account.id, "SIGNAL").await;
    let id = agent.clone();
    w.rt.call(move |r| r.start_agent(&id).unwrap())
        .await
        .unwrap();
    let resting = {
        let state = w.okx.state();
        let (key, row) = state.orders.iter().last().unwrap();
        (key.clone(), row.clone())
    };

    let hold = w.okx.hold_next("GET", "/trade/order");
    let mut events = w.rt.subscribe();
    let (synced, ()) = tokio::join!(w.rt.sync_exchanges(), async {
        hold.reached().await;
        w.okx.state().fill_resting();
        w.connect("OKX_LIVE").await;
        // The old session's lookup was answered before the fill, and arrives only now.
        w.okx
            .state()
            .orders
            .insert(resting.0.clone(), resting.1.clone());
        hold.release();
    });
    synced.unwrap();
    let id = placed.order.id.clone();
    let row =
        w.rt.call(move |r| r.engine().store().get_order(&id).unwrap().unwrap())
            .await
            .unwrap();
    assert_eq!(row.status, mochi_domain::OrderStatus::Filled);
    // The old session's refused sync is not the new session's: its agents keep running, and
    // nothing says sync was lost.
    assert!(w.agent(&agent).await.running);
    assert_eq!(w.statuses().await[0]["healthy"], true);
    assert!(toasts(&mut events)
        .iter()
        .all(|(_, message)| !message.contains("lost sync")));
    w.rt.shutdown().await;
}

#[tokio::test]
async fn reconnect_keeps_reduce_only_exit_available_when_okx_protection_reads_fail() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    let agent = w.agent_on(&account.id, "SIGNAL").await;
    let id = agent.clone();
    w.rt.call(move |r| r.start_agent(&id).unwrap())
        .await
        .unwrap();
    assert!(w.agent(&agent).await.running);
    {
        let mut okx = w.okx.state();
        okx.algo_lookup_down = true;
        okx.position = Some(
            json!({
                "instId": "BTC-USDT-SWAP", "pos": "0.1", "avgPx": "75000", "markPx": "75000",
                "upl": "0", "lever": "3", "liqPx": "50000", "margin": "10", "mgnMode": "isolated",
                "posSide": "net", "tradeId": "1",
            })
            .as_object()
            .unwrap()
            .clone(),
        );
    }
    let again = w.connect("OKX_LIVE").await;
    assert_eq!(again.id, account.id);
    let statuses = w.statuses().await;
    assert_eq!(statuses[0]["connected"], true);
    assert_eq!(statuses[0]["healthy"], false);
    assert_eq!(statuses[0]["armed"], false);
    assert!(!w.agent(&agent).await.running);
    for action in ["start", "resume"] {
        let (status, result) = w
            .send("POST", &format!("/api/agents/{agent}/{action}"), None)
            .await;
        assert_eq!(status, 400, "{result}");
        assert!(!w.agent(&agent).await.running);
    }
    w.rt.sync_exchanges().await.unwrap();
    assert!(!w.agent(&agent).await.running);
    let (_, positions) = w
        .send(
            "GET",
            &format!("/api/accounts/{}/positions", account.id),
            None,
        )
        .await;
    assert_eq!(positions[0]["symbol"], "BTCUSDT");
    assert_eq!(positions[0]["side"], "LONG");
    assert!(positions[0]["id"].as_str().is_some_and(|id| !id.is_empty()));
    let placed = w.placed();
    let (status, result) = w
        .send(
            "POST",
            &format!("/api/positions/{}/BTCUSDT/close", account.id),
            Some(json!({ "fraction": 1, "clientOrderId": "degraded-close", "positionId": positions[0]["id"] })),
        )
        .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["order"]["status"], "FILLED");
    assert_eq!(result["order"]["reduceOnly"], true);
    assert_eq!(w.placed(), placed + 1);
    assert!(w.okx.state().position.is_none());
    w.okx.state().algo_lookup_down = false;
    w.rt.sync_exchanges().await.unwrap();
    let (status, result) = w
        .send("POST", &format!("/api/agents/{agent}/resume"), None)
        .await;
    assert_eq!(status, 200, "{result}");
    assert!(w.agent(&agent).await.running);
    w.rt.shutdown().await;
}

#[tokio::test]
async fn reconnect_keeps_an_unanswered_market_target_and_allows_manual_exit() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let result = w.place(&account.id).await;
    let mut target: mochi_domain::Order = serde_json::from_value(result["order"].clone()).unwrap();
    target.id = format!("{}:deferred:tp", target.id);
    target.client_order_id = target.id.clone();
    target.side = mochi_domain::OrderSide::Sell;
    target.reduce_only = true;
    target.close_position = false;
    target.order_type = mochi_domain::OrderType::Market;
    target.trigger_price = Some("78000".into());
    target.status = mochi_domain::OrderStatus::Open;
    target.filled_quantity = "0".into();
    target.avg_fill_price = None;
    target.exchange_order_id = None;
    target.reject_reason = Some("Awaiting confirmation: OKX 50004".into());
    let target_id = target.id.clone();
    w.rt.call(move |r| r.engine().store().insert_order(&target).unwrap())
        .await
        .unwrap();
    w.okx.state().lookup_error_code = Some("50001".into());

    assert_eq!(w.connect("OKX_LIVE").await.id, account.id);
    assert_eq!(w.statuses().await[0]["healthy"], false);
    let (_, positions) = w
        .send(
            "GET",
            &format!("/api/accounts/{}/positions", account.id),
            None,
        )
        .await;
    assert_eq!(positions[0]["side"], "LONG");
    let placed = w.placed();
    let (status, result) = w.send("POST", &format!("/api/positions/{}/BTCUSDT/close", account.id), Some(json!({
        "fraction": 1, "clientOrderId": "manual-after-unknown-target", "positionId": positions[0]["id"]
    }))).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(w.placed(), placed + 1);
    assert!(w.okx.state().position.is_none());
    let target =
        w.rt.call(move |r| r.engine().store().get_order(&target_id).unwrap().unwrap())
            .await
            .unwrap();
    assert_eq!(target.status, mochi_domain::OrderStatus::Open);
    w.rt.shutdown().await;
}

/// A reconnect whose new limits cannot be saved changes nothing: the old session stays
/// connected with its agents and limits, so its positions can still be closed through it.
#[tokio::test]
async fn a_reconnect_that_cannot_save_its_limits_keeps_the_current_session() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.place(&account.id).await;
    let agent = w.agent_on(&account.id, "AUTO").await;
    let id = agent.clone();
    w.rt.call(move |r| r.start_agent(&id).unwrap())
        .await
        .unwrap();
    let id = account.id.clone();
    let limits = move |w: &World| {
        let id = id.clone();
        let rt = w.rt.clone();
        async move {
            rt.call(move |r| {
                r.engine()
                    .store()
                    .get_account(&id)
                    .unwrap()
                    .unwrap()
                    .live_limits
                    .unwrap()
            })
            .await
            .unwrap()
        }
    };
    let before = limits(&w).await;
    let mut input: OkxConnectInput = validate::parse_body(
        Schema::OkxConnect,
        &json!({
            "apiKey": API_KEY, "secretKey": SECRET_KEY, "passphrase": PASSPHRASE,
            "name": "OKX", "environment": "OKX_LIVE", "remember": false,
        }),
    )
    .unwrap();
    input.limits.max_quote_age_sec = before.max_quote_age_sec + 1.0;
    let hold = w.okx.hold_next("GET", "/account/balance");
    let (again, ()) = tokio::join!(w.rt.connect_okx(input, None), async {
        tokio::time::timeout(std::time::Duration::from_secs(2), hold.reached())
            .await
            .unwrap();
        w.rt.call(|r| {
            r.connection()
                .execute_batch(
                    "CREATE TRIGGER refuse_profile BEFORE UPDATE OF live_limits ON accounts
                     BEGIN SELECT RAISE(ABORT, 'profile refused'); END",
                )
                .unwrap()
        })
        .await
        .unwrap();
        hold.release();
    });
    assert!(again.is_err());
    assert_eq!(w.statuses().await[0]["connected"], true);
    assert_eq!(w.statuses().await[0]["armed"], true);
    assert_eq!(limits(&w).await, before);
    assert!(w.agent(&agent).await.running);

    let (_, positions) = w
        .send(
            "GET",
            &format!("/api/accounts/{}/positions", account.id),
            None,
        )
        .await;
    let placed = w.placed();
    let (status, result) = w
        .send(
            "POST",
            &format!("/api/positions/{}/BTCUSDT/close", account.id),
            Some(json!({ "fraction": 1, "clientOrderId": "close-after-failed-reconnect", "positionId": positions[0]["id"] })),
        )
        .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(w.placed(), placed + 1);
    assert!(w.okx.state().position.is_none());
    w.rt.shutdown().await;
}

/// An OKX position is only read back on sync: the agent's own order shows there one sync later
/// and does not wake it, while a stop that fills on OKX does, on the sync that sees it.
#[tokio::test(start_paused = true)]
async fn a_position_closed_on_okx_runs_the_agent_on_the_sync_that_sees_it() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let agent = w.agent_on(&account.id, "AUTO").await;
    let id = agent.clone();
    w.rt.call(move |r| {
        r.start_agent(&id).unwrap();
        // Far in the future, so only a position change runs it.
        r.connection()
            .execute(
                "UPDATE agents SET next_run_at=? WHERE id=?",
                rusqlite::params![i64::MAX, id],
            )
            .unwrap();
    })
    .await
    .unwrap();
    let calls = || w.seen.lock().unwrap().len();
    let record = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(record["outcome"], "EXECUTED", "{record}");
    assert!(w.okx.state().position.is_some());
    *w.answer.lock().unwrap() = decision("HOLD", 74_250.0, None);
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(5500)).await;
        w.rt.call(|_| ()).await.unwrap();
    }
    assert_eq!(calls(), 1, "its own order");

    // The stop filled on OKX: the position is gone on the next sync.
    w.okx.state().position = None;
    tokio::time::sleep(Duration::from_millis(5500)).await;
    w.rt.call(|_| ()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    w.rt.call(|_| ()).await.unwrap();
    assert_eq!(calls(), 2);
    let last =
        w.rt.call(move |r| {
            let records =
                mochi_store::rows::list_decisions(r.connection(), Some(&agent), 1).unwrap();
            serde_json::from_str::<Value>(records[0].get()).unwrap()
        })
        .await
        .unwrap();
    assert_eq!(last["trigger"], "POSITION_CHANGE");
    assert_eq!(last["outcome"], "HOLD", "{last}");
    tokio::time::sleep(Duration::from_millis(11_000)).await;
    w.rt.call(|_| ()).await.unwrap();
    assert_eq!(calls(), 2, "one run per change");
    w.rt.shutdown().await;
}

/// A running OKX agent whose next scheduled run is far away, so only a position change runs it.
async fn waiting_okx_agent(w: &World, account_id: &str) -> String {
    let agent = w.agent_on(account_id, "AUTO").await;
    let id = agent.clone();
    w.rt.call(move |r| {
        r.start_agent(&id).unwrap();
        r.connection()
            .execute(
                "UPDATE agents SET next_run_at=? WHERE id=?",
                rusqlite::params![i64::MAX, id],
            )
            .unwrap();
    })
    .await
    .unwrap();
    agent
}

/// One live tick with its sync, and the cycle it may start.
async fn sync_tick(w: &World) {
    tokio::time::sleep(Duration::from_millis(5500)).await;
    w.rt.call(|_| ()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    w.rt.call(|_| ()).await.unwrap();
}

async fn last_trigger(w: &World, agent: &str) -> Value {
    let agent = agent.to_string();
    w.rt.call(move |r| {
        let records = mochi_store::rows::list_decisions(r.connection(), Some(&agent), 1).unwrap();
        serde_json::from_str::<Value>(records[0].get()).unwrap()["trigger"].clone()
    })
    .await
    .unwrap()
}

/// An entry that fills and is stopped out on OKX before any sync sees it is flat both times, yet
/// it changed: the agent still runs.
#[tokio::test(start_paused = true)]
async fn an_okx_entry_stopped_out_before_the_next_sync_still_runs_the_agent() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    let agent = waiting_okx_agent(&w, &account.id).await;
    let record = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(record["outcome"], "EXECUTED", "{record}");
    w.okx.state().position = None;
    *w.answer.lock().unwrap() = decision("HOLD", 74_250.0, None);
    sync_tick(&w).await;
    assert_eq!(w.seen.lock().unwrap().len(), 2);
    assert_eq!(last_trigger(&w, &agent).await, "POSITION_CHANGE");
    sync_tick(&w).await;
    assert_eq!(w.seen.lock().unwrap().len(), 2);
    w.rt.shutdown().await;
}

/// A refused order moved nothing, so a close by the owner right after it still runs the agent.
#[tokio::test(start_paused = true)]
async fn a_refused_okx_order_does_not_hide_the_owners_close() {
    let w = World::new().await;
    let account = w.connect("OKX_LIVE").await;
    w.arm(&account.id, ARM_PHRASE).await.unwrap();
    w.place(&account.id).await;
    w.rt.sync_exchanges().await.unwrap();
    let agent = waiting_okx_agent(&w, &account.id).await;
    sync_tick(&w).await;
    assert_eq!(
        w.seen.lock().unwrap().len(),
        0,
        "a position it finds on start is no change"
    );
    *w.answer.lock().unwrap() = decision("CLOSE", 74_250.0, None);
    w.okx.state().reject_next = Some(mochi_okx::fake::Refusal::new("51008", "Refused"));
    let record = w.cycle(&agent, CycleTrigger::RunOnce).await;
    assert_eq!(record["outcome"], "ORDER_REJECTED", "{record}");
    *w.answer.lock().unwrap() = decision("HOLD", 74_250.0, None);
    w.okx.state().position = None;
    sync_tick(&w).await;
    assert_eq!(w.seen.lock().unwrap().len(), 2);
    assert_eq!(last_trigger(&w, &agent).await, "POSITION_CHANGE");
    w.rt.shutdown().await;
}
