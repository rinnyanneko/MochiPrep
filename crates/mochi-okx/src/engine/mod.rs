//! Trades through OKX: a port of `packages/exchanges/src/okx-engine.ts`. The exchange is the source
//! of truth for balances and positions; local rows record what MochiPrep asked for, so nothing here
//! pretends to be a ledger of the real account.
//!
//! The engine owns no database. Every read and write goes through [`StoreAccess`], which runs one
//! piece of work at a time against the store (in the server, on the runtime task), so network calls
//! never hold the store and the store never waits on the network.

mod history;
pub use history::OpenPositionResult;
mod protection;
pub use protection::{CheckedProtection, ProtectionOrderOptions, ProtectionRetention};
mod refusal;
mod sync;
mod views;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mochi_core::paper::{CloseAmount, CloseSelection, DailyRisk, PaperStore, StoreFailure};
use mochi_decimal::Dec;
use mochi_domain::{
    default_live_limits, AccountBalance, EntryOrderType, EquityPoint, ExchangeExecutionCursor,
    ExchangePositionRecord, Fill, InstrumentSpecification, Order, OrderRequest, OrderResult,
    OrderSide, OrderStatus, OrderType, PaperAccount, PositionSide, PositionView, Protection, Quote,
    RiskSettings, TrailingKind, TrailingStopSpec,
};

use protection::{Held, Miss, ProtectionMap};
use tokio::sync::oneshot;

use crate::client::{
    AttachAlgoOrd, BoxFuture, OkxClient, OkxError, OrdType, PlaceOrderBody, Row, Side, TdMode,
    Venue,
};
use crate::ids::{to_algo_cl_ord_id, to_cl_ord_id, to_inst_id};

/// Longest gap since the last good sync before new exposure is refused.
const MAX_SNAPSHOT_AGE_MS: i64 = 30_000;
/// An order OKX still has no record of after this long never arrived.
const NEVER_ARRIVED_AFTER_MS: i64 = 60_000;
/// Why a readback closes an order OKX has no record of.
pub(super) const ORDER_NOT_ARRIVED: &str = "OKX has no record of this order";
const EQUITY_EVERY_MS: i64 = 60_000;

/// The rows the engine reads and writes, which are the paper tables. `list_open_orders` must return
/// the account's OPEN orders oldest first (creation time, then insertion order).
pub trait LiveRows {
    /// Runs `work` atomically: an `Err` rolls back everything it wrote.
    fn transaction(
        &self,
        work: &mut dyn FnMut() -> Result<(), EngineError>,
    ) -> Result<(), EngineError>;
    fn get_account(&self, id: &str) -> Result<Option<PaperAccount>, StoreFailure>;
    fn insert_order(&self, order: &Order) -> Result<(), StoreFailure>;
    fn save_order(&self, order: &Order) -> Result<(), StoreFailure>;
    fn get_order(&self, id: &str) -> Result<Option<Order>, StoreFailure>;
    fn record_close_selection(
        &self,
        order_id: &str,
        selection: &CloseSelection,
    ) -> Result<(), StoreFailure>;
    fn close_selection(&self, order_id: &str) -> Result<Option<CloseSelection>, StoreFailure>;
    fn link_target_replacement(
        &self,
        deferred_order_id: &str,
        replacement_order_id: &str,
    ) -> Result<(), StoreFailure>;
    fn target_replacements(&self, deferred_order_id: &str) -> Result<Vec<Order>, StoreFailure>;
    fn get_order_by_client_id(
        &self,
        account_id: &str,
        client_order_id: &str,
    ) -> Result<Option<Order>, StoreFailure>;
    fn list_open_orders(&self, account_id: &str) -> Result<Vec<Order>, StoreFailure>;
    fn list_orders(&self, account_id: &str, limit: usize) -> Result<Vec<Order>, StoreFailure>;
    fn list_canceled_orders_for_recovery(
        &self,
        account_id: &str,
        reason: &str,
        limit: usize,
    ) -> Result<Vec<Order>, StoreFailure>;
    fn any_canceled_order_for_recovery(
        &self,
        account_id: &str,
        symbol: &str,
        side: OrderSide,
        reason: &str,
        matches: &dyn Fn(&Order) -> bool,
    ) -> Result<bool, StoreFailure>;
    fn mark_order_recovery_checked(
        &self,
        order_id: &str,
        reason: &str,
        checked_at: i64,
    ) -> Result<(), StoreFailure>;
    /// The account's orders carrying any of these exchange order ids, however old.
    fn find_orders_by_exchange_ids(
        &self,
        account_id: &str,
        exchange_order_ids: &[String],
    ) -> Result<Vec<Order>, StoreFailure>;
    fn insert_fill(&self, fill: &Fill) -> Result<(), StoreFailure>;
    fn list_fills_by_order(&self, order_id: &str) -> Result<Vec<Fill>, StoreFailure>;
    fn list_fills(&self, account_id: &str, limit: usize) -> Result<Vec<Fill>, StoreFailure>;
    fn upsert_equity_point(&self, point: &EquityPoint) -> Result<(), StoreFailure>;
    fn list_equity(&self, account_id: &str, limit: usize)
        -> Result<Vec<EquityPoint>, StoreFailure>;
    fn execution_cursor(&self, account_id: &str) -> Result<ExchangeExecutionCursor, StoreFailure>;
    fn save_execution_cursor(
        &self,
        account_id: &str,
        cursor: &ExchangeExecutionCursor,
    ) -> Result<(), StoreFailure>;
    fn record_exchange_execution(
        &self,
        order_id: &str,
        trade_id: &str,
        executed_at: i64,
    ) -> Result<(), StoreFailure>;
    fn upsert_position_record(&self, record: &ExchangePositionRecord) -> Result<(), StoreFailure>;
    fn latest_position_record_at(&self, account_id: &str) -> Result<Option<i64>, StoreFailure>;
    fn position_history_backfill(&self, account_id: &str) -> Result<Option<i64>, StoreFailure>;
    fn set_position_history_backfill(
        &self,
        account_id: &str,
        before: Option<i64>,
    ) -> Result<(), StoreFailure>;
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
    fn transaction(
        &self,
        work: &mut dyn FnMut() -> Result<(), EngineError>,
    ) -> Result<(), EngineError> {
        self.store.transaction(work)
    }
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
    fn record_close_selection(
        &self,
        order_id: &str,
        selection: &CloseSelection,
    ) -> Result<(), StoreFailure> {
        self.store.record_close_selection(order_id, selection)
    }
    fn close_selection(&self, order_id: &str) -> Result<Option<CloseSelection>, StoreFailure> {
        self.store.close_selection(order_id)
    }
    fn link_target_replacement(
        &self,
        deferred_order_id: &str,
        replacement_order_id: &str,
    ) -> Result<(), StoreFailure> {
        self.store
            .link_target_replacement(deferred_order_id, replacement_order_id)
    }
    fn target_replacements(&self, deferred_order_id: &str) -> Result<Vec<Order>, StoreFailure> {
        self.store.target_replacements(deferred_order_id)
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
    fn list_canceled_orders_for_recovery(
        &self,
        account_id: &str,
        reason: &str,
        limit: usize,
    ) -> Result<Vec<Order>, StoreFailure> {
        self.store
            .list_canceled_orders_for_recovery(account_id, reason, limit)
    }
    fn any_canceled_order_for_recovery(
        &self,
        account_id: &str,
        symbol: &str,
        side: OrderSide,
        reason: &str,
        matches: &dyn Fn(&Order) -> bool,
    ) -> Result<bool, StoreFailure> {
        self.store
            .any_canceled_order_for_recovery(account_id, symbol, side, reason, matches)
    }
    fn mark_order_recovery_checked(
        &self,
        order_id: &str,
        reason: &str,
        checked_at: i64,
    ) -> Result<(), StoreFailure> {
        self.store
            .mark_order_recovery_checked(order_id, reason, checked_at)
    }
    fn find_orders_by_exchange_ids(
        &self,
        account_id: &str,
        exchange_order_ids: &[String],
    ) -> Result<Vec<Order>, StoreFailure> {
        self.store
            .find_orders_by_exchange_ids(account_id, exchange_order_ids)
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
    fn execution_cursor(&self, account_id: &str) -> Result<ExchangeExecutionCursor, StoreFailure> {
        self.store.execution_cursor(account_id)
    }
    fn save_execution_cursor(
        &self,
        account_id: &str,
        cursor: &ExchangeExecutionCursor,
    ) -> Result<(), StoreFailure> {
        self.store.save_execution_cursor(account_id, cursor)
    }
    fn record_exchange_execution(
        &self,
        order_id: &str,
        trade_id: &str,
        executed_at: i64,
    ) -> Result<(), StoreFailure> {
        self.store
            .record_exchange_execution(order_id, trade_id, executed_at)
    }
    fn upsert_position_record(&self, record: &ExchangePositionRecord) -> Result<(), StoreFailure> {
        self.store.upsert_position_record(record)
    }
    fn latest_position_record_at(&self, account_id: &str) -> Result<Option<i64>, StoreFailure> {
        self.store.latest_position_record_at(account_id)
    }
    fn position_history_backfill(&self, account_id: &str) -> Result<Option<i64>, StoreFailure> {
        self.store.position_history_backfill(account_id)
    }
    fn set_position_history_backfill(
        &self,
        account_id: &str,
        before: Option<i64>,
    ) -> Result<(), StoreFailure> {
        self.store.set_position_history_backfill(account_id, before)
    }
    fn instruments(&self) -> Vec<InstrumentSpecification> {
        self.instruments.to_vec()
    }
}

/// One piece of store work. It runs to completion with nothing else touching the store.
pub type StoreWork = Box<dyn FnOnce(&dyn LiveRows) + Send>;

/// Where the store lives. The future resolves once `work` has run, or once it never will (its
/// owner has stopped, or refuses this engine's writes), in which case `work` is dropped unrun.
pub trait StoreAccess: Send + Sync {
    fn run(&self, work: StoreWork) -> BoxFuture<'static, ()>;

    /// Why work was dropped unrun, when it was not because the owner stopped.
    fn refusal(&self) -> Option<String> {
        None
    }
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
    /// Node's `ProtectionChangeError`: a protection change refused part-way. `after` is set
    /// only when the protection in force was confirmed after the refusal.
    #[error("{message}")]
    ProtectionChange {
        message: String,
        after: Option<Box<Protection>>,
    },
    #[error(transparent)]
    Okx(#[from] OkxError),
    #[error(transparent)]
    Store(#[from] StoreFailure),
    /// The store's owner stopped before the work could run.
    #[error("The backend is shutting down")]
    Stopped,
    #[error("The OKX session was replaced or disconnected before order dispatch")]
    SessionRetired,
    #[error("NOT_ARMED: real-money orders are switched off for this account")]
    OpeningDisarmed,
    #[error(transparent)]
    LeverageSetup(OkxError),
    /// A selected-position reduction refused before anything reached OKX, with any
    /// staged row already recorded as rejected. The caller may correct and resubmit
    /// under a new client order ID.
    #[error("{0}")]
    RefusedBeforeDispatch(String),
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
    /// When the closed-position history was last read.
    last_history_at: i64,
    /// A history pass is out.
    history_running: bool,
    open_results: Vec<OpenPositionResult>,
    sync_error: Option<String>,
    /// Our protection per symbol, as the last sync read it back from OKX.
    protection: ProtectionMap,
    /// Exchange trade and side that the last observed protection belonged to.
    protection_keys: HashMap<String, String>,
    /// Per instrument, the last position whose size was reconciled with its entry fills.
    confirmed_entry_positions: HashMap<String, Held>,
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
    dispatch: Arc<Mutex<DispatchState>>,
    /// Node's `serialized`: one operation at a time, in arrival order (tokio's mutex is fair), so
    /// two callers can never both pass the same exposure check. Sync takes the same queue.
    queue: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct DispatchState {
    retired: bool,
    staged_order: Option<UnsentOrder>,
    restore_order: Option<UnsentOrder>,
    /// Submissions refused before dispatch whose rejection could not be written yet.
    /// Kept apart from `restore_order`, so they never wait on a deferred restore.
    unsent_rejections: Vec<UnsentOrder>,
}

enum DeferredRestore {
    Required,
    AllowPending,
}

/// In-memory proof of non-dispatch for a staged order that needs finalizing.
pub enum UnsentOrder {
    Submission(String),
    /// A submission refused before dispatch, with the refusal to record.
    Rejection {
        id: String,
        reason: String,
    },
    Deferred {
        original: Box<Order>,
        marker: Box<Order>,
    },
}

impl UnsentOrder {
    fn id(&self) -> &str {
        match self {
            Self::Submission(id) | Self::Rejection { id, .. } => id,
            Self::Deferred { original, .. } => &original.id,
        }
    }

    pub fn finalize(
        &self,
        rows: &dyn LiveRows,
        account_id: &str,
        now: i64,
    ) -> Result<Option<Order>, EngineError> {
        let mut current = None;
        rows.transaction(&mut || {
            if let Some(mut order) = rows
                .get_order(self.id())?
                .filter(|order| order.account_id == account_id)
            {
                if self.restore(&mut order, now) {
                    rows.save_order(&order)?;
                }
                current = Some(order);
            }
            Ok(())
        })?;
        Ok(current)
    }

    fn restore(&self, order: &mut Order, now: i64) -> bool {
        if order
            .exchange_order_id
            .as_deref()
            .is_some_and(|id| !id.is_empty())
        {
            return false;
        }
        match self {
            Self::Submission(_) if order.status == OrderStatus::Open => {
                order.status = OrderStatus::Rejected;
                order.reject_reason = Some(EngineError::SessionRetired.to_string());
                order.updated_at = now;
            }
            // A connection candidate cannot see this proof, so its readback may already
            // have expired the never-sent order. The refusal is the accurate record.
            Self::Rejection { reason, .. }
                if order.status == OrderStatus::Open
                    || (order.status == OrderStatus::Canceled
                        && order.reject_reason.as_deref() == Some(ORDER_NOT_ARRIVED)) =>
            {
                order.status = OrderStatus::Rejected;
                order.reject_reason = Some(reason.clone());
                order.updated_at = now;
            }
            Self::Deferred { original, marker } => {
                if *order != **marker {
                    // A connection's readback may already have expired this marker.
                    // Confirm it is still the same attempt before restoring its intent.
                    let mut expired = marker.as_ref().clone();
                    expired.status = OrderStatus::Canceled;
                    expired.reject_reason = Some(if marker.order_type == OrderType::Market {
                        protection::REACHED_EXIT_UNCONFIRMED.into()
                    } else {
                        protection::ALGO_NOT_ARRIVED.into()
                    });
                    expired.updated_at = order.updated_at;
                    if *order != expired {
                        return false;
                    }
                }
                order.clone_from(original);
            }
            _ => return false,
        }
        true
    }
}

impl DispatchState {
    fn check(&self) -> Result<(), EngineError> {
        if self.retired {
            return Err(EngineError::SessionRetired);
        }
        Ok(())
    }
}

struct StagedOrder<'a>(&'a Mutex<DispatchState>);

impl Drop for StagedOrder<'_> {
    fn drop(&mut self) {
        let mut dispatch = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        dispatch.staged_order = None;
    }
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

struct SelectedClose {
    id: String,
    side: PositionSide,
    contracts: Dec,
    held: Dec,
    selection: CloseSelection,
}

struct CloseIntent {
    account_id: String,
    symbol: String,
    fraction: f64,
    client_order_id: String,
    position_id: Option<String>,
    agent_id: Option<String>,
    decision_id: Option<String>,
}

impl Prepared {
    fn take_duplicate(&mut self) -> Option<OrderResult> {
        let (order, fills) = self.prior.take()?;
        Some(OrderResult {
            order,
            fills,
            duplicate: true,
        })
    }
}

impl OkxEngine {
    pub fn new(deps: OkxEngineDeps) -> Arc<Self> {
        Arc::new(OkxEngine {
            deps,
            state: Mutex::new(State::default()),
            dispatch: Arc::new(Mutex::new(DispatchState::default())),
            queue: tokio::sync::Mutex::new(()),
        })
    }

    /// Stops new dispatches and returns the orders that never reached handoff.
    pub fn retire(&self) -> Vec<UnsentOrder> {
        let mut dispatch = self.dispatch();
        dispatch.retired = true;
        // Finalize the current submission first if an older restore keeps failing.
        let mut unsent: Vec<UnsentOrder> =
            [dispatch.staged_order.take(), dispatch.restore_order.take()]
                .into_iter()
                .flatten()
                .collect();
        unsent.append(&mut dispatch.unsent_rejections);
        unsent
    }

    pub(super) fn check_dispatch(&self) -> Result<(), EngineError> {
        // Retirement before this handoff forbids a mutation. After handoff,
        // a request with an unknown outcome must be resolved by lookup.
        self.dispatch().check()
    }

    fn dispatch(&self) -> MutexGuard<'_, DispatchState> {
        self.dispatch.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stage_order(&self, order: UnsentOrder) -> Result<StagedOrder<'_>, EngineError> {
        let mut dispatch = self.dispatch();
        dispatch.check()?;
        // The account's queue allows at most one submission at a time.
        debug_assert!(dispatch.staged_order.is_none());
        dispatch.staged_order = Some(order);
        Ok(StagedOrder(&self.dispatch))
    }

    fn abandon_staged_order(&self) -> Result<(), EngineError> {
        let mut dispatch = self.dispatch();
        dispatch.check()?;
        debug_assert!(dispatch.restore_order.is_none());
        dispatch.restore_order = dispatch.staged_order.take();
        Ok(())
    }

    /// Whether `id` was refused before dispatch and its rejection is still unwritten.
    pub(super) fn is_unsent_rejection(&self, id: &str) -> bool {
        self.dispatch()
            .unsent_rejections
            .iter()
            .any(|pending| pending.id() == id)
    }

    /// Writes the rejections held in memory. Each one that still fails stays held.
    pub(super) async fn finalize_unsent_rejections(&self) -> Result<(), EngineError> {
        if self.dispatch().unsent_rejections.is_empty() {
            return Ok(());
        }
        let dispatch = self.dispatch.clone();
        let account_id = self.deps.account_id.clone();
        let now = self.now();
        self.rows(move |rows| {
            let mut dispatch = dispatch.lock().unwrap_or_else(PoisonError::into_inner);
            let mut failure = None;
            for pending in std::mem::take(&mut dispatch.unsent_rejections) {
                if let Err(error) = pending.finalize(rows, &account_id, now) {
                    failure.get_or_insert(error);
                    dispatch.unsent_rejections.push(pending);
                }
            }
            failure.map_or(Ok(()), Err)
        })
        .await
    }

    async fn restore_unsent_order(&self) -> Result<Option<Order>, EngineError> {
        if self.dispatch().restore_order.is_none() {
            return Ok(None);
        }
        let dispatch = self.dispatch.clone();
        let account_id = self.deps.account_id.clone();
        let now = self.now();
        self.rows(move |rows| {
            let mut dispatch = dispatch.lock().unwrap_or_else(PoisonError::into_inner);
            dispatch.check()?;
            let Some(pending) = dispatch.restore_order.as_ref() else {
                return Ok(None);
            };
            let restored = pending.finalize(rows, &account_id, now)?;
            // A failed transaction leaves the token for the next queued operation or retirement.
            dispatch.restore_order = None;
            Ok(restored)
        })
        .await
    }

    fn handoff_order(&self, order: &Order) -> Result<(), EngineError> {
        let mut dispatch = self.dispatch();
        dispatch.check()?;
        if !order.reduce_only && !(self.deps.is_armed)() {
            return Err(EngineError::OpeningDisarmed);
        }
        if dispatch
            .staged_order
            .as_ref()
            .is_some_and(|pending| pending.id() == order.id)
        {
            dispatch.staged_order = None;
        }
        Ok(())
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
        answered.await.map_err(|_| {
            self.deps
                .store
                .refusal()
                .map_or(EngineError::Stopped, EngineError::Message)
        })?
    }

    pub(crate) async fn registry(&self) -> Result<Registry, EngineError> {
        self.rows(|rows| Ok(Registry(rows.instruments()))).await
    }

    /// Runs one operation in the account's queue, on its own task, so it finishes even if the
    /// caller stops waiting: an order half sent must still be recorded.
    async fn queued<T: Send + 'static>(
        self: &Arc<Self>,
        restore: DeferredRestore,
        work: impl FnOnce(Arc<Self>) -> BoxFuture<'static, Result<T, EngineError>> + Send + 'static,
    ) -> Result<T, EngineError> {
        let engine = self.clone();
        tokio::spawn(async move {
            let _turn = engine.queue.lock().await;
            // A held rejection is retried on every turn. While it fails, sync skips its
            // order and an exact retry still sees it open, never sent.
            let _ = engine.finalize_unsent_rejections().await;
            if matches!(restore, DeferredRestore::Required) {
                engine.restore_unsent_order().await?;
            }
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
        self.queued(DeferredRestore::Required, move |engine| {
            Box::pin(async move {
                let mut prepared = engine
                    .prepare(&request.account_id, &request.client_order_id)
                    .await?;
                if let Some(prior) = prepared.take_duplicate() {
                    if request.reduce_only == Some(true) {
                        let order_id = prior.order.id.clone();
                        let stored = engine
                            .rows(move |rows| Ok(rows.close_selection(&order_id)?))
                            .await?;
                        if stored.is_some_and(|stored| {
                            matches!(stored.amount, CloseAmount::Quantity { .. })
                        }) {
                            // The key already names a selected reduction, so this is not
                            // a refusal before dispatch.
                            return Err(EngineError::message(CLOSE_CHANGED));
                        }
                    }
                    return Ok(prior);
                }
                if request.reduce_only == Some(true) {
                    return Err(EngineError::message(
                        "Select an OKX position for this reduce-only order",
                    ));
                }
                engine.submit(request, None, prepared).await
            })
        })
        .await
    }

    pub async fn reduce_position(
        self: &Arc<Self>,
        request: OrderRequest,
        position_id: String,
    ) -> Result<OrderResult, EngineError> {
        self.queued(DeferredRestore::AllowPending, move |engine| {
            Box::pin(async move {
                let mut request = request;
                if request.reduce_only != Some(true)
                    || request.stop_loss.is_some()
                    || request.take_profit.is_some()
                    || request.trailing_stop.is_some()
                {
                    return Err(EngineError::message(
                        "A selected position requires a reduce-only order without attached protection",
                    ));
                }
                let limit_price = if request.order_type == EntryOrderType::Limit {
                    let price = request.price.as_deref().ok_or_else(|| {
                        EngineError::message("A reduce-only limit order requires a price")
                    })?;
                    let value = dec(price)?;
                    if !value.gt(&zero()) {
                        return Err(EngineError::message("Limit price must be positive"));
                    }
                    Some(value)
                } else {
                    None
                };
                request.price = limit_price.as_ref().map(Dec::to_fixed);
                let quantity = dec(&request.quantity)?;
                let selection = CloseSelection {
                    position_id: Some(position_id),
                    amount: CloseAmount::Quantity {
                        quantity: quantity.to_fixed(),
                        limit_price: request.price.clone(),
                    },
                };
                let mut prepared = engine.prepare(&request.account_id, &request.client_order_id).await?;
                if let Some(prior) = prepared.take_duplicate() {
                    if prior.order.side != request.side {
                        return Err(close_retry_changed());
                    }
                    return engine.check_close_retry(prior, &request.symbol, &selection).await;
                }
                if let Some(price) = limit_price {
                    let tick = &prepared.registry.get(&request.symbol)?.price_tick;
                    let aligned = price.round_to_step(&dec(tick)?).map_err(step_error)?;
                    if !price.eq_value(&aligned) {
                        return Err(EngineError::message(format!(
                            "Reduction limit price must be a multiple of {tick}"
                        )));
                    }
                }
                let (position, leverage) = engine
                    .read_selected_close(&request.symbol, selection, &prepared.registry)
                    .await?;
                if !quantity.gt(&zero()) || quantity.gt(&position.held) {
                    return Err(EngineError::RefusedBeforeDispatch(
                        "Reduction quantity must be positive and cannot exceed the selected position"
                            .into(),
                    ));
                }
                request.leverage = leverage;
                engine.submit(request, Some(position), prepared).await
            })
        })
        .await
    }

    /// Reduce-only market close of a fraction. Works even when disarmed: exits must always be
    /// possible. The fraction is sized inside the account's queue from the position as it is
    /// then, so two overlapping half closes leave a quarter, not nothing.
    pub async fn close_position(
        self: &Arc<Self>,
        account_id: &str,
        symbol: &str,
        fraction: f64,
        client_order_id: &str,
    ) -> Result<OrderResult, EngineError> {
        self.close_selected_position(account_id, symbol, fraction, client_order_id, None)
            .await
    }

    pub async fn close_selected_position(
        self: &Arc<Self>,
        account_id: &str,
        symbol: &str,
        fraction: f64,
        client_order_id: &str,
        position_id: Option<&str>,
    ) -> Result<OrderResult, EngineError> {
        self.dispatch_close(CloseIntent {
            account_id: account_id.to_owned(),
            symbol: symbol.to_owned(),
            fraction,
            client_order_id: client_order_id.to_owned(),
            position_id: position_id.map(str::to_owned),
            agent_id: None,
            decision_id: None,
        })
        .await
    }

    pub async fn close_agent_position(
        self: &Arc<Self>,
        request: OrderRequest,
        position_id: String,
    ) -> Result<OrderResult, EngineError> {
        self.dispatch_close(CloseIntent {
            account_id: request.account_id,
            symbol: request.symbol,
            fraction: 1.0,
            client_order_id: request.client_order_id,
            position_id: Some(position_id),
            agent_id: request.agent_id,
            decision_id: request.decision_id,
        })
        .await
    }

    async fn dispatch_close(
        self: &Arc<Self>,
        intent: CloseIntent,
    ) -> Result<OrderResult, EngineError> {
        self.queued(DeferredRestore::AllowPending, move |engine| {
            Box::pin(async move {
                let CloseIntent {
                    account_id,
                    symbol,
                    fraction,
                    client_order_id,
                    position_id,
                    agent_id,
                    decision_id,
                } = intent;
                let mut prepared = engine.prepare(&account_id, &client_order_id).await?;
                if let Some(prior) = prepared.take_duplicate() {
                    let selection = CloseSelection {
                        position_id,
                        amount: CloseAmount::Fraction(fraction),
                    };
                    return engine.check_close_retry(prior, &symbol, &selection).await;
                }
                let (mut request, position) = engine
                    .close_request(
                        account_id,
                        symbol,
                        fraction,
                        client_order_id,
                        position_id.as_deref(),
                    )
                    .await?;
                request.agent_id = agent_id;
                request.decision_id = decision_id;
                engine.submit(request, Some(position), prepared).await
            })
        })
        .await
    }

    async fn check_close_retry(
        &self,
        prior: OrderResult,
        symbol: &str,
        selection: &CloseSelection,
    ) -> Result<OrderResult, EngineError> {
        let order_id = prior.order.id.clone();
        let stored = self
            .rows(move |rows| Ok(rows.close_selection(&order_id)?))
            .await?;
        // Preserve same-symbol fraction retries from before their inputs were stored. A
        // request bound to a position needs a stored selection: without one it cannot
        // tell whether the earlier close reduced the position it names.
        let matches = stored.as_ref().map_or_else(
            || {
                selection.position_id.is_none()
                    && matches!(selection.amount, CloseAmount::Fraction(_))
            },
            |stored| stored == selection,
        );
        if !prior.order.reduce_only || prior.order.symbol != symbol || !matches {
            return Err(close_retry_changed());
        }
        Ok(prior)
    }

    async fn read_selected_close(
        &self,
        symbol: &str,
        selection: CloseSelection,
        registry: &Registry,
    ) -> Result<(SelectedClose, u32), EngineError> {
        let spec = registry.get(symbol)?;
        // The cached size can predate a fill or an unanswered reduce-only order.
        let positions = self.deps.client.get_positions().await?;
        let (position, side, contracts) =
            select_close(&positions, symbol, selection.position_id.as_deref())?;
        let held = contracts.mul(&dec(&spec.venue_contract_value)?);
        Ok((
            SelectedClose {
                id: views::position_id(position, side),
                side,
                contracts,
                held,
                selection,
            },
            views::leverage_of(position),
        ))
    }

    async fn close_request(
        &self,
        account_id: String,
        symbol: String,
        fraction: f64,
        client_order_id: String,
        position_id: Option<&str>,
    ) -> Result<(OrderRequest, SelectedClose), EngineError> {
        let registry = self.registry().await?;
        let spec = registry.get(&symbol)?;
        let selection = CloseSelection {
            position_id: position_id.map(str::to_owned),
            amount: CloseAmount::Fraction(fraction),
        };
        let (position, leverage) = self
            .read_selected_close(&symbol, selection, &registry)
            .await?;
        let quantity = if fraction >= 1.0 {
            position.held.clone()
        } else {
            let step = dec(&spec.quantity_step)?;
            position
                .held
                .mul(&Dec::from_js_number(fraction))
                .floor_to_step(&step)
                .map_err(|_| EngineError::message("Invalid quantity step"))?
        };
        let request = OrderRequest {
            account_id,
            client_order_id,
            symbol,
            side: match position.side {
                PositionSide::Long => OrderSide::Sell,
                PositionSide::Short => OrderSide::Buy,
            },
            order_type: EntryOrderType::Market,
            quantity: quantity.to_fixed(),
            price: None,
            leverage,
            reduce_only: Some(true),
            stop_loss: None,
            take_profit: None,
            trailing_stop: None,
            agent_id: None,
            decision_id: None,
        };
        Ok((request, position))
    }

    pub async fn cancel_order(self: &Arc<Self>, order_id: &str) -> Result<(), EngineError> {
        let order_id = order_id.to_string();
        self.queued(DeferredRestore::AllowPending, move |engine| {
            Box::pin(async move {
                let id = order_id.clone();
                let (order, registry) = engine
                    .rows(move |rows| Ok((rows.get_order(&id)?, Registry(rows.instruments()))))
                    .await?;
                let mut order = order
                    .ok_or_else(|| EngineError::message(format!("Unknown order {order_id}")))?;
                // OKX accepts a cancellation by client ID. Only this order's
                // own recovery can change it from a market order to an unsent algo.
                let needs_restore = engine
                    .dispatch()
                    .restore_order
                    .as_ref()
                    .is_some_and(|pending| pending.id() == order.id);
                if needs_restore {
                    if let Some(restored) = engine
                        .restore_unsent_order()
                        .await?
                        .filter(|restored| restored.id == order.id)
                    {
                        order = restored;
                    }
                }
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
                engine.check_dispatch()?;
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
        self.queued(DeferredRestore::AllowPending, |engine| {
            Box::pin(async move {
                match engine.restore_unsent_order().await {
                    Ok(_) => engine.sync_now().await,
                    Err(error @ (EngineError::Stopped | EngineError::SessionRetired)) => Err(error),
                    Err(error) => {
                        // Failed recovery only blocks more deferred submissions. Orders
                        // already at OKX can still fill and must be reconciled.
                        match engine.sync_without_deferred_orders().await {
                            Err(EngineError::Stopped) => Err(EngineError::Stopped),
                            _ => Err(error),
                        }
                    }
                }
            })
        })
        .await
    }

    /// A connection candidate can read back orders while the current session still
    /// owns dispatch. It sends nothing to OKX: deferred exits, covers and cleanup
    /// cancels wait for the first sync after registration.
    pub async fn sync_before_registration(self: &Arc<Self>) -> Result<(), EngineError> {
        self.queued(DeferredRestore::Required, |engine| {
            Box::pin(async move { engine.sync_read_only().await })
        })
        .await
    }

    /// Starts a history pass as a task of its own (see [`OkxEngine::sync_position_history`]).
    /// Only the arena reads the history, so a failure must not make the account unhealthy (that
    /// pauses its agents); the pass is tried again after the next sync.
    pub fn spawn_position_history(self: &Arc<Self>) {
        let engine = self.clone();
        tokio::spawn(async move {
            let _ = engine.sync_position_history().await;
        });
    }

    /// What each open position has made so far, as of the last sync.
    pub fn open_results(&self) -> Vec<OpenPositionResult> {
        self.state().open_results.clone()
    }

    async fn prepare(
        &self,
        account_id: &str,
        client_order_id: &str,
    ) -> Result<Prepared, EngineError> {
        let (account_id, client_order_id) = (account_id.to_string(), client_order_id.to_string());
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

    async fn submit(
        &self,
        request: OrderRequest,
        fresh_position: Option<SelectedClose>,
        prepared: Prepared,
    ) -> Result<OrderResult, EngineError> {
        let close_selection = fresh_position
            .as_ref()
            .map(|position| position.selection.clone());
        let registry = &prepared.registry;
        let mut order = self.build_order(&request);
        align_to_tick(&mut order, registry)?;
        let position = match &fresh_position {
            Some(position) => Some((position.side, position.held.clone())),
            None => self
                .positions()
                .into_iter()
                .find(|p| p.position.symbol == request.symbol)
                .map(|p| dec(&p.position.quantity).map(|qty| (p.position.side, qty)))
                .transpose()?,
        };
        if order.reduce_only {
            if let Some((_, held)) = &position {
                order.quantity = dec(&order.quantity)?.min(held).to_fixed();
            }
        }

        let refusal = self.pre_trade_refusal(&order, position.map(|(side, _)| side), &prepared)?;
        let contracts = to_contracts(&order, registry)?;
        let spec = registry.get(&order.symbol)?;
        let executable_quantity = contracts.mul(&dec(&spec.venue_contract_value)?);
        if close_selection
            .as_ref()
            .is_some_and(|selection| matches!(selection.amount, CloseAmount::Quantity { .. }))
            && !executable_quantity.eq_value(&dec(&order.quantity)?)
        {
            return Err(EngineError::message(format!(
                "Reduction quantity must be a multiple of {}",
                spec.quantity_step
            )));
        }
        if refusal.is_some() || contracts.lte(&zero()) {
            let reason = refusal.unwrap_or_else(|| "INVALID_QUANTITY: below one lot".into());
            return self.reject(order, reason, close_selection).await;
        }
        order.quantity = executable_quantity.to_fixed();

        // Written before anything is sent, so a crash can never leave an order we do not know
        // about.
        let _staged = self.stage_order(UnsentOrder::Submission(order.id.clone()))?;
        self.insert_submission(order.clone(), close_selection)
            .await?;

        let inst_id = to_inst_id(&order.symbol);
        let cl_ord_id = to_cl_ord_id(&order.client_order_id);
        let body = to_body(&order, &inst_id, &cl_ord_id, contracts.to_fixed());
        if let Some(selected) = fresh_position {
            // OKX's order endpoint has no position-version condition. Recheck just before
            // dispatch to narrow the window in which a concurrent trade can change the position.
            let checked = async {
                let positions = self.deps.client.get_positions().await?;
                let (_, _, current) = select_close(&positions, &order.symbol, Some(&selected.id))?;
                if !current.eq_value(&selected.contracts) {
                    return Err(close_changed());
                }
                Ok::<_, EngineError>(())
            }
            .await;
            if let Err(error) = checked {
                if order.agent_id.is_some() {
                    return self.finish_unsent(order, error.to_string()).await;
                }
                self.finish_unsent(order, error.to_string()).await?;
                return Err(error);
            }
        }
        match self.send(&mut order, &inst_id, &body).await {
            Ok(()) => {}
            Err(error @ EngineError::SessionRetired) => return Err(error),
            Err(error @ (EngineError::OpeningDisarmed | EngineError::LeverageSetup(_))) => {
                return self.finish_unsent(order, error.to_string()).await;
            }
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
        // Reduce-only orders can stage independently of a failed deferred restore.
        // Until that restore completes, reconcile positions without staging another protection.
        let restore_pending = self.dispatch().restore_order.is_some();
        if restore_pending {
            let _ = self.sync_without_deferred_orders().await;
        } else {
            let _ = self.sync_now().await;
        }
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
            self.check_dispatch()?;
            if !(self.deps.is_armed)() {
                return Err(EngineError::OpeningDisarmed);
            }
            // An uncertain leverage result still means the order itself never went out.
            client
                .set_leverage(inst_id, order.leverage)
                .await
                .map_err(EngineError::LeverageSetup)?;
        }
        // Stop All can disarm this account while the leverage request is in flight.
        self.handoff_order(order)?;
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

    async fn insert_submission(
        &self,
        row: Order,
        close_selection: Option<CloseSelection>,
    ) -> Result<(), EngineError> {
        self.rows(move |rows| {
            rows.transaction(&mut || {
                rows.insert_order(&row)?;
                if let Some(selection) = &close_selection {
                    rows.record_close_selection(&row.id, selection)?;
                }
                if row.status == OrderStatus::Open {
                    protection::record_deferred_protection(rows, &row)?;
                }
                Ok(())
            })
        })
        .await
    }

    /// A refused order is recorded, never an HTTP error.
    async fn reject(
        &self,
        mut order: Order,
        reason: String,
        close_selection: Option<CloseSelection>,
    ) -> Result<OrderResult, EngineError> {
        order.status = OrderStatus::Rejected;
        order.reject_reason = Some(reason);
        self.insert_submission(order.clone(), close_selection)
            .await?;
        Ok(OrderResult {
            order,
            fills: Vec::new(),
            duplicate: false,
        })
    }

    /// Records a refusal for a submission that may still be staged. If the write fails
    /// while the order is still known unsent, the proof is held until it commits.
    async fn finish_unsent(
        &self,
        order: Order,
        reason: String,
    ) -> Result<OrderResult, EngineError> {
        let id = order.id.clone();
        let result = self.finish_rejected(order, reason.clone()).await;
        if result.is_err() {
            let mut dispatch = self.dispatch();
            if dispatch
                .staged_order
                .as_ref()
                .is_some_and(|pending| pending.id() == id)
            {
                dispatch.staged_order = None;
                dispatch
                    .unsent_rejections
                    .push(UnsentOrder::Rejection { id, reason });
            }
        }
        result
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

const CLOSE_CHANGED: &str = "The selected OKX position has changed. Refresh and try again.";

/// Raised only before dispatch: by the first position read, or by the final recheck
/// once its rejection is recorded.
fn close_changed() -> EngineError {
    EngineError::RefusedBeforeDispatch(CLOSE_CHANGED.into())
}

fn close_retry_changed() -> EngineError {
    EngineError::message("This client order ID was already used for a different close request")
}

fn select_close<'a>(
    positions: &'a [Row],
    symbol: &str,
    position_id: Option<&str>,
) -> Result<(&'a Row, PositionSide, Dec), EngineError> {
    let inst_id = to_inst_id(symbol);
    let mut matches = positions.iter().filter_map(|p| {
        if p.get("instId").and_then(serde_json::Value::as_str) != Some(inst_id.as_str()) {
            return None;
        }
        let signed = dec(p.get("pos")?.as_str()?).ok()?;
        let side = views::position_side(p, signed.is_negative());
        let selected = match position_id {
            Some(id) => views::position_id(p, side) == id,
            None => sync::isolated_net(p),
        };
        if !selected || signed.is_zero() {
            return None;
        }
        let contracts = if signed.is_negative() {
            signed.neg()
        } else {
            signed
        };
        Some((p, side, contracts))
    });
    let found = matches.next().ok_or_else(|| {
        if position_id.is_some() {
            close_changed()
        } else {
            EngineError::message(format!("No open {symbol} position"))
        }
    })?;
    if matches.next().is_some() {
        return Err(EngineError::message(
            "Several OKX positions match. Select a position to close.",
        ));
    }
    if !sync::isolated_net(found.0) {
        return Err(EngineError::message(
            "Only isolated net positions can be closed in MochiPrep. Close this position in OKX.",
        ));
    }
    Ok(found)
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

/// Attach one protective leg. In particular, a single fixed stop remains eligible
/// for exchange placement when a partial fill is followed by cancellation.
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
        if let Some(target) = order
            .attached_take_profit
            .clone()
            .filter(|p| !p.is_empty() && !protection::defer_tp(order))
        {
            algo.push(AttachAlgoOrd {
                attach_algo_cl_ord_id: tag("tp"),
                tp_trigger_px: Some(target),
                tp_ord_px: Some("-1".into()),
                ..Default::default()
            });
        }
        if let Some(spec) = order
            .attached_trailing_stop
            .as_ref()
            .filter(|_| !protection::defer_trailing(order))
        {
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
