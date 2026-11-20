//! Protection on OKX against the in-process fake OKX, with an injected clock: ports of the
//! okx-live.test.ts group "protection on OKX", of `entry with a trailing stop sends it in
//! attachAlgoOrds with attachAlgoClOrdId`, and of the "added pairs" test `never treats a held
//! pair as closed because it is missing from the symbol registry`. Also the engine half of the
//! agent cases a PROTECT decision meets: a pending confirmation, a part-way refusal's `after`, and
//! a target that differs only off the tick.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mochi_core::paper::PaperStore;
use mochi_domain::{
    default_account_config, default_live_limits, AccountVenue, DataSource, EntryOrderType,
    ExternalProtection, ExternalProtectionKind, InstrumentSpecification, Order, OrderRequest,
    OrderSide, OrderStatus, OrderType, PaperAccount, PositionView, Protection, Quote, RiskSettings,
    TrailingKind, TrailingStopSpec, TrailingStopView,
};
use mochi_okx::engine::{
    CheckedProtection, EngineError, OkxEngine, OkxEngineDeps, ProtectionOrderOptions, Rows,
    StoreAccess, StoreWork,
};
use mochi_okx::fake::{self, FakeOkx, Refusal};
use mochi_okx::{
    to_algo_cl_ord_id, BoxFuture, HttpReply, HttpRequest, OkxClient, Row, Transport, Venue,
};
use mochi_store::{open_in_memory, SqlitePaperStore};
use serde_json::{json, Value};

const ACCOUNT: &str = "acct_okx";
/// 2027-01-15T08:00:00Z: far from a UTC day boundary.
const START: i64 = 1_800_000_000_000;

type DeferredSaveHook = (String, Box<dyn FnOnce() + Send>);

struct Local {
    inner: Mutex<(SqlitePaperStore, Vec<InstrumentSpecification>)>,
    after_deferred_save: Mutex<Option<DeferredSaveHook>>,
}

impl StoreAccess for Local {
    fn run(&self, work: StoreWork) -> BoxFuture<'static, ()> {
        {
            let inner = self.inner.lock().unwrap();
            work(&Rows {
                store: &inner.0,
                instruments: &inner.1,
            });
            let mut hook = self.after_deferred_save.lock().unwrap();
            if hook.as_ref().is_some_and(|(id, _)| {
                inner.0.get_order(id).unwrap().is_some_and(|order| {
                    order
                        .reject_reason
                        .as_deref()
                        .is_some_and(|reason| reason.starts_with("Awaiting confirmation: deferred"))
                })
            }) {
                let (_, action) = hook.take().unwrap();
                drop(inner);
                drop(hook);
                action();
            }
        }
        Box::pin(async {})
    }
}

/// Node's `vi.spyOn(engine, 'sync').mockRejectedValue(...)`: once armed, every sync after the next
/// algo placement fails, because OKX stops answering the balance read that opens it.
/// Also answers the next algo placement with JSON `null`, without letting it reach the fake.
struct SyncDown {
    inner: Arc<dyn Transport>,
    armed: AtomicBool,
    down: AtomicBool,
    null_next_algo: AtomicBool,
}

impl Transport for SyncDown {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpReply, String>> {
        if self.down.load(Ordering::SeqCst) && request.url.contains("/account/balance") {
            return Box::pin(async { Err("sync down".to_string()) });
        }
        let algo = request.method == "POST" && request.url.ends_with("/trade/order-algo");
        if algo && self.null_next_algo.swap(false, Ordering::SeqCst) {
            return Box::pin(async {
                Ok(HttpReply {
                    status: 200,
                    body: b"null".to_vec(),
                })
            });
        }
        let reply = self.inner.send(request);
        if algo && self.armed.load(Ordering::SeqCst) {
            self.down.store(true, Ordering::SeqCst);
        }
        reply
    }
}

struct Harness {
    okx: Arc<FakeOkx>,
    engine: Arc<OkxEngine>,
    store: Arc<Local>,
    clock: Arc<AtomicI64>,
    quote_time: Arc<AtomicI64>,
    armed: Arc<AtomicBool>,
    prices: Arc<Mutex<(String, String, String)>>,
    sync_down: Arc<SyncDown>,
}

fn account(limits: RiskSettings) -> PaperAccount {
    PaperAccount {
        id: ACCOUNT.into(),
        name: "OKX".into(),
        source: DataSource::Live,
        venue: AccountVenue::OkxLive,
        exchange_uid: Some("42".into()),
        live_limits: Some(limits),
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

fn pepe() -> InstrumentSpecification {
    mochi_market::instrument_parse::parse_okx_instrument(&json!({
        "instId": "PEPE-USDT-SWAP", "settleCcy": "USDT", "ctType": "linear", "state": "live",
        "ruleType": "normal", "instCategory": "1", "ctValCcy": "PEPE", "ctVal": "10000000",
        "lotSz": "0.1", "minSz": "0.1", "tickSz": "0.0000000001", "lever": "50",
    }))
    .unwrap()
    .unwrap()
}

/// Node's `connect()` and `arm`: the account row, then the engine and its first sync, armed.
async fn connect(extra: Vec<InstrumentSpecification>) -> Harness {
    let okx = FakeOkx::new();
    let store = SqlitePaperStore::new(open_in_memory().unwrap());
    store
        .insert_account(&account(default_live_limits()))
        .unwrap();
    let mut instruments = mochi_domain::default_instruments();
    instruments.extend(extra);
    let store = Arc::new(Local {
        inner: Mutex::new((store, instruments)),
        after_deferred_save: Mutex::new(None),
    });
    let clock = Arc::new(AtomicI64::new(START));
    let quote_time = Arc::new(AtomicI64::new(0));
    let armed = Arc::new(AtomicBool::new(true));
    let prices = Arc::new(Mutex::new((
        "74999".to_string(),
        "75001".to_string(),
        "75000".to_string(),
    )));
    let sync_down = Arc::new(SyncDown {
        inner: okx.transport(),
        armed: AtomicBool::new(false),
        down: AtomicBool::new(false),
        null_next_algo: AtomicBool::new(false),
    });
    let ticks = clock.clone();
    let client = OkxClient::new(fake::credentials(), Venue::OkxLive, sync_down.clone())
        .with_base_url("https://okx.invalid")
        .with_clock(move || ticks.load(Ordering::SeqCst));
    let ids = Arc::new(AtomicU64::new(0));
    let engine = OkxEngine::new(OkxEngineDeps {
        account_id: ACCOUNT.into(),
        venue: Venue::OkxLive,
        client,
        store: store.clone(),
        get_quote: {
            let (clock, prices, quote_time) = (clock.clone(), prices.clone(), quote_time.clone());
            Arc::new(move |symbol: &str| {
                let (bid, ask, last) = prices.lock().unwrap().clone();
                Some(Quote {
                    source: DataSource::Live,
                    symbol: symbol.into(),
                    bid,
                    ask,
                    mark: last.clone(),
                    last,
                    mark_estimated: false,
                    funding_rate: Some("0".into()),
                    next_funding_time: None,
                    ts: match quote_time.load(Ordering::SeqCst) {
                        0 => clock.load(Ordering::SeqCst),
                        at => at,
                    },
                })
            })
        },
        now: {
            let clock = clock.clone();
            Arc::new(move || clock.load(Ordering::SeqCst))
        },
        is_armed: {
            let armed = armed.clone();
            Arc::new(move || armed.load(Ordering::SeqCst))
        },
        new_id: Arc::new(move |prefix: &str| {
            format!("{prefix}_{}", ids.fetch_add(1, Ordering::SeqCst) + 1)
        }),
    });
    engine.sync().await.unwrap();
    Harness {
        okx,
        engine,
        store,
        clock,
        quote_time,
        armed,
        prices,
        sync_down,
    }
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(0);

/// Node's `order(accountId, overrides)`: a small BTC long with a stop at 74000.
fn order() -> OrderRequest {
    OrderRequest {
        account_id: ACCOUNT.into(),
        client_order_id: format!("key-{}", NEXT_KEY.fetch_add(1, Ordering::SeqCst)),
        symbol: "BTCUSDT".into(),
        side: OrderSide::Buy,
        order_type: EntryOrderType::Market,
        quantity: "0.001".into(),
        price: None,
        leverage: 3,
        reduce_only: None,
        stop_loss: Some("74000".into()),
        take_profit: None,
        trailing_stop: None,
        agent_id: None,
        decision_id: None,
    }
}

fn stop(price: &str) -> Protection {
    Protection {
        stop_loss: Some(price.into()),
        take_profit: None,
        trailing_stop: None,
    }
}

fn none() -> Protection {
    Protection {
        stop_loss: None,
        take_profit: None,
        trailing_stop: None,
    }
}

fn ratio(callback: &str) -> TrailingStopSpec {
    TrailingStopSpec {
        kind: TrailingKind::Ratio,
        callback: callback.into(),
        activation_price: None,
    }
}

fn row(fields: Value) -> Row {
    fields.as_object().unwrap().clone()
}

fn long_position() -> Row {
    row(json!({
        "instId": "BTC-USDT-SWAP", "pos": "0.1", "avgPx": "75000", "markPx": "75000", "upl": "0",
        "lever": "3", "liqPx": "50000", "margin": "10", "mgnMode": "isolated", "posSide": "net",
    }))
}

impl Harness {
    /// The protection group's `beforeEach`: connected, armed, and a long open with a stop at
    /// 74000 attached on entry; the call log cleared.
    async fn with_position() -> Harness {
        let h = connect(Vec::new()).await;
        let opened = h.engine.place_order(order()).await.unwrap();
        assert_eq!(opened.order.status, OrderStatus::Filled);
        h.okx.state().calls.clear();
        h
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }

    /// Moves the engine's clock forward.
    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    fn store<T>(&self, f: impl FnOnce(&SqlitePaperStore) -> T) -> T {
        f(&self.store.inner.lock().unwrap().0)
    }

    /// Node's `protectionRows()`: newest first, as `listOrders` returns them.
    fn protection_rows(&self) -> Vec<Order> {
        self.store(|s| s.list_orders(ACCOUNT, 100).unwrap())
            .into_iter()
            .filter(|o| o.close_position)
            .collect()
    }

    fn order_row(&self, id: &str) -> Order {
        self.store(|s| s.get_order(id).unwrap()).unwrap()
    }

    /// Node's `backdate()`: every protection row made 61 s old.
    fn backdate(&self) {
        let at = self.clock.load(Ordering::SeqCst) - 61_000;
        self.store(|s| {
            s.connection()
                .execute(
                    "UPDATE orders SET created_at=? WHERE close_position=1",
                    (at,),
                )
                .unwrap()
        });
    }

    fn set_limits(&self, limits: RiskSettings) {
        self.store(|s| {
            let mut account = s.get_account(ACCOUNT).unwrap().unwrap();
            account.live_limits = Some(limits);
            s.connection()
                .execute(
                    "UPDATE accounts SET live_limits=? WHERE id=?",
                    (
                        serde_json::to_string(&account.live_limits).unwrap(),
                        ACCOUNT,
                    ),
                )
                .unwrap()
        });
    }

    async fn set(
        &self,
        protection: Protection,
    ) -> Result<mochi_core::paper::ProtectionResult, EngineError> {
        self.engine
            .set_protection(ACCOUNT, "BTCUSDT", protection, None, None)
            .await
    }

    fn view(&self) -> PositionView {
        self.engine
            .positions()
            .into_iter()
            .find(|p| p.position.symbol == "BTCUSDT")
            .unwrap()
    }

    fn posts(&self) -> Vec<String> {
        self.okx
            .state()
            .calls
            .iter()
            .filter(|c| c.starts_with("POST"))
            .cloned()
            .collect()
    }

    fn count(&self, call: &str) -> usize {
        self.okx.state().calls.iter().filter(|c| *c == call).count()
    }

    fn index_of(&self, call: &str) -> Option<usize> {
        self.okx.state().calls.iter().position(|c| c == call)
    }

    fn algo_state(&self, id: &str) -> String {
        self.okx.state().algos[&to_algo_cl_ord_id(id)]["state"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn algo_placed(&self) -> Vec<Value> {
        self.okx.state().algo_placed.clone()
    }

    fn canceled(&self) -> Vec<String> {
        self.okx.state().canceled_algo_ids.clone()
    }

    async fn close(&self) -> mochi_domain::OrderResult {
        let key = format!("close-{}", NEXT_KEY.fetch_add(1, Ordering::SeqCst));
        self.engine
            .close_position(ACCOUNT, "BTCUSDT", 1.0, &key)
            .await
            .unwrap()
    }

    async fn sync(&self) {
        self.engine.sync().await.unwrap();
    }
}

fn message(error: EngineError) -> String {
    error.to_string()
}

fn cancel_pending(price: &str) -> ExternalProtection {
    ExternalProtection {
        kind: ExternalProtectionKind::StopLoss,
        trigger_price: Some(price.into()),
        detail: format!("ours, cancel pending: SL trigger {price}"),
    }
}

// ---- protection on OKX ------------------------------------------------------------------------

#[tokio::test]
async fn shows_the_stop_attached_on_entry_read_back_from_okx() {
    let h = Harness::with_position().await;
    let rows = h.protection_rows();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.order_type, OrderType::StopMarket);
    assert_eq!(row.trigger_price.as_deref(), Some("74000"));
    assert_eq!(row.status, OrderStatus::Open);
    assert!(row.close_position && row.reduce_only);
    let algo_id = h.okx.state().algos[&to_algo_cl_ord_id(&row.id)]["algoId"].clone();
    assert_eq!(row.exchange_order_id.as_deref(), algo_id.as_str());
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74000"));
    assert_eq!(h.view().stop_loss.as_deref(), Some("74000"));
}

#[tokio::test]
async fn a_tightened_stop_is_placed_before_the_old_one_is_cancelled() {
    let h = Harness::with_position().await;
    let old = h.protection_rows().remove(0);
    let result = h.set(stop("74500")).await.unwrap();
    assert_eq!(result.before, stop("74000"));
    assert_eq!(result.after, stop("74500"));
    assert!(!result.pending);

    let placed = h.index_of("POST /api/v5/trade/order-algo").unwrap();
    let cancelled = h.index_of("POST /api/v5/trade/cancel-algos").unwrap();
    assert!(cancelled > placed);
    let sent = h.algo_placed();
    assert_eq!(sent.len(), 1);
    let algo_cl_ord_id = sent[0]["algoClOrdId"].as_str().unwrap().to_string();
    assert_eq!(algo_cl_ord_id.len(), 32);
    assert_eq!(
        sent[0],
        json!({
            "instId": "BTC-USDT-SWAP", "tdMode": "isolated", "side": "sell",
            "algoClOrdId": algo_cl_ord_id, "sz": "0.1", "reduceOnly": true,
            "ordType": "conditional", "slTriggerPx": "74500", "slOrdPx": "-1",
        })
    );
    // Key order on the wire, as Node's algoBody builds it.
    let keys: Vec<_> = sent[0].as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        [
            "instId",
            "tdMode",
            "side",
            "algoClOrdId",
            "sz",
            "reduceOnly",
            "ordType",
            "slTriggerPx",
            "slOrdPx"
        ]
    );
    assert_eq!(h.canceled(), [old.exchange_order_id.clone().unwrap()]);
    assert_eq!(h.order_row(&old.id).status, OrderStatus::Canceled);
    let fresh = h
        .protection_rows()
        .into_iter()
        .find(|o| o.status == OrderStatus::Open)
        .unwrap();
    assert_eq!(algo_cl_ord_id, to_algo_cl_ord_id(&fresh.id));
    assert_eq!(fresh.client_order_id, fresh.id);
    assert_eq!(h.view().stop_loss.as_deref(), Some("74500"));
}

#[tokio::test]
async fn a_change_that_increases_risk_is_refused_while_disarmed() {
    let cases = [
        ("a loosened stop", stop("73000")),
        ("a removed stop", none()),
        (
            "an added target",
            Protection {
                take_profit: Some("77000".into()),
                ..stop("74000")
            },
        ),
    ];
    for (label, protection) in cases {
        let h = Harness::with_position().await;
        h.disarm();
        let error = h.set(protection).await.unwrap_err();
        assert_eq!(
            message(error),
            "NOT_ARMED: this change increases risk",
            "{label}"
        );
        assert!(h.posts().is_empty(), "{label}");
        assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74000"), "{label}");
    }
}

#[tokio::test]
async fn a_loosened_stop_is_placed_once_armed() {
    let h = Harness::with_position().await;
    h.set(stop("73000")).await.unwrap();
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("73000"));
}

#[tokio::test]
async fn a_tightened_stop_is_allowed_while_disarmed() {
    let h = Harness::with_position().await;
    h.disarm();
    let result = h.set(stop("74500")).await.unwrap();
    assert_eq!(result.after, stop("74500"));
    assert_eq!(h.algo_placed().len(), 1);
}

#[tokio::test]
async fn a_trailing_stop_is_added_while_disarmed_in_its_own_move_order_stop_algo() {
    let h = Harness::with_position().await;
    h.disarm();
    let trailing = TrailingStopSpec {
        kind: TrailingKind::Spread,
        callback: "500".into(),
        activation_price: Some("76000".into()),
    };
    h.set(Protection {
        trailing_stop: Some(trailing.clone()),
        ..stop("74000")
    })
    .await
    .unwrap();
    let sent = h.algo_placed();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["ordType"], "move_order_stop");
    assert_eq!(sent[0]["side"], "sell");
    assert_eq!(sent[0]["sz"], "0.1");
    assert_eq!(sent[0]["reduceOnly"], true);
    assert_eq!(sent[0]["callbackSpread"], "500");
    assert_eq!(sent[0]["activePx"], "76000");
    assert!(sent[0].get("callbackRatio").is_none());
    // The stop-loss did not change, so nothing was cancelled.
    assert!(h.canceled().is_empty());
    assert_eq!(
        h.view().trailing_stop,
        Some(TrailingStopView {
            spec: trailing,
            extreme: None,
            trigger_price: Some("74250".into()),
        })
    );
}

/// An SL-only edit that keeps the trailing stop, started while a trailing edit is held on OKX,
/// reads the trailing stop in its own queued step: it keeps the new one, never the one in force
/// when it was called.
#[tokio::test]
async fn an_sl_only_edit_started_during_a_trailing_edit_keeps_the_new_trailing_stop() {
    let h = Harness::with_position().await;
    let hold = h.okx.hold_next("POST", "/trade/order-algo");
    let engine = h.engine.clone();
    let trailing = tokio::spawn(async move {
        let protection = Protection {
            trailing_stop: Some(ratio("0.01")),
            ..stop("74000")
        };
        engine
            .set_protection(ACCOUNT, "BTCUSDT", protection, None, None)
            .await
    });
    hold.reached().await;
    // Polled once while the trailing placement is held, so anything it reads before queueing is
    // read now, before the trailing stop exists.
    let mut edit = Box::pin(h.engine.set_protection_with(
        ACCOUNT,
        "BTCUSDT",
        stop("74500"),
        mochi_okx::engine::ProtectionRetention {
            trailing_stop: true,
            ..Default::default()
        },
        None,
        None,
    ));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(edit.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    hold.release();
    trailing.await.unwrap().unwrap();
    let result = edit.await.unwrap();
    assert_eq!(result.after.stop_loss.as_deref(), Some("74500"));
    assert_eq!(result.after.trailing_stop, Some(ratio("0.01")));
    assert_eq!(
        h.engine.protection_of("BTCUSDT").trailing_stop,
        Some(ratio("0.01"))
    );
}

#[tokio::test]
async fn refuses_an_invalid_stop_before_sending_anything() {
    let h = Harness::with_position().await;
    let error = h.set(stop("75500")).await.unwrap_err();
    assert_eq!(
        message(error),
        "INVALID_PROTECTION: stop 75500 is on the wrong side of 75000 for LONG"
    );
    assert!(h.algo_placed().is_empty());
}

#[tokio::test]
async fn refuses_a_target_on_the_wrong_side_and_an_invalid_trail_with_node_texts() {
    let h = Harness::with_position().await;
    let error = h
        .set(Protection {
            take_profit: Some("74900".into()),
            ..stop("74000")
        })
        .await
        .unwrap_err();
    assert_eq!(
        message(error),
        "INVALID_PROTECTION: target 74900 is on the wrong side of 75000 for LONG"
    );
    let error = h
        .set(Protection {
            trailing_stop: Some(ratio("0.5")),
            ..stop("74000")
        })
        .await
        .unwrap_err();
    assert_eq!(
        message(error),
        "INVALID_PROTECTION: trailing ratio 0.5 outside 0.001-0.3"
    );
    let error = h.set(none()).await.unwrap_err();
    assert_eq!(
        message(error),
        "LIMIT_STOP_REQUIRED: positions must keep a stop loss"
    );
    assert!(h.algo_placed().is_empty());
}

#[tokio::test]
async fn refuses_a_change_without_a_recent_sync() {
    let h = Harness::with_position().await;
    h.advance(31_000);
    let error = h.set(stop("74500")).await.unwrap_err();
    assert_eq!(
        message(error),
        "EXCHANGE_NOT_SYNCED: no recent sync with OKX"
    );
    assert!(h.algo_placed().is_empty());
}

#[tokio::test]
async fn a_refused_replacement_keeps_the_old_stop() {
    let h = Harness::with_position().await;
    let old = h.protection_rows().remove(0);
    h.okx.state().reject_next_algo = Some(Refusal::new("51000", "Parameter slTriggerPx error"));
    let error = h.set(stop("74500")).await.unwrap_err();
    assert!(message(error).contains("51000"));
    assert!(h.canceled().is_empty());
    assert_eq!(h.order_row(&old.id).status, OrderStatus::Open);
    let rejected = h
        .protection_rows()
        .into_iter()
        .find(|o| o.status == OrderStatus::Rejected)
        .unwrap();
    assert!(rejected.reject_reason.unwrap().contains("51000"));
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74000"));
}

#[tokio::test]
async fn an_accepted_change_whose_confirming_sync_fails_is_pending_never_nothing_changed() {
    let h = Harness::with_position().await;
    h.sync_down.armed.store(true, Ordering::SeqCst);
    let result = h.set(stop("74500")).await.unwrap();
    assert_eq!(h.algo_placed().len(), 1);
    assert_eq!(result.before, stop("74000"));
    assert!(result.pending);
}

#[tokio::test]
async fn a_part_way_refusal_whose_sync_fails_says_the_protection_in_force_is_unconfirmed() {
    let h = Harness::with_position().await;
    h.sync_down.armed.store(true, Ordering::SeqCst);
    h.okx.state().reject_next_algo = Some(Refusal::new("51000", "Parameter slTriggerPx error"));
    match h.set(stop("74500")).await.unwrap_err() {
        EngineError::ProtectionChange { message, after } => {
            assert_eq!(
                message,
                "OKX 51000: Parameter slTriggerPx error; current protection not yet confirmed from OKX"
            );
            assert_eq!(after, None);
        }
        other => panic!("expected a ProtectionChange, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unknown_outcome_placing_a_replacement_keeps_the_old_stop_and_is_resolved_by_sync_without_a_second_place(
) {
    let h = Harness::with_position().await;
    let old = h.protection_rows().remove(0);
    {
        let mut okx = h.okx.state();
        okx.lose_next_algo_response = true;
        okx.algo_lookup_down = true;
    }
    let result = h.set(stop("74500")).await.unwrap();
    assert_eq!(result.after, stop("74000"));
    assert!(result.pending);
    let pending = h
        .protection_rows()
        .into_iter()
        .find(|o| o.id != old.id)
        .unwrap();
    assert_eq!(pending.status, OrderStatus::Open);
    assert_eq!(pending.exchange_order_id, None);
    assert_eq!(pending.trigger_price.as_deref(), Some("74500"));
    assert_eq!(
        pending.reject_reason.as_deref(),
        Some("Awaiting confirmation: No answer from OKX: socket hang up")
    );
    assert_eq!(h.order_row(&old.id).status, OrderStatus::Open);
    assert!(h.canceled().is_empty());
    // A further change waits until the first one is resolved, rather than risking a duplicate.
    let error = h.set(stop("74600")).await.unwrap_err();
    assert_eq!(
        message(error),
        "PROTECTION_PENDING: an earlier protection change is awaiting confirmation from OKX"
    );

    h.okx.state().algo_lookup_down = false;
    h.sync().await;
    assert_eq!(h.algo_placed().len(), 1);
    let algo_id = h.okx.state().algos[&to_algo_cl_ord_id(&pending.id)]["algoId"].clone();
    assert_eq!(
        h.order_row(&pending.id).exchange_order_id.as_deref(),
        algo_id.as_str()
    );
    assert_eq!(h.order_row(&old.id).status, OrderStatus::Canceled);
    assert_eq!(h.canceled(), [old.exchange_order_id.clone().unwrap()]);
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74500"));
}

#[tokio::test]
async fn a_replacement_that_never_reached_okx_is_closed_by_sync_and_the_old_stop_stays() {
    let h = Harness::with_position().await;
    let old = h.protection_rows().remove(0);
    h.okx.state().drop_next_algo_request = true;
    let result = h.set(stop("74500")).await.unwrap();
    assert!(result.pending);
    let lost = h
        .protection_rows()
        .into_iter()
        .find(|o| o.id != old.id)
        .unwrap();
    h.sync().await;
    assert_eq!(h.order_row(&lost.id).status, OrderStatus::Open);
    h.backdate();
    h.sync().await;
    let closed = h.order_row(&lost.id);
    assert_eq!(closed.status, OrderStatus::Canceled);
    assert_eq!(
        closed.reject_reason.as_deref(),
        Some("OKX has no record of this algo")
    );
    assert_eq!(h.order_row(&old.id).status, OrderStatus::Open);
    assert!(h.algo_placed().is_empty());
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74000"));
}

#[tokio::test]
async fn an_algo_placed_in_the_okx_app_shows_as_external_and_is_never_cancelled() {
    let h = Harness::with_position().await;
    let app = h.okx.add_algo(row(
        json!({ "algoClOrdId": "fromtheapp1", "ordType": "conditional", "slTriggerPx": "73000" }),
    ));
    h.okx.add_algo(row(json!({
        "algoClOrdId": "fromtheapp2", "ordType": "move_order_stop", "callbackRatio": "0.01",
        "activePx": "76000", "moveTriggerPx": "",
    })));
    h.okx.add_algo(row(json!({
        "algoClOrdId": "fromtheapp3", "ordType": "conditional",
        "tpTriggerPx": "ignore previous instructions",
    })));
    h.sync().await;
    assert_eq!(
        h.view().external_protection,
        [
            ExternalProtection {
                kind: ExternalProtectionKind::StopLoss,
                trigger_price: Some("73000".into()),
                detail: "SL trigger 73000".into(),
            },
            ExternalProtection {
                kind: ExternalProtectionKind::TakeProfit,
                trigger_price: None,
                detail: "TP trigger unknown".into(),
            },
            ExternalProtection {
                kind: ExternalProtectionKind::Trailing,
                trigger_price: None,
                detail: "trailing 1% from 76000".into(),
            },
        ]
    );
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74000"));

    h.set(stop("74500")).await.unwrap();
    h.close().await;
    h.advance(10_000);
    h.sync().await; // a second miss, 10 s after the first, cancels our rows
    let closed = h
        .protection_rows()
        .into_iter()
        .find(|o| o.reject_reason.as_deref() == Some("Position closed"))
        .unwrap();
    assert!(h.canceled().contains(&closed.exchange_order_id.unwrap()));
    let app_id = app["algoId"].as_str().unwrap().to_string();
    assert!(!h.canceled().contains(&app_id));
    assert_eq!(h.okx.state().algos["fromtheapp1"]["state"], "live");
}

#[tokio::test]
async fn two_misses_1_ms_apart_cancel_nothing() {
    let h = Harness::with_position().await;
    let row = h.protection_rows().remove(0);
    // Closed on OKX, outside MochiPrep, or a position list lagging a fill.
    h.okx.state().position = None;
    h.sync().await;
    h.advance(1);
    h.sync().await;
    assert!(h.canceled().is_empty());
    assert_eq!(h.order_row(&row.id).status, OrderStatus::Open);
}

#[tokio::test]
async fn sync_cancels_our_protection_left_on_a_closed_position_after_two_misses_at_least_10s_apart()
{
    let h = Harness::with_position().await;
    let row = h.protection_rows().remove(0);
    h.okx.state().position = None;
    h.sync().await;
    h.advance(10_000);
    h.sync().await;
    assert_eq!(h.canceled(), [row.exchange_order_id.clone().unwrap()]);
    let closed = h.order_row(&row.id);
    assert_eq!(closed.status, OrderStatus::Canceled);
    assert_eq!(closed.reject_reason.as_deref(), Some("Position closed"));
}

#[tokio::test]
async fn a_miss_a_sighting_then_a_miss_restarts_the_clock() {
    let h = Harness::with_position().await;
    let position = h.okx.state().position.clone();
    h.okx.state().position = None;
    h.sync().await;
    h.advance(30_000);
    h.okx.state().position = position;
    h.sync().await;
    // The first miss was 30 s ago, but the sighting in between restarted the clock.
    h.okx.state().position = None;
    h.sync().await;
    h.advance(9_000);
    h.sync().await;
    assert!(h.canceled().is_empty());
    h.advance(1_000);
    h.sync().await;
    assert_eq!(h.canceled().len(), 1);
}

fn no_stop_required() -> RiskSettings {
    RiskSettings {
        require_stop_loss: false,
        ..default_live_limits()
    }
}

#[tokio::test]
async fn swapping_the_stop_for_a_trailing_stop_cancels_nothing_when_the_trailing_stop_is_refused() {
    let h = Harness::with_position().await;
    h.set_limits(no_stop_required());
    let old = h.protection_rows().remove(0);
    h.okx.state().reject_algo_if = Some(Box::new(|body| body["ordType"] == "move_order_stop"));
    let swap = Protection {
        trailing_stop: Some(ratio("0.01")),
        ..none()
    };
    let error = h.set(swap).await.unwrap_err();
    assert!(message(error).contains("51000"));
    assert_eq!(h.index_of("POST /api/v5/trade/cancel-algos"), None);
    assert_eq!(h.order_row(&old.id).status, OrderStatus::Open);
    assert_eq!(h.algo_state(&old.id), "live");
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74000"));
}

#[tokio::test]
async fn swapping_the_stop_for_a_trailing_stop_cancels_nothing_while_the_outcome_is_unknown() {
    let h = Harness::with_position().await;
    h.set_limits(no_stop_required());
    let old = h.protection_rows().remove(0);
    {
        let mut okx = h.okx.state();
        okx.lose_next_algo_response = true;
        okx.algo_lookup_down = true;
    }
    let trailing = ratio("0.01");
    let result = h
        .set(Protection {
            trailing_stop: Some(trailing.clone()),
            ..none()
        })
        .await
        .unwrap();
    assert_eq!(result.after, stop("74000"));
    assert!(result.pending);
    h.okx.state().algo_lookup_down = false;
    h.sync().await;
    assert_eq!(h.index_of("POST /api/v5/trade/cancel-algos"), None);
    assert_eq!(h.algo_state(&old.id), "live");
    // The removal was never carried out, so the stop is still shown as in force beside the trail.
    assert_eq!(
        h.engine.protection_of("BTCUSDT"),
        Protection {
            trailing_stop: Some(trailing),
            ..stop("74000")
        }
    );
    assert_eq!(h.algo_placed().len(), 1);
}

#[tokio::test]
async fn an_unknown_outcome_that_the_follow_up_sync_confirms_completes_the_change_without_a_second_place(
) {
    let h = Harness::with_position().await;
    h.set_limits(no_stop_required());
    let old = h.protection_rows().remove(0);
    h.okx.state().lose_next_algo_response = true;
    let trailing = ratio("0.01");
    let target = Protection {
        trailing_stop: Some(trailing),
        ..none()
    };
    let result = h.set(target.clone()).await.unwrap();
    assert_eq!(result.before, stop("74000"));
    assert_eq!(result.after, target);
    assert!(!result.pending);
    assert_eq!(h.algo_placed().len(), 1);
    // The stop is removed only after OKX listed the trailing stop as live.
    let confirmed = h.index_of("GET /api/v5/trade/orders-algo-pending").unwrap();
    assert!(h.index_of("POST /api/v5/trade/cancel-algos").unwrap() > confirmed);
    assert_eq!(h.canceled(), [old.exchange_order_id.clone().unwrap()]);
    let removed = h.order_row(&old.id);
    assert_eq!(removed.status, OrderStatus::Canceled);
    assert_eq!(removed.reject_reason.as_deref(), Some("Removed"));
}

#[tokio::test]
async fn a_change_refused_part_way_reports_what_is_in_force_including_the_leg_already_placed() {
    let h = Harness::with_position().await;
    h.okx.state().reject_algo_if = Some(Box::new(|body| body["ordType"] == "move_order_stop"));
    let target = Protection {
        trailing_stop: Some(ratio("0.01")),
        ..stop("74500")
    };
    match h.set(target).await.unwrap_err() {
        EngineError::ProtectionChange { message, after } => {
            assert_eq!(message, "OKX 51000: Parameter error");
            assert_eq!(after.as_deref(), Some(&stop("74500")));
        }
        other => panic!("expected a ProtectionChange, got {other:?}"),
    }
}

#[tokio::test]
async fn an_algo_whose_cancel_keeps_failing_stays_on_the_view_marked_and_the_cancel_is_retried() {
    let h = Harness::with_position().await;
    let old = h.protection_rows().remove(0);
    h.okx.state().cancel_fails = true;
    h.set(stop("74500")).await.unwrap();
    assert_eq!(h.order_row(&old.id).status, OrderStatus::Canceled);
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74500"));
    assert_eq!(h.view().external_protection, [cancel_pending("74000")]);
    assert!(h
        .engine
        .error()
        .unwrap()
        .contains("Could not cancel an OKX algo order"));
    assert!(h.engine.healthy());

    let attempts = h.count("POST /api/v5/trade/cancel-algos");
    h.sync().await;
    assert_eq!(h.count("POST /api/v5/trade/cancel-algos"), attempts + 1);

    h.okx.state().cancel_fails = false;
    h.sync().await;
    assert_eq!(h.algo_state(&old.id), "canceled");
    h.sync().await;
    assert!(h.view().external_protection.is_empty());
    assert_eq!(h.engine.error(), None);
}

#[tokio::test]
async fn a_cancelled_algo_that_is_still_pending_is_cancelled_again() {
    let h = Harness::with_position().await;
    let old = h.protection_rows().remove(0);
    h.set(stop("74500")).await.unwrap();
    {
        // The cancel did not take.
        let mut okx = h.okx.state();
        let key = to_algo_cl_ord_id(&old.id);
        okx.algos
            .get_mut(&key)
            .unwrap()
            .insert("state".into(), json!("live"));
        okx.canceled_algo_ids.clear();
    }
    h.sync().await;
    assert_eq!(h.canceled(), [old.exchange_order_id.clone().unwrap()]);
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74500"));
}

#[tokio::test]
async fn marks_our_stop_gone_when_okx_no_longer_has_it_pending() {
    let h = Harness::with_position().await;
    let row = h.protection_rows().remove(0);
    {
        let mut okx = h.okx.state();
        let key = to_algo_cl_ord_id(&row.id);
        okx.algos
            .get_mut(&key)
            .unwrap()
            .insert("state".into(), json!("canceled"));
    }
    h.sync().await;
    let gone = h.order_row(&row.id);
    assert_eq!(gone.status, OrderStatus::Canceled);
    assert_eq!(gone.reject_reason.as_deref(), Some("Gone from OKX"));
    assert_eq!(h.engine.protection_of("BTCUSDT"), none());
}

#[tokio::test]
async fn a_temporary_algo_lookup_refusal_preserves_the_last_confirmed_stop() {
    let h = Harness::with_position().await;
    let row = h.protection_rows().remove(0);
    {
        let mut okx = h.okx.state();
        okx.algos[&to_algo_cl_ord_id(&row.id)].insert("state".into(), json!("partially_effective"));
        okx.algo_lookup_error_code = Some("50011".into());
    }
    assert!(h.engine.sync().await.is_err());
    assert_eq!(h.order_row(&row.id).status, OrderStatus::Open);
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74000"));

    // A positive not-found reply can retire a missing stop.
    {
        let mut okx = h.okx.state();
        okx.algos.shift_remove(&to_algo_cl_ord_id(&row.id));
        okx.algo_lookup_error_code = Some("51603".into());
    }
    h.sync().await;
    assert_eq!(h.order_row(&row.id).status, OrderStatus::Canceled);
}

#[tokio::test]
async fn still_lets_a_disarmed_account_close_the_position() {
    let h = Harness::with_position().await;
    h.disarm();
    let exit = h.close().await;
    assert_eq!(exit.order.status, OrderStatus::Filled);
}

#[tokio::test]
async fn refuses_to_reopen_while_the_previous_positions_protection_is_being_cleaned_up() {
    let h = Harness::with_position().await;
    let old = h.protection_rows().remove(0);
    h.close().await;
    let placed = h.okx.state().placed.len();
    let early = h.engine.place_order(order()).await.unwrap();
    assert_eq!(early.order.status, OrderStatus::Rejected);
    assert_eq!(
        early.order.reject_reason.as_deref(),
        Some("PROTECTION_PENDING: protection from the previous position is still being cleaned up")
    );
    assert_eq!(h.okx.state().placed.len(), placed);

    // The cleanup cancel fails: the algo is still live, so reopening is still refused, now by
    // the leftovers alone (the row itself is closed).
    h.okx.state().cancel_fails = true;
    h.sync().await;
    h.advance(10_000);
    h.sync().await;
    let closed = h.order_row(&old.id);
    assert_eq!(closed.status, OrderStatus::Canceled);
    assert_eq!(closed.reject_reason.as_deref(), Some("Position closed"));
    let again = h.engine.place_order(order()).await.unwrap();
    assert!(again
        .order
        .reject_reason
        .unwrap()
        .starts_with("PROTECTION_PENDING"));

    // Reopened in the OKX app, same side: the old algo is never re-adopted, and is cancelled
    // again.
    h.okx.state().position = Some(long_position());
    h.sync().await;
    assert_eq!(h.engine.protection_of("BTCUSDT"), none());
    h.okx.state().position = None;

    h.okx.state().cancel_fails = false;
    h.sync().await;
    h.advance(20_000);
    h.sync().await;
    assert_eq!(h.algo_state(&old.id), "canceled");
    h.sync().await;
    let mut request = order();
    request.stop_loss = Some("74500".into());
    let reopened = h.engine.place_order(request).await.unwrap();
    assert_eq!(reopened.order.status, OrderStatus::Filled);
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74500"));
}

#[tokio::test]
async fn a_position_on_the_other_side_never_shows_the_old_algos_as_in_force_and_they_are_cancelled()
{
    let h = Harness::with_position().await;
    let old = h.protection_rows().remove(0);
    h.close().await;
    h.okx.state().cancel_fails = true;
    // A short opened in the OKX app within the cleanup window: the old SELL stop does not close
    // it.
    h.okx.state().position = Some(short_position());
    h.sync().await;
    assert_eq!(h.engine.protection_of("BTCUSDT"), none());
    let closed = h.order_row(&old.id);
    assert_eq!(closed.status, OrderStatus::Canceled);
    assert_eq!(closed.reject_reason.as_deref(), Some("Position closed"));
    assert_eq!(h.view().stop_loss, None);
    assert_eq!(h.view().external_protection, [cancel_pending("74000")]);
    // Still live after the failed cancel: retried, never re-adopted.
    let attempts = h.count("POST /api/v5/trade/cancel-algos");
    h.sync().await;
    assert_eq!(h.count("POST /api/v5/trade/cancel-algos"), attempts + 1);
    assert_eq!(h.engine.protection_of("BTCUSDT"), none());

    // Exits are unaffected, even disarmed.
    h.disarm();
    let exit = h.close().await;
    assert_eq!(exit.order.status, OrderStatus::Filled);
    assert_eq!(exit.order.side, OrderSide::Buy);
}

#[tokio::test]
async fn entry_with_fixed_stop_defers_trailing_until_fill() {
    let h = connect(Vec::new()).await;
    let trailing = ratio("0.01");
    let mut request = order();
    request.trailing_stop = Some(trailing.clone());
    let entry = h.engine.place_order(request).await.unwrap().order;
    assert_eq!(
        h.okx.state().placed[0]["attachAlgoOrds"],
        json!([
            { "attachAlgoClOrdId": to_algo_cl_ord_id(&format!("{}:sl", entry.id)), "slTriggerPx": "74000", "slOrdPx": "-1" },
        ])
    );
    assert_eq!(entry.status, OrderStatus::Filled);
    assert_eq!(h.algo_placed().len(), 1);
    assert_eq!(h.algo_placed()[0]["ordType"], "move_order_stop");
    assert_eq!(
        h.algo_placed()[0]["algoClOrdId"],
        to_algo_cl_ord_id(&format!("{}:trail", entry.id))
    );
    assert_eq!(
        h.engine.protection_of("BTCUSDT"),
        Protection {
            trailing_stop: Some(trailing.clone()),
            ..stop("74000")
        }
    );
    assert_eq!(
        h.view().trailing_stop,
        Some(TrailingStopView {
            spec: trailing,
            extreme: None,
            trigger_price: Some("74250".into()),
        })
    );
    let trail = h.order_row(&format!("{}:trail", entry.id));
    assert_eq!(trail.order_type, OrderType::TrailingStopMarket);
    assert_eq!(trail.trailing_kind, Some(TrailingKind::Ratio));
    assert_eq!(trail.trailing_callback.as_deref(), Some("0.01"));
    assert_eq!(trail.status, OrderStatus::Open);
}

// ---- added pairs ------------------------------------------------------------------------------

#[tokio::test]
async fn never_treats_a_held_pair_as_closed_because_it_is_missing_from_the_symbol_registry() {
    let h = connect(vec![pepe()]).await;
    *h.prices.lock().unwrap() = ("0.0000099".into(), "0.0000101".into(), "0.00001".into());
    let mut request = order();
    request.symbol = "PEPEUSDT".into();
    request.quantity = "3500000".into();
    request.stop_loss = Some("0.000009".into());
    h.engine.place_order(request).await.unwrap();
    let row = h.protection_rows().remove(0);
    assert!(row.exchange_order_id.is_some());

    // Node's unregisterInstrument: the pair leaves the registry while OKX still holds it.
    h.store
        .inner
        .lock()
        .unwrap()
        .1
        .retain(|s| s.symbol != "PEPEUSDT");
    h.sync().await;
    h.sync().await;
    assert!(h.canceled().is_empty());
    assert_eq!(h.order_row(&row.id).status, OrderStatus::Open);
}

// ---- what a PROTECT decision meets --------------------------------------------------------------

#[tokio::test]
async fn a_target_that_differs_only_off_the_tick_changes_nothing() {
    let h = Harness::with_position().await;
    // BTC's tick is 0.1, so 74000.04 is the stop already in force.
    let result = h.set(stop("74000.04")).await.unwrap();
    assert_eq!(result.before, stop("74000"));
    assert_eq!(result.after, stop("74000"));
    assert!(!result.pending);
    assert!(h.posts().is_empty());
}

#[tokio::test]
async fn no_position_is_an_error_with_node_text() {
    let h = connect(Vec::new()).await;
    let error = h.set(stop("74000")).await.unwrap_err();
    assert_eq!(message(error), "No open BTCUSDT position");
}

/// Node reads an answer it cannot parse as a plain `Error` (here, a JSON `null` body), and
/// `applyProtection` treats every error but an unknown outcome as a refusal: the row is REJECTED
/// and nothing is cancelled.
#[tokio::test]
async fn an_unreadable_answer_to_a_placement_is_a_refusal_not_an_unknown_outcome() {
    let h = Harness::with_position().await;
    let old = h.protection_rows().remove(0);
    h.sync_down.null_next_algo.store(true, Ordering::SeqCst);
    match h.set(stop("74500")).await.unwrap_err() {
        EngineError::ProtectionChange { message, after } => {
            assert_eq!(message, "Cannot read properties of null (reading 'code')");
            assert_eq!(after.as_deref(), Some(&stop("74000")));
        }
        other => panic!("expected a ProtectionChange, got {other:?}"),
    }
    let rejected = h
        .protection_rows()
        .into_iter()
        .find(|o| o.status == OrderStatus::Rejected)
        .unwrap();
    assert_eq!(
        rejected.reject_reason.as_deref(),
        Some("Cannot read properties of null (reading 'code')")
    );
    assert!(h.canceled().is_empty());
    assert_eq!(h.order_row(&old.id).status, OrderStatus::Open);
}

// ---- stacked entries ----------------------------------------------------------------------------

/// The live stop-loss algos on the fake, as (sz, trigger), oldest first.
fn live_stops(h: &Harness) -> Vec<(String, String)> {
    h.okx
        .state()
        .algos
        .values()
        .filter(|a| a["state"] == "live" && a["ordType"] == "conditional")
        .filter_map(|a| {
            Some((
                a["sz"].as_str()?.to_string(),
                a.get("slTriggerPx")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// The contracts the live stops cover between them, against the fake's position.
fn stop_cover(h: &Harness) -> (f64, f64) {
    let covered: f64 = live_stops(h)
        .iter()
        .map(|(sz, _)| sz.parse::<f64>().unwrap())
        .sum();
    let held = h.okx.state().position.as_ref().unwrap()["pos"]
        .as_str()
        .unwrap()
        .parse::<f64>()
        .unwrap();
    (covered, held)
}

/// Two resting BTC longs, each with its own stop, both filled on OKX before the next sync.
async fn two_stacked_fills() -> Harness {
    stacked_fills(["74000", "74200"].map(stop)).await
}

/// Resting BTC longs with this protection each, oldest first, all filled on OKX before the next
/// sync. Without a stop among them, the account does not require one.
async fn stacked_fills<const N: usize>(protection: [Protection; N]) -> Harness {
    let h = connect(Vec::new()).await;
    if protection.iter().all(|p| p.stop_loss.is_none()) {
        h.set_limits(no_stop_required());
    }
    h.okx.state().rest_limit_orders = true;
    for protection in protection {
        let mut request = order();
        request.order_type = EntryOrderType::Limit;
        request.price = Some("74500".into());
        request.stop_loss = protection.stop_loss;
        request.take_profit = protection.take_profit;
        request.trailing_stop = protection.trailing_stop;
        let placed = h.engine.place_order(request).await.unwrap();
        assert_eq!(placed.order.status, OrderStatus::Open);
    }
    h.okx.state().fill_resting();
    h.okx.state().calls.clear();
    h
}

/// Each fill attaches a stop sized to itself. Retiring the older one as replaced would leave half
/// the position without a stop on OKX, so the older stops stay until one stop covers it all,
/// placed before anything is cancelled.
#[tokio::test]
async fn two_entries_filling_into_one_position_keep_the_whole_position_stopped() {
    let h = two_stacked_fills().await;
    h.sync().await;
    let (covered, held) = stop_cover(&h);
    assert_eq!(held, 0.2);
    assert!(covered >= held, "{covered} of {held} contracts stopped");
    assert!(h.canceled().is_empty());
    // One stop for the whole position, at the newest entry's level.
    let sent = h.algo_placed();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["sz"], "0.2");
    assert_eq!(sent[0]["slTriggerPx"], "74200");
    assert_eq!(sent[0]["reduceOnly"], true);
    assert_eq!(h.view().stop_loss.as_deref(), Some("74200"));

    // OKX lists it: only now are the per-fill stops retired.
    h.sync().await;
    assert_eq!(live_stops(&h), [("0.2".to_string(), "74200".to_string())]);
    assert_eq!(h.canceled().len(), 2);
    let placed = h.index_of("POST /api/v5/trade/order-algo").unwrap();
    let cancelled = h.index_of("POST /api/v5/trade/cancel-algos").unwrap();
    assert!(cancelled > placed);
    assert_eq!(h.view().stop_loss.as_deref(), Some("74200"));

    // Settled: nothing more is placed or cancelled, and nothing is left marked cancel pending.
    h.sync().await;
    assert_eq!(h.algo_placed().len(), 1);
    assert_eq!(h.canceled().len(), 2);
    assert!(h.view().external_protection.is_empty());
}

#[tokio::test]
async fn unsent_optional_exits_do_not_block_fixed_stop_coverage() {
    let h = stacked_fills([
        Protection {
            take_profit: Some("78000".into()),
            ..stop("74000")
        },
        stop("74200"),
    ])
    .await;
    // This quote crossed the target, but the fill has the same millisecond timestamp.
    *h.prices.lock().unwrap() = ("78999".into(), "79001".into(), "79000".into());
    h.sync().await;
    h.sync().await;
    assert_eq!(live_stops(&h), [("0.2".to_string(), "74200".to_string())]);
    assert_eq!(h.okx.state().placed.len(), 2);
}

/// Disarmed, nothing is placed, and each fill keeps its own stop: the position stays covered.
#[tokio::test]
async fn disarmed_two_stacked_fills_keep_their_own_stops() {
    let h = two_stacked_fills().await;
    h.disarm();
    h.sync().await;
    h.sync().await;
    assert!(h.algo_placed().is_empty());
    assert!(h.canceled().is_empty());
    let (covered, held) = stop_cover(&h);
    assert!(covered >= held, "{covered} of {held} contracts stopped");
    assert_eq!(h.view().stop_loss.as_deref(), Some("74200"));
}

/// A refused full-size stop is not retried every sync, and the per-fill stops stay.
#[tokio::test]
async fn a_refused_full_size_stop_leaves_the_per_fill_stops_and_is_not_retried() {
    let h = two_stacked_fills().await;
    h.okx.state().reject_next_algo = Some(Refusal::new("51000", "Parameter error"));
    h.sync().await;
    h.sync().await;
    assert_eq!(h.algo_placed().len(), 1);
    assert!(h.canceled().is_empty());
    let (covered, held) = stop_cover(&h);
    assert!(covered >= held, "{covered} of {held} contracts stopped");
}

/// A full-size stop whose answer is lost is looked up on the next sync, never sent again.
#[tokio::test]
async fn a_full_size_stop_with_an_unknown_outcome_is_resolved_by_lookup_never_resent() {
    let h = two_stacked_fills().await;
    h.okx.state().lose_next_algo_response = true;
    h.sync().await;
    assert_eq!(h.algo_placed().len(), 1);
    assert!(h.canceled().is_empty());
    h.sync().await;
    assert_eq!(h.algo_placed().len(), 1);
    assert_eq!(live_stops(&h), [("0.2".to_string(), "74200".to_string())]);
    assert_eq!(h.canceled().len(), 2);
}

/// The full-size stop takes the most protective live level, never the newest when an older fill's
/// stop is tighter: retiring the older stop at 74200 for one at 74000 would loosen it.
#[tokio::test]
async fn a_tighter_older_stop_is_kept_as_the_full_size_stops_level() {
    let h = stacked_fills(["74200", "74000"].map(stop)).await;
    h.sync().await;
    let sent = h.algo_placed();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["sz"], "0.2");
    assert_eq!(sent[0]["slTriggerPx"], "74200");
    h.sync().await;
    assert_eq!(live_stops(&h), [("0.2".to_string(), "74200".to_string())]);
    assert_eq!(h.view().stop_loss.as_deref(), Some("74200"));
}

/// The same for take-profit: the target closest to the price wins, for a long the lowest.
#[tokio::test]
async fn a_nearer_older_target_is_kept_as_the_full_size_targets_level() {
    let target = |price: &str| Protection {
        stop_loss: None,
        take_profit: Some(price.into()),
        trailing_stop: None,
    };
    let h = stacked_fills([target("76000"), target("77000")]).await;
    h.sync().await;
    let sent = h.algo_placed();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["sz"], "0.2");
    assert_eq!(sent[0]["tpTriggerPx"], "76000");
}

/// Stacked trailing stops are never replaced by a full-size one: each per-fill trail stays, and
/// between them they cover the position. A new trail would start from the current price.
#[tokio::test]
async fn stacked_trailing_stops_are_never_covered_by_a_new_one() {
    let trail = || Protection {
        stop_loss: None,
        take_profit: None,
        trailing_stop: Some(ratio("0.01")),
    };
    let h = stacked_fills([trail(), trail()]).await;
    h.sync().await;
    h.sync().await;
    assert!(h.algo_placed().is_empty());
    assert!(h.canceled().is_empty());
    let trails = h
        .okx
        .state()
        .algos
        .values()
        .filter(|a| a["state"] == "live" && a["ordType"] == "move_order_stop")
        .count();
    assert_eq!(trails, 2);
}

/// A full-size stop on the wrong side of the price is refused by the protection-change checks:
/// nothing is written or sent, the per-fill stops stay, and it is tried again once the price is
/// back.
#[tokio::test]
async fn a_full_size_stop_on_the_wrong_side_of_the_price_sends_nothing() {
    let h = two_stacked_fills().await;
    *h.prices.lock().unwrap() = ("74149".into(), "74151".into(), "74150".into());
    h.sync().await;
    h.sync().await;
    assert!(h.algo_placed().is_empty());
    assert!(h.canceled().is_empty());
    assert!(!h.protection_rows().iter().any(|r| r.id.contains(":cover:")));
    let (covered, held) = stop_cover(&h);
    assert!(covered >= held, "{covered} of {held} contracts stopped");

    *h.prices.lock().unwrap() = ("74999".into(), "75001".into(), "75000".into());
    h.sync().await;
    assert_eq!(h.algo_placed().len(), 1);
}

/// A stale quote refuses the full-size stop too: nothing is sent.
#[tokio::test]
async fn a_full_size_stop_with_a_stale_quote_sends_nothing() {
    let h = two_stacked_fills().await;
    let mut limits = default_live_limits();
    limits.max_quote_age_sec = -1.0;
    h.set_limits(limits);
    h.sync().await;
    assert!(h.algo_placed().is_empty());
    assert!(h.canceled().is_empty());
}

/// An order whose fill keeps failing to write stays OPEN and fails the sync, but never stops the
/// other orders or protection from being brought in line, and its fill is recorded once the
/// write succeeds. Nothing is resent.
#[tokio::test]
async fn a_fill_that_keeps_failing_to_write_does_not_block_the_other_orders() {
    let h = connect(Vec::new()).await;
    h.okx.state().rest_limit_orders = true;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let mut request = order();
        request.order_type = EntryOrderType::Limit;
        request.price = Some("74500".into());
        ids.push(h.engine.place_order(request).await.unwrap().order.id);
    }
    h.okx.state().fill_resting();
    h.okx.state().calls.clear();
    h.store(|s| {
        s.connection()
            .execute_batch(&format!(
                "CREATE TRIGGER refuse_fill BEFORE INSERT ON fills WHEN NEW.order_id='{}'
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
                ids[0]
            ))
            .unwrap()
    });
    for _ in 0..2 {
        let error = h.engine.sync().await.unwrap_err().to_string();
        assert!(error.contains("refused"), "{error}");
        assert!(!h.engine.healthy());
        assert_eq!(h.order_row(&ids[0]).status, OrderStatus::Open);
        assert_eq!(h.order_row(&ids[1]).status, OrderStatus::Filled);
        // Protection was still reconciled: the second entry's stop is matched as ours.
        assert_eq!(h.view().stop_loss.as_deref(), Some("74000"));
    }
    h.store(|s| {
        s.connection()
            .execute_batch("DROP TRIGGER refuse_fill")
            .unwrap()
    });
    h.sync().await;
    assert!(h.engine.healthy());
    assert_eq!(h.order_row(&ids[0]).status, OrderStatus::Filled);
    let fills = h.store(|s| s.list_fills_by_order(&ids[0]).unwrap());
    assert_eq!(fills.len(), 1);
    assert_eq!(h.count("POST /api/v5/trade/order"), 0);
}

fn short_position() -> Row {
    row(json!({
        "instId": "BTC-USDT-SWAP", "pos": "-0.1", "avgPx": "75000", "markPx": "75000", "upl": "0",
        "lever": "3", "liqPx": "90000", "margin": "10", "mgnMode": "isolated", "posSide": "net",
    }))
}

/// A replaced stop whose cancel keeps failing is retired for good once its position closes: a
/// later same-side position never takes it up.
#[tokio::test]
async fn a_replaced_stop_retired_with_its_closed_position_is_never_taken_up_again() {
    let h = Harness::with_position().await;
    let old = h.protection_rows().remove(0);
    h.okx.state().cancel_fails = true;
    h.set(stop("74500")).await.unwrap();
    assert_eq!(
        h.order_row(&old.id).reject_reason.as_deref(),
        Some("Replaced")
    );
    h.close().await;
    h.advance(10_000);
    h.sync().await;
    let retired = h.order_row(&old.id);
    assert_eq!(retired.status, OrderStatus::Canceled);
    assert_eq!(retired.reject_reason.as_deref(), Some("Position closed"));
    assert_eq!(h.algo_state(&old.id), "live");

    // Reopened in the OKX app, same side, while both cancels still fail.
    h.okx.state().position = Some(long_position());
    h.sync().await;
    assert_eq!(h.engine.protection_of("BTCUSDT"), none());
    assert_eq!(h.order_row(&old.id).status, OrderStatus::Canceled);
}

/// The same for a replaced stop retired because the position flipped to the other side.
#[tokio::test]
async fn a_replaced_stop_retired_by_a_flip_is_never_taken_up_by_the_next_same_side_position() {
    let h = Harness::with_position().await;
    let old = h.protection_rows().remove(0);
    h.okx.state().cancel_fails = true;
    h.set(stop("74500")).await.unwrap();
    h.okx.state().position = Some(short_position());
    h.sync().await;
    assert_eq!(
        h.order_row(&old.id).reject_reason.as_deref(),
        Some("Position closed")
    );
    h.okx.state().position = Some(long_position());
    h.sync().await;
    assert_eq!(h.engine.protection_of("BTCUSDT"), none());
    assert_eq!(h.order_row(&old.id).status, OrderStatus::Canceled);
}

/// A cancel OKX refused is still retried once 500 newer orders have pushed its row out of the
/// recent window: the live algo is matched to its row by algoId, never read as external.
#[tokio::test]
async fn a_refused_cancel_is_still_retried_after_its_row_leaves_the_recent_window() {
    let h = Harness::with_position().await;
    let old = h.protection_rows().remove(0);
    h.okx.state().cancel_fails = true;
    h.set(stop("74500")).await.unwrap();
    let entry = h
        .store(|s| s.list_orders(ACCOUNT, 100).unwrap())
        .into_iter()
        .find(|o| !o.close_position)
        .unwrap();
    let now = h.clock.load(Ordering::SeqCst);
    h.store(|s| {
        for i in 0..500 {
            let filler = Order {
                id: format!("filler_{i}"),
                client_order_id: format!("filler_{i}"),
                created_at: now + 1 + i,
                updated_at: now + 1 + i,
                ..entry.clone()
            };
            s.insert_order(&filler).unwrap();
        }
    });
    let attempts = h.count("POST /api/v5/trade/cancel-algos");
    h.sync().await;
    assert_eq!(h.count("POST /api/v5/trade/cancel-algos"), attempts + 1);
    assert_eq!(h.view().external_protection, [cancel_pending("74000")]);
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74500"));

    h.okx.state().cancel_fails = false;
    h.sync().await;
    assert_eq!(h.algo_state(&old.id), "canceled");
}

/// The position list can lag behind the fills the same sync confirmed. Read with only the first
/// of two stacked fills, the newest per-fill stop looks as if it covers the position, but it
/// covers only its own fill: the older stop must stay until a stop sized to both is live.
#[tokio::test]
async fn a_lagging_position_never_retires_the_older_stop_of_two_stacked_fills() {
    let h = two_stacked_fills().await;
    h.okx.state().position.as_mut().unwrap()["pos"] = json!("0.1");
    h.sync().await;
    assert!(h.canceled().is_empty());
    // The full-size stop covers both confirmed fills, not the lagging position.
    let sent = h.algo_placed();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["sz"], "0.2");

    h.okx.state().position.as_mut().unwrap()["pos"] = json!("0.2");
    h.sync().await;
    assert_eq!(live_stops(&h), [("0.2".to_string(), "74200".to_string())]);
    assert_eq!(h.canceled().len(), 2);
    let (covered, held) = stop_cover(&h);
    assert!(covered >= held, "{covered} of {held} contracts stopped");
}

/// A per-fill stop that OKX lists at a size larger than its entry filled (the size first sent,
/// say) is still read as covering only its own fill: the older stop stays.
#[tokio::test]
async fn an_attached_stop_listed_larger_than_its_fill_never_retires_the_older_stop() {
    let h = two_stacked_fills().await;
    {
        let mut state = h.okx.state();
        let newest = state.algos.values_mut().last().unwrap();
        newest["sz"] = json!("0.2");
    }
    h.disarm();
    h.sync().await;
    h.sync().await;
    assert!(h.canceled().is_empty());
    assert_eq!(live_stops(&h).len(), 2);
}

#[tokio::test]
async fn combined_entry_keeps_fixed_stop_while_a_deferred_response_is_lost() {
    let h = connect(Vec::new()).await;
    let mut request = order();
    request.take_profit = Some("78000".into());
    request.trailing_stop = Some(ratio("0.015"));
    h.okx.state().lose_next_algo_response = true;
    let result = h.engine.place_order(request.clone()).await.unwrap();
    assert_eq!(result.order.status, OrderStatus::Filled);
    assert_eq!(result.fills.len(), 1);
    assert_eq!(
        h.okx.state().placed[0]["attachAlgoOrds"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        h.okx.state().placed[0]["attachAlgoOrds"][0]["slTriggerPx"],
        "74000"
    );
    h.sync().await;
    let duplicate = h.engine.place_order(request).await.unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(h.okx.state().placed.len(), 1);
    assert_eq!(h.algo_placed().len(), 2);
    assert_eq!(
        h.engine.protection_of("BTCUSDT"),
        Protection {
            stop_loss: Some("74000".into()),
            take_profit: Some("78000".into()),
            trailing_stop: Some(ratio("0.015")),
        }
    );
    assert!(h.canceled().is_empty());
}

#[tokio::test]
async fn deferred_trailing_waits_for_limit_fill_without_expiring() {
    let h = connect(Vec::new()).await;
    h.okx.state().rest_limit_orders = true;
    let mut request = order();
    request.order_type = EntryOrderType::Limit;
    request.price = Some("74500".into());
    request.trailing_stop = Some(ratio("0.01"));
    let entry = h.engine.place_order(request).await.unwrap().order;
    h.advance(120_000);
    h.sync().await;
    assert!(h.algo_placed().is_empty());
    assert_eq!(
        h.order_row(&format!("{}:trail", entry.id)).status,
        OrderStatus::Open
    );
    h.okx.state().fill_resting();
    h.sync().await;
    h.sync().await;
    assert_eq!(h.algo_placed().len(), 1);
    assert_eq!(
        h.engine.protection_of("BTCUSDT").trailing_stop,
        Some(ratio("0.01"))
    );
}

#[tokio::test]
async fn deferred_trailing_starts_tracking_when_activation_was_reached_while_waiting() {
    let h = connect(Vec::new()).await;
    h.okx.state().rest_limit_orders = true;
    let mut request = order();
    request.order_type = EntryOrderType::Limit;
    request.price = Some("74500".into());
    request.trailing_stop = Some(TrailingStopSpec {
        activation_price: Some("76000".into()),
        ..ratio("0.01")
    });
    let entry = h.engine.place_order(request).await.unwrap().order;
    *h.prices.lock().unwrap() = ("76999".into(), "77001".into(), "77000".into());
    h.okx.state().fill_resting();
    let client_id = mochi_okx::to_cl_ord_id(&entry.client_order_id);
    h.okx.state().orders[&client_id].insert("fillTime".into(), json!((START + 1).to_string()));
    h.quote_time.store(START, Ordering::SeqCst);
    h.sync().await;
    assert!(
        h.algo_placed().is_empty(),
        "the pre-fill quote cannot activate trailing"
    );
    h.advance(2);
    h.sync().await;
    assert!(h.algo_placed().is_empty());
    h.quote_time.store(START + 2, Ordering::SeqCst);
    h.sync().await;
    let submitted = h.algo_placed();
    assert_eq!(submitted.len(), 1);
    assert!(submitted[0].get("activePx").is_none());
    assert_eq!(
        h.engine.protection_of("BTCUSDT").trailing_stop,
        Some(ratio("0.01"))
    );
}

async fn deferred_limit() -> (Harness, Order) {
    let h = connect(Vec::new()).await;
    h.okx.state().rest_limit_orders = true;
    let mut request = order();
    request.order_type = EntryOrderType::Limit;
    request.price = Some("74500".into());
    request.trailing_stop = Some(ratio("0.01"));
    let entry = h.engine.place_order(request).await.unwrap().order;
    (h, entry)
}

#[tokio::test]
async fn deferred_trailing_keeps_waiting_when_its_first_position_update_is_unexplained() {
    let (h, entry) = deferred_limit().await;
    {
        let mut okx = h.okx.state();
        okx.fill_resting();
        let position = okx.position.as_mut().unwrap();
        position.insert("margin".into(), json!("15"));
        position.insert("uTime".into(), json!((START + 2).to_string()));
    }
    h.sync().await;
    h.advance(60_001);
    restart(&h).sync().await.unwrap();

    assert!(h.algo_placed().is_empty());
    let trail = h.order_row(&format!("{}:trail", entry.id));
    assert_eq!(trail.status, OrderStatus::Open);
    assert!(trail.reject_reason.unwrap().contains("position adjustment"));
}

#[tokio::test]
async fn deferred_trailing_can_use_the_same_proven_position_after_a_margin_adjustment() {
    let (h, entry) = deferred_limit().await;
    h.engine.place_order(order()).await.unwrap();
    h.okx.state().fill_resting();
    h.quote_time.store(START - 120_000, Ordering::SeqCst);
    h.sync().await;
    assert!(h.algo_placed().is_empty());

    {
        let mut okx = h.okx.state();
        let position = okx.position.as_mut().unwrap();
        position.insert("margin".into(), json!("15"));
        position.insert("uTime".into(), json!((START + 10_001).to_string()));
    }
    h.advance(10_001);
    h.quote_time.store(0, Ordering::SeqCst);
    h.sync().await;

    let trailing: Vec<_> = h
        .algo_placed()
        .into_iter()
        .filter(|algo| algo["ordType"] == "move_order_stop")
        .collect();
    assert_eq!(trailing.len(), 1);
    assert_eq!(trailing[0]["sz"], "0.1");
    assert!(h
        .order_row(&format!("{}:trail", entry.id))
        .exchange_order_id
        .is_some());
}

#[tokio::test]
async fn deferred_trailing_is_canceled_when_parent_never_fills() {
    let (h, entry) = deferred_limit().await;
    let key = mochi_okx::to_cl_ord_id(&entry.client_order_id);
    h.okx
        .state()
        .orders
        .get_mut(&key)
        .unwrap()
        .insert("state".into(), json!("canceled"));
    h.sync().await;
    assert!(h.algo_placed().is_empty());
    assert_eq!(
        h.order_row(&format!("{}:trail", entry.id)).status,
        OrderStatus::Canceled
    );
}

#[tokio::test]
async fn deferred_trailing_reports_partial_cancel_without_a_confirmed_fixed_stop() {
    let (h, entry) = deferred_limit().await;
    let key = mochi_okx::to_cl_ord_id(&entry.client_order_id);
    {
        let mut state = h.okx.state();
        state.fill_resting();
        let order = state.orders.get_mut(&key).unwrap();
        order.insert("state".into(), json!("canceled"));
        order.insert("accFillSz".into(), json!("0.05"));
        state.algos.clear();
    }
    let id = format!("{}:trail", entry.id);
    h.sync().await;
    assert_eq!(h.order_row(&id).status, OrderStatus::Open);
    h.advance(10_000);
    h.sync().await;
    assert!(h.algo_placed().is_empty());
    let row = h.order_row(&id);
    assert_eq!(row.status, OrderStatus::Canceled);
    assert!(row
        .reject_reason
        .unwrap()
        .contains("did not confirm the fixed stop"));
}

#[tokio::test]
async fn deferred_trailing_never_attaches_to_a_flat_or_reopened_or_ambiguous_position() {
    for scenario in ["flat", "reopen", "missing-trade-id", "cross", "flip"] {
        let (h, entry) = deferred_limit().await;
        h.okx.state().fill_resting();
        {
            let mut state = h.okx.state();
            if scenario == "flat" {
                state.position = None;
            } else {
                let p = state.position.as_mut().unwrap();
                match scenario {
                    "reopen" => {
                        p.insert("uTime".into(), json!("1800000000100"));
                        p.insert("tradeId".into(), json!("999"));
                    }
                    "missing-trade-id" => {
                        p.remove("tradeId");
                    }
                    "cross" => {
                        p.insert("mgnMode".into(), json!("cross"));
                    }
                    "flip" => {
                        p.insert("pos".into(), json!("-0.1"));
                    }
                    _ => unreachable!(),
                }
            }
        }
        h.sync().await;
        if scenario != "flip" {
            assert_eq!(
                h.order_row(&format!("{}:trail", entry.id)).status,
                OrderStatus::Open
            );
            h.advance(10_000);
            h.sync().await;
        }
        assert!(h.algo_placed().is_empty(), "{scenario}");
        let trail = h.order_row(&format!("{}:trail", entry.id));
        assert_eq!(trail.status, OrderStatus::Canceled, "{scenario}");
        assert!(trail
            .reject_reason
            .unwrap()
            .starts_with("Deferred protection canceled:"));
    }
}

#[tokio::test]
async fn deferred_trailing_refusal_leaves_fixed_stop_in_force_and_is_never_retried() {
    let h = connect(Vec::new()).await;
    h.okx.state().reject_next_algo = Some(Refusal::new("51000", "trailing rejected"));
    let mut request = order();
    request.trailing_stop = Some(ratio("0.01"));
    let entry = h.engine.place_order(request).await.unwrap().order;
    h.sync().await;
    assert_eq!(entry.status, OrderStatus::Filled);
    assert_eq!(h.algo_placed().len(), 1);
    assert_eq!(
        h.order_row(&format!("{}:trail", entry.id)).status,
        OrderStatus::Rejected
    );
    assert_eq!(
        h.engine.protection_of("BTCUSDT").stop_loss.as_deref(),
        Some("74000")
    );
    assert!(h.canceled().is_empty());
}

#[tokio::test]
async fn deferred_trailing_survives_disarming_before_fill() {
    let (h, _) = deferred_limit().await;
    h.disarm();
    h.okx.state().fill_resting();
    h.sync().await;
    assert_eq!(h.algo_placed().len(), 1);
    assert_eq!(
        h.engine.protection_of("BTCUSDT").stop_loss.as_deref(),
        Some("74000")
    );
}

fn restart(h: &Harness) -> Arc<OkxEngine> {
    let clock = h.clock.clone();
    let client = OkxClient::new(fake::credentials(), Venue::OkxLive, h.sync_down.clone())
        .with_base_url("https://okx.invalid")
        .with_clock(move || clock.load(Ordering::SeqCst));
    let clock = h.clock.clone();
    let prices = h.prices.clone();
    let now = h.clock.clone();
    let armed = h.armed.clone();
    OkxEngine::new(OkxEngineDeps {
        account_id: ACCOUNT.into(),
        venue: Venue::OkxLive,
        client,
        store: h.store.clone(),
        get_quote: Arc::new(move |symbol| {
            let (bid, ask, last) = prices.lock().unwrap().clone();
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
                ts: clock.load(Ordering::SeqCst),
            })
        }),
        now: Arc::new(move || now.load(Ordering::SeqCst)),
        is_armed: Arc::new(move || armed.load(Ordering::SeqCst)),
        new_id: Arc::new(|prefix| {
            format!(
                "{prefix}_restart_{}",
                NEXT_KEY.fetch_add(1, Ordering::SeqCst)
            )
        }),
    })
}

#[tokio::test]
async fn restart_resumes_unsent_trailing_intent_after_terminal_partial_fill() {
    let (h, entry) = deferred_limit().await;
    h.okx.state().cancel_resting_after_partial_fill("0.05");
    let engine = restart(&h);
    engine.sync().await.unwrap();
    engine.sync().await.unwrap();
    assert_eq!(h.algo_placed().len(), 1);
    assert_eq!(h.algo_placed()[0]["sz"], "0.05");
    assert_eq!(
        h.order_row(&format!("{}:trail", entry.id)).quantity,
        "0.0005"
    );
}

#[tokio::test]
async fn restart_never_resends_a_trailing_submission_marked_started_before_send() {
    let (h, entry) = deferred_limit().await;
    h.okx.state().fill_resting();
    // Simulate the durable boundary immediately before a process crash: it is impossible
    // to know whether the request left the process, so only lookup is permissible.
    h.store(|s| {
        s.connection()
            .execute(
                "UPDATE orders SET reject_reason=? WHERE id=?",
                (
                    "Awaiting confirmation: deferred trailing submission started",
                    format!("{}:trail", entry.id),
                ),
            )
            .unwrap()
    });
    let engine = restart(&h);
    engine.sync().await.unwrap();
    h.advance(61_000);
    engine.sync().await.unwrap();
    assert!(h.algo_placed().is_empty());
    assert_eq!(
        h.order_row(&format!("{}:trail", entry.id)).status,
        OrderStatus::Canceled
    );
    assert_eq!(
        engine.protection_of("BTCUSDT").stop_loss.as_deref(),
        Some("74000")
    );
}

#[tokio::test]
async fn restart_looks_up_accepted_trailing_without_resending() {
    let h = connect(Vec::new()).await;
    let mut request = order();
    request.trailing_stop = Some(ratio("0.01"));
    let entry = h.engine.place_order(request).await.unwrap().order;
    h.store(|s| {
        s.connection()
            .execute(
                "UPDATE orders SET exchange_order_id=NULL, reject_reason=? WHERE id=?",
                (
                    "Awaiting confirmation: deferred trailing submission started",
                    format!("{}:trail", entry.id),
                ),
            )
            .unwrap()
    });
    let engine = restart(&h);
    engine.sync().await.unwrap();
    assert_eq!(h.algo_placed().len(), 1);
    assert_eq!(
        engine.protection_of("BTCUSDT").trailing_stop,
        Some(ratio("0.01"))
    );
}

#[tokio::test]
async fn deferred_trailing_rechecks_position_after_looking_up_the_parent() {
    let (h, entry) = deferred_limit().await;
    h.okx.state().fill_resting();
    h.store(|s| {
        s.connection()
            .execute(
                "UPDATE orders SET status='FILLED', filled_quantity=quantity WHERE id=?",
                [&entry.id],
            )
            .unwrap()
    });
    let hold = h.okx.hold_next("GET", "/trade/order");
    let engine = h.engine.clone();
    let sync = tokio::spawn(async move { engine.sync().await });
    tokio::time::timeout(std::time::Duration::from_secs(2), hold.reached())
        .await
        .unwrap();
    h.okx
        .state()
        .position
        .as_mut()
        .unwrap()
        .insert("tradeId".into(), json!("999"));
    hold.release();
    sync.await.unwrap().unwrap();
    assert_eq!(
        h.order_row(&format!("{}:trail", entry.id)).status,
        OrderStatus::Open
    );
    h.advance(10_000);
    h.sync().await;
    assert_eq!(
        h.order_row(&format!("{}:trail", entry.id)).status,
        OrderStatus::Canceled
    );
    assert!(h.algo_placed().is_empty());
}

#[tokio::test]
async fn protection_edits_do_not_cancel_trailing_for_a_resting_entry() {
    let (h, entry) = deferred_limit().await;
    // A position opens outside MochiPrep while the entry is resting.
    let other = Harness::with_position().await;
    h.okx.state().position = other.okx.state().position.clone();
    h.sync().await;
    let id = format!("{}:trail", entry.id);

    h.engine
        .set_protection_with(
            ACCOUNT,
            "BTCUSDT",
            Protection {
                stop_loss: None,
                take_profit: None,
                trailing_stop: None,
            },
            mochi_okx::engine::ProtectionRetention {
                trailing_stop: true,
                ..Default::default()
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(h.order_row(&id).status, OrderStatus::Open);
    h.set(Protection {
        stop_loss: None,
        take_profit: None,
        trailing_stop: None,
    })
    .await
    .unwrap();
    assert_eq!(h.order_row(&id).status, OrderStatus::Open);
    h.okx.state().fill_resting();
    h.sync().await;
    assert_eq!(h.algo_placed().len(), 1);
}

#[tokio::test]
async fn split_tp_is_not_used_and_a_canceled_partial_fill_keeps_its_stop() {
    let h = connect(Vec::new()).await;
    h.okx.state().rest_limit_orders = true;
    let mut request = order();
    request.order_type = EntryOrderType::Limit;
    request.price = Some("74500".into());
    request.take_profit = Some("78000".into());
    request.trailing_stop = Some(ratio("0.01"));
    h.engine.place_order(request).await.unwrap();
    h.okx.state().cancel_resting_after_partial_fill("0.05");
    h.sync().await;
    let stops = live_stops(&h);
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].0, "0.05");
    assert_eq!(h.algo_placed().len(), 2);
    assert!(h.algo_placed().iter().all(|algo| algo["sz"] == "0.05"));
}

#[tokio::test]
async fn missing_protection_snapshot_does_not_hide_a_position_from_reduce_only_close() {
    let (h, _) = deferred_limit().await;
    h.okx.state().fill_resting();
    h.okx.state().algo_lookup_down = true;
    assert!(h.engine.sync().await.is_err());
    h.disarm();
    assert_eq!(
        h.engine
            .close_position(ACCOUNT, "BTCUSDT", 1.0, "close-after-protection-timeout")
            .await
            .unwrap()
            .order
            .status,
        OrderStatus::Filled
    );
}

#[tokio::test]
async fn a_resting_entrys_deferred_protection_does_not_block_a_second_entry() {
    let (h, _) = deferred_limit().await;
    let mut second = order();
    second.client_order_id = "another-entry".into();
    second.order_type = EntryOrderType::Limit;
    second.price = Some("74500".into());
    assert_eq!(
        h.engine.place_order(second).await.unwrap().order.status,
        OrderStatus::Open
    );
    assert_eq!(h.okx.state().placed.len(), 2);
}

#[tokio::test]
async fn deferred_trailing_uses_isolated_position_when_cross_appears_first() {
    let (h, _) = deferred_limit().await;
    h.okx.state().fill_resting();
    {
        let mut state = h.okx.state();
        let mut cross = state.position.clone().unwrap();
        cross.insert("mgnMode".into(), json!("cross"));
        state.other_positions.push(cross);
    }
    h.sync().await;
    assert_eq!(h.algo_placed().len(), 1);
}

#[tokio::test]
async fn invalid_take_profit_refuses_entry_before_sending_it_to_okx() {
    let h = connect(Vec::new()).await;
    for (side, stop, target) in [
        (OrderSide::Buy, "74000", "74500"),
        (OrderSide::Sell, "76000", "75500"),
    ] {
        let mut request = order();
        request.side = side;
        request.stop_loss = Some(stop.into());
        request.take_profit = Some(target.into());
        let result = h.engine.place_order(request).await.unwrap();
        assert_eq!(result.order.status, OrderStatus::Rejected);
        assert!(result
            .order
            .reject_reason
            .unwrap()
            .starts_with("LIMIT_TARGET_INVALID"));
    }
    assert!(h.okx.state().placed.is_empty());
}

#[tokio::test]
async fn each_resting_entry_keeps_its_take_profit_when_both_fill_before_sync() {
    let h = connect(Vec::new()).await;
    // An older close shares the same millisecond with both new entries.
    h.okx.state().position = Some(long_position());
    h.sync().await;
    assert_eq!(h.close().await.order.status, OrderStatus::Filled);
    h.okx.state().rest_limit_orders = true;
    for target in ["78000", "79000"] {
        let mut request = order();
        request.order_type = EntryOrderType::Limit;
        request.price = Some("74500".into());
        request.take_profit = Some(target.into());
        let entry = h.engine.place_order(request).await.unwrap().order;
        assert_eq!(entry.status, OrderStatus::Open, "{:?}", entry.reject_reason);
    }
    h.okx.state().fill_resting();
    let latest_fill = h.okx.state().recent_fills.pop().unwrap();
    h.sync().await;
    assert!(
        h.algo_placed()
            .iter()
            .all(|row| row.get("tpTriggerPx").is_none()),
        "both targets wait for the latest position trade to appear in fills"
    );
    h.okx.state().recent_fills.push(latest_fill);
    h.sync().await;
    h.sync().await;
    let mut targets: Vec<_> = h
        .algo_placed()
        .into_iter()
        .filter_map(|row| Some((row.get("tpTriggerPx")?.clone(), row["sz"].clone())))
        .collect();
    targets.sort_by(|a, b| a.0.as_str().cmp(&b.0.as_str()));
    assert_eq!(
        targets,
        [
            (json!("78000"), json!("0.1")),
            (json!("79000"), json!("0.1"))
        ]
    );
    let live_targets = || {
        h.okx
            .state()
            .algos
            .values()
            .filter(|algo| algo.get("tpTriggerPx").is_some() && algo["state"] == "live")
            .cloned()
            .collect::<Vec<_>>()
    };
    let before = live_targets();
    assert_eq!(before.len(), 2);
    h.store
        .inner
        .lock()
        .unwrap()
        .1
        .iter_mut()
        .find(|spec| spec.symbol == "BTCUSDT")
        .unwrap()
        .price_tick = "75".into();
    h.engine
        .set_protection_with(
            ACCOUNT,
            "BTCUSDT",
            stop("74025"),
            mochi_okx::engine::ProtectionRetention {
                take_profit: true,
                trailing_stop: true,
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(live_targets(), before);

    // Clearing the target explicitly removes both per-entry take profits.
    assert!(h
        .set(stop("74025"))
        .await
        .unwrap()
        .after
        .take_profit
        .is_none());
    assert!(h
        .okx
        .state()
        .algos
        .values()
        .all(|algo| { algo.get("tpTriggerPx").is_none() || algo["state"] != "live" }));
}

#[tokio::test]
async fn deferred_protection_does_not_cross_a_close_and_reopen_proven_by_fills() {
    let (h, entry) = deferred_limit().await;
    h.okx.state().fill_resting();
    {
        let mut okx = h.okx.state();
        let position = okx.position.as_mut().unwrap();
        position.insert("tradeId".into(), json!("3"));
        position.insert("uTime".into(), json!("1800000000001"));
        for (id, subtype) in [("2", "5"), ("3", "3")] {
            okx.recent_fills.push(
                json!({
                    "instId": "BTC-USDT-SWAP", "tradeId": id, "fillTime": "1800000000001",
                    "posSide": "net", "subType": subtype,
                })
                .as_object()
                .unwrap()
                .clone(),
            );
        }
    }
    h.sync().await;
    assert_eq!(
        h.order_row(&format!("{}:trail", entry.id)).status,
        OrderStatus::Canceled
    );
    assert!(h.algo_placed().is_empty());
}

async fn resting_target() -> (Harness, Order) {
    let h = connect(Vec::new()).await;
    h.okx.state().rest_limit_orders = true;
    let mut request = order();
    request.order_type = EntryOrderType::Limit;
    request.price = Some("74500".into());
    request.take_profit = Some("78000".into());
    let entry = h.engine.place_order(request).await.unwrap().order;
    assert_eq!(entry.status, OrderStatus::Open);
    (h, entry)
}

#[tokio::test]
async fn removing_a_deferred_target_cannot_execute_it_during_an_unknown_stop_replacement() {
    let (h, entry) = resting_target().await;
    h.okx.state().fill_resting();
    *h.prices.lock().unwrap() = ("78099".into(), "78101".into(), "78100".into());
    // Equal fill and quote timestamps leave the reached target unsent.
    h.sync().await;
    let id = format!("{}:deferred:tp", entry.id);
    assert_eq!(h.order_row(&id).status, OrderStatus::Open);
    assert!(h.algo_placed().is_empty());

    h.advance(2);
    h.okx.state().lose_next_algo_response = true;
    let result = h.set(stop("74500")).await.unwrap();
    assert_eq!(result.after.stop_loss.as_deref(), Some("74500"));
    assert_eq!(h.okx.state().placed.len(), 1);
    assert!(h.okx.state().position.is_some());
    let target = h.order_row(&id);
    assert_eq!(target.order_type, OrderType::TakeProfitMarket);
    assert_eq!(target.status, OrderStatus::Canceled);
    assert_eq!(target.reject_reason.as_deref(), Some("Removed"));
}

#[tokio::test]
async fn raising_a_deferred_target_does_not_execute_the_previous_target_during_readback() {
    let (h, entry) = resting_target().await;
    h.okx.state().fill_resting();
    *h.prices.lock().unwrap() = ("78999".into(), "79001".into(), "79000".into());
    h.sync().await;
    let deferred_id = format!("{}:deferred:tp", entry.id);
    assert_eq!(h.order_row(&deferred_id).status, OrderStatus::Open);
    h.advance(2);
    h.okx.state().lose_next_algo_response = true;

    let result = h
        .set(Protection {
            take_profit: Some("81000".into()),
            ..stop("74500")
        })
        .await
        .unwrap();
    assert_eq!(result.after.take_profit.as_deref(), Some("81000"));
    assert_eq!(
        h.order_row(&deferred_id).reject_reason.as_deref(),
        Some("Replaced")
    );
    assert_eq!(h.okx.state().placed.len(), 1);
    assert_eq!(h.view().position.quantity, "0.001");
}

#[tokio::test]
async fn a_refused_edit_leaves_the_previous_target_for_the_next_full_sync() {
    let (h, entry) = resting_target().await;
    h.okx.state().fill_resting();
    *h.prices.lock().unwrap() = ("78999".into(), "79001".into(), "79000".into());
    h.sync().await;
    h.advance(2);
    h.okx.state().reject_next_algo = Some(Refusal::new("51000", "Stop refused"));
    let request = Protection {
        take_profit: Some("81000".into()),
        ..stop("74500")
    };
    assert!(h.set(request).await.is_err());
    assert_eq!(h.okx.state().placed.len(), 1);
    let deferred_id = format!("{}:deferred:tp", entry.id);
    assert_eq!(h.order_row(&deferred_id).status, OrderStatus::Open);

    h.sync().await;
    assert_eq!(h.order_row(&deferred_id).status, OrderStatus::Filled);
    assert!(h.okx.state().position.is_none());
}

#[tokio::test]
async fn an_accepted_target_that_no_longer_covers_the_position_cannot_retire_the_deferred_target() {
    let (h, entry) = resting_target().await;
    h.okx.state().fill_resting();
    *h.prices.lock().unwrap() = ("78999".into(), "79001".into(), "79000".into());
    h.sync().await;
    h.advance(2);
    let post = h.okx.hold_next("POST", "/trade/order-algo");
    let engine = h.engine.clone();
    let edit = tokio::spawn(async move {
        let protection = Protection {
            take_profit: Some("81000".into()),
            ..stop("74000")
        };
        engine
            .set_protection(ACCOUNT, "BTCUSDT", protection, None, None)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), post.reached())
        .await
        .unwrap();
    let mut replacement_position = h.okx.state().position.clone().unwrap();
    replacement_position.insert("pos".into(), json!("0.2"));
    replacement_position.insert("tradeId".into(), json!("2"));
    h.okx.state().position = Some(replacement_position);
    post.release();
    assert!(matches!(
        edit.await.unwrap(),
        Err(EngineError::ProtectionChange { .. })
    ));

    h.sync().await;
    let deferred_id = format!("{}:deferred:tp", entry.id);
    assert_eq!(h.order_row(&deferred_id).status, OrderStatus::Open);
    assert_eq!(h.okx.state().placed.len(), 1);
    assert_eq!(h.view().position.quantity, "0.002");

    // Even when readback starts with enough coverage, a fill during the
    // algo lookup can make the accepted replacement too small.
    h.okx
        .state()
        .position
        .as_mut()
        .unwrap()
        .insert("pos".into(), json!("0.1"));
    let coverage = h.okx.hold_next("GET", "/trade/order-algo");
    let engine = h.engine.clone();
    let sync = tokio::spawn(async move { engine.sync().await });
    tokio::time::timeout(std::time::Duration::from_secs(2), coverage.reached())
        .await
        .unwrap();
    h.okx
        .state()
        .position
        .as_mut()
        .unwrap()
        .insert("pos".into(), json!("0.2"));
    coverage.release();
    sync.await.unwrap().unwrap();
    assert_eq!(h.order_row(&deferred_id).status, OrderStatus::Open);

    let target = Protection {
        take_profit: Some("81000".into()),
        ..stop("74000")
    };
    h.set(target.clone()).await.unwrap();
    let algo = h.algo_placed().last().unwrap().clone();
    assert_eq!(algo["tpTriggerPx"], "81000");
    assert_eq!(algo["sz"], "0.2");
    assert_eq!(h.order_row(&deferred_id).status, OrderStatus::Canceled);
    let placements = h.algo_placed().len();
    h.set(target.clone()).await.unwrap();
    assert_eq!(h.algo_placed().len(), placements);
    h.okx.state().algo_lookup_down = true;
    assert!(h.set(target.clone()).await.is_err());
    assert_eq!(h.algo_placed().len(), placements);
    h.okx.state().algo_lookup_down = false;

    let read = h.okx.hold_next("GET", "/trade/order-algo");
    let engine = h.engine.clone();
    let edit = tokio::spawn(async move {
        engine
            .set_protection(ACCOUNT, "BTCUSDT", target, None, None)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), read.reached())
        .await
        .unwrap();
    {
        let mut okx = h.okx.state();
        let position = okx.position.as_mut().unwrap();
        position.insert("pos".into(), json!("0.3"));
        position.insert("tradeId".into(), json!("3"));
    }
    read.release();
    let error = edit.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("position changed"), "{error}");
    assert_eq!(h.algo_placed().len(), placements);
}

#[tokio::test]
async fn the_next_protection_leg_is_not_sent_if_readback_observes_a_replacement_position() {
    let h = Harness::with_position().await;
    h.okx.state().lose_next_algo_response = true;
    let first = h.okx.hold_next("POST", "/trade/order-algo");
    let engine = h.engine.clone();
    let edit = tokio::spawn(async move {
        let protection = Protection {
            take_profit: Some("81000".into()),
            ..stop("74500")
        };
        engine
            .set_protection(ACCOUNT, "BTCUSDT", protection, None, None)
            .await
    });
    let timeout = std::time::Duration::from_secs(2);
    tokio::time::timeout(timeout, first.reached())
        .await
        .unwrap();
    let readback = h.okx.hold_next("GET", "/account/positions");
    first.release();
    tokio::time::timeout(timeout, readback.reached())
        .await
        .unwrap();
    h.okx
        .state()
        .position
        .as_mut()
        .unwrap()
        .insert("tradeId".into(), json!("9001"));
    readback.release();

    let error = edit.await.unwrap().unwrap_err();
    assert!(
        matches!(error, EngineError::ProtectionChange { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("position changed"), "{error}");
    let placements = h.algo_placed();
    assert_eq!(placements.len(), 1, "{placements:?}");
    assert_eq!(placements[0]["slTriggerPx"], "74500");
}

async fn partial_target() -> (Harness, Order, Row, Row) {
    let (h, entry) = resting_target().await;
    partial_entry(h, entry).await
}

async fn partial_entry(h: Harness, entry: Order) -> (Harness, Order, Row, Row) {
    let (filled_order, full_position) = {
        let mut okx = h.okx.state();
        okx.fill_resting();
        let order = okx
            .orders
            .get_mut(&mochi_okx::to_cl_ord_id(&entry.client_order_id))
            .unwrap();
        let filled = order.clone();
        order.insert("state".into(), json!("partially_filled"));
        order.insert("accFillSz".into(), json!("0.05"));
        let position = okx.position.as_mut().unwrap();
        let full = position.clone();
        position.insert("pos".into(), json!("0.05"));
        (filled, full)
    };
    h.sync().await;
    assert_eq!(h.order_row(&entry.id).status, OrderStatus::Open);
    assert_eq!(h.view().position.quantity, "0.0005");
    (h, entry, filled_order, full_position)
}

#[tokio::test]
async fn removing_a_partial_entrys_trailing_stop_persists_through_its_final_fill() {
    let (h, entry) = deferred_limit().await;
    let (h, entry, filled_order, full_position) = partial_entry(h, entry).await;
    let id = format!("{}:trail", entry.id);
    assert_eq!(h.order_row(&id).status, OrderStatus::Open);
    h.set(stop("74000")).await.unwrap();
    h.advance(2);
    {
        let mut okx = h.okx.state();
        okx.orders.insert(
            mochi_okx::to_cl_ord_id(&entry.client_order_id),
            filled_order,
        );
        okx.position = Some(full_position);
    }
    let restarted = restart(&h);
    restarted.sync().await.unwrap();
    assert_eq!(h.order_row(&entry.id).status, OrderStatus::Filled);
    assert_eq!(h.order_row(&id).status, OrderStatus::Canceled);
    assert_eq!(h.order_row(&id).reject_reason.as_deref(), Some("Removed"));
    assert!(h
        .algo_placed()
        .iter()
        .all(|order| order["ordType"] != "move_order_stop"));
}

#[tokio::test]
async fn a_partially_filled_canceled_entry_can_use_a_replacement_fixed_stop() {
    for (reported_size, expects_trail) in [("0.05", true), ("0.01", false)] {
        let (h, entry) = deferred_limit().await;
        let (h, entry, _, _) = partial_entry(h, entry).await;
        h.engine
            .set_protection_with(
                ACCOUNT,
                "BTCUSDT",
                stop("74300"),
                mochi_okx::engine::ProtectionRetention {
                    trailing_stop: true,
                    take_profit: true,
                },
                None,
                None,
            )
            .await
            .unwrap();
        let key = mochi_okx::to_cl_ord_id(&entry.client_order_id);
        {
            let mut okx = h.okx.state();
            let order = &mut okx.orders[&key];
            order.insert("state".into(), json!("canceled"));
            order.insert("fillSz".into(), json!("0.05"));
            let ord_id = order["ordId"].clone();
            okx.recent_fills
                .iter_mut()
                .find(|fill| fill["ordId"] == ord_id)
                .unwrap()
                .insert("fillSz".into(), json!("0.05"));
            okx.algos[&to_algo_cl_ord_id(&format!("{}:sl", entry.id))]
                .insert("state".into(), json!("canceled"));
            okx.algos
                .values_mut()
                .find(|algo| algo.get("slTriggerPx") == Some(&json!("74300")))
                .unwrap()
                .insert("sz".into(), json!(reported_size));
        }
        h.sync().await;
        h.advance(10_001);
        h.sync().await;
        let trail = h.order_row(&format!("{}:trail", entry.id));
        assert_eq!(trail.exchange_order_id.is_some(), expects_trail);
        assert_eq!(
            trail.status,
            if expects_trail {
                OrderStatus::Open
            } else {
                OrderStatus::Canceled
            },
        );
    }
}

#[tokio::test]
async fn a_pending_fixed_stop_change_remembers_to_remove_the_deferred_trailing_stop() {
    let (h, entry) = deferred_trailing_with_unconfirmed_fixed_stop().await;
    let id = format!("{}:trail", entry.id);
    let restarted = restart(&h);
    restarted.sync().await.unwrap();
    assert_eq!(restarted.protection_of("BTCUSDT"), stop("74500"));
    assert_eq!(h.order_row(&id).status, OrderStatus::Canceled);
    assert_eq!(h.order_row(&id).reject_reason.as_deref(), Some("Removed"));
    assert_eq!(h.algo_placed().len(), 1);
}

#[tokio::test]
async fn an_unconfirmed_fixed_stop_that_turns_out_undersized_keeps_the_deferred_trailing_stop() {
    let (h, entry) = deferred_trailing_with_unconfirmed_fixed_stop().await;
    h.okx
        .state()
        .algos
        .values_mut()
        .find(|algo| algo.get("slTriggerPx") == Some(&json!("74500")))
        .unwrap()
        .insert("sz".into(), json!("0.05"));
    let restarted = restart(&h);
    restarted.sync().await.unwrap();
    assert_eq!(
        h.order_row(&format!("{}:trail", entry.id)).status,
        OrderStatus::Open
    );
    assert!(restarted.protection_of("BTCUSDT").trailing_stop.is_some());
    assert_eq!(
        h.algo_placed().last().unwrap()["ordType"],
        "move_order_stop"
    );
}

async fn filled_entry_with_deferred_trailing_stop() -> (Harness, Order) {
    let (h, entry) = deferred_limit().await;
    h.okx.state().fill_resting();
    h.quote_time.store(START - 120_000, Ordering::SeqCst);
    h.sync().await;
    let id = format!("{}:trail", entry.id);
    assert_eq!(h.order_row(&id).status, OrderStatus::Open);
    assert!(h.algo_placed().is_empty());
    h.quote_time.store(0, Ordering::SeqCst);
    (h, entry)
}

async fn deferred_trailing_with_unconfirmed_fixed_stop() -> (Harness, Order) {
    let (h, entry) = filled_entry_with_deferred_trailing_stop().await;
    let id = format!("{}:trail", entry.id);
    {
        let mut okx = h.okx.state();
        okx.lose_next_algo_response = true;
        okx.algo_lookup_down = true;
    }
    let result = h.set(stop("74500")).await.unwrap();
    assert!(result.pending);
    assert_eq!(h.order_row(&id).status, OrderStatus::Open);
    assert_eq!(h.algo_placed().len(), 1);

    h.okx.state().algo_lookup_down = false;
    h.advance(2);
    (h, entry)
}

#[tokio::test]
async fn a_refused_take_profit_preserves_the_deferred_trailing_stop_in_a_combined_edit() {
    let (h, entry) = filled_entry_with_deferred_trailing_stop().await;
    h.okx.state().reject_algo_if = Some(Box::new(|body| body.get("tpTriggerPx").is_some()));
    let request = Protection {
        take_profit: Some("78000".into()),
        ..stop("74500")
    };
    assert!(h.set(request).await.is_err());
    h.advance(2);
    let restarted = restart(&h);
    restarted.sync().await.unwrap();
    assert_eq!(
        h.order_row(&format!("{}:trail", entry.id)).status,
        OrderStatus::Open
    );
    assert!(restarted.protection_of("BTCUSDT").trailing_stop.is_some());
}

#[tokio::test]
async fn a_combined_edit_remembers_trailing_removal_when_its_last_answer_is_lost() {
    let (h, entry) = filled_entry_with_deferred_trailing_stop().await;
    let fixed = h.okx.hold_next("POST", "/trade/order-algo");
    let target = h.okx.hold_next("POST", "/trade/order-algo");
    let engine = h.engine.clone();
    let edit = tokio::spawn(async move {
        let protection = Protection {
            take_profit: Some("78000".into()),
            ..stop("74500")
        };
        engine
            .set_protection(ACCOUNT, "BTCUSDT", protection, None, None)
            .await
    });
    let timeout = std::time::Duration::from_secs(2);
    tokio::time::timeout(timeout, fixed.reached())
        .await
        .unwrap();
    fixed.release();
    tokio::time::timeout(timeout, target.reached())
        .await
        .unwrap();
    {
        let mut okx = h.okx.state();
        okx.lose_next_algo_response = true;
        okx.algo_lookup_down = true;
    }
    target.release();
    assert!(edit.await.unwrap().unwrap().pending);
    assert_eq!(h.algo_placed().len(), 2);
    h.okx.state().algo_lookup_down = false;
    h.advance(2);
    let restarted = restart(&h);
    restarted.sync().await.unwrap();
    let deferred = h.order_row(&format!("{}:trail", entry.id));
    assert_eq!(deferred.status, OrderStatus::Canceled);
    assert_eq!(deferred.reject_reason.as_deref(), Some("Removed"));
    assert_eq!(h.algo_placed().len(), 2, "must not send the trailing stop");
}

async fn pending_take_profit_with_retained_fixed_stop() -> (Harness, Order, Protection) {
    let (h, entry) = filled_entry_with_deferred_trailing_stop().await;
    assert_eq!(h.engine.protection_of("BTCUSDT"), stop("74000"));
    let request = Protection {
        take_profit: Some("78000".into()),
        ..stop("74000")
    };
    let target = h.okx.hold_next("POST", "/trade/order-algo");
    let engine = h.engine.clone();
    let submitted = request.clone();
    let edit = tokio::spawn(async move {
        engine
            .set_protection(ACCOUNT, "BTCUSDT", submitted, None, None)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), target.reached())
        .await
        .unwrap();
    {
        let mut okx = h.okx.state();
        okx.lose_next_algo_response = true;
        okx.algo_lookup_down = true;
    }
    target.release();
    assert!(edit.await.unwrap().unwrap().pending);
    assert_eq!(h.algo_placed().len(), 1);

    h.okx.state().algo_lookup_down = false;
    h.advance(2);
    (h, entry, request)
}

#[tokio::test]
async fn a_pending_take_profit_remembers_trailing_removal_with_a_retained_fixed_stop() {
    let (h, entry, request) = pending_take_profit_with_retained_fixed_stop().await;
    let restarted = restart(&h);
    restarted.sync().await.unwrap();
    assert_eq!(restarted.protection_of("BTCUSDT"), request);
    let deferred = h.order_row(&format!("{}:trail", entry.id));
    assert_eq!(deferred.status, OrderStatus::Canceled);
    assert_eq!(deferred.reject_reason.as_deref(), Some("Removed"));
    assert_eq!(h.algo_placed().len(), 1, "must not send the trailing stop");
}

#[tokio::test]
async fn pending_take_profit_recovery_keeps_the_trail_if_the_retained_stop_shrinks() {
    let (h, entry, _) = pending_take_profit_with_retained_fixed_stop().await;
    let restarted = restart(&h);
    let initial = h.okx.hold_next("GET", "/account/positions");
    let recheck = h.okx.hold_next("GET", "/account/positions");
    let engine = restarted.clone();
    let sync = tokio::spawn(async move { engine.sync().await });
    let timeout = std::time::Duration::from_secs(2);
    tokio::time::timeout(timeout, initial.reached())
        .await
        .unwrap();
    initial.release();
    tokio::time::timeout(timeout, recheck.reached())
        .await
        .unwrap();
    h.okx
        .state()
        .algos
        .values_mut()
        .find(|algo| algo.get("slTriggerPx") == Some(&json!("74000")))
        .unwrap()
        .insert("sz".into(), json!("0.05"));
    recheck.release();
    sync.await.unwrap().unwrap();
    let deferred = h.order_row(&format!("{}:trail", entry.id));
    assert_eq!(deferred.status, OrderStatus::Open);
    assert!(restarted.protection_of("BTCUSDT").trailing_stop.is_some());
    assert_eq!(
        h.algo_placed().last().unwrap()["ordType"],
        "move_order_stop"
    );
}

#[tokio::test]
async fn raising_a_partially_filled_entry_target_survives_completion_of_its_parent() {
    let (h, entry, filled_order, full_position) = partial_target().await;
    h.advance(2);
    let target = Protection {
        take_profit: Some("81000".into()),
        ..stop("74000")
    };
    h.set(target).await.unwrap();
    let id = format!("{}:deferred:tp", entry.id);
    assert_eq!(h.order_row(&id).status, OrderStatus::Open);

    h.advance(2);
    *h.prices.lock().unwrap() = ("78999".into(), "79001".into(), "79000".into());
    {
        let mut fake = h.okx.state();
        fake.orders.insert(
            mochi_okx::to_cl_ord_id(&entry.client_order_id),
            filled_order,
        );
        fake.position = Some(full_position);
    }
    let restarted = restart(&h);
    restarted.sync().await.unwrap();
    assert_eq!(h.order_row(&entry.id).status, OrderStatus::Filled);
    assert_eq!(h.okx.state().placed.len(), 1);
    assert_eq!(restarted.positions()[0].position.quantity, "0.001");
    assert_eq!(h.order_row(&id).status, OrderStatus::Open);
}

#[tokio::test]
async fn a_new_undersized_fixed_stop_cannot_replace_a_trailing_stop() {
    let h = Harness::with_position().await;
    h.set(Protection {
        trailing_stop: Some(ratio("0.01")),
        ..stop("74000")
    })
    .await
    .unwrap();
    let post = h.okx.hold_next("POST", "/trade/order-algo");
    let engine = h.engine.clone();
    let edit = tokio::spawn(async move {
        engine
            .set_protection(ACCOUNT, "BTCUSDT", stop("74500"), None, None)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), post.reached())
        .await
        .unwrap();
    {
        let mut okx = h.okx.state();
        let position = okx.position.as_mut().unwrap();
        position.insert("pos".into(), json!("0.2"));
        position.insert("tradeId".into(), json!("2"));
    }
    post.release();

    assert!(matches!(
        edit.await.unwrap(),
        Err(EngineError::ProtectionChange { .. })
    ));
    assert!(h.engine.protection_of("BTCUSDT").trailing_stop.is_some());
}

#[tokio::test]
async fn a_partial_entry_cannot_submit_a_removed_target_when_it_finishes_during_the_edit() {
    let (h, entry, filled_order, full_position) = partial_target().await;
    h.advance(2);
    *h.prices.lock().unwrap() = ("78099".into(), "78101".into(), "78100".into());
    let hold = h.okx.hold_next("POST", "/trade/order-algo");
    let engine = h.engine.clone();
    let edit = tokio::spawn(async move {
        engine
            .set_protection(ACCOUNT, "BTCUSDT", stop("74500"), None, None)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), hold.reached())
        .await
        .unwrap();
    {
        let mut okx = h.okx.state();
        okx.orders.insert(
            mochi_okx::to_cl_ord_id(&entry.client_order_id),
            filled_order,
        );
        okx.position = Some(full_position);
        okx.lose_next_algo_response = true;
    }
    hold.release();
    // The old target stays removed even though readback detects that the position
    // grew after the first algo was submitted.
    assert!(matches!(
        edit.await.unwrap(),
        Err(EngineError::ProtectionChange { .. })
    ));
    assert_eq!(h.order_row(&entry.id).status, OrderStatus::Filled);
    assert_eq!(h.okx.state().placed.len(), 1);
    assert_eq!(h.view().position.quantity, "0.001");
    let target = h.order_row(&format!("{}:deferred:tp", entry.id));
    assert_eq!(target.status, OrderStatus::Canceled);
    assert_eq!(target.reject_reason.as_deref(), Some("Removed"));
}

#[tokio::test]
async fn a_stop_edit_rechecks_position_identity_after_reading_an_entry() {
    let (h, entry, _, _) = partial_target().await;
    let hold = h.okx.hold_next("GET", "/trade/order");
    let engine = h.engine.clone();
    let edit = tokio::spawn(async move {
        engine
            .set_protection(ACCOUNT, "BTCUSDT", stop("74500"), None, None)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), hold.reached())
        .await
        .unwrap();
    h.okx
        .state()
        .position
        .as_mut()
        .unwrap()
        .insert("tradeId".into(), json!("42"));
    hold.release();
    let error = edit.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("position changed"), "{error}");
    assert!(h.algo_placed().is_empty());
    assert_eq!(
        h.order_row(&format!("{}:deferred:tp", entry.id)).status,
        OrderStatus::Open
    );
}

#[tokio::test]
async fn a_retained_stop_must_still_protect_the_whole_position_before_trailing_is_removed() {
    for (shrink_stop, raise_liquidation) in [(true, false), (false, true), (false, false)] {
        let h = Harness::with_position().await;
        h.set(Protection {
            trailing_stop: Some(ratio("0.01")),
            ..stop("74000")
        })
        .await
        .unwrap();
        let fixed = h
            .protection_rows()
            .into_iter()
            .find(|row| row.order_type == OrderType::StopMarket)
            .unwrap();
        {
            let mut okx = h.okx.state();
            if shrink_stop {
                okx.algos
                    .get_mut(&to_algo_cl_ord_id(&fixed.id))
                    .unwrap()
                    .insert("sz".into(), json!("0.05"));
            }
            if raise_liquidation {
                okx.position
                    .as_mut()
                    .unwrap()
                    .insert("liqPx".into(), json!("74200"));
            }
        }
        let placed = h.algo_placed().len();
        let result = h
            .set(Protection {
                take_profit: (shrink_stop || raise_liquidation).then(|| "78000".into()),
                ..stop("74000")
            })
            .await;
        if shrink_stop || raise_liquidation {
            let error = result.unwrap_err().to_string();
            let expected = if shrink_stop {
                "cover the entire"
            } else {
                "before liquidation"
            };
            assert!(error.contains(expected), "{error}");
            assert_eq!(h.algo_placed().len(), placed);
            assert!(h.canceled().is_empty());
            assert!(h.engine.protection_of("BTCUSDT").trailing_stop.is_some());
        } else {
            assert!(result.unwrap().after.trailing_stop.is_none());
            assert_eq!(h.canceled().len(), 1);
        }
    }
}

#[tokio::test]
async fn an_amended_limit_stop_cannot_take_the_place_of_a_trailing_stop() {
    let h = Harness::with_position().await;
    h.set(Protection {
        trailing_stop: Some(ratio("0.01")),
        ..stop("74000")
    })
    .await
    .unwrap();
    let fixed = h
        .protection_rows()
        .into_iter()
        .find(|row| row.order_type == OrderType::StopMarket)
        .unwrap();
    let fixed_id = to_algo_cl_ord_id(&fixed.id);
    h.okx
        .state()
        .algos
        .get_mut(&fixed_id)
        .unwrap()
        .insert("slOrdPx".into(), json!("73900"));
    h.sync().await;

    let error = h.set(stop("74000")).await.unwrap_err();
    assert!(
        error.to_string().contains("must be a market order"),
        "{error}"
    );
    assert!(h.canceled().is_empty());
    assert!(h.engine.protection_of("BTCUSDT").trailing_stop.is_some());
}

#[tokio::test]
async fn a_deferred_target_only_sizes_the_parent_fills_from_the_current_position() {
    let (h, entry) = resting_target().await;
    {
        let mut okx = h.okx.state();
        okx.fill_resting();
        let order = okx
            .orders
            .get_mut(&mochi_okx::to_cl_ord_id(&entry.client_order_id))
            .unwrap();
        order.insert("fillSz".into(), json!("0.05"));
        order.insert("tradeId".into(), json!("4"));
        let ord_id = order["ordId"].clone();
        okx.position
            .as_mut()
            .unwrap()
            .insert("tradeId".into(), json!("4"));
        okx.position
            .as_mut()
            .unwrap()
            .insert("pos".into(), json!("0.2"));
        okx.recent_fills.clear();
        for (id, order_id, kind, size) in [
            ("1", ord_id.clone(), "3", "0.05"),
            ("2", json!("close"), "5", "0.05"),
            ("3", json!("another-entry"), "3", "0.15"),
            ("4", ord_id, "3", "0.05"),
        ] {
            okx.recent_fills.push(
                json!({
                    "instId": "BTC-USDT-SWAP", "ordId": order_id, "tradeId": id,
                    "fillTime": "1800000000000", "ts": "1800000000001",
                    "posSide": "net", "subType": kind, "fillSz": size,
                })
                .as_object()
                .unwrap()
                .clone(),
            );
        }
    }
    h.sync().await;
    let targets = h.algo_placed();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0]["tpTriggerPx"], "78000");
    assert_eq!(targets[0]["sz"], "0.05");
    assert_eq!(
        h.order_row(&format!("{}:deferred:tp", entry.id)).quantity,
        "0.0005"
    );
}

#[tokio::test]
async fn a_deferred_target_does_not_use_fill_record_time_to_rule_out_late_adl() {
    let (h, entry) = resting_target().await;
    h.engine.place_order(order()).await.unwrap();
    {
        let mut okx = h.okx.state();
        okx.fill_resting();
        let position = okx.position.as_mut().unwrap();
        // ADL can leave the last ordinary tradeId unchanged even while reducing size.
        // The fills endpoint can lag behind the position read.
        // The combined 0.2 contracts shrank to 0.15, which still has room
        // for the parent's entire 0.1. Size alone cannot detect the reduction.
        position.insert("pos".into(), json!("0.15"));
        position.insert("uTime".into(), json!("1800000000002"));
        // The most recent ordinary trade executed before ADL. Its record was
        // generated later, which says nothing about the position's adjustment.
        okx.recent_fills
            .last_mut()
            .unwrap()
            .insert("ts".into(), json!("1800000000003"));
    }
    h.advance(1);
    *h.prices.lock().unwrap() = ("78999".into(), "79001".into(), "79000".into());
    let placed = h.count("POST /api/v5/trade/order");
    h.sync().await;
    assert_eq!(h.count("POST /api/v5/trade/order"), placed);
    assert_eq!(h.view().position.quantity, "0.0015");
    assert_eq!(
        h.order_row(&format!("{}:deferred:tp", entry.id)).status,
        OrderStatus::Open
    );
}

#[tokio::test]
async fn a_deferred_target_accounts_for_adl_after_the_last_ordinary_trade() {
    let (h, _) = resting_target().await;
    {
        let mut okx = h.okx.state();
        okx.fill_resting();
        let position = okx.position.as_mut().unwrap();
        let later_trade = position["tradeId"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            + 1;
        position.insert("tradeId".into(), json!(later_trade.to_string()));
        position.insert("pos".into(), json!("0.2"));
        position.insert("uTime".into(), json!("1800000000002"));
        // The position also held 0.1 older contracts. ADL lowers it from 0.3 to 0.2,
        // which still reconciles arithmetically with the two recent buys.
        okx.recent_fills.extend([
            row(json!({
                "instId": "BTC-USDT-SWAP", "ordId": "later-entry",
                "tradeId": later_trade.to_string(), "fillTime": "1800000000001",
                "ts": "1800000000002",
                "posSide": "net", "subType": "3", "fillSz": "0.1",
            })),
            row(json!({
                "instId": "BTC-USDT-SWAP", "ordId": "adl",
                "tradeId": "-1", "fillTime": "1800000000002",
                "posSide": "net", "subType": "125", "fillSz": "0.1",
            })),
        ]);
    }
    h.advance(1);
    *h.prices.lock().unwrap() = ("78999".into(), "79001".into(), "79000".into());
    let placed = h.count("POST /api/v5/trade/order");
    h.sync().await;
    assert_eq!(h.count("POST /api/v5/trade/order"), placed);
    assert_eq!(h.view().position.quantity, "0.002");
}

#[tokio::test]
async fn a_deferred_target_waits_for_late_adl_after_a_previously_proven_snapshot() {
    let (h, entry) = resting_target().await;
    h.engine.place_order(order()).await.unwrap();
    h.okx.state().fill_resting();
    h.quote_time.store(START - 120_000, Ordering::SeqCst);
    h.sync().await;

    let at = (START + 10_001).to_string();
    {
        let mut okx = h.okx.state();
        let position = okx.position.as_mut().unwrap();
        // The position keeps the same ordinary trade ID and could still hold
        // the parent's 0.1. It no longer matches the previously proven 0.2.
        position.insert("pos".into(), json!("0.15"));
        position.insert("uTime".into(), json!(at));
    }
    h.advance(10_001);
    h.quote_time.store(0, Ordering::SeqCst);
    *h.prices.lock().unwrap() = ("78999".into(), "79001".into(), "79000".into());
    let placed = h.count("POST /api/v5/trade/order");
    h.sync().await;
    assert_eq!(h.count("POST /api/v5/trade/order"), placed);
    let id = format!("{}:deferred:tp", entry.id);
    assert_eq!(h.order_row(&id).status, OrderStatus::Open);

    h.okx.state().recent_fills.push(row(json!({
        "instId": "BTC-USDT-SWAP", "ordId": "adl", "tradeId": "-1",
        "fillTime": at, "posSide": "net", "subType": "125", "fillSz": "0.05",
    })));
    h.sync().await;
    assert_eq!(h.count("POST /api/v5/trade/order"), placed);
    assert_eq!(h.order_row(&id).status, OrderStatus::Canceled);
}

#[tokio::test]
async fn protection_and_the_published_position_use_the_same_snapshot_after_a_flip() {
    let (h, _) = resting_target().await;
    let long = {
        let mut okx = h.okx.state();
        okx.fill_resting();
        let position = okx.position.as_mut().unwrap();
        let long = position.clone();
        position.insert("pos".into(), json!("-0.1"));
        long
    };
    let first = h.okx.hold_next("GET", "/account/positions");
    let second = h.okx.hold_next("GET", "/account/positions");
    let sync = tokio::spawn({
        let engine = h.engine.clone();
        async move { engine.sync().await }
    });
    let timeout = std::time::Duration::from_secs(2);
    tokio::time::timeout(timeout, first.reached())
        .await
        .unwrap();
    first.release();
    tokio::time::timeout(timeout, second.reached())
        .await
        .unwrap();
    h.okx.state().position = Some(long);
    second.release();
    sync.await.unwrap().unwrap();
    let view = h.view();
    assert_eq!(view.position.side, mochi_domain::PositionSide::Long);
    assert_eq!(view.stop_loss.as_deref(), Some("74000"));
    assert_eq!(view.take_profit.as_deref(), Some("78000"));
    let results = h.engine.open_results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].opening_side.as_deref(), Some("BUY"));
}

#[tokio::test]
async fn a_reached_deferred_target_exits_at_most_once_even_if_the_response_is_lost() {
    for api_timeout in [false, true] {
        let (h, entry) = resting_target().await;
        h.okx.state().fill_resting();
        h.okx.state().lose_next_response = !api_timeout;
        h.okx.state().timeout_next_response = api_timeout;
        h.okx.state().fail_lookup_after_timeout = api_timeout;
        h.advance(1);
        *h.prices.lock().unwrap() = ("78099".into(), "78101".into(), "78100".into());
        h.disarm();
        let id = format!("{}:deferred:tp", entry.id);
        assert_eq!(h.engine.sync().await.is_err(), api_timeout);
        if api_timeout {
            let pending = h.order_row(&id);
            assert_eq!(pending.status, OrderStatus::Open);
            assert_eq!(pending.order_type, OrderType::Market);
            assert!(pending.reject_reason.unwrap().contains("50004"));
            h.okx.state().lookup_error_code = None;
            restart(&h).sync().await.unwrap();
        }
        h.sync().await;
        assert!(h.algo_placed().is_empty());
        let orders = &h.okx.state().placed;
        assert_eq!(orders.len(), 2);
        assert_eq!(orders[1]["ordType"], "market");
        assert_eq!(orders[1]["side"], "sell");
        assert_eq!(orders[1]["reduceOnly"], true);
        assert_eq!(orders[1]["sz"], "0.1");
        assert_eq!(h.order_row(&id).status, OrderStatus::Filled);
        assert!(h.engine.positions().is_empty());
    }
}

#[tokio::test]
async fn a_reached_deferred_target_revalidates_the_position_after_recording_submission() {
    let (h, entry) = resting_target().await;
    h.okx.state().fill_resting();
    h.advance(1);
    *h.prices.lock().unwrap() = ("78099".into(), "78101".into(), "78100".into());
    let first = h.okx.hold_next("GET", "/account/positions");
    let parent_read = h.okx.hold_next("GET", "/account/positions");
    let attribution = h.okx.hold_next("GET", "/account/positions");
    let before_send = h.okx.hold_next("GET", "/account/positions");
    let engine = h.engine.clone();
    let sync = tokio::spawn(async move { engine.sync().await });
    let timeout = std::time::Duration::from_secs(2);
    tokio::time::timeout(timeout, first.reached())
        .await
        .unwrap();
    first.release();
    tokio::time::timeout(timeout, parent_read.reached())
        .await
        .unwrap();
    parent_read.release();
    tokio::time::timeout(timeout, attribution.reached())
        .await
        .unwrap();
    attribution.release();
    tokio::time::timeout(timeout, before_send.reached())
        .await
        .unwrap();
    let id = format!("{}:deferred:tp", entry.id);
    assert_eq!(h.order_row(&id).order_type, OrderType::Market);
    {
        let mut okx = h.okx.state();
        assert_eq!(
            okx.placed.len(),
            1,
            "market order sent before final position read"
        );
        okx.algos.clear();
        okx.position
            .as_mut()
            .unwrap()
            .insert("tradeId".into(), json!("2"));
    }
    before_send.release();
    sync.await.unwrap().unwrap();
    assert_eq!(h.okx.state().placed.len(), 1);
    let unsent = h.order_row(&id);
    assert_eq!(unsent.status, OrderStatus::Open);
    assert_ne!(unsent.order_type, OrderType::Market);
    assert!(h.engine.protection_of("BTCUSDT").stop_loss.is_none());
}

#[tokio::test]
async fn a_deferred_algo_revalidates_the_position_after_recording_submission() {
    let (h, entry) = resting_target().await;
    let id = format!("{}:deferred:tp", entry.id);
    let unsent = h.order_row(&id);
    h.okx.state().fill_resting();
    h.advance(1);
    let fake = h.okx.clone();
    *h.store.after_deferred_save.lock().unwrap() = Some((
        id.clone(),
        Box::new(move || {
            fake.state()
                .position
                .as_mut()
                .unwrap()
                .insert("tradeId".into(), json!("2"));
        }),
    ));
    h.sync().await;
    assert!(h.store.after_deferred_save.lock().unwrap().is_none());
    assert!(h.algo_placed().is_empty());
    assert_eq!(h.order_row(&id), unsent);
}

#[tokio::test]
async fn a_quote_from_before_the_fill_cannot_trigger_a_market_exit() {
    let (h, entry) = resting_target().await;
    let client_id = mochi_okx::to_cl_ord_id(&entry.client_order_id);
    {
        let mut okx = h.okx.state();
        okx.fill_resting();
        okx.orders[&client_id].insert("fillTime".into(), json!((START + 1).to_string()));
    }
    *h.prices.lock().unwrap() = ("78099".into(), "78101".into(), "78100".into());
    h.sync().await;
    assert_eq!(h.okx.state().placed.len(), 1);
    assert!(h.algo_placed().is_empty());
    h.advance(2);
    *h.prices.lock().unwrap() = ("74999".into(), "75001".into(), "75000".into());
    h.sync().await;
    assert_eq!(h.okx.state().placed.len(), 1);
    assert_eq!(h.algo_placed()[0]["tpTriggerPx"], "78000");
}

#[tokio::test]
async fn both_reached_targets_close_their_own_entries_even_after_a_failed_refresh() {
    for fail_refresh in [false, true] {
        let (h, first) = resting_target().await;
        let mut request = order();
        request.order_type = EntryOrderType::Limit;
        request.price = Some("74500".into());
        request.take_profit = Some("79000".into());
        let second = h.engine.place_order(request).await.unwrap().order;
        assert_eq!(second.status, OrderStatus::Open);
        h.okx.state().fill_resting();
        h.okx.state().fail_positions_after_reduce = fail_refresh;
        h.advance(1);
        *h.prices.lock().unwrap() = ("79999".into(), "80001".into(), "80000".into());
        assert_eq!(h.engine.sync().await.is_err(), fail_refresh);
        h.sync().await;
        for entry in [first, second] {
            let row = h.order_row(&format!("{}:deferred:tp", entry.id));
            assert_eq!(
                row.status,
                OrderStatus::Filled,
                "refresh {fail_refresh} row {}: {:?}",
                row.id,
                row.reject_reason
            );
        }
        let state = h.okx.state();
        assert!(state.position.is_none());
        assert_eq!(state.placed.len(), 4);
        assert!(state.placed[2..]
            .iter()
            .all(|order| order["sz"] == "0.1" && order["reduceOnly"] == true));
    }
}

#[tokio::test]
async fn another_reached_target_keeps_its_allocation_when_the_first_is_partially_filled() {
    let (h, first) = resting_target().await;
    let mut request = order();
    request.order_type = EntryOrderType::Limit;
    request.price = Some("74500".into());
    request.take_profit = Some("79000".into());
    let second = h.engine.place_order(request).await.unwrap().order;
    h.okx.state().fill_resting();
    h.advance(1);
    *h.prices.lock().unwrap() = ("79999".into(), "80001".into(), "80000".into());

    let post = h.okx.hold_next("POST", "/trade/order");
    let engine = h.engine.clone();
    let sync = tokio::spawn(async move { engine.sync().await });
    let timeout = std::time::Duration::from_secs(2);
    tokio::time::timeout(timeout, post.reached()).await.unwrap();
    let lookup = h.okx.hold_next("GET", "/trade/order");
    post.release();
    tokio::time::timeout(timeout, lookup.reached())
        .await
        .unwrap();
    let first_id = format!("{}:deferred:tp", first.id);
    {
        let mut okx = h.okx.state();
        let key = mochi_okx::to_cl_ord_id(&first_id);
        okx.orders[&key].insert("state".into(), json!("partially_filled"));
        okx.orders[&key].insert("accFillSz".into(), json!("0.05"));
        okx.orders[&key].insert("fillSz".into(), json!("0.05"));
        okx.recent_fills
            .last_mut()
            .unwrap()
            .insert("fillSz".into(), json!("0.05"));
        okx.position
            .as_mut()
            .unwrap()
            .insert("pos".into(), json!("0.15"));
    }
    lookup.release();
    sync.await.unwrap().unwrap();
    assert_eq!(h.order_row(&first_id).status, OrderStatus::Open);
    assert_eq!(
        h.order_row(&format!("{}:deferred:tp", second.id)).status,
        OrderStatus::Filled
    );
    assert_eq!(h.okx.state().placed.last().unwrap()["sz"], "0.1");
    assert_eq!(h.okx.state().position.as_ref().unwrap()["pos"], "0.05");
}

#[tokio::test]
async fn a_partial_market_target_keeps_trailing_on_the_remaining_entry_quantity() {
    let h = connect(Vec::new()).await;
    h.okx.state().rest_limit_orders = true;
    let mut request = order();
    request.order_type = EntryOrderType::Limit;
    request.price = Some("74500".into());
    request.take_profit = Some("78000".into());
    request.trailing_stop = Some(ratio("0.01"));
    let entry = h.engine.place_order(request).await.unwrap().order;
    assert_eq!(entry.status, OrderStatus::Open);
    h.okx.state().fill_resting();
    let mut remaining = h.okx.state().position.clone().unwrap();
    h.advance(1);
    *h.prices.lock().unwrap() = ("78999".into(), "79001".into(), "79000".into());
    let post = h.okx.hold_next("POST", "/trade/order");
    let engine = h.engine.clone();
    let sync = tokio::spawn(async move { engine.sync().await });
    let timeout = std::time::Duration::from_secs(2);
    tokio::time::timeout(timeout, post.reached()).await.unwrap();
    let lookup = h.okx.hold_next("GET", "/trade/order");
    post.release();
    tokio::time::timeout(timeout, lookup.reached())
        .await
        .unwrap();
    {
        let mut okx = h.okx.state();
        let target_id = mochi_okx::to_cl_ord_id(&format!("{}:deferred:tp", entry.id));
        let target = &mut okx.orders[&target_id];
        target.insert("state".into(), json!("canceled"));
        target.insert("accFillSz".into(), json!("0.05"));
        target.insert("fillSz".into(), json!("0.05"));
        let fill = okx.recent_fills.last_mut().unwrap();
        fill.insert("fillSz".into(), json!("0.05"));
        remaining.insert("pos".into(), json!("0.05"));
        remaining.insert("tradeId".into(), fill["tradeId"].clone());
        okx.position = Some(remaining);
    }
    lookup.release();
    sync.await.unwrap().unwrap();
    let trail = h.order_row(&format!("{}:trail", entry.id));
    assert_eq!(trail.status, OrderStatus::Open);
    assert_eq!(trail.quantity, "0.0005");
    assert_eq!(h.algo_placed()[0]["sz"], "0.05");
}

#[tokio::test]
async fn a_late_partial_target_fill_keeps_the_deferred_trail_on_the_remainder() {
    let h = connect(Vec::new()).await;
    h.okx.state().rest_limit_orders = true;
    let mut request = order();
    request.order_type = EntryOrderType::Limit;
    request.price = Some("74500".into());
    request.take_profit = Some("78000".into());
    request.trailing_stop = Some(ratio("0.01"));
    let entry = h.engine.place_order(request).await.unwrap().order;
    h.okx.state().fill_resting();
    h.okx.state().drop_next_request = true;
    h.advance(1);
    *h.prices.lock().unwrap() = ("78999".into(), "79001".into(), "79000".into());

    let post = h.okx.hold_next("POST", "/trade/order");
    let engine = h.engine.clone();
    let sync = tokio::spawn(async move { engine.sync().await });
    tokio::time::timeout(std::time::Duration::from_secs(2), post.reached())
        .await
        .unwrap();
    // Let the target send, but leave the unsent trail waiting for a fresh quote.
    h.quote_time.store(START - 60_000, Ordering::SeqCst);
    post.release();
    sync.await.unwrap().unwrap();
    let target_id = format!("{}:deferred:tp", entry.id);
    let trail_id = format!("{}:trail", entry.id);
    assert_eq!(h.order_row(&target_id).status, OrderStatus::Open);
    let placements = h.count("POST /api/v5/trade/order");

    h.advance(60_001);
    h.sync().await;
    let target = h.order_row(&target_id);
    assert_eq!(target.status, OrderStatus::Canceled);
    assert_eq!(h.order_row(&trail_id).status, OrderStatus::Open);
    assert!(h.algo_placed().is_empty());

    // The sibling still matters after other orders push it out of the recent list.
    h.store(|s| {
        for i in 0..500 {
            let filler = Order {
                id: format!("filler_{i}"),
                client_order_id: format!("filler_{i}"),
                created_at: target.created_at + i + 1,
                reject_reason: None,
                ..target.clone()
            };
            s.insert_order(&filler).unwrap();
        }
    });

    let client_id = mochi_okx::to_cl_ord_id(&target.client_order_id);
    let filled_at = h.clock.load(Ordering::SeqCst).to_string();
    {
        let mut okx = h.okx.state();
        // The fill and the reduced position are visible before order lookup recovers.
        okx.recent_fills.push(row(json!({
            "instId": "BTC-USDT-SWAP", "ordId": "late-target", "tradeId": "2",
            "clOrdId": client_id, "fillSz": "0.05", "fillTime": filled_at,
            "posSide": "net", "subType": "5",
        })));
        let position = okx.position.as_mut().unwrap();
        position.insert("tradeId".into(), json!("2"));
        position.insert("uTime".into(), json!(filled_at));
        position.insert("pos".into(), json!("0.05"));
    }
    h.quote_time.store(0, Ordering::SeqCst);
    h.sync().await;
    let trail = h.order_row(&trail_id);
    assert_eq!(trail.status, OrderStatus::Open, "{:?}", trail.reject_reason);
    assert!(h.algo_placed().is_empty());
    assert_eq!(h.order_row(&target_id).status, OrderStatus::Canceled);
    assert_eq!(h.count("POST /api/v5/trade/order"), placements);

    h.okx.state().orders.insert(
        client_id,
        row(json!({
            "ordId": "late-target", "state": "canceled", "accFillSz": "0.05",
            "avgPx": "79000", "fee": "0", "pnl": "0",
        })),
    );
    h.sync().await;
    let trail = h.order_row(&trail_id);
    assert_eq!(trail.status, OrderStatus::Open, "{:?}", trail.reject_reason);
    assert_eq!(trail.quantity, "0.0005");
    assert_eq!(h.algo_placed()[0]["sz"], "0.05");
    assert_eq!(h.order_row(&target_id).filled_quantity, "0.0005");
    assert_eq!(h.count("POST /api/v5/trade/order"), placements);
}

#[tokio::test]
async fn an_agent_protection_change_refuses_a_replacement_or_resize_after_risk_check() {
    let h = Harness::with_position().await;
    let position = h.view().position;
    let checked = CheckedProtection {
        position_id: position.id,
        quantity: position.quantity,
        before: h.engine.protection_of("BTCUSDT"),
    };
    let initial = h.okx.state().position.clone().unwrap();
    for (field, value) in [("pos", "0.4"), ("tradeId", "42")] {
        let mut changed = initial.clone();
        changed.insert(field.into(), json!(value));
        h.okx.state().position = Some(changed);
        let result = h
            .engine
            .set_protection_order(
                ACCOUNT,
                "BTCUSDT",
                stop("73500"),
                ProtectionOrderOptions {
                    retain: Default::default(),
                    agent_id: Some("agent".into()),
                    decision_id: Some("checked".into()),
                    checked: Some(checked.clone()),
                },
            )
            .await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("changed after the agent's risk check"));
    }
    assert!(h.algo_placed().is_empty());
}

#[tokio::test]
async fn an_agent_protection_change_checks_the_isolated_row_and_its_live_stop() {
    let h = Harness::with_position().await;
    {
        let mut okx = h.okx.state();
        let mut cross = okx.position.clone().unwrap();
        cross.insert("mgnMode".into(), json!("cross"));
        cross.insert("posId".into(), json!("cross-position"));
        okx.other_positions.push(cross);
    }
    h.sync().await;
    let position = h
        .engine
        .positions()
        .into_iter()
        .find(|view| view.close_supported == Some(true))
        .unwrap()
        .position;
    let checked = CheckedProtection {
        position_id: position.id,
        quantity: position.quantity,
        before: h.engine.protection_of("BTCUSDT"),
    };
    let keep_stop = || {
        h.engine.set_protection_order(
            ACCOUNT,
            "BTCUSDT",
            stop("74000"),
            ProtectionOrderOptions {
                retain: Default::default(),
                agent_id: Some("agent".into()),
                decision_id: Some("checked".into()),
                checked: Some(checked.clone()),
            },
        )
    };
    assert_eq!(
        keep_stop().await.unwrap().after.stop_loss.as_deref(),
        Some("74000")
    );
    let id = to_algo_cl_ord_id(&h.protection_rows()[0].id);
    h.okx
        .state()
        .algos
        .get_mut(&id)
        .unwrap()
        .insert("slTriggerPx".into(), json!("74500"));
    assert!(keep_stop()
        .await
        .unwrap_err()
        .to_string()
        .contains("changed after the agent's risk check"));
    assert!(h.algo_placed().is_empty());
    h.sync().await;
    assert_eq!(
        h.engine.protection_of("BTCUSDT").stop_loss.as_deref(),
        Some("74500")
    );
    let view = h
        .engine
        .positions()
        .into_iter()
        .find(|view| view.close_supported == Some(true))
        .unwrap();
    assert_eq!(view.stop_loss.as_deref(), Some("74500"));
    assert_eq!(
        h.order_row(&h.protection_rows()[0].id)
            .trigger_price
            .as_deref(),
        Some("74500")
    );
}

#[tokio::test]
async fn a_stop_change_uses_the_liquidation_price_from_the_live_position() {
    let h = Harness::with_position().await;
    let position = h.view().position;
    let checked = CheckedProtection {
        position_id: position.id,
        quantity: position.quantity,
        before: h.engine.protection_of("BTCUSDT"),
    };
    h.okx
        .state()
        .position
        .as_mut()
        .unwrap()
        .insert("liqPx".into(), json!("73700"));
    for checked in [Some(checked), None] {
        let error = h
            .engine
            .set_protection_order(
                ACCOUNT,
                "BTCUSDT",
                stop("73500"),
                ProtectionOrderOptions {
                    retain: Default::default(),
                    agent_id: None,
                    decision_id: None,
                    checked,
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("before liquidation"));
    }
    assert!(h.algo_placed().is_empty());
}

#[tokio::test]
async fn a_never_received_market_target_expires_without_resending() {
    let (h, entry) = resting_target().await;
    h.okx.state().fill_resting();
    h.okx.state().drop_next_request = true;
    h.advance(1);
    *h.prices.lock().unwrap() = ("78999".into(), "79001".into(), "79000".into());
    h.sync().await;
    let id = format!("{}:deferred:tp", entry.id);
    assert_eq!(h.order_row(&id).status, OrderStatus::Open);
    let placed = h.count("POST /api/v5/trade/order");
    h.advance(60_001);
    h.sync().await;
    assert_eq!(h.order_row(&id).status, OrderStatus::Canceled);
    assert_eq!(h.count("POST /api/v5/trade/order"), placed);
    assert_eq!(h.view().position.quantity, "0.001");

    // A delayed terminal answer can still settle the inactive row by its client ID
    // after unrelated activity pushes it out of the recent-orders window.
    let prior = h.order_row(&id);
    h.store(|s| {
        for i in 0..500 {
            let filler = Order {
                id: format!("filler_{i}"),
                client_order_id: format!("filler_{i}"),
                created_at: prior.created_at + i + 1,
                updated_at: prior.updated_at + i + 1,
                reject_reason: None,
                ..prior.clone()
            };
            s.insert_order(&filler).unwrap();
        }
        for i in 0..10 {
            let filler = Order {
                id: format!("older_{i}:deferred:tp"),
                client_order_id: format!("older_{i}"),
                created_at: prior.created_at - i - 1,
                updated_at: prior.updated_at - i - 1,
                ..prior.clone()
            };
            s.insert_order(&filler).unwrap();
        }
    });
    {
        let mut okx = h.okx.state();
        let key = mochi_okx::to_cl_ord_id(&id);
        okx.orders.insert(
            key,
            row(json!({
                "ordId": "late-target", "state": "filled", "accFillSz": "0.1", "avgPx": "79000",
                "fee": "0", "pnl": "0"
            })),
        );
        okx.position = None;
    }
    h.sync().await;
    assert_eq!(h.order_row(&id).status, OrderStatus::Canceled);
    h.sync().await;
    assert_eq!(h.order_row(&id).status, OrderStatus::Filled);
    restart(&h).sync().await.unwrap();
    assert_eq!(h.order_row(&id).status, OrderStatus::Filled);
    assert_eq!(h.count("POST /api/v5/trade/order"), placed);
}

#[tokio::test]
async fn an_unattributed_market_exit_leaves_the_other_target_pending_until_lookup() {
    let (h, first) = resting_target().await;
    let mut request = order();
    request.order_type = EntryOrderType::Limit;
    request.price = Some("74500".into());
    request.take_profit = Some("79000".into());
    let second = h.engine.place_order(request).await.unwrap().order;
    h.okx.state().fill_resting();
    h.okx.state().lose_next_response = true;
    h.advance(1);
    *h.prices.lock().unwrap() = ("79999".into(), "80001".into(), "80000".into());
    let post = h.okx.hold_next("POST", "/trade/order");
    let engine = h.engine.clone();
    let sync = tokio::spawn(async move { engine.sync().await });
    let timeout = std::time::Duration::from_secs(2);
    tokio::time::timeout(timeout, post.reached()).await.unwrap();
    let lookup = h.okx.hold_next("GET", "/trade/order");
    post.release();
    tokio::time::timeout(timeout, lookup.reached())
        .await
        .unwrap();
    let key = mochi_okx::to_cl_ord_id(&format!("{}:deferred:tp", first.id));
    let pending = h.okx.state().orders.swap_remove(&key).unwrap();
    lookup.release();
    sync.await.unwrap().unwrap();
    let second_id = format!("{}:deferred:tp", second.id);
    assert_eq!(h.order_row(&second_id).status, OrderStatus::Open);
    assert_eq!(h.okx.state().placed.len(), 3);
    h.advance(60_001);
    h.sync().await;
    assert_eq!(h.order_row(&second_id).status, OrderStatus::Open);
    assert_eq!(h.okx.state().placed.len(), 3);
    h.okx.state().orders.insert(key, pending);
    h.sync().await;
    assert_eq!(h.order_row(&second_id).status, OrderStatus::Filled);
    assert_eq!(h.okx.state().placed.len(), 4);
}

#[tokio::test]
async fn a_reopened_position_never_displays_protection_read_before_a_market_target() {
    let (h, _) = resting_target().await;
    h.okx.state().fill_resting();
    h.advance(1);
    *h.prices.lock().unwrap() = ("78099".into(), "78101".into(), "78100".into());
    let post = h.okx.hold_next("POST", "/trade/order");
    let engine = h.engine.clone();
    let sync = tokio::spawn(async move { engine.sync().await });
    let timeout = std::time::Duration::from_secs(2);
    tokio::time::timeout(timeout, post.reached()).await.unwrap();
    let refresh = h.okx.hold_next("GET", "/trade/orders-algo-pending");
    post.release();
    tokio::time::timeout(timeout, refresh.reached())
        .await
        .unwrap();
    {
        let mut okx = h.okx.state();
        assert!(okx.position.is_none());
        let mut reopened = long_position();
        reopened.insert("tradeId".into(), json!("3"));
        okx.position = Some(reopened);
        okx.equity = "950".into();
        okx.algos.clear();
    }
    refresh.release();
    sync.await.unwrap().unwrap();
    assert!(h.view().position.id.ends_with(":3"));
    assert_eq!(h.view().stop_loss, None);
    assert_eq!(h.engine.balance().unwrap().equity, "950");
    h.okx.state().algo_lookup_down = true;
    assert!(h.engine.sync().await.is_err());
    assert!(h.view().position.id.ends_with(":3"));
    assert_eq!(h.view().stop_loss, None);
}

#[tokio::test]
async fn a_failed_target_refresh_does_not_use_the_old_size_for_a_fractional_close() {
    let (h, _) = resting_target().await;
    h.store
        .inner
        .lock()
        .unwrap()
        .1
        .iter_mut()
        .find(|spec| spec.symbol == "BTCUSDT")
        .unwrap()
        .quantity_step = "0.001".into();
    {
        let mut okx = h.okx.state();
        okx.fill_resting();
        okx.position
            .as_mut()
            .unwrap()
            .insert("pos".into(), json!("0.4"));
        okx.fail_positions_after_reduce = true;
    }
    h.advance(1);
    *h.prices.lock().unwrap() = ("78099".into(), "78101".into(), "78100".into());
    assert!(h.engine.sync().await.is_err());
    assert!(h.engine.positions().is_empty());
    assert_eq!(h.okx.state().position.as_ref().unwrap()["pos"], "0.3");
    h.disarm();
    let close = h
        .engine
        .close_position(ACCOUNT, "BTCUSDT", 0.5, "fresh-half")
        .await
        .unwrap();
    assert_eq!(close.order.status, OrderStatus::Filled);
    assert_eq!(h.okx.state().placed.last().unwrap()["sz"], "0.1");
    h.okx.state().down = true;
    let repeat = h
        .engine
        .close_position(ACCOUNT, "BTCUSDT", 0.5, "fresh-half")
        .await
        .unwrap();
    assert!(repeat.duplicate);
    assert_eq!(repeat.order.id, close.order.id);
    assert_eq!(h.okx.state().placed.len(), 3);
}

#[tokio::test]
async fn protection_read_failure_does_not_copy_a_long_stop_to_a_new_short() {
    let h = Harness::with_position().await;
    assert_eq!(h.view().stop_loss.as_deref(), Some("74000"));
    {
        let mut okx = h.okx.state();
        okx.algo_lookup_down = true;
        let position = okx.position.as_mut().unwrap();
        position.insert("tradeId".into(), json!("9"));
        position.insert("pos".into(), json!("-0.1"));
    }
    assert!(h.engine.sync().await.is_err());
    let view = h.view();
    assert_eq!(view.position.side, mochi_domain::PositionSide::Short);
    assert_eq!(view.stop_loss, None);
    assert_eq!(view.take_profit, None);
    assert_eq!(h.engine.protection_of("BTCUSDT").stop_loss, None);
}

#[tokio::test]
async fn a_fixed_stop_edit_keeps_unsent_optional_exits_on_the_filled_entry() {
    let h = connect(Vec::new()).await;
    h.okx.state().rest_limit_orders = true;
    let mut request = order();
    request.order_type = EntryOrderType::Limit;
    request.price = Some("74500".into());
    request.take_profit = Some("78000".into());
    request.trailing_stop = Some(ratio("0.01"));
    let entry = h.engine.place_order(request).await.unwrap().order;
    h.okx.state().fill_resting();
    let key = mochi_okx::to_cl_ord_id(&entry.client_order_id);
    {
        let mut okx = h.okx.state();
        // The order read can lead the position snapshot.
        okx.orders
            .get_mut(&key)
            .unwrap()
            .insert("tradeId".into(), json!("2"));
    }
    h.sync().await;
    let result = h
        .engine
        .set_protection_with(
            ACCOUNT,
            "BTCUSDT",
            stop("74500"),
            mochi_okx::engine::ProtectionRetention {
                trailing_stop: true,
                take_profit: true,
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.after.stop_loss.as_deref(), Some("74500"));
    for suffix in [":deferred:tp", ":trail"] {
        let row = h.order_row(&format!("{}{suffix}", entry.id));
        assert_eq!(row.status, OrderStatus::Open);
        assert!(row.exchange_order_id.is_none());
    }
    {
        let mut okx = h.okx.state();
        okx.position
            .as_mut()
            .unwrap()
            .insert("tradeId".into(), json!("2"));
        okx.recent_fills[0].insert("tradeId".into(), json!("2"));
    }
    h.sync().await;
    let protection = h.engine.protection_of("BTCUSDT");
    assert_eq!(protection.take_profit.as_deref(), Some("78000"));
    assert_eq!(protection.trailing_stop, Some(ratio("0.01")));
}
