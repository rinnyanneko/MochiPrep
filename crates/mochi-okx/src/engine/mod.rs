//! Trades through OKX: a port of `packages/exchanges/src/okx-engine.ts`. The exchange is the source
//! of truth for balances and positions; local rows record what MochiPrep asked for, so nothing here
//! pretends to be a ledger of the real account.
//!
//! The engine owns no database. Every read and write goes through [`StoreAccess`], which runs one
//! piece of work at a time against the store (in the server, on the runtime task), so network calls
//! never hold the store and the store never waits on the network.

mod protection;
mod refusal;
mod sync;
mod views;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mochi_core::paper::{DailyRisk, PaperStore, StoreFailure};
use mochi_decimal::Dec;
use mochi_domain::{
    default_live_limits, AccountBalance, EntryOrderType, EquityPoint, Fill,
    InstrumentSpecification, Order, OrderRequest, OrderResult, OrderSide, OrderStatus, OrderType,
    PaperAccount, PositionSide, PositionView, Protection, Quote, RiskSettings, TrailingKind,
    TrailingStopSpec,
};

use protection::{Miss, ProtectionMap};
use tokio::sync::oneshot;

use crate::client::{
    AttachAlgoOrd, BoxFuture, OkxClient, OkxError, OrdType, PlaceOrderBody, Side, TdMode, Venue,
};
use crate::ids::{to_algo_cl_ord_id, to_cl_ord_id, to_inst_id};

/// Longest gap since the last good sync before new exposure is refused.
const MAX_SNAPSHOT_AGE_MS: i64 = 30_000;
/// An order OKX still has no record of after this long never arrived.
const NEVER_ARRIVED_AFTER_MS: i64 = 60_000;
const EQUITY_EVERY_MS: i64 = 60_000;

/// The rows the engine reads and writes, which are the paper tables. `list_open_orders` must return
/// the account's OPEN orders oldest first (creation time, then insertion order).
pub trait LiveRows {
    fn get_account(&self, id: &str) -> Result<Option<PaperAccount>, StoreFailure>;
    fn insert_order(&self, order: &Order) -> Result<(), StoreFailure>;
    fn save_order(&self, order: &Order) -> Result<(), StoreFailure>;
    fn get_order(&self, id: &str) -> Result<Option<Order>, StoreFailure>;
    fn get_order_by_client_id(
        &self,
        account_id: &str,
        client_order_id: &str,
    ) -> Result<Option<Order>, StoreFailure>;
    fn list_open_orders(&self, account_id: &str) -> Result<Vec<Order>, StoreFailure>;
    fn list_orders(&self, account_id: &str, limit: usize) -> Result<Vec<Order>, StoreFailure>;
    fn insert_fill(&self, fill: &Fill) -> Result<(), StoreFailure>;
    fn list_fills_by_order(&self, order_id: &str) -> Result<Vec<Fill>, StoreFailure>;
    fn list_fills(&self, account_id: &str, limit: usize) -> Result<Vec<Fill>, StoreFailure>;
    fn upsert_equity_point(&self, point: &EquityPoint) -> Result<(), StoreFailure>;
    fn list_equity(&self, account_id: &str, limit: usize)
        -> Result<Vec<EquityPoint>, StoreFailure>;
    /// The instrument registry: built-ins first, then added pairs in the order registered
    /// (Node's `listSymbols()` order), so an instId maps back to the symbol Node would pick.
    fn instruments(&self) -> Vec<InstrumentSpecification>;
}

/// A paper store and the registry it trades, as [`LiveRows`].
pub struct Rows<'a, S: PaperStore> {
    pub store: &'a S,
    pub instruments: &'a [InstrumentSpecification],
}

impl<S: PaperStore> LiveRows for Rows<'_, S> {
    fn get_account(&self, id: &str) -> Result<Option<PaperAccount>, StoreFailure> {
        self.store.get_account(id)
    }
    fn insert_order(&self, order: &Order) -> Result<(), StoreFailure> {
        self.store.insert_order(order)
    }
    fn save_order(&self, order: &Order) -> Result<(), StoreFailure> {
        self.store.save_order(order)
    }
    fn get_order(&self, id: &str) -> Result<Option<Order>, StoreFailure> {
        self.store.get_order(id)
    }
    fn get_order_by_client_id(
        &self,
        account_id: &str,
        client_order_id: &str,
    ) -> Result<Option<Order>, StoreFailure> {
        self.store
            .get_order_by_client_id(account_id, client_order_id)
    }
    fn list_open_orders(&self, account_id: &str) -> Result<Vec<Order>, StoreFailure> {
        self.store.list_open_orders(account_id)
    }
    fn list_orders(&self, account_id: &str, limit: usize) -> Result<Vec<Order>, StoreFailure> {
        self.store.list_orders(account_id, limit)
    }
    fn insert_fill(&self, fill: &Fill) -> Result<(), StoreFailure> {
        self.store.insert_fill(fill)
    }
    fn list_fills_by_order(&self, order_id: &str) -> Result<Vec<Fill>, StoreFailure> {
        self.store.list_fills_by_order(order_id)
    }
    fn list_fills(&self, account_id: &str, limit: usize) -> Result<Vec<Fill>, StoreFailure> {
        self.store.list_fills(account_id, limit)
    }
    fn upsert_equity_point(&self, point: &EquityPoint) -> Result<(), StoreFailure> {
        self.store.upsert_equity_point(point)
    }
    fn list_equity(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<EquityPoint>, StoreFailure> {
        self.store.list_equity(account_id, limit)
    }
    fn instruments(&self) -> Vec<InstrumentSpecification> {
        self.instruments.to_vec()
    }
}

/// One piece of store work. It runs to completion with nothing else touching the store.
pub type StoreWork = Box<dyn FnOnce(&dyn LiveRows) + Send>;

/// Where the store lives. The future resolves once `work` has run, or once it never will (its
/// owner has stopped), in which case `work` is dropped unrun.
pub trait StoreAccess: Send + Sync {
    fn run(&self, work: StoreWork) -> BoxFuture<'static, ()>;
}

pub type QuoteSource = Arc<dyn Fn(&str) -> Option<Quote> + Send + Sync>;
pub type Now = Arc<dyn Fn() -> i64 + Send + Sync>;
pub type ArmedFlag = Arc<dyn Fn() -> bool + Send + Sync>;
pub type NewId = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Node's `OkxEngineDeps`. Live limits are not a dependency: they are re-read from the account
/// row on every call, as Node's `limits()` callback does.
pub struct OkxEngineDeps {
    pub account_id: String,
    pub venue: Venue,
    pub client: OkxClient,
    pub store: Arc<dyn StoreAccess>,
    pub get_quote: QuoteSource,
    pub now: Now,
    pub is_armed: ArmedFlag,
    /// `<prefix>_<unique>`; Node draws `crypto.randomUUID()`.
    pub new_id: NewId,
}

/// Why an engine call failed. `Display` is the message Node's thrown error carries.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// Node's plain `Error`s, such as `Unknown order <id>`.
    #[error("{0}")]
    Message(String),
    /// Node's `ProtectionChangeError`: a protection change refused part-way. `after` is what is
    /// in force, including legs already placed.
    #[error("{message}")]
    ProtectionChange {
        message: String,
        after: Box<Protection>,
    },
    #[error(transparent)]
    Okx(#[from] OkxError),
    #[error(transparent)]
    Store(#[from] StoreFailure),
    /// The store's owner stopped before the work could run.
    #[error("The backend is shutting down")]
    Stopped,
}

impl EngineError {
    fn message(text: impl Into<String>) -> Self {
        EngineError::Message(text.into())
    }
}

pub(crate) fn zero() -> Dec {
    Dec::parse("0").expect("0 is a decimal")
}

pub(crate) fn dec(text: &str) -> Result<Dec, EngineError> {
    // decimal.js's own text for a value it cannot read.
    Dec::parse(text)
        .map_err(|_| EngineError::message(format!("[DecimalError] Invalid argument: {text}")))
}

/// What the last sync read from OKX, and how it went.
#[derive(Default)]
struct State {
    balance: Option<AccountBalance>,
    positions: Vec<PositionView>,
    last_sync_at: Option<i64>,
    last_equity_at: i64,
    sync_error: Option<String>,
    /// Our protection per symbol, as the last sync read it back from OKX.
    protection: ProtectionMap,
    /// The last failed algo cancel, until a sync finishes with every cancel accepted.
    cancel_error: Option<String>,
    /// Per instId: consecutive syncs that found our protection but no position.
    missing_position: HashMap<String, Miss>,
    /// instIds where the last sync saw our algos live with no position: leftovers still being
    /// cancelled.
    leftovers: HashSet<String>,
}

/// One account's OKX trading. Cheap to share: callers hold it in an `Arc`.
pub struct OkxEngine {
    deps: OkxEngineDeps,
    state: Mutex<State>,
    /// Node's `serialized`: one operation at a time, in arrival order (tokio's mutex is fair), so
    /// two callers can never both pass the same exposure check. Sync takes the same queue.
    queue: tokio::sync::Mutex<()>,
}

/// The registry as one operation saw it.
pub(crate) struct Registry(Vec<InstrumentSpecification>);

impl Registry {
    /// Node's `getInstrument`.
    pub(crate) fn get(&self, symbol: &str) -> Result<&InstrumentSpecification, EngineError> {
        self.0
            .iter()
            .find(|spec| spec.symbol == symbol)
            .ok_or_else(|| EngineError::message(format!("Unknown instrument {symbol}")))
    }

    /// Node's `fromInstId`: unknown instIds are not ours to show.
    pub(crate) fn symbol_of_inst_id(&self, inst_id: &str) -> Option<&InstrumentSpecification> {
        self.0
            .iter()
            .find(|spec| to_inst_id(&spec.symbol) == inst_id)
    }
}

/// A protection leg row: closes the position, and is changed only through protection settings.
pub(crate) fn is_leg(order: &Order) -> bool {
    order.close_position
        && matches!(
            order.order_type,
            OrderType::StopMarket | OrderType::TakeProfitMarket | OrderType::TrailingStopMarket
        )
}

fn status_text(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::Open => "OPEN",
        OrderStatus::Filled => "FILLED",
        OrderStatus::Canceled => "CANCELED",
        OrderStatus::Rejected => "REJECTED",
    }
}

pub(crate) fn side_text(side: PositionSide) -> &'static str {
    match side {
        PositionSide::Long => "LONG",
        PositionSide::Short => "SHORT",
    }
}

/// What `submit` reads before deciding: everything Node reads synchronously at that point.
struct Prepared {
    prior: Option<(Order, Vec<Fill>)>,
    limits: RiskSettings,
    registry: Registry,
    open_orders: Vec<Order>,
    equity: Vec<EquityPoint>,
}

impl OkxEngine {
    pub fn new(deps: OkxEngineDeps) -> Arc<Self> {
        Arc::new(OkxEngine {
            deps,
            state: Mutex::new(State::default()),
            queue: tokio::sync::Mutex::new(()),
        })
    }

    pub fn account_id(&self) -> &str {
        &self.deps.account_id
    }

    pub fn venue(&self) -> Venue {
        self.deps.venue
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Nothing panics halfway through updating the state, so a poisoned lock is still whole.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> i64 {
        (self.deps.now)()
    }

    pub fn last_sync(&self) -> Option<i64> {
        self.state().last_sync_at
    }

    /// The sync failure, else the last failed algo cancel. Only the first makes the account
    /// unhealthy.
    pub fn error(&self) -> Option<String> {
        let state = self.state();
        state
            .sync_error
            .clone()
            .or_else(|| state.cancel_error.clone())
    }

    pub fn healthy(&self) -> bool {
        let now = self.now();
        let state = self.state();
        state.sync_error.is_none()
            && state
                .last_sync_at
                .is_some_and(|at| now - at <= MAX_SNAPSHOT_AGE_MS)
    }

    /// Runs `work` on the store and returns its answer.
    pub(crate) async fn rows<T: Send + 'static>(
        &self,
        work: impl FnOnce(&dyn LiveRows) -> Result<T, EngineError> + Send + 'static,
    ) -> Result<T, EngineError> {
        let (answer, answered) = oneshot::channel();
        self.deps
            .store
            .run(Box::new(move |rows| {
                let _ = answer.send(work(rows));
            }))
            .await;
        answered.await.map_err(|_| EngineError::Stopped)?
    }

    pub(crate) async fn registry(&self) -> Result<Registry, EngineError> {
        self.rows(|rows| Ok(Registry(rows.instruments()))).await
    }

    /// Runs one operation in the account's queue, on its own task, so it finishes even if the
    /// caller stops waiting: an order half sent must still be recorded.
    async fn queued<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(Arc<Self>) -> BoxFuture<'static, Result<T, EngineError>> + Send + 'static,
    ) -> Result<T, EngineError> {
        let engine = self.clone();
        tokio::spawn(async move {
            let _turn = engine.queue.lock().await;
            work(engine.clone()).await
        })
        .await
        .map_err(|_| EngineError::message("The OKX operation failed unexpectedly"))?
    }

    /// Node's `getBalance`.
    pub fn balance(&self) -> Result<AccountBalance, EngineError> {
        self.state()
            .balance
            .clone()
            .ok_or_else(|| EngineError::message("No balance from OKX yet"))
    }

    /// Last synced balance, for read models that must not fail while disconnected.
    pub fn cached_balance(&self) -> Option<AccountBalance> {
        self.state().balance.clone()
    }

    /// The positions OKX reported on the last sync.
    pub fn positions(&self) -> Vec<PositionView> {
        self.state().positions.clone()
    }

    pub async fn open_orders(&self, account_id: &str) -> Result<Vec<Order>, EngineError> {
        let account_id = account_id.to_string();
        self.rows(move |rows| Ok(rows.list_open_orders(&account_id)?))
            .await
    }

    /// Change since 00:00 UTC, for the daily-loss limit.
    pub async fn daily_risk(&self, account_id: &str) -> Result<DailyRisk, EngineError> {
        let balance = self.balance()?;
        let account_id = account_id.to_string();
        let equity = self
            .rows(move |rows| Ok(rows.list_equity(&account_id, 2000)?))
            .await?;
        self.daily_risk_of(&balance, &equity)
    }

    /// [`Self::daily_risk`] over equity points the caller already read (the latest 2000), for a
    /// caller that owns the store and cannot wait on it.
    pub fn daily_risk_from(&self, equity: &[EquityPoint]) -> Result<DailyRisk, EngineError> {
        self.daily_risk_of(&self.balance()?, equity)
    }

    fn daily_risk_of(
        &self,
        balance: &AccountBalance,
        equity: &[EquityPoint],
    ) -> Result<DailyRisk, EngineError> {
        let base = day_start_equity(equity, self.now(), &balance.equity);
        Ok(DailyRisk {
            daily_pnl: dec(&balance.equity)?.sub(&dec(&base)?).to_fixed(),
            day_start_equity: base,
        })
    }

    pub async fn place_order(
        self: &Arc<Self>,
        request: OrderRequest,
    ) -> Result<OrderResult, EngineError> {
        self.queued(move |engine| Box::pin(async move { engine.submit(request).await }))
            .await
    }

    /// Reduce-only market close of a fraction. Works even when disarmed: exits must always be
    /// possible.
    pub async fn close_position(
        self: &Arc<Self>,
        account_id: &str,
        symbol: &str,
        fraction: f64,
        client_order_id: &str,
    ) -> Result<OrderResult, EngineError> {
        let position = self
            .positions()
            .into_iter()
            .find(|p| p.position.symbol == symbol)
            .ok_or_else(|| EngineError::message(format!("No open {symbol} position")))?;
        let position = position.position;
        let held = dec(&position.quantity)?;
        let quantity = if fraction >= 1.0 {
            held
        } else {
            let registry = self.registry().await?;
            let step = dec(&registry.get(symbol)?.quantity_step)?;
            held.mul(&Dec::from_js_number(fraction))
                .floor_to_step(&step)
                .map_err(|_| EngineError::message("Invalid quantity step"))?
        };
        self.place_order(OrderRequest {
            account_id: account_id.to_string(),
            client_order_id: client_order_id.to_string(),
            symbol: symbol.to_string(),
            side: match position.side {
                PositionSide::Long => OrderSide::Sell,
                PositionSide::Short => OrderSide::Buy,
            },
            order_type: EntryOrderType::Market,
            quantity: quantity.to_fixed(),
            price: None,
            leverage: position.leverage,
            reduce_only: Some(true),
            stop_loss: None,
            take_profit: None,
            trailing_stop: None,
            agent_id: None,
            decision_id: None,
        })
        .await
    }

    pub async fn cancel_order(self: &Arc<Self>, order_id: &str) -> Result<(), EngineError> {
        let order_id = order_id.to_string();
        self.queued(move |engine| {
            Box::pin(async move {
                let id = order_id.clone();
                let (order, registry) = engine
                    .rows(move |rows| Ok((rows.get_order(&id)?, Registry(rows.instruments()))))
                    .await?;
                let mut order = order
                    .ok_or_else(|| EngineError::message(format!("Unknown order {order_id}")))?;
                if order.status != OrderStatus::Open {
                    return Err(EngineError::message(format!(
                        "Order {order_id} is {}",
                        status_text(order.status)
                    )));
                }
                // A protective algo is not an order on OKX, and removing one may need arming.
                if is_leg(&order) {
                    return Err(EngineError::message(
                        "Change protection on OKX through the protection settings",
                    ));
                }
                engine
                    .deps
                    .client
                    .cancel_order(
                        &to_inst_id(&order.symbol),
                        &to_cl_ord_id(&order.client_order_id),
                    )
                    .await?;
                engine.sync_order(&mut order, &registry).await
            })
        })
        .await
    }

    /// Full reconciliation with the exchange. Throws, and marks the account unhealthy, on failure.
    pub async fn sync(self: &Arc<Self>) -> Result<(), EngineError> {
        self.queued(|engine| Box::pin(async move { engine.sync_now().await }))
            .await
    }

    async fn prepare(&self, request: &OrderRequest) -> Result<Prepared, EngineError> {
        let (account_id, client_order_id) =
            (request.account_id.clone(), request.client_order_id.clone());
        let own = self.deps.account_id.clone();
        self.rows(move |rows| {
            let prior = match rows.get_order_by_client_id(&account_id, &client_order_id)? {
                Some(order) => {
                    let fills = rows.list_fills_by_order(&order.id)?;
                    Some((order, fills))
                }
                None => None,
            };
            let limits = rows
                .get_account(&own)?
                .and_then(|account| account.live_limits)
                .unwrap_or_else(default_live_limits);
            Ok(Prepared {
                prior,
                limits,
                registry: Registry(rows.instruments()),
                open_orders: rows.list_open_orders(&account_id)?,
                equity: rows.list_equity(&account_id, 2000)?,
            })
        })
        .await
    }

    async fn submit(&self, request: OrderRequest) -> Result<OrderResult, EngineError> {
        let prepared = self.prepare(&request).await?;
        if let Some((order, fills)) = prepared.prior {
            return Ok(OrderResult {
                order,
                fills,
                duplicate: true,
            });
        }
        let registry = &prepared.registry;
        let mut order = self.build_order(&request);
        align_to_tick(&mut order, registry)?;
        let position = self
            .positions()
            .into_iter()
            .find(|p| p.position.symbol == request.symbol);
        if order.reduce_only {
            if let Some(position) = &position {
                order.quantity = dec(&order.quantity)?
                    .min(&dec(&position.position.quantity)?)
                    .to_fixed();
            }
        }

        let refusal = self.pre_trade_refusal(&order, position.as_ref(), &prepared)?;
        let contracts = to_contracts(&order, registry)?;
        if refusal.is_some() || contracts.lte(&zero()) {
            let reason = refusal.unwrap_or_else(|| "INVALID_QUANTITY: below one lot".into());
            return self.reject(order, reason).await;
        }
        let spec = registry.get(&order.symbol)?;
        order.quantity = contracts.mul(&dec(&spec.venue_contract_value)?).to_fixed();

        // Written before anything is sent, so a crash can never leave an order we do not know
        // about.
        let row = order.clone();
        self.rows(move |rows| Ok(rows.insert_order(&row)?)).await?;

        let inst_id = to_inst_id(&order.symbol);
        let cl_ord_id = to_cl_ord_id(&order.client_order_id);
        let body = to_body(&order, &inst_id, &cl_ord_id, contracts.to_fixed());
        match self.send(&mut order, &inst_id, &body).await {
            Ok(()) => {}
            Err(EngineError::Okx(OkxError::Rejection { code, message })) => {
                let reason = OkxError::Rejection { code, message }.to_string();
                return self.finish_rejected(order, reason).await;
            }
            // Unknown outcome, or an answer that could not be read: the order stays OPEN and is
            // resolved by lookup. It is never resent.
            Err(error) => {
                order.reject_reason = Some(format!("Awaiting confirmation: {error}"));
                let row = order.clone();
                self.rows(move |rows| Ok(rows.save_order(&row)?)).await?;
            }
        }

        let _ = self.sync_order(&mut order, registry).await;
        let _ = self.sync_now().await;
        let id = order.id.clone();
        self.rows(move |rows| {
            let order = rows
                .get_order(&id)?
                .ok_or_else(|| EngineError::message(format!("Unknown order {id}")))?;
            let fills = rows.list_fills_by_order(&id)?;
            Ok(OrderResult {
                order,
                fills,
                duplicate: false,
            })
        })
        .await
    }

    /// Node's `try` block: leverage for an opening order, the order, and the saved ordId.
    async fn send(
        &self,
        order: &mut Order,
        inst_id: &str,
        body: &PlaceOrderBody,
    ) -> Result<(), EngineError> {
        let client = &self.deps.client;
        if !order.reduce_only {
            client.set_leverage(inst_id, order.leverage).await?;
        }
        order.exchange_order_id = Some(client.place_order(body).await?);
        let row = order.clone();
        self.rows(move |rows| Ok(rows.save_order(&row)?)).await
    }

    fn build_order(&self, request: &OrderRequest) -> Order {
        let now = self.now();
        Order {
            id: (self.deps.new_id)("ord"),
            account_id: request.account_id.clone(),
            client_order_id: request.client_order_id.clone(),
            symbol: request.symbol.clone(),
            side: request.side,
            order_type: match request.order_type {
                EntryOrderType::Market => OrderType::Market,
                EntryOrderType::Limit => OrderType::Limit,
            },
            quantity: request.quantity.clone(),
            price: request.price.clone(),
            trigger_price: None,
            leverage: request.leverage,
            reduce_only: request.reduce_only.unwrap_or(false),
            close_position: false,
            status: OrderStatus::Open,
            filled_quantity: "0".into(),
            avg_fill_price: None,
            reserved_margin: "0".into(),
            attached_stop_loss: request.stop_loss.clone(),
            attached_take_profit: request.take_profit.clone(),
            trailing_kind: None,
            trailing_callback: None,
            trailing_activation_price: None,
            trailing_extreme: None,
            attached_trailing_stop: request.trailing_stop.clone(),
            reject_reason: None,
            exchange_order_id: None,
            agent_id: request.agent_id.clone(),
            decision_id: request.decision_id.clone(),
            created_at: now,
            updated_at: now,
        }
    }

    /// A refused order is recorded, never an HTTP error.
    async fn reject(&self, mut order: Order, reason: String) -> Result<OrderResult, EngineError> {
        order.status = OrderStatus::Rejected;
        order.reject_reason = Some(reason);
        let row = order.clone();
        self.rows(move |rows| Ok(rows.insert_order(&row)?)).await?;
        Ok(OrderResult {
            order,
            fills: Vec::new(),
            duplicate: false,
        })
    }

    async fn finish_rejected(
        &self,
        mut order: Order,
        reason: String,
    ) -> Result<OrderResult, EngineError> {
        order.status = OrderStatus::Rejected;
        order.reject_reason = Some(reason);
        order.updated_at = self.now();
        let row = order.clone();
        self.rows(move |rows| Ok(rows.save_order(&row)?)).await?;
        Ok(OrderResult {
            order,
            fills: Vec::new(),
            duplicate: false,
        })
    }
}

/// The first equity point of the UTC day, else the current equity.
pub(crate) fn day_start_equity(points: &[EquityPoint], now: i64, equity: &str) -> String {
    let day_start = now.div_euclid(86_400_000) * 86_400_000;
    points
        .iter()
        .find(|p| p.ts >= day_start)
        .map_or_else(|| equity.to_string(), |p| p.equity.clone())
}

fn step_error(_: mochi_decimal::DecError) -> EngineError {
    EngineError::message("[DecimalError] Division by zero")
}

/// Node's aligner: `price ? roundToStep(d(price), priceTick).toFixed() : null`, so an empty
/// string becomes null as well.
fn align(price: &Option<String>, tick: &Dec) -> Result<Option<String>, EngineError> {
    price
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(|p| Ok(dec(p)?.round_to_step(tick).map_err(step_error)?.to_fixed()))
        .transpose()
}

/// OKX refuses prices off the tick, and models rarely produce them on it.
fn align_to_tick(order: &mut Order, registry: &Registry) -> Result<(), EngineError> {
    let tick = dec(&registry.get(&order.symbol)?.price_tick)?;
    order.price = align(&order.price, &tick)?;
    order.attached_stop_loss = align(&order.attached_stop_loss, &tick)?;
    order.attached_take_profit = align(&order.attached_take_profit, &tick)?;
    if let Some(spec) = order.attached_trailing_stop.take() {
        order.attached_trailing_stop = Some(align_trailing(spec, &tick)?);
    }
    Ok(())
}

fn align_trailing(spec: TrailingStopSpec, tick: &Dec) -> Result<TrailingStopSpec, EngineError> {
    let callback = match spec.kind {
        TrailingKind::Spread => align(&Some(spec.callback), tick)?.unwrap_or_default(),
        TrailingKind::Ratio => spec.callback,
    };
    Ok(TrailingStopSpec {
        kind: spec.kind,
        callback,
        activation_price: align(&spec.activation_price, tick)?,
    })
}

/// Base units to whole lots of contracts, rounded down.
fn to_contracts(order: &Order, registry: &Registry) -> Result<Dec, EngineError> {
    let spec = registry.get(&order.symbol)?;
    dec(&order.quantity)?
        .div(&dec(&spec.venue_contract_value)?)
        .map_err(step_error)?
        .floor_to_step(&dec(&spec.venue_lot_size)?)
        .map_err(step_error)
}

/// The trailing fields OKX takes: exactly one callback, and the activation price when set.
fn trailing_fields(spec: &TrailingStopSpec, leg: &mut AttachAlgoOrd) {
    match spec.kind {
        TrailingKind::Ratio => leg.callback_ratio = Some(spec.callback.clone()),
        TrailingKind::Spread => leg.callback_spread = Some(spec.callback.clone()),
    }
    leg.active_px = spec.activation_price.clone().filter(|p| !p.is_empty());
}

/// One attachAlgoOrds element per leg, each tagged with the id of the local row that will track it
/// once the entry fills (`<order id>:sl`, `:tp`, `:trail`), so every attached algo can be found by
/// its algoClOrdId. Whether OKX keeps separate SL and TP elements apart is UNCONFIRMED (Demo
/// check).
fn to_body(order: &Order, inst_id: &str, cl_ord_id: &str, sz: String) -> PlaceOrderBody {
    let mut algo = Vec::new();
    if !order.reduce_only {
        let tag = |leg: &str| Some(to_algo_cl_ord_id(&format!("{}:{leg}", order.id)));
        // Node's truthiness: an empty price attaches nothing.
        if let Some(stop) = order.attached_stop_loss.clone().filter(|p| !p.is_empty()) {
            algo.push(AttachAlgoOrd {
                attach_algo_cl_ord_id: tag("sl"),
                sl_trigger_px: Some(stop),
                sl_ord_px: Some("-1".into()),
                ..Default::default()
            });
        }
        if let Some(target) = order.attached_take_profit.clone().filter(|p| !p.is_empty()) {
            algo.push(AttachAlgoOrd {
                attach_algo_cl_ord_id: tag("tp"),
                tp_trigger_px: Some(target),
                tp_ord_px: Some("-1".into()),
                ..Default::default()
            });
        }
        if let Some(spec) = &order.attached_trailing_stop {
            let mut leg = AttachAlgoOrd {
                attach_algo_cl_ord_id: tag("trail"),
                ..Default::default()
            };
            trailing_fields(spec, &mut leg);
            algo.push(leg);
        }
    }
    let limit = order.order_type == OrderType::Limit;
    PlaceOrderBody {
        inst_id: inst_id.to_string(),
        td_mode: TdMode::Isolated,
        side: match order.side {
            OrderSide::Buy => Side::Buy,
            OrderSide::Sell => Side::Sell,
        },
        ord_type: if limit {
            OrdType::Limit
        } else {
            OrdType::Market
        },
        sz,
        cl_ord_id: cl_ord_id.to_string(),
        px: if limit { order.price.clone() } else { None },
        reduce_only: order.reduce_only.then_some(true),
        attach_algo_ords: (!algo.is_empty()).then_some(algo),
    }
}
