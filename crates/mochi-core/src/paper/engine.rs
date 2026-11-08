//! The paper execution engine's order path: accounts, placing, cancelling and closing orders,
//! protection orders, and fills with their ledger postings. A port of
//! `packages/core/src/paper/engine.ts`; `docs/PAPER_ENGINE.md` has the rules.
//!
//! The rows written, the events emitted, the ids drawn and every message must match the Node
//! engine, so each function keeps the TypeScript order of operations, including the order in
//! which ids are taken from `new_id`.

use std::cell::RefCell;

use mochi_decimal::Dec;
use mochi_domain::jsnum::{js_number_string, js_round};
use mochi_domain::registry::Instruments;
use mochi_domain::{
    default_account_config, AccountVenue, DataSource, EntryOrderType, EquityPoint, Fill,
    InstrumentSpecification, LedgerEvent, LedgerEventType, Liquidity, Order, OrderRequest,
    OrderResult, OrderSide, OrderStatus, OrderType, PaperAccount, PaperAccountConfig, Position,
    PositionSide, PositionStatus, Quote,
};
use serde::{Deserialize, Serialize};

use super::store::{PaperStore, StoreFailure};
use crate::{fill_model, perp, CoreError};

/// No fill may be priced off a quote older than this, whatever the caller's own limits are.
const MAX_FILL_QUOTE_AGE_MS: i64 = 60_000;

/// What the engine reports after a change commits. Serialized as `{ kind, ... }` like the TS union.
// Events are built once and handed straight to `on_event`, so boxing the large variants would
// only make them harder to construct and match.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EngineEvent {
    Fill {
        fill: Fill,
        order: Order,
    },
    Liquidation {
        position: Position,
    },
    #[serde(rename_all = "camelCase")]
    Funding {
        account_id: String,
        symbol: String,
        amount: String,
    },
    Order {
        order: Order,
    },
}

/// A failure the caller sees. `Invalid` is what TypeScript throws as a plain `Error`, with the
/// same message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] StoreFailure),
}

// The callbacks are `Send` so the engine can move between async tasks with its owner.

/// The latest observed quote for a source and symbol, if any.
pub type QuoteLookup = Box<dyn Fn(DataSource, &str) -> Option<Quote> + Send>;
pub type Clock = Box<dyn Fn(DataSource) -> i64 + Send>;
/// Returns a fresh id for a prefix such as `ord` or `fill`.
pub type IdSource = Box<dyn FnMut(&str) -> String + Send>;
pub type EventSink = Box<dyn FnMut(EngineEvent) + Send>;

pub struct PaperEngineDeps<S: PaperStore> {
    pub store: S,
    pub instruments: Instruments,
    pub get_quote: QuoteLookup,
    pub now: Clock,
    pub new_id: IdSource,
    pub on_event: EventSink,
}

/// `Partial<PaperAccountConfig>`: fields left out keep the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountConfigOverrides {
    pub taker_fee_rate: Option<String>,
    pub maker_fee_rate: Option<String>,
    pub slippage_bps: Option<String>,
    pub impact_bps_per_million: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAccountArgs {
    pub name: String,
    pub source: DataSource,
    pub initial_balance: Option<String>,
    pub is_demo: Option<bool>,
    pub config: Option<AccountConfigOverrides>,
}

/// TS `OrderRejection`: the order is refused, and recorded as REJECTED with this reason.
pub(super) struct OrderRejection {
    code: &'static str,
    detail: String,
}

impl OrderRejection {
    pub(super) fn message(&self) -> String {
        format!("{}: {}", self.code, self.detail)
    }
}

/// How an order attempt ends when it does not succeed. TS tells the two apart with
/// `instanceof OrderRejection`.
pub(super) enum Attempt {
    Rejected(OrderRejection),
    Failed(EngineError),
}

impl From<EngineError> for Attempt {
    fn from(e: EngineError) -> Self {
        Attempt::Failed(e)
    }
}

impl From<StoreFailure> for Attempt {
    fn from(e: StoreFailure) -> Self {
        Attempt::Failed(EngineError::Store(e))
    }
}

impl Attempt {
    /// Outside an order attempt nothing catches a rejection, so TS surfaces it as an Error.
    fn into_error(self) -> EngineError {
        match self {
            Attempt::Rejected(r) => EngineError::Invalid(r.message()),
            Attempt::Failed(e) => e,
        }
    }
}

fn reject(code: &'static str, detail: impl Into<String>) -> Attempt {
    Attempt::Rejected(OrderRejection {
        code,
        detail: detail.into(),
    })
}

/// `d(s)` on a string. decimal.js throws exactly this for text that is not a number.
pub(super) fn dec(s: &str) -> Result<Dec, EngineError> {
    Dec::parse(s).map_err(|_| EngineError::Invalid(format!("[DecimalError] Invalid argument: {s}")))
}

/// A core-math failure TypeScript has no error for: decimal.js would return Infinity or NaN and
/// carry it into the books, so the port refuses instead.
pub(super) fn math_error(what: &str, e: impl Into<CoreError>) -> EngineError {
    EngineError::Invalid(format!("Cannot compute {what}: {}", e.into()))
}

pub(super) fn money(v: &Dec) -> Dec {
    v.to_decimal_places(8)
}

pub(super) fn zero() -> Dec {
    Dec::from_js_number(0.0)
}

/// JS truthiness of an optional string: `null`, `undefined` and `''` are all "not given".
fn given(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|s| !s.is_empty())
}

fn side_of(side: OrderSide) -> PositionSide {
    match side {
        OrderSide::Buy => PositionSide::Long,
        OrderSide::Sell => PositionSide::Short,
    }
}

pub(super) fn closing_side(side: PositionSide) -> OrderSide {
    match side {
        PositionSide::Long => OrderSide::Sell,
        PositionSide::Short => OrderSide::Buy,
    }
}

fn side_text(side: PositionSide) -> &'static str {
    match side {
        PositionSide::Long => "LONG",
        PositionSide::Short => "SHORT",
    }
}

fn status_text(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::Open => "OPEN",
        OrderStatus::Filled => "FILLED",
        OrderStatus::Canceled => "CANCELED",
        OrderStatus::Rejected => "REJECTED",
    }
}

fn liquidity_text(liquidity: Liquidity) -> &'static str {
    match liquidity {
        Liquidity::Maker => "MAKER",
        Liquidity::Taker => "TAKER",
        Liquidity::Liquidation => "LIQUIDATION",
    }
}

/// One ledger posting before `post` assigns its id and running wallet. Absent TS fields are
/// zero amounts and `None` references.
pub(super) struct Posting {
    pub(super) event_type: LedgerEventType,
    pub(super) amount: Dec,
    pub(super) margin_delta: Dec,
    pub(super) memo: String,
    pub(super) ts: i64,
    pub(super) symbol: Option<String>,
    pub(super) order_id: Option<String>,
    pub(super) fill_id: Option<String>,
    pub(super) position_id: Option<String>,
}

impl Posting {
    pub(super) fn new(event_type: LedgerEventType, memo: impl Into<String>, ts: i64) -> Self {
        Posting {
            event_type,
            amount: zero(),
            margin_delta: zero(),
            memo: memo.into(),
            ts,
            symbol: None,
            order_id: None,
            fill_id: None,
            position_id: None,
        }
    }
}

/// A self-contained perpetuals simulator. See docs/PAPER_ENGINE.md for the exact rules.
pub struct PaperExecutionEngine<S: PaperStore> {
    pub(super) store: S,
    pub(super) instruments: Instruments,
    pub(super) get_quote: QuoteLookup,
    pub(super) now: Clock,
    // Interior mutability because ids and events are produced inside `store.transaction`
    // closures, which can only borrow the engine shared.
    new_id: RefCell<IdSource>,
    pub(super) queued: RefCell<Vec<EngineEvent>>,
    on_event: EventSink,
}

impl<S: PaperStore> PaperExecutionEngine<S> {
    pub fn new(deps: PaperEngineDeps<S>) -> Self {
        PaperExecutionEngine {
            store: deps.store,
            instruments: deps.instruments,
            get_quote: deps.get_quote,
            now: deps.now,
            new_id: RefCell::new(deps.new_id),
            queued: RefCell::new(Vec::new()),
            on_event: deps.on_event,
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn instruments(&self) -> &Instruments {
        &self.instruments
    }

    /// TypeScript registers OKX instruments into a global the engine reads live, so pairs added
    /// after startup must reach the engine's own registry through here.
    pub fn instruments_mut(&mut self) -> &mut Instruments {
        &mut self.instruments
    }

    pub(super) fn next_id(&self, prefix: &str) -> String {
        (self.new_id.borrow_mut())(prefix)
    }

    pub(super) fn queue(&self, event: EngineEvent) {
        self.queued.borrow_mut().push(event);
    }

    /// Emits what the committed transaction queued.
    pub(super) fn flush(&mut self) {
        for event in std::mem::take(self.queued.get_mut()) {
            (self.on_event)(event);
        }
    }

    /// A rolled-back transaction's events never happened.
    fn drop_queued(&mut self) {
        self.queued.get_mut().clear();
    }

    pub(super) fn instrument(&self, symbol: &str) -> Result<&InstrumentSpecification, EngineError> {
        self.instruments
            .get(symbol)
            .ok_or_else(|| EngineError::Invalid(format!("Unknown instrument {symbol}")))
    }

    /// The funding interval in ms. A zero interval would never advance the funding clock, so
    /// `apply_funding` would loop forever holding the write lock. TypeScript only ever sees the
    /// hardcoded 8 hours, but `instruments_mut` lets any spec in here.
    pub(super) fn funding_interval_ms(
        instrument: &InstrumentSpecification,
    ) -> Result<i64, EngineError> {
        if instrument.funding_interval_hours == 0 {
            return Err(EngineError::Invalid(format!(
                "Instrument {} has no funding interval",
                instrument.symbol
            )));
        }
        Ok(i64::from(instrument.funding_interval_hours) * 3_600_000)
    }

    pub(super) fn require_account(&self, id: &str) -> Result<PaperAccount, EngineError> {
        self.store
            .get_account(id)?
            .ok_or_else(|| EngineError::Invalid(format!("Unknown account {id}")))
    }

    // ---- accounts -------------------------------------------------------------------------

    pub fn create_account(&mut self, args: CreateAccountArgs) -> Result<PaperAccount, EngineError> {
        let now = (self.now)(args.source);
        let defaults = default_account_config();
        let overrides = args.config.unwrap_or_default();
        let config = PaperAccountConfig {
            taker_fee_rate: overrides.taker_fee_rate.unwrap_or(defaults.taker_fee_rate),
            maker_fee_rate: overrides.maker_fee_rate.unwrap_or(defaults.maker_fee_rate),
            slippage_bps: overrides.slippage_bps.unwrap_or(defaults.slippage_bps),
            impact_bps_per_million: overrides
                .impact_bps_per_million
                .unwrap_or(defaults.impact_bps_per_million),
        };
        let mut account = PaperAccount {
            id: self.next_id("acct"),
            name: args.name,
            source: args.source,
            venue: AccountVenue::Paper,
            exchange_uid: None,
            live_limits: None,
            is_demo: args.is_demo.unwrap_or(false),
            initial_balance: args.initial_balance.unwrap_or_else(|| "10000".to_string()),
            wallet_balance: "0".to_string(),
            used_margin: "0".to_string(),
            realized_pnl: "0".to_string(),
            fees_paid: "0".to_string(),
            funding_paid: "0".to_string(),
            config,
            created_at: now,
        };
        self.store.transaction(|| {
            self.store.insert_account(&account)?;
            self.seed_account(&mut account, now)
        })?;
        self.require_account(&account.id)
    }

    /// Wipes orders, fills, positions, ledger and equity history, then redeposits the initial balance.
    pub fn reset_account(&mut self, account_id: &str) -> Result<PaperAccount, EngineError> {
        let mut account = self.require_account(account_id)?;
        if account.venue != AccountVenue::Paper {
            return Err(EngineError::Invalid(
                "Exchange accounts cannot be reset".to_string(),
            ));
        }
        let now = (self.now)(account.source);
        self.store.transaction(|| {
            self.store.delete_account_data(account_id, true)?;
            for balance in [
                &mut account.wallet_balance,
                &mut account.used_margin,
                &mut account.realized_pnl,
                &mut account.fees_paid,
                &mut account.funding_paid,
            ] {
                *balance = "0".to_string();
            }
            self.seed_account(&mut account, now)
        })?;
        self.require_account(account_id)
    }

    fn seed_account(&self, account: &mut PaperAccount, now: i64) -> Result<(), EngineError> {
        let mut deposit = Posting::new(LedgerEventType::Deposit, "Initial virtual balance", now);
        deposit.amount = dec(&account.initial_balance)?;
        self.post(account, deposit)?;
        self.store.save_account(account)?;
        self.store.upsert_equity_point(&EquityPoint {
            account_id: account.id.clone(),
            ts: now,
            equity: account.initial_balance.clone(),
            wallet: account.initial_balance.clone(),
            unrealized: "0".to_string(),
        })?;
        Ok(())
    }

    // ---- orders ---------------------------------------------------------------------------

    pub fn place_order(&mut self, request: OrderRequest) -> Result<OrderResult, EngineError> {
        let prior = self
            .store
            .get_order_by_client_id(&request.account_id, &request.client_order_id)?;
        if let Some(prior) = prior {
            let fills = self.store.list_fills_by_order(&prior.id)?;
            return Ok(OrderResult {
                order: prior,
                fills,
                duplicate: true,
            });
        }

        let mut account = self.require_account(&request.account_id)?;
        let now = (self.now)(account.source);
        let attempt = self
            .store
            .transaction(|| self.execute_request(&mut account, &request, now));
        let rejection = match attempt {
            Ok(result) => {
                self.flush();
                return Ok(result);
            }
            Err(Attempt::Failed(e)) => {
                self.drop_queued();
                return Err(e);
            }
            Err(Attempt::Rejected(rejection)) => rejection,
        };
        self.drop_queued();
        // The attempt rolled back; only the rejected order itself is recorded.
        let mut order = self.build_order(&request, now);
        order.status = OrderStatus::Rejected;
        order.reject_reason = Some(rejection.message());
        self.store.insert_order(&order)?;
        (self.on_event)(EngineEvent::Order {
            order: order.clone(),
        });
        Ok(OrderResult {
            order,
            fills: Vec::new(),
            duplicate: false,
        })
    }

    pub fn cancel_order(&mut self, order_id: &str) -> Result<(), EngineError> {
        let mut order = self
            .store
            .get_order(order_id)?
            .ok_or_else(|| EngineError::Invalid(format!("Unknown order {order_id}")))?;
        if order.status != OrderStatus::Open {
            return Err(EngineError::Invalid(format!(
                "Order {order_id} is {}",
                status_text(order.status)
            )));
        }
        let account = self.require_account(&order.account_id)?;
        self.close_order(&mut order, None, (self.now)(account.source))?;
        (self.on_event)(EngineEvent::Order { order });
        Ok(())
    }

    /// Closes a fraction of a position with a reduce-only market order.
    pub fn close_position(
        &mut self,
        account_id: &str,
        symbol: &str,
        fraction: f64,
        client_order_id: &str,
    ) -> Result<OrderResult, EngineError> {
        let position = self
            .store
            .get_open_position(account_id, symbol)?
            .ok_or_else(|| EngineError::Invalid(format!("No open {symbol} position")))?;
        let instrument = self.instrument(symbol)?;
        let held = dec(&position.quantity)?;
        let quantity = if fraction >= 1.0 {
            held
        } else {
            // NaN and -Infinity land here; decimal.js would turn them into a NaN quantity.
            if !fraction.is_finite() {
                return Err(EngineError::Invalid(format!(
                    "Cannot close a fraction of {}",
                    js_number_string(fraction)
                )));
            }
            held.mul(&Dec::from_js_number(fraction))
                .floor_to_step(&dec(&instrument.quantity_step)?)
                .map_err(|e| math_error("the close quantity", e))?
        };
        self.place_order(OrderRequest {
            account_id: account_id.to_string(),
            client_order_id: client_order_id.to_string(),
            symbol: symbol.to_string(),
            side: closing_side(position.side),
            order_type: EntryOrderType::Market,
            quantity: quantity.to_fixed(),
            price: None,
            leverage: position.leverage,
            reduce_only: Some(true),
            stop_loss: None,
            take_profit: None,
            agent_id: None,
            decision_id: None,
        })
    }

    /// Replaces the stop-loss and take-profit protecting an open position. `None` removes one.
    pub fn set_protection(
        &mut self,
        account_id: &str,
        symbol: &str,
        stop_loss: Option<&str>,
        take_profit: Option<&str>,
    ) -> Result<(), EngineError> {
        let account = self.require_account(account_id)?;
        let position = self
            .store
            .get_open_position(account_id, symbol)?
            .ok_or_else(|| EngineError::Invalid(format!("No open {symbol} position")))?;
        let quote = (self.get_quote)(account.source, symbol)
            .ok_or_else(|| EngineError::Invalid("No market data".to_string()))?;
        let stop_loss = stop_loss.map(str::to_string);
        let take_profit = take_profit.map(str::to_string);
        self.validate_protection(position.side, &dec(&quote.last)?, &stop_loss, &take_profit)
            .map_err(Attempt::into_error)?;
        let now = (self.now)(account.source);
        let outcome = self
            .store
            .transaction(|| self.replace_protection(&position, &stop_loss, &take_profit, now));
        match outcome {
            Ok(()) => {
                self.flush();
                Ok(())
            }
            // TS leaves these queued until the next flush; they describe rolled-back orders.
            Err(e) => {
                self.drop_queued();
                Err(e)
            }
        }
    }

    pub(super) fn build_order(&self, request: &OrderRequest, now: i64) -> Order {
        Order {
            id: self.next_id("ord"),
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
            filled_quantity: "0".to_string(),
            avg_fill_price: None,
            reserved_margin: "0".to_string(),
            attached_stop_loss: request.stop_loss.clone(),
            attached_take_profit: request.take_profit.clone(),
            reject_reason: None,
            exchange_order_id: None,
            agent_id: request.agent_id.clone(),
            decision_id: request.decision_id.clone(),
            created_at: now,
            updated_at: now,
        }
    }

    fn execute_request(
        &self,
        account: &mut PaperAccount,
        request: &OrderRequest,
        now: i64,
    ) -> Result<OrderResult, Attempt> {
        let instrument = self.instrument(&request.symbol)?;
        let mut order = self.build_order(request, now);
        let Some(quote) = (self.get_quote)(account.source, &request.symbol) else {
            return Err(reject(
                "NO_MARKET_DATA",
                format!("no quote for {}", request.symbol),
            ));
        };
        let age = now - quote.ts;
        if age > MAX_FILL_QUOTE_AGE_MS {
            let seconds = js_round(age as f64 / 1000.0);
            return Err(reject(
                "MARKET_DATA_STALE",
                format!("quote is {}s old", js_number_string(seconds)),
            ));
        }
        if request.leverage < 1 || request.leverage > instrument.max_leverage {
            return Err(reject(
                "INVALID_LEVERAGE",
                format!(
                    "{}x outside 1-{}x",
                    js_number_string(f64::from(request.leverage)),
                    js_number_string(f64::from(instrument.max_leverage))
                ),
            ));
        }

        let mut quantity = dec(&request.quantity)?
            .floor_to_step(&dec(&instrument.quantity_step)?)
            .map_err(|e| math_error("the order quantity", e))?;
        let position = self.store.get_open_position(&account.id, &request.symbol)?;
        if order.reduce_only {
            let held = match &position {
                Some(p) if p.side != side_of(order.side) => dec(&p.quantity)?,
                _ => return Err(reject("REDUCE_ONLY_NO_POSITION", "nothing to reduce")),
            };
            if given(&request.stop_loss).is_some() || given(&request.take_profit).is_some() {
                return Err(reject(
                    "INVALID_PROTECTION",
                    "reduce-only orders cannot attach SL/TP",
                ));
            }
            quantity = quantity.min(&held);
        }
        if quantity.lt(&dec(&instrument.min_quantity)?) {
            return Err(reject(
                "INVALID_QUANTITY",
                format!("minimum is {}", instrument.min_quantity),
            ));
        }
        order.quantity = quantity.to_fixed();

        // Some only for LIMIT orders, as `order.price` is set only for them.
        let mut limit = None;
        if request.order_type == EntryOrderType::Limit {
            let Some(price) = given(&request.price) else {
                return Err(reject("INVALID_PRICE", "limit orders need a price"));
            };
            let rounded = dec(price)?
                .round_to_step(&dec(&instrument.price_tick)?)
                .map_err(|e| math_error("the limit price", e))?;
            if rounded.lte(&zero()) {
                return Err(reject("INVALID_PRICE", "price rounds to zero"));
            }
            order.price = Some(rounded.to_fixed());
            limit = Some(rounded);
        }

        let reference = match &limit {
            Some(limit) => limit.clone(),
            None => dec(match request.side {
                OrderSide::Buy => &quote.ask,
                OrderSide::Sell => &quote.bid,
            })?,
        };
        if !order.reduce_only {
            self.validate_protection(
                side_of(order.side),
                &reference,
                &order.attached_stop_loss,
                &order.attached_take_profit,
            )?;
        }

        let crosses = match &limit {
            None => true,
            Some(limit) => fill_model::is_marketable(request.side, limit, &quote)
                .map_err(|e| math_error("whether the limit crosses", e))?,
        };
        if let (false, Some(limit)) = (crosses, &limit) {
            if !order.reduce_only {
                let notional = quantity.mul(limit);
                let leverage = position.as_ref().map_or(request.leverage, |p| p.leverage);
                let margin = perp::initial_margin(&notional, leverage)
                    .map_err(|e| math_error("the reserved margin", e))?;
                let fee = notional.mul(&dec(&account.config.maker_fee_rate)?);
                let reserve = money(&margin.add(&fee));
                let available = self.available_of(account)?;
                if reserve.gt(&available) {
                    return Err(reject(
                        "INSUFFICIENT_MARGIN",
                        format!(
                            "needs {}, available {}",
                            reserve.to_fixed_dp(2),
                            available.to_fixed_dp(2)
                        ),
                    ));
                }
                order.reserved_margin = reserve.to_fixed();
            }
            self.store.insert_order(&order)?;
            self.queue(EngineEvent::Order {
                order: order.clone(),
            });
            return Ok(OrderResult {
                order,
                fills: Vec::new(),
                duplicate: false,
            });
        }

        let mut price = fill_model::market_fill_price(
            request.side,
            &quantity,
            &quote,
            &account.config,
            instrument,
        )
        .map_err(|e| math_error("the market fill price", e))?;
        if let Some(limit) = &limit {
            // Marketable limit: slippage may not push the fill past the limit.
            price = match request.side {
                OrderSide::Buy => price.min(limit),
                OrderSide::Sell => price.max(limit),
            };
        }
        self.store.insert_order(&order)?;
        let fill = self.fill_order(
            account,
            &mut order,
            &quantity,
            &price,
            Liquidity::Taker,
            now,
        )?;
        Ok(OrderResult {
            order,
            fills: vec![fill],
            duplicate: false,
        })
    }

    fn validate_protection(
        &self,
        side: PositionSide,
        reference: &Dec,
        stop_loss: &Option<String>,
        take_profit: &Option<String>,
    ) -> Result<(), Attempt> {
        if let Some(stop) = given(stop_loss) {
            let level = dec(stop)?;
            let ok = match side {
                PositionSide::Long => level.lt(reference),
                PositionSide::Short => level.gt(reference),
            };
            if !ok {
                return Err(reject(
                    "INVALID_PROTECTION",
                    format!(
                        "stop {stop} is on the wrong side of {} for {}",
                        reference.to_fixed(),
                        side_text(side)
                    ),
                ));
            }
        }
        if let Some(target) = given(take_profit) {
            let level = dec(target)?;
            let ok = match side {
                PositionSide::Long => level.gt(reference),
                PositionSide::Short => level.lt(reference),
            };
            if !ok {
                return Err(reject(
                    "INVALID_PROTECTION",
                    format!(
                        "target {target} is on the wrong side of {} for {}",
                        reference.to_fixed(),
                        side_text(side)
                    ),
                ));
            }
        }
        Ok(())
    }

    // ---- fills & accounting ---------------------------------------------------------------

    /// Applies one fill to the position and ledger. Must run inside a transaction: it can still
    /// reject after writing, and only a rollback undoes those writes.
    pub(super) fn fill_order(
        &self,
        account: &mut PaperAccount,
        order: &mut Order,
        quantity: &Dec,
        price: &Dec,
        liquidity: Liquidity,
        ts: i64,
    ) -> Result<Fill, Attempt> {
        let instrument = self.instrument(&order.symbol)?;
        let fee_rate = match liquidity {
            Liquidity::Maker => account.config.maker_fee_rate.clone(),
            Liquidity::Taker => account.config.taker_fee_rate.clone(),
            // TS types `fillOrder` as MAKER | TAKER only: `liquidate` books its own fill with no
            // fee, so a liquidation reaching here is a bug, not a taker fill.
            Liquidity::Liquidation => {
                return Err(Attempt::Failed(EngineError::Invalid(
                    "Liquidations are booked by liquidate, not fill_order".to_string(),
                )))
            }
        };
        let fee = money(&perp::notional(quantity, price).mul(&dec(&fee_rate)?));
        let fill_id = self.next_id("fill");
        let posting = |event_type, memo: String, position_id: Option<&str>| Posting {
            symbol: Some(order.symbol.clone()),
            order_id: Some(order.id.clone()),
            fill_id: Some(fill_id.clone()),
            position_id: position_id.map(str::to_string),
            ..Posting::new(event_type, memo, ts)
        };
        let target_side = side_of(order.side);

        let mut position = self.store.get_open_position(&account.id, &order.symbol)?;
        let mut remaining = quantity.clone();
        let mut realized = zero();

        if let Some(mut open) = position.take_if(|p| p.side != target_side) {
            let held = dec(&open.quantity)?;
            let close_qty = remaining.min(&held);
            let full_close = close_qty.eq_value(&held);
            realized = money(&perp::pnl(
                open.side,
                &dec(&open.entry_price)?,
                price,
                &close_qty,
            ));
            let margin = dec(&open.margin)?;
            let release = if full_close {
                margin.clone()
            } else {
                money(
                    &margin
                        .mul(&close_qty)
                        .div(&held)
                        .map_err(|e| math_error("the released margin", e))?,
                )
            };
            let close_fee = if close_qty.eq_value(quantity) {
                fee.clone()
            } else {
                money(
                    &fee.mul(&close_qty)
                        .div(quantity)
                        .map_err(|e| math_error("the closing fee share", e))?,
                )
            };

            let memo = format!("Closed {} @ {}", close_qty.to_fixed(), price.to_fixed());
            let mut pnl = posting(LedgerEventType::RealizedPnl, memo, Some(&open.id));
            pnl.amount = realized.clone();
            self.post(account, pnl)?;
            let memo = "Margin released".to_string();
            let mut released = posting(LedgerEventType::MarginRelease, memo, Some(&open.id));
            released.margin_delta = release.neg();
            self.post(account, released)?;

            open.quantity = held.sub(&close_qty).to_fixed();
            open.margin = margin.sub(&release).to_fixed();
            open.realized_pnl = dec(&open.realized_pnl)?.add(&realized).to_fixed();
            open.fees_paid = dec(&open.fees_paid)?.add(&close_fee).to_fixed();
            open.exit_price = Some(price.to_fixed());
            if full_close {
                open.status = PositionStatus::Closed;
                open.closed_at = Some(ts);
                self.store.save_position(&open)?;
                self.cancel_protection(&account.id, &order.symbol, Some(&order.id), ts)?;
            } else {
                refresh_liquidation(&mut open, &instrument.maintenance_margin_rate)?;
                self.store.save_position(&open)?;
                position = Some(open);
            }
            remaining = remaining.sub(&close_qty);
        }

        let opens_exposure = remaining.gt(&zero());
        if opens_exposure {
            let open_fee = if remaining.eq_value(quantity) {
                fee.clone()
            } else {
                money(
                    &fee.mul(&remaining)
                        .div(quantity)
                        .map_err(|e| math_error("the opening fee share", e))?,
                )
            };
            let leverage = position.as_ref().map_or(order.leverage, |p| p.leverage);
            let margin = money(
                &perp::initial_margin(&perp::notional(&remaining, price), leverage)
                    .map_err(|e| math_error("the initial margin", e))?,
            );
            let opened = match position.take() {
                Some(mut p) => {
                    let held = dec(&p.quantity)?;
                    let total = held.add(&remaining);
                    let weighted = held
                        .mul(&dec(&p.entry_price)?)
                        .add(&remaining.mul(price))
                        .div(&total)
                        .map_err(|e| math_error("the average entry", e))?;
                    p.entry_price = money(&weighted).to_fixed();
                    p.quantity = total.to_fixed();
                    p.margin = dec(&p.margin)?.add(&margin).to_fixed();
                    p.fees_paid = dec(&p.fees_paid)?.add(&open_fee).to_fixed();
                    p.max_quantity = dec(&p.max_quantity)?.max(&total).to_fixed();
                    refresh_liquidation(&mut p, &instrument.maintenance_margin_rate)?;
                    self.store.save_position(&p)?;
                    p
                }
                None => {
                    let interval = Self::funding_interval_ms(instrument)?;
                    let mut p = Position {
                        id: self.next_id("pos"),
                        account_id: account.id.clone(),
                        symbol: order.symbol.clone(),
                        side: target_side,
                        quantity: remaining.to_fixed(),
                        entry_price: price.to_fixed(),
                        leverage,
                        margin: margin.to_fixed(),
                        liquidation_price: "0".to_string(),
                        status: PositionStatus::Open,
                        realized_pnl: "0".to_string(),
                        fees_paid: open_fee.to_fixed(),
                        funding_paid: "0".to_string(),
                        max_quantity: remaining.to_fixed(),
                        exit_price: None,
                        // `Math.floor(ts / interval) * interval`, in floating point as JS does.
                        last_funding_time: (ts as f64 / interval as f64).floor() as i64 * interval,
                        opened_at: ts,
                        closed_at: None,
                    };
                    refresh_liquidation(&mut p, &instrument.maintenance_margin_rate)?;
                    self.store.insert_position(&p)?;
                    p
                }
            };
            let memo = format!(
                "Isolated margin at {}x",
                js_number_string(f64::from(leverage))
            );
            let mut lock = posting(LedgerEventType::MarginLock, memo, Some(&opened.id));
            lock.margin_delta = margin;
            self.post(account, lock)?;
            position = Some(opened);
        }

        let memo = format!("{} fee at {fee_rate}", liquidity_text(liquidity));
        let position_id = position.as_ref().map(|p| p.id.as_str());
        let mut charged = posting(LedgerEventType::TradeFee, memo, position_id);
        charged.amount = fee.neg();
        self.post(account, charged)?;

        let fill = Fill {
            id: fill_id.clone(),
            order_id: order.id.clone(),
            account_id: account.id.clone(),
            symbol: order.symbol.clone(),
            side: order.side,
            quantity: quantity.to_fixed(),
            price: price.to_fixed(),
            fee: fee.to_fixed(),
            realized_pnl: realized.to_fixed(),
            liquidity,
            ts,
        };
        self.store.insert_fill(&fill)?;

        order.filled_quantity = quantity.to_fixed();
        order.avg_fill_price = Some(price.to_fixed());
        order.status = OrderStatus::Filled;
        order.reserved_margin = "0".to_string();
        order.updated_at = ts;
        self.store.save_order(order)?;
        self.store.save_account(account)?;

        if opens_exposure {
            let available = self.available_of(account)?;
            if available.lt(&zero()) {
                return Err(reject(
                    "INSUFFICIENT_MARGIN",
                    format!("short by {} USDT after fee", available.neg().to_fixed_dp(2)),
                ));
            }
            let protected = given(&order.attached_stop_loss).is_some()
                || given(&order.attached_take_profit).is_some();
            if let (Some(position), true) = (&position, protected) {
                self.replace_protection(
                    position,
                    &order.attached_stop_loss,
                    &order.attached_take_profit,
                    ts,
                )?;
            }
        }
        self.queue(EngineEvent::Fill {
            fill: fill.clone(),
            order: order.clone(),
        });
        Ok(fill)
    }

    /// Moves the wallet and used margin and appends the matching ledger event. Zero postings
    /// are skipped so the ledger only holds real changes.
    pub(super) fn post(&self, account: &mut PaperAccount, e: Posting) -> Result<(), EngineError> {
        if e.amount.is_zero() && e.margin_delta.is_zero() {
            return Ok(());
        }
        account.wallet_balance = dec(&account.wallet_balance)?.add(&e.amount).to_fixed();
        account.used_margin = dec(&account.used_margin)?.add(&e.margin_delta).to_fixed();
        match e.event_type {
            LedgerEventType::RealizedPnl | LedgerEventType::LiquidationPenalty => {
                account.realized_pnl = dec(&account.realized_pnl)?.add(&e.amount).to_fixed();
            }
            LedgerEventType::TradeFee => {
                account.fees_paid = dec(&account.fees_paid)?.sub(&e.amount).to_fixed();
            }
            LedgerEventType::Funding => {
                account.funding_paid = dec(&account.funding_paid)?.sub(&e.amount).to_fixed();
            }
            _ => {}
        }
        self.store.append_ledger(&LedgerEvent {
            seq: 0, // assigned by the store
            id: self.next_id("evt"),
            account_id: account.id.clone(),
            event_type: e.event_type,
            amount: e.amount.to_fixed(),
            margin_delta: e.margin_delta.to_fixed(),
            wallet_after: account.wallet_balance.clone(),
            symbol: e.symbol,
            order_id: e.order_id,
            fill_id: e.fill_id,
            position_id: e.position_id,
            memo: e.memo,
            ts: e.ts,
        })?;
        Ok(())
    }

    // ---- protection orders ----------------------------------------------------------------

    fn replace_protection(
        &self,
        position: &Position,
        stop_loss: &Option<String>,
        take_profit: &Option<String>,
        ts: i64,
    ) -> Result<(), EngineError> {
        for mut order in self.store.list_open_orders(&position.account_id)? {
            if order.symbol == position.symbol && order.close_position {
                self.close_order(&mut order, None, ts)?;
            }
        }
        let make = |order_type: OrderType, trigger: &str| -> Result<(), EngineError> {
            // The protect id is drawn before the order id, as in the TS object literal.
            let request = OrderRequest {
                account_id: position.account_id.clone(),
                client_order_id: self.next_id("protect"),
                symbol: position.symbol.clone(),
                side: closing_side(position.side),
                order_type: EntryOrderType::Market,
                quantity: position.quantity.clone(),
                price: None,
                leverage: position.leverage,
                reduce_only: Some(true),
                stop_loss: None,
                take_profit: None,
                agent_id: None,
                decision_id: None,
            };
            let order = Order {
                order_type,
                trigger_price: Some(trigger.to_string()),
                close_position: true,
                ..self.build_order(&request, ts)
            };
            self.store.insert_order(&order)?;
            self.queue(EngineEvent::Order { order });
            Ok(())
        };
        if let Some(stop) = given(stop_loss) {
            make(OrderType::StopMarket, stop)?;
        }
        if let Some(target) = given(take_profit) {
            make(OrderType::TakeProfitMarket, target)?;
        }
        Ok(())
    }

    /// Flat positions cannot be reduced: clears every reduce-only order on the symbol.
    pub(super) fn cancel_protection(
        &self,
        account_id: &str,
        symbol: &str,
        except_order_id: Option<&str>,
        ts: i64,
    ) -> Result<(), EngineError> {
        for mut order in self.store.list_open_orders(account_id)? {
            if order.symbol == symbol
                && order.reduce_only
                && Some(order.id.as_str()) != except_order_id
            {
                self.close_order(&mut order, Some("Position closed"), ts)?;
            }
        }
        Ok(())
    }

    /// Cancels an order and releases its reservation. CANCELED is the only way TS closes one.
    pub(super) fn close_order(
        &self,
        order: &mut Order,
        reason: Option<&str>,
        ts: i64,
    ) -> Result<(), EngineError> {
        order.status = OrderStatus::Canceled;
        order.reserved_margin = "0".to_string();
        order.reject_reason = reason.map(str::to_string);
        order.updated_at = ts;
        self.store.save_order(order)?;
        Ok(())
    }

    /// Wallet minus used margin minus what open orders hold back.
    fn available_of(&self, account: &PaperAccount) -> Result<Dec, EngineError> {
        let mut reserved = zero();
        for order in self.store.list_open_orders(&account.id)? {
            reserved = reserved.add(&dec(&order.reserved_margin)?);
        }
        Ok(dec(&account.wallet_balance)?
            .sub(&dec(&account.used_margin)?)
            .sub(&reserved))
    }
}

fn refresh_liquidation(position: &mut Position, maintenance_rate: &str) -> Result<(), EngineError> {
    let price = perp::liquidation_price(
        position.side,
        &dec(&position.entry_price)?,
        &dec(&position.quantity)?,
        &dec(&position.margin)?,
        &dec(maintenance_rate)?,
    )
    .map_err(|e| math_error("the liquidation price", e))?;
    position.liquidation_price = money(&price).to_fixed();
    Ok(())
}
