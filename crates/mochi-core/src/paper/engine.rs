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
    default_account_config, AccountVenue, EntryOrderType, EquityPoint, InstrumentSpecification,
    LedgerEventType, Liquidity, Order, OrderRequest, OrderResult, OrderSide, OrderStatus,
    OrderType, PaperAccount, PaperAccountConfig, PositionSide, Protection, TrailingStopSpec,
};

use super::store::PaperStore;
use super::trailing::validate_trailing;
use super::types::*;
use crate::{fill_model, perp};

/// A self-contained perpetuals simulator. See docs/PAPER_ENGINE.md for the exact rules.
pub struct PaperExecutionEngine<S: PaperStore> {
    pub(super) store: S,
    pub(super) instruments: Instruments,
    pub(super) get_quote: QuoteLookup,
    pub(super) now: Clock,
    // Interior mutability because ids and events are produced inside `store.transaction`
    // closures, which can only borrow the engine shared.
    pub(super) new_id: RefCell<IdSource>,
    pub(super) queued: RefCell<Vec<EngineEvent>>,
    pub(super) on_event: EventSink,
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
            trailing_stop: None,
            agent_id: None,
            decision_id: None,
        })
    }

    /// Replaces the whole protection of an open position. A `None` leg removes that leg.
    pub fn set_protection(
        &mut self,
        account_id: &str,
        symbol: &str,
        protection: &Protection,
    ) -> Result<ProtectionResult, EngineError> {
        let account = self.require_account(account_id)?;
        let position = self
            .store
            .get_open_position(account_id, symbol)?
            .ok_or_else(|| EngineError::Invalid(format!("No open {symbol} position")))?;
        let quote = (self.get_quote)(account.source, symbol)
            .ok_or_else(|| EngineError::Invalid("No market data".to_string()))?;
        let before = self.protection_of(account_id, symbol)?;
        let tick = self.instrument(symbol)?.price_tick.clone();
        self.validate_protection(
            position.side,
            &dec(&quote.last)?,
            protection,
            &tick,
            before.trailing_stop.as_ref(),
        )
        .map_err(Attempt::into_error)?;
        let now = (self.now)(account.source);
        let outcome = self
            .store
            .transaction(|| self.replace_protection(&position, protection, &quote.last, now));
        match outcome {
            Ok(()) => self.flush(),
            // TS leaves these queued until the next flush; they describe rolled-back orders.
            Err(e) => {
                self.drop_queued();
                return Err(e);
            }
        }
        let after = self.protection_of(account_id, symbol)?;
        Ok(ProtectionResult { before, after })
    }

    /// The stop-loss, take-profit and trailing stop resting for a symbol; `None` legs are not set.
    pub fn protection_of(&self, account_id: &str, symbol: &str) -> Result<Protection, EngineError> {
        self.require_account(account_id)?;
        let orders: Vec<Order> = self
            .store
            .list_open_orders(account_id)?
            .into_iter()
            .filter(|o| o.symbol == symbol && o.close_position)
            .collect();
        let trigger_of = |order_type| {
            orders
                .iter()
                .find(|o| o.order_type == order_type)
                .and_then(|o| o.trigger_price.clone())
        };
        let trailing_stop = match orders
            .iter()
            .find(|o| o.order_type == OrderType::TrailingStopMarket)
        {
            Some(order) => Some(trailing_spec_of(order)?),
            None => None,
        };
        Ok(Protection {
            stop_loss: trigger_of(OrderType::StopMarket),
            take_profit: trigger_of(OrderType::TakeProfitMarket),
            trailing_stop,
        })
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
            if request.trailing_stop.is_some() {
                return Err(reject(
                    "INVALID_PROTECTION",
                    "reduce-only orders cannot attach a trailing stop",
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
            // Adding to a position keeps its trailing stop when the entry restates it; a flip
            // does not.
            let kept = match &position {
                Some(p) if p.side == side_of(order.side) => {
                    self.protection_of(&account.id, &request.symbol)?
                        .trailing_stop
                }
                _ => None,
            };
            self.validate_protection(
                side_of(order.side),
                &reference,
                &attached_protection(&order),
                &instrument.price_tick,
                kept.as_ref(),
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
            &quote.last,
            now,
        )?;
        Ok(OrderResult {
            order,
            fills: vec![fill],
            duplicate: false,
        })
    }

    /// `kept` is the trailing stop already resting on the position. Restating it unchanged skips
    /// its checks: it was validated when created, and an activated one would otherwise fail
    /// forever because its activation price is no longer ahead of the market.
    fn validate_protection(
        &self,
        side: PositionSide,
        reference: &Dec,
        protection: &Protection,
        tick: &str,
        kept: Option<&TrailingStopSpec>,
    ) -> Result<(), Attempt> {
        if let Some(stop) = given(&protection.stop_loss) {
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
        if let Some(target) = given(&protection.take_profit) {
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
        if let Some(trailing) = &protection.trailing_stop {
            let unchanged = match kept {
                Some(kept) => same_trailing(kept, trailing)?,
                None => false,
            };
            if !unchanged {
                let refused = validate_trailing(side, reference, trailing, &dec(tick)?)
                    .map_err(|e| math_error("the trailing stop check", e))?;
                if let Some(refused) = refused {
                    return Err(reject("INVALID_PROTECTION", refused));
                }
            }
        }
        Ok(())
    }

    // ---- fills & accounting ---------------------------------------------------------------

    /// Wallet minus used margin minus what open orders hold back.
    pub(super) fn available_of(&self, account: &PaperAccount) -> Result<Dec, EngineError> {
        let mut reserved = zero();
        for order in self.store.list_open_orders(&account.id)? {
            reserved = reserved.add(&dec(&order.reserved_margin)?);
        }
        Ok(dec(&account.wallet_balance)?
            .sub(&dec(&account.used_margin)?)
            .sub(&reserved))
    }
}
