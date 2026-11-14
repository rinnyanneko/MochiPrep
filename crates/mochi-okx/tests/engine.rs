//! The OKX engine against the in-process fake OKX, with an injected clock: ports of the
//! okx-live.test.ts groups "real-money safety", "added pairs" (the engine-level tests) and
//! "duplicate protection and ambiguous outcomes".

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mochi_core::paper::PaperStore;
use mochi_domain::{
    default_account_config, default_live_limits, AccountVenue, DataSource, EntryOrderType,
    InstrumentSpecification, OrderRequest, OrderSide, OrderStatus, PaperAccount, Quote,
    RiskSettings,
};
use mochi_okx::engine::{OkxEngine, OkxEngineDeps, Rows, StoreAccess, StoreWork};
use mochi_okx::fake::{self, FakeOkx, Refusal};
use mochi_okx::{BoxFuture, HttpReply, HttpRequest, OkxClient, Transport, Venue};
use mochi_store::{open_in_memory, SqlitePaperStore};
use serde_json::{json, Value};

const ACCOUNT: &str = "acct_okx";
/// 2027-01-15T08:00:00Z: far from a UTC day boundary, so the daily-loss base is stable.
const START: i64 = 1_800_000_000_000;

/// The store as the server's actor runs it: every piece of work alone, to completion.
struct Local {
    inner: Mutex<(SqlitePaperStore, Vec<InstrumentSpecification>)>,
}

impl StoreAccess for Local {
    fn run(&self, work: StoreWork) -> BoxFuture<'static, ()> {
        {
            let inner = self.inner.lock().unwrap();
            work(&Rows {
                store: &inner.0,
                instruments: &inner.1,
            });
        }
        Box::pin(async {})
    }
}

/// Answers the next order placement with JSON `null`, which the client reads as a plain error
/// (`OkxError::Invalid`), without letting it reach the fake.
struct NullNextOrder {
    inner: Arc<dyn Transport>,
    armed: AtomicBool,
}

impl Transport for NullNextOrder {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpReply, String>> {
        if request.method == "POST"
            && request.url.ends_with("/trade/order")
            && self.armed.swap(false, Ordering::SeqCst)
        {
            return Box::pin(async {
                Ok(HttpReply {
                    status: 200,
                    body: b"null".to_vec(),
                })
            });
        }
        self.inner.send(request)
    }
}

/// The quote the engine sees: the fake's prices, or a pair's own, `age_ms` old.
#[derive(Clone)]
struct Prices {
    bid: String,
    ask: String,
    last: String,
}

struct Harness {
    okx: Arc<FakeOkx>,
    engine: Arc<OkxEngine>,
    store: Arc<Local>,
    clock: Arc<AtomicI64>,
    armed: Arc<AtomicBool>,
    quote_age_ms: Arc<AtomicI64>,
    prices: Arc<Mutex<Prices>>,
    null_next_order: Arc<NullNextOrder>,
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

/// Node's `connect()`: the account row, then the engine and its first full sync.
async fn connect(limits: RiskSettings, extra: Vec<InstrumentSpecification>) -> Harness {
    let okx = FakeOkx::new();
    let store = SqlitePaperStore::new(open_in_memory().unwrap());
    store.insert_account(&account(limits)).unwrap();
    let mut instruments = mochi_domain::default_instruments();
    instruments.extend(extra);
    let store = Arc::new(Local {
        inner: Mutex::new((store, instruments)),
    });
    let clock = Arc::new(AtomicI64::new(START));
    let armed = Arc::new(AtomicBool::new(false));
    let quote_age_ms = Arc::new(AtomicI64::new(0));
    let prices = Arc::new(Mutex::new(Prices {
        bid: "74999".into(),
        ask: "75001".into(),
        last: "75000".into(),
    }));
    let null_next_order = Arc::new(NullNextOrder {
        inner: okx.transport(),
        armed: AtomicBool::new(false),
    });
    let ticks = clock.clone();
    let client = OkxClient::new(fake::credentials(), Venue::OkxLive, null_next_order.clone())
        .with_base_url("https://okx.invalid")
        .with_clock(move || ticks.load(Ordering::SeqCst));
    let ids = Arc::new(AtomicU64::new(0));
    let engine = OkxEngine::new(OkxEngineDeps {
        account_id: ACCOUNT.into(),
        venue: Venue::OkxLive,
        client,
        store: store.clone(),
        get_quote: {
            let (clock, age, prices) = (clock.clone(), quote_age_ms.clone(), prices.clone());
            Arc::new(move |symbol: &str| {
                let prices = prices.lock().unwrap().clone();
                Some(Quote {
                    source: DataSource::Live,
                    symbol: symbol.into(),
                    bid: prices.bid,
                    ask: prices.ask,
                    mark: prices.last.clone(),
                    last: prices.last,
                    mark_estimated: false,
                    funding_rate: Some("0".into()),
                    next_funding_time: None,
                    ts: clock.load(Ordering::SeqCst) - age.load(Ordering::SeqCst),
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
        armed,
        quote_age_ms,
        prices,
        null_next_order,
    }
}

async fn connect_default() -> Harness {
    connect(default_live_limits(), Vec::new()).await
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(0);

/// Node's `order(accountId, overrides)`: a small BTC long with a stop, under a fresh key.
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

impl Harness {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn placed(&self) -> Vec<Value> {
        self.okx.state().placed.clone()
    }

    fn store<T>(&self, f: impl FnOnce(&SqlitePaperStore) -> T) -> T {
        f(&self.store.inner.lock().unwrap().0)
    }

    fn backdate(&self, order_id: &str, ms: i64) {
        self.store(|s| {
            s.connection()
                .execute(
                    "UPDATE orders SET created_at=? WHERE id=?",
                    (START - ms, order_id.to_string()),
                )
                .unwrap()
        });
    }
}

fn reason(result: &mochi_domain::OrderResult) -> String {
    result.order.reject_reason.clone().unwrap_or_default()
}

// ---- real-money safety ------------------------------------------------------------------------

/// The engine half of `sends nothing until armed, and only the exact phrase arms`: the phrase
/// itself is checked by the runtime's `arm`, which comes with connecting.
#[tokio::test]
async fn sends_nothing_until_armed() {
    let h = connect_default().await;
    let result = h.engine.place_order(order()).await.unwrap();
    assert_eq!(result.order.status, OrderStatus::Rejected);
    assert_eq!(
        reason(&result),
        "NOT_ARMED: real-money orders are switched off for this account"
    );
    assert!(h.placed().is_empty());
    h.arm();
    let result = h.engine.place_order(order()).await.unwrap();
    assert_eq!(result.order.status, OrderStatus::Filled);
}

#[tokio::test]
async fn sends_a_well_formed_isolated_order_with_exchange_side_protection() {
    let h = connect_default().await;
    h.arm();
    let mut request = order();
    request.take_profit = Some("77000".into());
    h.engine.place_order(request).await.unwrap();
    let sent = &h.placed()[0];
    assert_eq!(sent["instId"], "BTC-USDT-SWAP");
    assert_eq!(sent["tdMode"], "isolated");
    assert_eq!(sent["side"], "buy");
    assert_eq!(sent["ordType"], "market");
    assert_eq!(sent["sz"], "0.1");
    let hex32 = |v: &Value| {
        let s = v.as_str().unwrap();
        s.len() == 32
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    assert!(hex32(&sent["clOrdId"]));
    let legs = sent["attachAlgoOrds"].as_array().unwrap();
    assert_eq!(legs.len(), 2);
    assert!(hex32(&legs[0]["attachAlgoClOrdId"]));
    assert!(hex32(&legs[1]["attachAlgoClOrdId"]));
    assert_eq!(
        legs[0],
        json!({ "attachAlgoClOrdId": legs[0]["attachAlgoClOrdId"], "slTriggerPx": "74000", "slOrdPx": "-1" })
    );
    assert_eq!(
        legs[1],
        json!({ "attachAlgoClOrdId": legs[1]["attachAlgoClOrdId"], "tpTriggerPx": "77000", "tpOrdPx": "-1" })
    );
    // Key order on the wire, as Node builds the body.
    let keys: Vec<_> = sent.as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        [
            "instId",
            "tdMode",
            "side",
            "ordType",
            "sz",
            "clOrdId",
            "attachAlgoOrds"
        ]
    );
}

type Patch = fn(&mut OrderRequest);

#[tokio::test]
async fn enforces_the_account_ceiling() {
    let cases: [(&str, Patch, &str); 4] = [
        (
            "a missing stop",
            |o| o.stop_loss = None,
            "LIMIT_STOP_REQUIRED: new positions must carry a stop loss",
        ),
        (
            "a stop on the wrong side",
            |o| o.stop_loss = Some("75500".into()),
            "LIMIT_STOP_INVALID: stop 75500 is on the wrong side of 75001",
        ),
        (
            "too much notional",
            |o| o.quantity = "0.01".into(),
            "LIMIT_NOTIONAL: 750.01 USDT exceeds 100 USDT",
        ),
        (
            "too much leverage",
            |o| o.leverage = 10,
            "LIMIT_LEVERAGE: 10x exceeds 3x",
        ),
    ];
    for (label, patch, expected) in cases {
        let h = connect_default().await;
        h.arm();
        let mut request = order();
        patch(&mut request);
        let result = h.engine.place_order(request).await.unwrap();
        assert_eq!(reason(&result), expected, "{label}");
        assert!(h.placed().is_empty(), "{label}");
    }
}

#[tokio::test]
async fn sets_isolated_leverage_before_an_opening_order() {
    let h = connect_default().await;
    h.arm();
    h.engine.place_order(order()).await.unwrap();
    assert_eq!(
        h.okx.state().leverage_calls,
        vec![json!({ "instId": "BTC-USDT-SWAP", "lever": "3", "mgnMode": "isolated" })]
    );
    let calls = h.okx.state().calls.clone();
    let leverage = calls
        .iter()
        .position(|c| c == "POST /api/v5/account/set-leverage");
    let place = calls.iter().position(|c| c == "POST /api/v5/trade/order");
    assert!(leverage.unwrap() < place.unwrap());
}

#[tokio::test]
async fn enforces_the_open_position_limit_across_symbols() {
    let h = connect(
        RiskSettings {
            max_open_positions: 1.0,
            ..default_live_limits()
        },
        Vec::new(),
    )
    .await;
    h.arm();
    h.engine.place_order(order()).await.unwrap();
    let mut eth = order();
    eth.symbol = "ETHUSDT".into();
    let result = h.engine.place_order(eth).await.unwrap();
    assert_eq!(reason(&result), "LIMIT_POSITIONS: 1 open, limit 1");
    assert_eq!(h.placed().len(), 1);
}

#[tokio::test]
async fn refuses_an_order_larger_than_available_margin() {
    let h = connect(
        RiskSettings {
            max_notional: 1_000_000.0,
            ..default_live_limits()
        },
        Vec::new(),
    )
    .await;
    h.arm();
    let mut request = order();
    request.quantity = "0.1".into();
    let result = h.engine.place_order(request).await.unwrap();
    assert_eq!(reason(&result), "INSUFFICIENT_MARGIN: needs 2500.03 USDT");
    assert!(h.placed().is_empty());
}

#[tokio::test]
async fn refuses_a_reduce_only_order_on_the_same_side_as_the_position() {
    let h = connect_default().await;
    h.arm();
    h.engine.place_order(order()).await.unwrap();
    let mut request = order();
    request.reduce_only = Some(true);
    request.stop_loss = None;
    let result = h.engine.place_order(request).await.unwrap();
    assert_eq!(
        reason(&result),
        "REDUCE_ONLY_NO_POSITION: nothing to reduce"
    );
    assert_eq!(h.placed().len(), 1);
}

#[tokio::test]
async fn refuses_new_exposure_on_a_stale_quote() {
    let h = connect_default().await;
    h.arm();
    h.quote_age_ms.store(20_000, Ordering::SeqCst);
    let result = h.engine.place_order(order()).await.unwrap();
    assert_eq!(reason(&result), "MARKET_DATA_STALE: quote is 20s old");
    assert!(h.placed().is_empty());
}

#[tokio::test]
async fn stops_new_positions_once_the_daily_loss_limit_is_reached() {
    let h = connect_default().await;
    h.arm();
    h.okx.state().equity = "970".into();
    h.engine.sync().await.unwrap();
    let result = h.engine.place_order(order()).await.unwrap();
    assert_eq!(reason(&result), "LIMIT_DAILY_LOSS: down 30.00 USDT today");
}

#[tokio::test]
async fn still_lets_a_disarmed_account_exit_clipped_to_the_position_size() {
    let h = connect_default().await;
    h.arm();
    h.engine.place_order(order()).await.unwrap();
    h.armed.store(false, Ordering::SeqCst);
    let mut eth = order();
    eth.symbol = "ETHUSDT".into();
    eth.stop_loss = Some("1".into());
    let blocked = h.engine.place_order(eth).await.unwrap();
    assert!(reason(&blocked).starts_with("NOT_ARMED"));
    let mut exit = order();
    exit.side = OrderSide::Sell;
    exit.quantity = "5".into();
    exit.reduce_only = Some(true);
    exit.stop_loss = None;
    let exit = h.engine.place_order(exit).await.unwrap();
    assert_eq!(exit.order.status, OrderStatus::Filled);
    assert_eq!(exit.order.quantity, "0.001");
    assert_eq!(h.placed()[1]["sz"], "0.1");
    assert_eq!(h.placed()[1]["reduceOnly"], true);
    assert!(h.okx.state().position.is_none());
}

/// Node's `closePosition`: a reduce-only market order for a fraction, sent while disarmed.
#[tokio::test]
async fn closes_a_fraction_of_the_position_while_disarmed() {
    let h = connect_default().await;
    h.arm();
    let mut request = order();
    request.quantity = "0.0013".into();
    h.engine.place_order(request).await.unwrap();
    h.armed.store(false, Ordering::SeqCst);
    let result = h
        .engine
        .close_position(ACCOUNT, "BTCUSDT", 0.5, "close-1")
        .await
        .unwrap();
    assert_eq!(result.order.status, OrderStatus::Filled);
    assert_eq!(result.order.side, OrderSide::Sell);
    assert!(result.order.reduce_only);
    // 0.0013 BTC is 0.13 contracts; half, floored to the 0.0001 step, is 0.0006 BTC.
    assert_eq!(h.placed()[1]["sz"], "0.06");
    let error = h
        .engine
        .close_position(ACCOUNT, "ETHUSDT", 1.0, "close-2")
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "No open ETHUSDT position");
}

// ---- added pairs ------------------------------------------------------------------------------

#[tokio::test]
async fn converts_to_contracts_from_the_pair_spec_and_sends_prices_in_plain_decimals_on_the_tick() {
    let h = connect(default_live_limits(), vec![pepe()]).await;
    h.arm();
    *h.prices.lock().unwrap() = Prices {
        bid: "0.0000099".into(),
        ask: "0.0000101".into(),
        last: "0.00001".into(),
    };
    let mut request = order();
    request.symbol = "PEPEUSDT".into();
    request.quantity = "3500000".into();
    request.stop_loss = Some("0.00000901234".into());
    h.engine.place_order(request).await.unwrap();
    // 0.35 contracts, floored to this pair's 0.1 lot.
    let sent = &h.placed()[0];
    assert_eq!(sent["instId"], "PEPE-USDT-SWAP");
    assert_eq!(sent["sz"], "0.3");
    assert_eq!(sent["attachAlgoOrds"][0]["slTriggerPx"], "0.0000090123");
}

// ---- duplicate protection and ambiguous outcomes ----------------------------------------------

#[tokio::test]
async fn sends_one_order_for_one_idempotency_key() {
    let h = connect_default().await;
    h.arm();
    let request = order();
    let first = h.engine.place_order(request.clone()).await.unwrap();
    let second = h.engine.place_order(request).await.unwrap();
    assert!(second.duplicate);
    assert_eq!(second.order.id, first.order.id);
    assert_eq!(h.placed().len(), 1);
}

#[tokio::test]
async fn serializes_concurrent_orders_so_only_one_passes_the_position_check() {
    let h = connect_default().await;
    h.arm();
    let (a, b) = tokio::join!(h.engine.place_order(order()), h.engine.place_order(order()));
    let mut statuses = vec![a.unwrap().order.status, b.unwrap().order.status];
    statuses.sort_by_key(|s| format!("{s:?}"));
    assert_eq!(statuses, [OrderStatus::Filled, OrderStatus::Rejected]);
    assert_eq!(h.placed().len(), 1);
}

#[tokio::test]
async fn resolves_a_lost_response_by_lookup_and_never_resends() {
    let h = connect_default().await;
    h.arm();
    h.okx.state().lose_next_response = true;
    let result = h.engine.place_order(order()).await.unwrap();
    assert_eq!(h.placed().len(), 1);
    assert_eq!(result.order.status, OrderStatus::Filled);
    assert_eq!(result.fills.len(), 1);
    assert_eq!(result.order.reject_reason, None);
}

#[tokio::test]
async fn keeps_an_order_that_never_arrived_open_then_closes_it_once_okx_has_no_record() {
    let h = connect_default().await;
    h.arm();
    h.okx.state().drop_next_request = true;
    let result = h.engine.place_order(order()).await.unwrap();
    assert_eq!(result.order.status, OrderStatus::Open);
    assert_eq!(
        reason(&result),
        "Awaiting confirmation: No answer from OKX: socket hang up"
    );
    assert!(h.placed().is_empty());
    h.backdate(&result.order.id, 61_000);
    h.engine.sync().await.unwrap();
    let row = h.store(|s| s.get_order(&result.order.id).unwrap().unwrap());
    assert_eq!(row.status, OrderStatus::Canceled);
    assert_eq!(
        row.reject_reason.as_deref(),
        Some("OKX has no record of this order")
    );
    assert!(h.placed().is_empty());
}

/// An order younger than a minute that OKX does not know is left alone: it may still arrive.
#[tokio::test]
async fn a_recent_order_okx_does_not_know_yet_stays_open() {
    let h = connect_default().await;
    h.arm();
    h.okx.state().drop_next_request = true;
    let result = h.engine.place_order(order()).await.unwrap();
    h.clock.fetch_add(59_000, Ordering::SeqCst);
    h.engine.sync().await.unwrap();
    let row = h.store(|s| s.get_order(&result.order.id).unwrap().unwrap());
    assert_eq!(row.status, OrderStatus::Open);
    h.clock.fetch_add(1_000, Ordering::SeqCst);
    h.engine.sync().await.unwrap();
    let row = h.store(|s| s.get_order(&result.order.id).unwrap().unwrap());
    assert_eq!(row.status, OrderStatus::Canceled);
}

#[tokio::test]
async fn does_not_give_up_on_an_order_when_the_lookup_itself_fails() {
    let h = connect_default().await;
    h.arm();
    {
        let mut okx = h.okx.state();
        okx.drop_next_request = true;
        okx.lookup_error_code = Some("50001".into());
    }
    let result = h.engine.place_order(order()).await.unwrap();
    h.backdate(&result.order.id, 61_000);
    let error = h.engine.sync().await.unwrap_err();
    assert_eq!(error.to_string(), "OKX 50001: Systems are busy");
    let row = h.store(|s| s.get_order(&result.order.id).unwrap().unwrap());
    assert_eq!(row.status, OrderStatus::Open);
    // A failed sync marks the account unhealthy, and new exposure is refused with its reason.
    assert!(!h.engine.healthy());
    assert_eq!(
        h.engine.error().as_deref(),
        Some("OKX 50001: Systems are busy")
    );
    let refused = h.engine.place_order(order()).await.unwrap();
    assert_eq!(
        reason(&refused),
        "EXCHANGE_NOT_SYNCED: OKX 50001: Systems are busy"
    );
    assert!(h.placed().is_empty());
}

#[tokio::test]
async fn treats_a_per_order_refusal_inside_a_successful_response_as_rejected() {
    let h = connect_default().await;
    h.arm();
    h.okx.state().reject_row_next = Some(Refusal::new(
        "51121",
        "Order quantity must be a multiple of the lot size",
    ));
    let result = h.engine.place_order(order()).await.unwrap();
    assert_eq!(result.order.status, OrderStatus::Rejected);
    assert_eq!(
        reason(&result),
        "OKX 51121: Order quantity must be a multiple of the lot size"
    );
}

#[tokio::test]
async fn records_an_explicit_refusal_from_okx() {
    let h = connect_default().await;
    h.arm();
    h.okx.state().reject_next = Some(Refusal::new("51008", "Insufficient balance"));
    let result = h.engine.place_order(order()).await.unwrap();
    assert_eq!(result.order.status, OrderStatus::Rejected);
    assert_eq!(reason(&result), "OKX 51008: Insufficient balance");
    let row = h.store(|s| s.get_order(&result.order.id).unwrap().unwrap());
    assert_eq!(row.status, OrderStatus::Rejected);
}

/// An answer the client cannot read at all (Node's TypeError, Rust's `OkxError::Invalid`) is not a
/// refusal: the order may have reached OKX, so it stays OPEN and is never sent again.
#[tokio::test]
async fn an_unreadable_answer_to_an_order_keeps_it_open_and_never_resends() {
    let h = connect_default().await;
    h.arm();
    h.null_next_order.armed.store(true, Ordering::SeqCst);
    let result = h.engine.place_order(order()).await.unwrap();
    assert_eq!(result.order.status, OrderStatus::Open);
    assert_eq!(
        reason(&result),
        "Awaiting confirmation: Cannot read properties of null (reading 'code')"
    );
    h.engine.sync().await.unwrap();
    h.engine.sync().await.unwrap();
    let calls = h.okx.state().calls.clone();
    assert_eq!(
        calls
            .iter()
            .filter(|c| *c == "POST /api/v5/trade/order")
            .count(),
        0
    );
    let row = h.store(|s| s.get_order(&result.order.id).unwrap().unwrap());
    assert_eq!(row.status, OrderStatus::Open);
}

// ---- reads and the local rows -----------------------------------------------------------------

#[tokio::test]
async fn writes_the_order_row_before_sending_and_records_the_fill() {
    let h = connect_default().await;
    h.arm();
    let result = h.engine.place_order(order()).await.unwrap();
    let row = h.store(|s| s.get_order(&result.order.id).unwrap().unwrap());
    assert_eq!(row.status, OrderStatus::Filled);
    assert_eq!(row.exchange_order_id.as_deref(), Some("1"));
    assert_eq!(row.filled_quantity, "0.001");
    assert_eq!(row.avg_fill_price.as_deref(), Some("75000"));
    let fills = h.store(|s| s.list_fills_by_order(&result.order.id).unwrap());
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].id, "fill_okx_1");
    assert_eq!(fills[0].fee, "0.05");
    assert_eq!(fills[0].quantity, "0.001");
}

#[tokio::test]
async fn reads_positions_and_balance_from_okx() {
    let h = connect_default().await;
    h.arm();
    h.engine.place_order(order()).await.unwrap();
    let positions = h.engine.positions();
    assert_eq!(positions.len(), 1);
    let view = serde_json::to_value(&positions[0]).unwrap();
    assert_eq!(view["id"], "okx:BTC-USDT-SWAP");
    assert_eq!(view["side"], "LONG");
    assert_eq!(view["quantity"], "0.001");
    assert_eq!(view["entryPrice"], "75000");
    assert_eq!(view["leverage"], 3);
    assert_eq!(view["liquidationPrice"], "50000");
    assert_eq!(view["markPrice"], "75000");
    let balance = h.engine.balance().unwrap();
    assert_eq!(balance.equity, "1000");
    assert_eq!(balance.available_balance, "1000");
    assert_eq!(balance.fees_paid, "0.05");
    assert_eq!(balance.initial_balance, "1000");
    assert_eq!(h.engine.cached_balance(), Some(balance));
    let daily = h.engine.daily_risk(ACCOUNT).await.unwrap();
    assert_eq!(daily.daily_pnl, "0");
    assert_eq!(daily.day_start_equity, "1000");
    assert_eq!(h.engine.last_sync(), Some(START));
    assert!(h.engine.healthy());
    // Without a sync for over 30 s the account is no longer healthy.
    h.clock.fetch_add(30_001, Ordering::SeqCst);
    assert!(!h.engine.healthy());
}

#[tokio::test]
async fn cancels_an_open_order_and_refuses_unknown_or_closed_ones() {
    let h = connect_default().await;
    h.arm();
    let error = h.engine.cancel_order("nope").await.unwrap_err();
    assert_eq!(error.to_string(), "Unknown order nope");
    let filled = h.engine.place_order(order()).await.unwrap();
    let error = h.engine.cancel_order(&filled.order.id).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("Order {} is FILLED", filled.order.id)
    );
}

#[tokio::test]
async fn re_reads_live_limits_from_the_account_row_on_every_call() {
    let h = connect_default().await;
    h.arm();
    h.store(|s| {
        let mut row = s.get_account(ACCOUNT).unwrap().unwrap();
        row.live_limits = Some(RiskSettings {
            max_leverage: 2.0,
            ..default_live_limits()
        });
        s.connection()
            .execute(
                "UPDATE accounts SET live_limits=? WHERE id=?",
                (
                    serde_json::to_string(&row.live_limits).unwrap(),
                    ACCOUNT.to_string(),
                ),
            )
            .unwrap();
    });
    let result = h.engine.place_order(order()).await.unwrap();
    assert_eq!(reason(&result), "LIMIT_LEVERAGE: 3x exceeds 2x");
}
