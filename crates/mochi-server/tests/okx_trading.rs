//! Orders and positions on an OKX account reach the OKX engine through the `ExecutionEngine`
//! trait, against the in-process fake OKX with an injected clock. Connecting is not ported yet,
//! so the session comes from the runtime's test seam; the paper paths keep their own tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use mochi_ai::{AiProvider, AiRequest, AiResponse, CompletionFuture, ProviderKind};
use mochi_core::paper::{CreateAccountArgs, PaperStore};
use mochi_domain::{
    default_account_config, default_live_limits, default_risk_settings, AccountVenue, Candle,
    DataSource, FeedState, FeedStatus, InstrumentSpecification, PaperAccount, Quote, Timeframe,
};
use mochi_okx::fake::{self, FakeOkx};
use mochi_server::config::{config_from, Config};
use mochi_server::runtime::agents::CycleTrigger;
use mochi_server::runtime::{live_feed::FeedResult, LiveFeed, OkxOptions, ProtectionMeta};
use mochi_server::{app, AppState, RuntimeHandle, RuntimeOptions};
use mochi_store::{open_in_memory, rows};
use serde_json::{json, Value};
use tower::ServiceExt;

const HOST: &str = "localhost:8787";
const ACCOUNT: &str = "acct_okx";
/// 2027-01-15T08:00:00Z, the OKX engine's clock in these tests.
const START: i64 = 1_800_000_000_000;

/// Quotes for any pair at fixed prices, stamped with the shared clock.
struct Feed {
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    prices: Mutex<(String, String, String)>,
}

impl LiveFeed for Feed {
    fn track(&self, _: &str) {}
    fn untrack(&self, _: &str) {}
    fn get_quote(&self, symbol: &str) -> Option<Quote> {
        let (bid, ask, last) = self.prices.lock().unwrap().clone();
        Some(Quote {
            source: DataSource::Live,
            symbol: symbol.into(),
            bid,
            ask,
            mark: last.clone(),
            last,
            mark_estimated: false,
            funding_rate: None,
            next_funding_time: None,
            ts: (self.clock)(),
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
        // Node's FakeFeed: 60 closed 15-minute bars at 75000.
        let size = 900_000;
        let last = ((self.clock)() / size) * size - size;
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

fn wall_clock() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn config() -> Config {
    let mut config = config_from(|_| None);
    config.data_dir = PathBuf::from(".");
    config
}

struct World {
    state: AppState,
    okx: Arc<FakeOkx>,
    clock: Arc<AtomicI64>,
}

fn okx_account() -> PaperAccount {
    PaperAccount {
        id: ACCOUNT.into(),
        name: "OKX".into(),
        source: DataSource::Live,
        venue: AccountVenue::OkxLive,
        exchange_uid: Some("42".into()),
        live_limits: Some(default_live_limits()),
        is_demo: false,
        initial_balance: "1000".into(),
        wallet_balance: "0".into(),
        used_margin: "0".into(),
        realized_pnl: "0".into(),
        fees_paid: "0".into(),
        funding_paid: "0".into(),
        config: default_account_config(),
        created_at: START,
    }
}

/// A runtime whose OKX goes to the fake, with an OKX_LIVE account connected through the seam.
async fn world(armed: bool) -> World {
    let okx = FakeOkx::new();
    let clock = Arc::new(AtomicI64::new(START));
    let now: Arc<dyn Fn() -> i64 + Send + Sync> = {
        let clock = clock.clone();
        Arc::new(move || clock.load(Ordering::SeqCst))
    };
    let mut options = RuntimeOptions::new(open_in_memory().unwrap());
    options.replay_minutes = Some(mochi_market::tape::WARMUP_MINUTES + 30);
    options.live = Arc::new(Feed {
        clock: now.clone(),
        prices: Mutex::new(("74999".into(), "75001".into(), "75000".into())),
    });
    options.okx = OkxOptions {
        transport: Some(okx.transport()),
        base_url: Some("https://okx.invalid".into()),
        now,
    };
    let runtime = RuntimeHandle::start(options).unwrap();
    runtime
        .call(|r| r.engine().store().insert_account(&okx_account()).unwrap())
        .await
        .unwrap();
    runtime
        .attach_okx_for_test(ACCOUNT, fake::credentials(), armed)
        .await
        .unwrap();
    World {
        state: AppState::new(Arc::new(config()), runtime),
        okx,
        clock,
    }
}

async fn send(state: &AppState, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", HOST)
        .header("authorization", format!("Bearer {}", state.token));
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let response = app(state.clone())
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn order(client: &str) -> Value {
    json!({
        "accountId": ACCOUNT, "clientOrderId": client, "symbol": "BTCUSDT", "side": "BUY",
        "type": "MARKET", "quantity": "0.001", "leverage": 3, "stopLoss": "74000",
    })
}

#[tokio::test]
async fn an_order_on_an_okx_account_goes_to_okx_and_is_refused_until_armed() {
    let w = world(false).await;
    let (status, refused) = send(&w.state, "POST", "/api/orders", Some(order("a"))).await;
    assert_eq!(status, 200);
    assert_eq!(refused["order"]["status"], "REJECTED");
    assert_eq!(
        refused["order"]["rejectReason"],
        "NOT_ARMED: real-money orders are switched off for this account"
    );
    assert!(w.okx.state().placed.is_empty());

    w.state
        .runtime
        .set_armed_for_test(ACCOUNT, true)
        .await
        .unwrap();
    let (status, filled) = send(&w.state, "POST", "/api/orders", Some(order("b"))).await;
    assert_eq!(status, 200);
    assert_eq!(filled["order"]["status"], "FILLED");
    assert_eq!(filled["duplicate"], false);
    assert_eq!(filled["fills"][0]["id"], "fill_okx_1");
    assert_eq!(w.okx.state().placed.len(), 1);
    assert_eq!(w.okx.state().placed[0]["sz"], "0.1");

    // The same idempotency key again: the first order, nothing sent.
    let (_, again) = send(&w.state, "POST", "/api/orders", Some(order("b"))).await;
    assert_eq!(again["duplicate"], true);
    assert_eq!(again["order"]["id"], filled["order"]["id"]);
    assert_eq!(w.okx.state().placed.len(), 1);
    w.state.runtime.shutdown().await;
}

#[tokio::test]
async fn positions_and_balance_of_a_connected_okx_account_come_from_okx() {
    let w = world(true).await;
    send(&w.state, "POST", "/api/orders", Some(order("a"))).await;
    let (status, positions) = send(
        &w.state,
        "GET",
        &format!("/api/accounts/{ACCOUNT}/positions"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    let positions = positions.as_array().unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0]["id"], "okx:BTC-USDT-SWAP");
    assert_eq!(positions[0]["quantity"], "0.001");
    assert_eq!(positions[0]["side"], "LONG");

    let (_, accounts) = send(&w.state, "GET", "/api/accounts", None).await;
    let balance = &accounts[0]["balance"];
    assert_eq!(balance["equity"], "1000");
    assert_eq!(balance["availableBalance"], "1000");
    assert_eq!(balance["feesPaid"], "0.05");
    w.state.runtime.shutdown().await;
}

#[tokio::test]
async fn a_disarmed_okx_account_can_still_close_and_cancel_refusals_keep_node_texts() {
    let w = world(true).await;
    let (_, filled) = send(&w.state, "POST", "/api/orders", Some(order("a"))).await;
    w.state
        .runtime
        .set_armed_for_test(ACCOUNT, false)
        .await
        .unwrap();

    let id = filled["order"]["id"].as_str().unwrap();
    let (status, error) = send(&w.state, "DELETE", &format!("/api/orders/{id}"), None).await;
    assert_eq!(
        (status, error),
        (400, json!({ "error": format!("Order {id} is FILLED") }))
    );

    let (status, closed) = send(
        &w.state,
        "POST",
        &format!("/api/positions/{ACCOUNT}/BTCUSDT/close"),
        Some(json!({ "fraction": 1, "clientOrderId": "close" })),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(closed["order"]["status"], "FILLED");
    assert_eq!(closed["order"]["reduceOnly"], true);
    assert_eq!(w.okx.state().placed[1]["reduceOnly"], true);
    assert!(w.okx.state().position.is_none());

    let (status, error) = send(
        &w.state,
        "POST",
        &format!("/api/positions/{ACCOUNT}/ETHUSDT/close"),
        Some(json!({ "fraction": 1, "clientOrderId": "close-2" })),
    )
    .await;
    assert_eq!(
        (status, error),
        (400, json!({ "error": "No open ETHUSDT position" }))
    );
    w.state.runtime.shutdown().await;
}

/// The engine runs off the runtime task: while OKX is slow to answer, the runtime still serves
/// other requests, and a lost answer leaves the order OPEN until a sync resolves it.
#[tokio::test]
async fn a_lost_answer_stays_open_until_sync_resolves_it_and_is_never_resent() {
    let w = world(true).await;
    {
        let mut okx = w.okx.state();
        okx.lose_next_response = true;
        // The lookup straight after the lost answer fails too.
        okx.lookup_error_code = Some("50001".into());
    }
    let (_, pending) = send(&w.state, "POST", "/api/orders", Some(order("a"))).await;
    assert_eq!(pending["order"]["status"], "OPEN");
    assert_eq!(
        pending["order"]["rejectReason"],
        "Awaiting confirmation: No answer from OKX: socket hang up"
    );
    w.okx.state().lookup_error_code = None;
    w.clock.fetch_add(1_000, Ordering::SeqCst);
    w.state.runtime.sync_okx_for_test(ACCOUNT).await.unwrap();
    let (_, orders) = send(
        &w.state,
        "GET",
        &format!("/api/accounts/{ACCOUNT}/orders"),
        None,
    )
    .await;
    // The entry, and the stop row its fill created.
    let entry = orders
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == pending["order"]["id"])
        .unwrap();
    assert_eq!(entry["status"], "FILLED");
    assert_eq!(entry["rejectReason"], Value::Null);
    let stop_row = orders
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["closePosition"] == true)
        .unwrap();
    assert_eq!(
        stop_row["id"],
        format!("{}:sl", pending["order"]["id"].as_str().unwrap())
    );
    assert_eq!(stop_row["status"], "OPEN");
    assert_eq!(w.okx.state().placed.len(), 1);
    w.state.runtime.shutdown().await;
}

#[tokio::test]
async fn an_exchange_account_without_a_session_still_answers_501() {
    let w = world(true).await;
    w.state
        .runtime
        .call(|r| {
            let mut other = okx_account();
            other.id = "acct_other".into();
            r.engine().store().insert_account(&other).unwrap()
        })
        .await
        .unwrap();
    let mut request = order("x");
    request["accountId"] = json!("acct_other");
    let (status, _) = send(&w.state, "POST", "/api/orders", Some(request)).await;
    assert_eq!(status, 501);
    w.state.runtime.shutdown().await;
}

// ---- added pairs: "keeps exponent notation out of an agent order" ------------------------------

struct Tiny;

impl AiProvider for Tiny {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Openai
    }
    fn complete<'a>(&'a self, _: &'a AiRequest) -> CompletionFuture<'a> {
        Box::pin(async {
            Ok(AiResponse {
                text: json!({
                    "symbol": "PEPEUSDT", "action": "LONG", "orderType": "MARKET",
                    "entryPrice": null, "stopLoss": 0.0000009, "takeProfit": null,
                    "reason": "test", "confidence": 0.5, "expiresAt": null,
                })
                .to_string(),
                input_tokens: Some(1.0),
                output_tokens: Some(1.0),
            })
        })
    }
}

fn pepe() -> InstrumentSpecification {
    mochi_market::instrument_parse::parse_okx_instrument(&json!({
        "instId": "PEPE-USDT-SWAP", "settleCcy": "USDT", "ctType": "linear", "state": "live",
        "ruleType": "normal", "instCategory": "1", "ctValCcy": "PEPE", "ctVal": "10000000",
        "lotSz": "0.1", "minSz": "0.1", "tickSz": "0.0000000001", "lever": "50",
    }))
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn keeps_exponent_notation_out_of_an_agent_order() {
    let mut options = RuntimeOptions::new(open_in_memory().unwrap());
    options.replay_minutes = Some(mochi_market::tape::WARMUP_MINUTES + 30);
    options.live = Arc::new(Feed {
        clock: Arc::new(wall_clock),
        prices: Mutex::new(("0.00000099".into(), "0.00000101".into(), "0.000001".into())),
    });
    options.provider_factory = Some(Arc::new(|_, _| Ok(Arc::new(Tiny))));
    let runtime = RuntimeHandle::start(options).unwrap();
    let account = runtime
        .call(|r| {
            r.add_instrument(pepe()).unwrap();
            let account = r
                .engine_mut()
                .create_account(CreateAccountArgs {
                    name: "paper".into(),
                    source: DataSource::Live,
                    initial_balance: None,
                    is_demo: None,
                    config: None,
                })
                .unwrap();
            rows::insert_provider(
                r.connection(),
                &serde_json::from_value(json!({
                    "id": "remote", "name": "r", "kind": "OPENAI", "baseUrl": null,
                    "defaultModel": "m", "inputPricePerMTok": null, "outputPricePerMTok": null,
                    "createdAt": 1,
                }))
                .unwrap(),
            )
            .unwrap();
            r.set_key("remote", "ai-key").unwrap();
            rows::insert_agent(
                r.connection(),
                &rows::Agent {
                    id: "pepe".into(),
                    name: "pepe".into(),
                    provider_id: "remote".into(),
                    model: "m".into(),
                    symbol: "PEPEUSDT".into(),
                    timeframe: "15m".into(),
                    strategy: String::new(),
                    account_id: account.id.clone(),
                    risk: serde_json::value::to_raw_value(&default_risk_settings()).unwrap(),
                    interval_sec: 60,
                    execution_mode: "AUTO".into(),
                    use_news: false,
                    allow_trailing_stop: false,
                    status: "IDLE".into(),
                    running: false,
                    next_run_at: None,
                    last_run_at: None,
                    status_detail: None,
                    consecutive_errors: 0,
                    is_demo: false,
                    created_at: 1,
                },
            )
            .unwrap();
            account.id
        })
        .await
        .unwrap();
    let record = runtime
        .run_cycle("pepe", CycleTrigger::RunOnce)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record["outcome"], "EXECUTED", "{record}");
    let placed = runtime
        .call(move |r| r.engine().store().list_orders(&account, 10).unwrap())
        .await
        .unwrap()
        .into_iter()
        .find(|o| o.order_type == mochi_domain::OrderType::Market)
        .unwrap();
    assert_eq!(placed.attached_stop_loss.as_deref(), Some("0.0000009"));
    runtime.shutdown().await;
}

// ---- protection on OKX, through the route and the trait ----------------------------------------

fn stop(price: &str) -> mochi_domain::Protection {
    mochi_domain::Protection {
        stop_loss: Some(price.into()),
        take_profit: None,
        trailing_stop: None,
    }
}

fn meta(id: &str) -> ProtectionMeta {
    ProtectionMeta {
        client_order_id: format!("protect:{id}"),
        agent_id: Some("agent_1".into()),
        decision_id: Some(id.into()),
    }
}

/// A connected, armed account holding a long with a stop at 74000 attached on entry.
async fn world_with_position() -> World {
    let w = world(true).await;
    let (_, filled) = send(&w.state, "POST", "/api/orders", Some(order("entry"))).await;
    assert_eq!(filled["order"]["status"], "FILLED");
    w.okx.state().calls.clear();
    w
}

fn posts(w: &World) -> Vec<String> {
    w.okx
        .state()
        .calls
        .iter()
        .filter(|c| c.starts_with("POST"))
        .cloned()
        .collect()
}

#[tokio::test]
async fn api_protection_route_on_an_okx_account_goes_through_the_armed_gate() {
    let w = world_with_position().await;
    let path = format!("/api/positions/{ACCOUNT}/BTCUSDT/protection");
    w.state
        .runtime
        .set_armed_for_test(ACCOUNT, false)
        .await
        .unwrap();
    let (status, refused) = send(
        &w.state,
        "PUT",
        &path,
        Some(json!({ "stopLoss": "73000", "takeProfit": null })),
    )
    .await;
    assert_eq!(status, 400);
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("NOT_ARMED: this change increases risk"));
    assert!(posts(&w).is_empty());

    let (status, tightened) = send(
        &w.state,
        "PUT",
        &path,
        Some(json!({ "stopLoss": "74500", "takeProfit": null })),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(tightened["ok"], true);
    assert_eq!(tightened["pending"], false);
    assert_eq!(
        tightened["after"],
        json!({ "stopLoss": "74500", "takeProfit": null, "trailingStop": null })
    );
    assert_eq!(w.okx.state().algo_placed.len(), 1);

    // The position view reads the new stop back from OKX.
    let (_, positions) = send(
        &w.state,
        "GET",
        &format!("/api/accounts/{ACCOUNT}/positions"),
        None,
    )
    .await;
    assert_eq!(positions[0]["stopLoss"], "74500");
    assert_eq!(positions[0]["externalProtection"], json!([]));
    w.state.runtime.shutdown().await;
}

/// What the runner will meet once it trades through the trait: a placement OKX did not answer is
/// pending, with the old stop still in force.
#[tokio::test]
async fn the_trait_reports_an_unanswered_placement_as_pending() {
    let w = world_with_position().await;
    {
        let mut okx = w.okx.state();
        okx.lose_next_algo_response = true;
        okx.algo_lookup_down = true;
    }
    let engine = w.state.runtime.execution(ACCOUNT).await.unwrap();
    let result = engine
        .set_protection(ACCOUNT.into(), "BTCUSDT".into(), stop("74500"), meta("d1"))
        .await
        .unwrap();
    assert!(result.pending);
    assert_eq!(result.before, stop("74000"));
    assert_eq!(result.after, stop("74000"));
    // A second change waits for the first to resolve.
    let refused = engine
        .set_protection(ACCOUNT.into(), "BTCUSDT".into(), stop("74600"), meta("d2"))
        .await
        .unwrap_err();
    assert_eq!(refused.error.status().as_u16(), 400);
    assert_eq!(
        refused.error.message(),
        "PROTECTION_PENDING: an earlier protection change is awaiting confirmation from OKX"
    );
    assert_eq!(refused.after, None);
    // The row carries who asked.
    let row = w
        .state
        .runtime
        .call(|r| {
            r.engine()
                .store()
                .list_open_orders(ACCOUNT)
                .unwrap()
                .into_iter()
                .find(|o| o.trigger_price.as_deref() == Some("74500"))
                .unwrap()
        })
        .await
        .unwrap();
    assert_eq!(row.agent_id.as_deref(), Some("agent_1"));
    assert_eq!(row.decision_id.as_deref(), Some("d1"));
    w.state.runtime.shutdown().await;
}

/// A change refused part-way: the failure carries what is in force, including the leg placed.
#[tokio::test]
async fn the_trait_carries_what_is_in_force_after_a_part_way_refusal() {
    let w = world_with_position().await;
    w.okx.state().reject_algo_if = Some(Box::new(|body| body["ordType"] == "move_order_stop"));
    let engine = w.state.runtime.execution(ACCOUNT).await.unwrap();
    let target = mochi_domain::Protection {
        trailing_stop: Some(mochi_domain::TrailingStopSpec {
            kind: mochi_domain::TrailingKind::Ratio,
            callback: "0.01".into(),
            activation_price: None,
        }),
        ..stop("74500")
    };
    let failure = engine
        .set_protection(ACCOUNT.into(), "BTCUSDT".into(), target, meta("d1"))
        .await
        .unwrap_err();
    assert_eq!(failure.error.status().as_u16(), 400);
    assert_eq!(failure.error.message(), "OKX 51000: Parameter error");
    assert_eq!(failure.after, Some(stop("74500")));
    assert_eq!(
        engine
            .protection_of(ACCOUNT.into(), "BTCUSDT".into())
            .await
            .unwrap(),
        stop("74500")
    );
    w.state.runtime.shutdown().await;
}

/// The engine aligns prices to the tick, so a target that differs only off the tick comes back
/// unchanged and sends nothing: the runner reads that as "already as requested".
#[tokio::test]
async fn the_trait_returns_an_unchanged_result_for_a_target_off_the_tick() {
    let w = world_with_position().await;
    let engine = w.state.runtime.execution(ACCOUNT).await.unwrap();
    let result = engine
        .set_protection(
            ACCOUNT.into(),
            "BTCUSDT".into(),
            stop("74000.04"),
            meta("d1"),
        )
        .await
        .unwrap();
    assert!(!result.pending);
    assert_eq!(result.before, result.after);
    assert_eq!(result.after, stop("74000"));
    assert!(posts(&w).is_empty());
    w.state.runtime.shutdown().await;
}
