//! Orders and positions on an OKX account reach the OKX engine through the `ExecutionEngine`
//! trait, against the in-process fake OKX with an injected clock. The session comes from the
//! `test-seams` feature, so each test sets its own account; connecting is in `okx_runtime.rs`.

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
    assert_eq!(positions[0]["id"], "okx:BTC-USDT-SWAP:isolated:LONG::1");
    assert_eq!(positions[0]["marginMode"], "isolated");
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

    let (_, positions) = send(
        &w.state,
        "GET",
        &format!("/api/accounts/{ACCOUNT}/positions"),
        None,
    )
    .await;
    let (status, closed) = send(
        &w.state,
        "POST",
        &format!("/api/positions/{ACCOUNT}/BTCUSDT/close"),
        Some(json!({ "fraction": 1, "clientOrderId": "close", "positionId": positions[0]["id"] })),
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
        Some(
            json!({ "fraction": 1, "clientOrderId": "close-2", "positionId": positions[0]["id"] }),
        ),
    )
    .await;
    assert_eq!(
        (status, error),
        (
            400,
            json!({ "error": "The selected OKX position has changed. Refresh and try again." })
        )
    );
    w.state.runtime.shutdown().await;
}

#[tokio::test]
async fn generic_okx_reduce_only_orders_require_the_selected_close_route() {
    let w = world(true).await;
    let (_, opened) = send(&w.state, "POST", "/api/orders", Some(order("entry"))).await;
    let mut exit = order("exit");
    exit["side"] = json!("SELL");
    exit["reduceOnly"] = json!(true);
    exit["stopLoss"] = Value::Null;
    w.okx
        .state()
        .position
        .as_mut()
        .unwrap()
        .insert("tradeId".into(), json!("11"));
    assert_eq!(
        send(&w.state, "POST", "/api/orders", Some(exit.clone()))
            .await
            .0,
        400
    );
    assert_eq!(w.okx.state().placed.len(), 1);

    exit["clientOrderId"] = json!("entry");
    let (status, duplicate) = send(&w.state, "POST", "/api/orders", Some(exit)).await;
    assert_eq!(status, 200);
    assert_eq!(duplicate["duplicate"], true);
    assert_eq!(duplicate["order"]["id"], opened["order"]["id"]);
    w.state.runtime.shutdown().await;
}

#[tokio::test]
async fn disconnect_rejects_an_order_waiting_on_leverage_before_a_retry() {
    let w = world(true).await;
    w.state
        .runtime
        .suspend_periodic_okx_sync_for_test()
        .await
        .unwrap();
    let leverage = w.okx.hold_next("POST", "/account/set-leverage");
    let request = order("retired-before-dispatch");
    let ((status, _), ()) = tokio::join!(
        send(&w.state, "POST", "/api/orders", Some(request.clone())),
        async {
            let timeout = std::time::Duration::from_secs(2);
            tokio::time::timeout(timeout, leverage.reached())
                .await
                .unwrap();
            let path = format!("/api/exchange/okx/{ACCOUNT}/disconnect");
            assert_eq!(send(&w.state, "POST", &path, None).await.0, 200);
            w.state
                .runtime
                .attach_okx_for_test(ACCOUNT, fake::credentials(), true)
                .await
                .unwrap();
            let (status, retry) = send(&w.state, "POST", "/api/orders", Some(request)).await;
            assert_eq!(status, 200);
            assert_eq!(retry["duplicate"], true);
            assert_eq!(retry["order"]["status"], "REJECTED");
            assert!(retry["order"]["rejectReason"]
                .as_str()
                .unwrap()
                .contains("before order dispatch"));
            assert!(w.okx.state().placed.is_empty());
            leverage.release();
        }
    );
    assert_eq!(status, 400);
    assert!(w.okx.state().placed.is_empty());
    w.state.runtime.shutdown().await;
}

#[tokio::test]
async fn disconnect_leaves_a_handed_off_order_for_lookup() {
    let w = world(true).await;
    w.state
        .runtime
        .suspend_periodic_okx_sync_for_test()
        .await
        .unwrap();
    let placement = w.okx.hold_next("POST", "/trade/order");
    let request = order("retired-after-dispatch");
    let ((status, _), ()) = tokio::join!(
        send(&w.state, "POST", "/api/orders", Some(request.clone())),
        async {
            tokio::time::timeout(std::time::Duration::from_secs(2), placement.reached())
                .await
                .unwrap();
            let path = format!("/api/exchange/okx/{ACCOUNT}/disconnect");
            assert_eq!(send(&w.state, "POST", &path, None).await.0, 200);
            w.state
                .runtime
                .attach_okx_for_test(ACCOUNT, fake::credentials(), true)
                .await
                .unwrap();
            let (status, retry) =
                send(&w.state, "POST", "/api/orders", Some(request.clone())).await;
            assert_eq!(status, 200);
            assert_eq!(retry["duplicate"], true);
            assert_eq!(retry["order"]["status"], "OPEN");
            placement.release();
        }
    );
    assert_eq!(status, 400);
    w.state.runtime.sync_exchanges().await.unwrap();
    let (status, retry) = send(&w.state, "POST", "/api/orders", Some(request)).await;
    assert_eq!(status, 200);
    assert_eq!(retry["duplicate"], true);
    assert_eq!(retry["order"]["status"], "FILLED");
    assert_eq!(w.okx.state().placed.len(), 1);
    w.state.runtime.shutdown().await;
}

#[tokio::test]
async fn stop_all_prevents_opening_while_leverage_is_in_flight() {
    let w = world(true).await;
    w.state
        .runtime
        .suspend_periodic_okx_sync_for_test()
        .await
        .unwrap();
    let leverage = w.okx.hold_next("POST", "/account/set-leverage");
    let order = order("stop-all-race");
    let ((status, result), ()) =
        tokio::join!(send(&w.state, "POST", "/api/orders", Some(order)), async {
            tokio::time::timeout(std::time::Duration::from_secs(2), leverage.reached())
                .await
                .unwrap();
            assert_eq!(
                send(&w.state, "POST", "/api/agents/stop-all", None).await.0,
                200
            );
            leverage.release();
        });
    assert_eq!(status, 200);
    assert_eq!(result["order"]["status"], "REJECTED");
    assert!(result["order"]["rejectReason"]
        .as_str()
        .unwrap()
        .starts_with("NOT_ARMED"));
    assert!(w.okx.state().placed.is_empty());
    w.state.runtime.shutdown().await;
}

#[tokio::test]
async fn selected_okx_market_and_limit_orders_can_reduce_an_exact_quantity() {
    let w = world_with_position().await;
    w.state
        .runtime
        .set_armed_for_test(ACCOUNT, false)
        .await
        .unwrap();
    let (_, positions) = send(
        &w.state,
        "GET",
        &format!("/api/accounts/{ACCOUNT}/positions"),
        None,
    )
    .await;
    let mut request = order("reduce-limit");
    request["side"] = json!("SELL");
    request["reduceOnly"] = json!(true);
    request["stopLoss"] = Value::Null;
    request["type"] = json!("LIMIT");
    request["price"] = json!("76000");
    request["quantity"] = json!("0.0004");
    request["positionId"] = positions[0]["id"].clone();
    w.okx.state().rest_limit_orders = true;
    let mut oversized = request.clone();
    oversized["quantity"] = json!("0.002");
    assert_eq!(
        send(&w.state, "POST", "/api/orders", Some(oversized))
            .await
            .0,
        400
    );
    assert_eq!(w.okx.state().placed.len(), 1);
    let mut off_step = request.clone();
    off_step["quantity"] = json!("0.00019");
    assert_eq!(
        send(&w.state, "POST", "/api/orders", Some(off_step))
            .await
            .0,
        400
    );
    assert_eq!(w.okx.state().placed.len(), 1);
    let mut off_tick = request.clone();
    off_tick["price"] = json!("76000.04");
    let (status, result) = send(&w.state, "POST", "/api/orders", Some(off_tick)).await;
    assert_eq!(status, 400, "{result}");
    assert_eq!(w.okx.state().placed.len(), 1);
    {
        let mut okx = w.okx.state();
        okx.lose_next_response = true;
        okx.lookup_error_code = Some("50001".into());
    }
    let (status, result) = send(&w.state, "POST", "/api/orders", Some(request.clone())).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["order"]["status"], "OPEN");
    assert_eq!(result["order"]["quantity"], "0.0004");
    assert_eq!(result["order"]["exchangeOrderId"], Value::Null);
    assert_eq!(w.okx.state().placed.last().unwrap()["px"], "76000");
    let lookup = format!("/api/orders/{}", result["order"]["id"].as_str().unwrap());
    let (status, observed) = send(&w.state, "GET", &lookup, None).await;
    assert_eq!(status, 200);
    assert_eq!(observed, result["order"]);
    w.okx.state().lookup_error_code = None;
    w.state.runtime.sync_okx_for_test(ACCOUNT).await.unwrap();
    let (status, confirmed) = send(&w.state, "GET", &lookup, None).await;
    assert_eq!(status, 200);
    assert_eq!(confirmed["clientOrderId"], "reduce-limit");
    assert_eq!(confirmed["status"], "OPEN");
    assert!(confirmed["exchangeOrderId"].is_string());
    assert_eq!(w.okx.state().placed.len(), 2);

    let mut mismatch = request.clone();
    mismatch["quantity"] = json!("0.0003");
    assert_eq!(
        send(&w.state, "POST", "/api/orders", Some(mismatch))
            .await
            .0,
        400
    );
    let (status, retry) = send(&w.state, "POST", "/api/orders", Some(request.clone())).await;
    assert_eq!(status, 200);
    assert_eq!(retry["duplicate"], true);

    request["clientOrderId"] = json!("reduce-market");
    request["type"] = json!("MARKET");
    request["price"] = Value::Null;
    let (status, result) = send(&w.state, "POST", "/api/orders", Some(request.clone())).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["order"]["status"], "FILLED");
    assert_eq!(result["order"]["quantity"], "0.0004");
    assert_eq!(w.okx.state().placed.last().unwrap()["reduceOnly"], true);

    request["clientOrderId"] = json!("stale-selection");
    assert_eq!(
        send(&w.state, "POST", "/api/orders", Some(request)).await.0,
        400
    );
    assert_eq!(w.okx.state().placed.len(), 3);
    w.state.runtime.shutdown().await;
}

#[tokio::test]
async fn disconnect_during_the_last_close_read_prevents_order_dispatch() {
    let w = world(true).await;
    w.state
        .runtime
        .suspend_periodic_okx_sync_for_test()
        .await
        .unwrap();
    assert_eq!(
        send(&w.state, "POST", "/api/orders", Some(order("entry")))
            .await
            .0,
        200
    );
    let (_, positions) = send(
        &w.state,
        "GET",
        &format!("/api/accounts/{ACCOUNT}/positions"),
        None,
    )
    .await;
    let first = w.okx.hold_next("GET", "/account/positions");
    let final_read = w.okx.hold_next("GET", "/account/positions");
    let close_path = format!("/api/positions/{ACCOUNT}/BTCUSDT/close");
    let body = json!({ "fraction": 1, "clientOrderId": "disconnect-race", "positionId": positions[0]["id"] });
    let ((status, _), ()) = tokio::join!(send(&w.state, "POST", &close_path, Some(body)), async {
        let timeout = std::time::Duration::from_secs(2);
        tokio::time::timeout(timeout, first.reached())
            .await
            .unwrap();
        first.release();
        tokio::time::timeout(timeout, final_read.reached())
            .await
            .unwrap();
        let path = format!("/api/exchange/okx/{ACCOUNT}/disconnect");
        assert_eq!(send(&w.state, "POST", &path, None).await.0, 200);
        final_read.release();
    });
    assert_eq!(status, 400);
    assert_eq!(w.okx.state().placed.len(), 1);
    assert!(w.okx.state().position.is_some());
    w.state.runtime.shutdown().await;
}

#[tokio::test]
async fn manual_okx_close_never_substitutes_the_isolated_position_for_a_selected_cross_position() {
    let w = world(true).await;
    w.state
        .runtime
        .suspend_periodic_okx_sync_for_test()
        .await
        .unwrap();
    send(&w.state, "POST", "/api/orders", Some(order("a"))).await;
    {
        let mut okx = w.okx.state();
        let mut cross = okx.position.as_ref().unwrap().clone();
        cross.insert("mgnMode".into(), json!("cross"));
        cross.insert("pos".into(), json!("-0.2"));
        cross.insert("posId".into(), json!("cross-position"));
        okx.other_positions.push(cross);
    }
    w.state.runtime.sync_exchanges().await.unwrap();
    let path = format!("/api/accounts/{ACCOUNT}/positions");
    let (_, positions) = send(&w.state, "GET", &path, None).await;
    let positions = positions.as_array().unwrap();
    let cross = positions
        .iter()
        .find(|p| p["marginMode"] == "cross")
        .unwrap();
    let isolated = positions
        .iter()
        .find(|p| p["marginMode"] == "isolated")
        .unwrap();
    assert_ne!(cross["id"], isolated["id"]);
    assert!(cross["stopLoss"].is_null());
    let close_path = format!("/api/positions/{ACCOUNT}/BTCUSDT/close");
    for body in [
        json!({ "fraction": 1, "clientOrderId": "no-position" }),
        json!({ "fraction": 1, "clientOrderId": "cross", "positionId": cross["id"] }),
    ] {
        assert_eq!(send(&w.state, "POST", &close_path, Some(body)).await.0, 400);
        assert_eq!(w.okx.state().placed.len(), 1);
    }
    w.okx
        .state()
        .position
        .as_mut()
        .unwrap()
        .insert("tradeId".into(), json!("99"));
    let body = json!({ "fraction": 1, "clientOrderId": "stale", "positionId": isolated["id"] });
    assert_eq!(send(&w.state, "POST", &close_path, Some(body)).await.0, 400);
    assert_eq!(w.okx.state().placed.len(), 1);
    w.state.runtime.sync_exchanges().await.unwrap();
    let (_, fresh) = send(&w.state, "GET", &path, None).await;
    let isolated = fresh
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["marginMode"] == "isolated")
        .unwrap();
    let first_check = w.okx.hold_next("GET", "/account/positions");
    let final_check = w.okx.hold_next("GET", "/account/positions");
    let body = json!({ "fraction": 1, "clientOrderId": "changed-before-send", "positionId": isolated["id"] });
    let ((status, _), ()) = tokio::join!(send(&w.state, "POST", &close_path, Some(body)), async {
        let timeout = std::time::Duration::from_secs(2);
        tokio::time::timeout(timeout, first_check.reached())
            .await
            .unwrap();
        first_check.release();
        tokio::time::timeout(timeout, final_check.reached())
            .await
            .unwrap();
        w.okx
            .state()
            .position
            .as_mut()
            .unwrap()
            .insert("tradeId".into(), json!("100"));
        final_check.release();
    });
    assert_eq!(status, 400);
    assert_eq!(w.okx.state().placed.len(), 1);
    w.state.runtime.sync_exchanges().await.unwrap();
    let (_, fresh) = send(&w.state, "GET", &path, None).await;
    let isolated = fresh
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["marginMode"] == "isolated")
        .unwrap();
    let body = json!({ "fraction": 1, "clientOrderId": "isolated", "positionId": isolated["id"] });
    assert_eq!(
        send(&w.state, "POST", &close_path, Some(body.clone()))
            .await
            .0,
        200
    );
    let (status, retry) = send(&w.state, "POST", &close_path, Some(body.clone())).await;
    assert_eq!(status, 200);
    assert_eq!(retry["duplicate"], true);
    for wrong in [
        json!({ "fraction": 1, "clientOrderId": "isolated", "positionId": cross["id"] }),
        json!({ "fraction": 0.5, "clientOrderId": "isolated", "positionId": isolated["id"] }),
    ] {
        assert_eq!(
            send(&w.state, "POST", &close_path, Some(wrong)).await.0,
            400
        );
    }
    assert_eq!(
        send(
            &w.state,
            "POST",
            &format!("/api/positions/{ACCOUNT}/ETHUSDT/close"),
            Some(body)
        )
        .await
        .0,
        400
    );
    {
        let okx = w.okx.state();
        assert!(okx.position.is_none());
        assert_eq!(okx.other_positions[0]["pos"], "-0.2");
        assert_eq!(okx.placed.len(), 2);
    }
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
async fn an_exchange_account_without_a_session_is_not_connected() {
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
    let answer = send(&w.state, "POST", "/api/orders", Some(request)).await;
    assert_eq!(
        answer,
        (
            400,
            json!({ "error": "OKX is not connected to OKX. Enter its API key in Settings." })
        )
    );
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
        keep_trailing: false,
        keep_take_profit: false,
        checked: None,
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

/// An SL-only edit (no trailingStop) racing a trailing-stop edit: the trailing stop it keeps is
/// read inside the engine's queue, after the trailing edit, so it is not undone.
#[tokio::test]
async fn an_sl_only_edit_overlapping_a_trailing_edit_keeps_the_new_trailing_stop() {
    let w = world_with_position().await;
    let path = format!("/api/positions/{ACCOUNT}/BTCUSDT/protection");
    let hold = w.okx.hold_next("POST", "/trade/order-algo");
    let trail = {
        let (state, path) = (w.state.clone(), path.clone());
        tokio::spawn(async move {
            let body = json!({
                "stopLoss": "74000", "takeProfit": null,
                "trailingStop": {"kind": "RATIO", "callback": "0.01", "activationPrice": null},
            });
            send(&state, "PUT", &path, Some(body)).await
        })
    };
    // The trailing edit is in the account's queue, out on OKX.
    hold.reached().await;
    let edit = {
        let (state, path) = (w.state.clone(), path.clone());
        tokio::spawn(async move {
            let body = json!({ "stopLoss": "74500", "takeProfit": null });
            send(&state, "PUT", &path, Some(body)).await
        })
    };
    // Round trips through the runtime let the SL edit run up to the engine's queue.
    for _ in 0..10 {
        w.state.runtime.call(|_| ()).await.unwrap();
    }
    hold.release();
    let (status, _) = trail.await.unwrap();
    assert_eq!(status, 200);
    let (status, edited) = edit.await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(edited["after"]["stopLoss"], "74500");
    assert_eq!(edited["after"]["trailingStop"]["callback"], "0.01");
    w.state.runtime.shutdown().await;
}

#[tokio::test]
async fn disconnect_during_the_final_stop_check_keeps_the_live_trailing_stop() {
    let w = world_with_position().await;
    w.state
        .runtime
        .suspend_periodic_okx_sync_for_test()
        .await
        .unwrap();
    let path = format!("/api/positions/{ACCOUNT}/BTCUSDT/protection");
    let trail = json!({
        "stopLoss": "74000", "takeProfit": null,
        "trailingStop": {"kind": "RATIO", "callback": "0.01", "activationPrice": null},
    });
    assert_eq!(send(&w.state, "PUT", &path, Some(trail)).await.0, 200);
    let preflight = w.okx.hold_next("GET", "/trade/order-algo");
    let after_placement = w.okx.hold_next("GET", "/trade/order-algo");
    let before_cancel = w.okx.hold_next("GET", "/trade/order-algo");
    let edit = json!({ "stopLoss": "74000", "takeProfit": "78000", "trailingStop": null });
    let ((status, _), ()) = tokio::join!(send(&w.state, "PUT", &path, Some(edit)), async {
        let timeout = std::time::Duration::from_secs(2);
        for check in [preflight, after_placement] {
            tokio::time::timeout(timeout, check.reached())
                .await
                .unwrap();
            check.release();
        }
        tokio::time::timeout(timeout, before_cancel.reached())
            .await
            .unwrap();
        let disconnect = format!("/api/exchange/okx/{ACCOUNT}/disconnect");
        assert_eq!(send(&w.state, "POST", &disconnect, None).await.0, 200);
        before_cancel.release();
    });
    assert_eq!(status, 400);
    assert!(w.okx.state().canceled_algo_ids.is_empty());
    assert!(w
        .okx
        .state()
        .algos
        .values()
        .any(|algo| algo["ordType"] == "move_order_stop" && algo["state"] == "live"));
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

/// The arena row of an agent alone on its pair: OKX's history and open position, counted only from
/// when the agent was created.
#[tokio::test]
async fn the_arena_reads_an_okx_agents_result_from_okx_history_and_its_open_position() {
    let w = world(true).await;
    {
        let mut okx = w.okx.state();
        okx.position = Some(
            json!({
                "instId": "BTC-USDT-SWAP", "pos": "1", "avgPx": "75000", "markPx": "75420",
                "upl": "4.2", "realizedPnl": "-0.1", "posId": "9", "lever": "3", "liqPx": "50000", "margin": "10", "mgnMode": "isolated",
                "cTime": (START + 1_000).to_string(), "uTime": (START + 1_100).to_string(), "posSide": "net",
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        for (pos_id, opened, realized) in [
            ("old", START - 10_000, "50"),
            ("mine", START + 600, "-1.25"),
        ] {
            okx.positions_history.push(
                json!({
                    "posId": pos_id, "instId": "BTC-USDT-SWAP", "cTime": opened.to_string(),
                    "uTime": (opened + 100).to_string(), "realizedPnl": realized, "type": "2",
                    "mgnMode": "isolated", "posSide": "net", "direction": "long",
                })
                .as_object()
                .unwrap()
                .clone(),
            );
        }
    }
    let set_created = |at: i64| {
        let runtime = w.state.runtime.clone();
        async move {
            runtime
                .call(move |r| {
                    r.connection()
                        .execute(
                            "INSERT INTO agents (id,name,provider_id,model,symbol,timeframe,strategy,
                               account_id,risk,interval_sec,status,running,created_at)
                             VALUES ('agent_x','X','mock','m','BTCUSDT','15m','',?1,'{}',900,'IDLE',0,?2)
                             ON CONFLICT(id) DO UPDATE SET created_at=excluded.created_at",
                            rusqlite::params![ACCOUNT, at],
                        )
                        .unwrap()
                })
                .await
                .unwrap();
        }
    };
    set_created(START + 500).await;
    // The agent's entry, which opened the position the close records as "mine".
    w.state
        .runtime
        .call(|r| {
            r.connection()
                .execute(
                    "INSERT INTO orders (id,account_id,client_order_id,symbol,side,type,quantity,
                       leverage,reduce_only,close_position,status,filled_quantity,reserved_margin,
                       agent_id,created_at,updated_at)
                     VALUES ('o_x',?1,'cx','BTCUSDT','BUY','MARKET','1',3,0,0,'FILLED','1','0',
                       'agent_x',?2,?2),
                            ('o_open',?1,'co','BTCUSDT','BUY','MARKET','1',3,0,0,'FILLED','1','0',
                       'agent_x',?3,?3)",
                    rusqlite::params![ACCOUNT, START + 650, START + 1_050],
                )
                .unwrap();
            r.connection()
                .execute(
                    "INSERT INTO exchange_order_executions (order_id,trade_id,executed_at)
                     VALUES ('o_x','51',?1),('o_open','52',?2)",
                    rusqlite::params![START + 650, START + 1_050],
                )
                .unwrap();
            r.connection()
                .execute(
                    "INSERT INTO fills (id,order_id,account_id,symbol,side,quantity,price,fee,
                       realized_pnl,liquidity,ts)
                     VALUES ('f_x','o_x',?1,'BTCUSDT','BUY','1','75000','0','0','TAKER',?2)",
                    rusqlite::params![ACCOUNT, START + 650],
                )
                .unwrap()
        })
        .await
        .unwrap();
    w.clock.fetch_add(60_000, Ordering::SeqCst);
    w.state.runtime.sync_okx_for_test(ACCOUNT).await.unwrap();

    let (_, arena) = send(&w.state, "GET", "/api/arena", None).await;
    let row = &arena[0];
    assert_eq!(row["attribution"], "AGENT");
    // The close from before the agent existed is not its own. The open position adds what OKX
    // has realized on it so far (fees, funding) and its unrealized P&L.
    assert_eq!(row["realizedPnl"], "-1.35");
    assert_eq!(row["pnl"], "2.85");
    assert_eq!(row["tradeCount"], 1);
    assert_eq!(row["winRatePct"], json!(0));

    // Created after the position opened: that position is not its own either.
    set_created(START + 2_000).await;
    let (_, arena) = send(&w.state, "GET", "/api/arena", None).await;
    assert_eq!(arena[0]["pnl"], "0");
    assert_eq!(arena[0]["realizedPnl"], "0");
    assert_eq!(arena[0]["tradeCount"], 0);
    w.state.runtime.shutdown().await;
}

/// A partially filled or late-confirmed order has an exchange execution even when its
/// local aggregate fill is missing or recorded much later.
async fn exchange_arena_recovers_execution(remote_state: &str, position_closed: bool) {
    let w = world(false).await;
    let archived = remote_state == "archived";
    let opened = START + 100;
    let inst = "BTC-USDT-SWAP";
    let cl_ord_id = mochi_okx::ids::to_cl_ord_id("arena-entry");
    {
        let mut okx = w.okx.state();
        okx.orders.insert(
            cl_ord_id.clone(),
            json!({
                "ordId": "10", "clOrdId": cl_ord_id, "instId": inst, "state": remote_state,
                "accFillSz": "1", "avgPx": "75000", "fee": "0", "pnl": "0", "side": "buy",
                "tradeId": "100", "fillTime": opened.to_string(), "fillSz": "1",
                "tdMode": "isolated", "posSide": "net",
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        if archived {
            for bill in 1..=302 {
                okx.execution_history.push(
                    json!({
                        "billId": bill.to_string(), "ordId": if bill <= 2 { "10" } else { "other" },
                        "instId": inst, "tradeId": bill.to_string(), "posSide": "net", "side": "buy",
                        "fillTime": (opened + bill).to_string(), "ts": (START + 4_000_000).to_string(),
                    }).as_object().unwrap().clone()
                );
            }
        }
        if position_closed {
            okx.positions_history.push(
                json!({
                    "posId": "p1", "instId": inst, "cTime": opened.to_string(),
                    "uTime": (opened + 500).to_string(), "realizedPnl": "2.5", "type": "2",
                    "mgnMode": "isolated", "posSide": "net", "direction": "long",
                })
                .as_object()
                .unwrap()
                .clone(),
            );
        } else {
            okx.position = Some(
                json!({
                    "posId": "p1", "instId": inst, "cTime": opened.to_string(),
                    "uTime": opened.to_string(), "tradeId": "100", "pos": "1",
                    "realizedPnl": "0", "upl": "2.5", "avgPx": "75000", "markPx": "75250",
                    "lever": "3", "margin": "10", "mgnMode": "isolated", "posSide": "net", "direction": "long",
                })
                .as_object()
                .unwrap()
                .clone(),
            );
        }
    }
    w.state
        .runtime
        .call(move |r| {
            let conn = r.connection();
            conn.execute(
                "INSERT INTO agents (id,name,provider_id,model,symbol,timeframe,strategy,
                   account_id,risk,interval_sec,status,running,created_at)
                 VALUES ('agent_x','X','mock','m','BTCUSDT','15m','',?1,'{}',900,'IDLE',0,?2)",
                rusqlite::params![ACCOUNT, START],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO orders (id,account_id,client_order_id,symbol,side,type,quantity,
                   leverage,reduce_only,close_position,status,filled_quantity,reserved_margin,
                   agent_id,created_at,updated_at,exchange_order_id)
                 VALUES ('o_x',?1,'arena-entry','BTCUSDT','BUY','LIMIT','1',3,0,0,?3,?4,'0',
                   'agent_x',?2,?2,?5)",
                rusqlite::params![
                    ACCOUNT,
                    START,
                    if archived { "FILLED" } else { "OPEN" },
                    if archived { "1" } else { "0" },
                    archived.then_some("10"),
                ],
            )
            .unwrap();
        })
        .await
        .unwrap();
    w.clock.fetch_add(240_000, Ordering::SeqCst);
    w.state.runtime.sync_okx_for_test(ACCOUNT).await.unwrap();

    if archived {
        let (_, first) = send(&w.state, "GET", "/api/arena", None).await;
        assert_eq!(first[0]["pnl"], "0", "old fills are beyond the first pass");
        let pending = mochi_okx::ids::to_cl_ord_id("pending-entry");
        w.okx.state().orders.insert(
            pending.clone(),
            json!({ "state": "live", "accFillSz": "0" })
                .as_object()
                .unwrap()
                .clone(),
        );
        w.state
            .runtime
            .call(|r| {
                r.connection().execute(
                "INSERT INTO orders (id,account_id,client_order_id,symbol,side,type,quantity,
                   leverage,reduce_only,close_position,status,filled_quantity,reserved_margin,
                   agent_id,created_at,updated_at)
                 VALUES ('o_pending',?1,'pending-entry','BTCUSDT','BUY','LIMIT','1',3,0,0,
                         'OPEN','0','0','agent_x',?2,?2)",
                rusqlite::params![ACCOUNT, START],
            ).unwrap();
            })
            .await
            .unwrap();
        w.clock.fetch_add(60_000, Ordering::SeqCst);
        w.state.runtime.sync_okx_for_test(ACCOUNT).await.unwrap();
        let (_, stalled) = send(&w.state, "GET", "/api/arena", None).await;
        assert_eq!(
            stalled[0]["pnl"], "0",
            "a missing exchange order ID must not advance the cursor"
        );

        w.okx.state().orders.insert(
            pending,
            json!({ "state": "canceled", "ordId": "20", "accFillSz": "0" })
                .as_object()
                .unwrap()
                .clone(),
        );
        w.clock.fetch_add(60_000, Ordering::SeqCst);
        w.state.runtime.sync_okx_for_test(ACCOUNT).await.unwrap();
        let count = w
            .state
            .runtime
            .call(|r| {
                r.connection()
                    .query_row(
                        "SELECT COUNT(*) FROM exchange_order_executions WHERE order_id='o_x'",
                        [],
                        |row| row.get::<_, usize>(0),
                    )
                    .unwrap()
            })
            .await
            .unwrap();
        assert_eq!(count, 2, "record each individual exchange execution");
    }
    let (_, arena) = send(&w.state, "GET", "/api/arena", None).await;
    assert_eq!(arena[0]["pnl"], "2.5");
    let fills = w
        .state
        .runtime
        .call(|r| {
            r.connection()
                .query_row(
                    "SELECT COUNT(*) FROM fills WHERE order_id = 'o_x'",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap()
        })
        .await
        .unwrap();
    assert_eq!(fills, usize::from(position_closed && !archived));
    w.state.runtime.shutdown().await;
}

#[tokio::test]
async fn arena_backfills_all_old_order_executions_after_a_bounded_history_pass() {
    exchange_arena_recovers_execution("archived", true).await;
}

#[tokio::test]
async fn arena_counts_a_position_opened_by_a_partially_filled_limit_order() {
    exchange_arena_recovers_execution("partially_filled", false).await;
}

#[tokio::test]
async fn arena_counts_a_position_closed_before_late_confirmation() {
    exchange_arena_recovers_execution("filled", true).await;
}
