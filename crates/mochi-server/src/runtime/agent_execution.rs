//! The only AI-to-order path: validated decisions and a current-state risk verdict.
//!
//! The runner trades through the account's engine, as Node's runner does through
//! `execution(accountId)`. Reads come from the task: a paper account's engine, or an OKX session's
//! last sync. A paper order is placed on the task, in the same turn as its risk check. An OKX
//! order or protection change goes out through the `ExecutionEngine` trait, off the task, and its
//! result comes back to be recorded.
use super::execution::{ExecutionEngine, ExecutionTarget, ProtectionFailure, ProtectionMeta};
use super::{internal_error, Runtime, RuntimeHandle};
use crate::error::ApiError;
use mochi_core::{
    paper::{DailyRisk, PaperStore, ProtectionResult},
    risk::{evaluate_risk, RiskInput},
};
use mochi_decimal::Dec;
use mochi_domain::{
    AccountBalance, AiAction, AiDecisionData, AiTrailingStop, DataSource, EntryOrderType, Order,
    OrderRequest, OrderResult, OrderSide, OrderStatus, PaperAccount, Position, PositionSide,
    PositionView, Protection, Quote, RiskResult, RiskSettings, TrailingStopSpec,
};
use mochi_store::rows;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn settle(r: &mut Value, outcome: &str, detail: Option<String>) {
    r["outcome"] = json!(outcome);
    r["outcomeDetail"] = json!(detail);
}
fn plain(n: Option<f64>) -> Option<String> {
    n.map(|n| Dec::from_js_number(n).to_fixed())
}
fn trailing(t: &AiTrailingStop) -> TrailingStopSpec {
    TrailingStopSpec {
        kind: t.kind,
        callback: plain(Some(t.callback)).unwrap(),
        activation_price: plain(t.activation_price),
    }
}
pub(super) fn target_protection(d: &AiDecisionData, current: &Protection) -> Protection {
    Protection {
        stop_loss: plain(d.stop_loss),
        take_profit: plain(d.take_profit),
        trailing_stop: match &d.trailing_stop {
            None => current.trailing_stop.clone(),
            Some(t) => t.as_ref().map(trailing),
        },
    }
}
fn same_leg(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => match (Dec::parse(a), Dec::parse(b)) {
            (Ok(a), Ok(b)) => a.eq_value(&b),
            _ => false,
        },
        _ => false,
    }
}
pub(super) fn same_protection(a: &Protection, b: &Protection) -> bool {
    same_leg(&a.stop_loss, &b.stop_loss)
        && same_leg(&a.take_profit, &b.take_profit)
        && match (&a.trailing_stop, &b.trailing_stop) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.kind == b.kind
                    && same_leg(&Some(a.callback.clone()), &Some(b.callback.clone()))
                    && same_leg(&a.activation_price, &b.activation_price)
            }
            _ => false,
        }
}
pub fn effective_risk(a: &RiskSettings, c: Option<&RiskSettings>) -> RiskSettings {
    match c {
        None => a.clone(),
        Some(c) => RiskSettings {
            max_leverage: a.max_leverage.min(c.max_leverage),
            max_risk_per_trade_pct: a.max_risk_per_trade_pct.min(c.max_risk_per_trade_pct),
            max_daily_loss_pct: a.max_daily_loss_pct.min(c.max_daily_loss_pct),
            max_open_positions: a.max_open_positions.min(c.max_open_positions),
            require_stop_loss: a.require_stop_loss || c.require_stop_loss,
            max_quote_age_sec: a.max_quote_age_sec.min(c.max_quote_age_sec),
            max_notional: a.max_notional.min(c.max_notional),
        },
    }
}

/// What an approved decision sends to its engine.
pub(super) enum Act {
    Place(OrderRequest),
    Protect {
        account_id: String,
        symbol: String,
        target: Protection,
        meta: ProtectionMeta,
    },
}

/// What the engine answered.
pub(super) enum Acted {
    Placed(Result<Box<OrderResult>, ApiError>),
    Protected(Result<ProtectionResult, ProtectionFailure>),
}

impl Act {
    /// Sends the act through the trait, as Node's runner awaits its engine.
    pub(super) async fn run(self, engine: Arc<dyn ExecutionEngine>) -> Acted {
        match self {
            Act::Place(request) => Acted::Placed(engine.place_order(request).await.map(Box::new)),
            Act::Protect {
                account_id,
                symbol,
                target,
                meta,
            } => Acted::Protected(
                engine
                    .set_protection(account_id, symbol, target, meta)
                    .await,
            ),
        }
    }
}

/// An act for an exchange: the engine to send it to, then the record to settle with the answer.
pub(super) type Outgoing = (Arc<dyn ExecutionEngine>, Act);

/// Node's `execute` and `protect` after the engine answers.
pub(super) fn settle_acted(r: &mut Value, acted: Acted) {
    match acted {
        Acted::Placed(Err(e)) => settle(r, "ORDER_REJECTED", Some(e.message().to_string())),
        Acted::Placed(Ok(result)) => {
            r["orderId"] = json!(result.order.id);
            r["fillIds"] = json!(result.fills.iter().map(|f| &f.id).collect::<Vec<_>>());
            match result.order.status {
                OrderStatus::Filled => settle(
                    r,
                    "EXECUTED",
                    Some(format!(
                        "Filled {} @ {}",
                        result.order.quantity,
                        result.order.avg_fill_price.unwrap_or_default()
                    )),
                ),
                OrderStatus::Open => settle(
                    r,
                    "ORDER_PLACED",
                    Some(format!(
                        "Resting {} limit @ {}",
                        if result.order.side == OrderSide::Buy {
                            "BUY"
                        } else {
                            "SELL"
                        },
                        result.order.price.unwrap_or_default()
                    )),
                ),
                _ => settle(r, "ORDER_REJECTED", result.order.reject_reason),
            }
        }
        Acted::Protected(Err(failure)) => {
            // Refused part-way: legs already placed stay, so History shows what is in force now.
            if let Some(after) = failure.after {
                r["protectionAfter"] = json!(after);
            }
            settle(
                r,
                "ORDER_REJECTED",
                Some(failure.error.message().to_string()),
            );
        }
        Acted::Protected(Ok(result)) => {
            r["protectionBefore"] = json!(result.before);
            r["protectionAfter"] = json!(result.after);
            if result.pending {
                settle(
                    r,
                    "ORDER_PLACED",
                    Some("Protection change sent to OKX, awaiting confirmation".into()),
                );
            } else if same_protection(&result.before, &result.after) {
                // The engine aligns prices to the tick, so a target that differed only off the
                // tick changes nothing.
                settle(r, "HOLD", Some("Protection already as requested".into()));
            } else {
                settle(r, "EXECUTED", Some("Protection updated".into()));
            }
        }
    }
}

/// Where a suggestion stands after the checks on the task.
pub(super) enum SignalStep {
    /// Settled and saved.
    Done(Value),
    /// Approved for an exchange: send it, then record the answer. The agent is busy meanwhile.
    Send {
        record: Value,
        agent_id: String,
        token: String,
        outgoing: Box<Outgoing>,
    },
}

impl RuntimeHandle {
    /// Node's `executeSignal`. Runs as its own task, so an order sent to OKX is always recorded,
    /// even if the client goes away while OKX answers.
    pub async fn execute_signal(&self, id: impl Into<String>) -> Result<Value, ApiError> {
        let id = id.into();
        let handle = self.clone();
        tokio::spawn(async move {
            match handle.call(move |r| r.begin_signal(&id)).await?? {
                SignalStep::Done(record) => Ok(record),
                SignalStep::Send {
                    record,
                    agent_id,
                    token,
                    outgoing,
                } => {
                    let (engine, act) = *outgoing;
                    let acted = act.run(engine).await;
                    handle
                        .call(move |r| r.finish_signal(record, &agent_id, &token, acted))
                        .await?
                }
            }
        })
        .await
        .unwrap_or_else(|_| Err(internal_error()))
    }
}

impl Runtime {
    // ---- reads, through the account's engine ---------------------------------------------

    /// Node's `engine.getBalance(accountId)`.
    pub(super) fn venue_balance(
        &self,
        target: &ExecutionTarget,
        account_id: &str,
    ) -> Result<AccountBalance, ApiError> {
        match target {
            ExecutionTarget::Paper => self
                .engine
                .balance_of(account_id)
                .map_err(|e| self.api_error(e)),
            ExecutionTarget::Okx(engine) => engine.balance().map_err(|e| self.api_error(e)),
        }
    }

    /// Node's `engine.getPositions(accountId)`.
    pub(super) fn venue_positions(
        &self,
        target: &ExecutionTarget,
        account_id: &str,
    ) -> Result<Vec<PositionView>, ApiError> {
        match target {
            ExecutionTarget::Paper => self
                .engine
                .position_views(account_id)
                .map_err(|e| self.api_error(e)),
            ExecutionTarget::Okx(engine) => Ok(engine.positions()),
        }
    }

    /// The open positions as the risk engine reads them.
    fn venue_open_positions(
        &self,
        target: &ExecutionTarget,
        account_id: &str,
    ) -> Result<Vec<Position>, ApiError> {
        match target {
            ExecutionTarget::Paper => self
                .engine
                .store()
                .list_open_positions(account_id)
                .map_err(|e| self.api_error(e)),
            ExecutionTarget::Okx(engine) => {
                Ok(engine.positions().into_iter().map(|v| v.position).collect())
            }
        }
    }

    /// Node's `engine.getOpenOrders(accountId)`: both engines keep their orders in the store.
    pub(super) fn venue_open_orders(
        &self,
        target: &ExecutionTarget,
        account_id: &str,
    ) -> Result<Vec<Order>, ApiError> {
        match target {
            ExecutionTarget::Paper => self
                .engine
                .open_orders(account_id)
                .map_err(|e| self.api_error(e)),
            ExecutionTarget::Okx(_) => self
                .engine
                .store()
                .list_open_orders(account_id)
                .map_err(|e| self.api_error(e)),
        }
    }

    fn venue_daily_risk(
        &self,
        target: &ExecutionTarget,
        account_id: &str,
    ) -> Result<DailyRisk, ApiError> {
        match target {
            ExecutionTarget::Paper => self
                .engine
                .daily_risk(account_id)
                .map_err(|e| self.api_error(e)),
            ExecutionTarget::Okx(engine) => {
                let equity = self
                    .engine
                    .store()
                    .list_equity(account_id, 2000)
                    .map_err(|e| self.api_error(e))?;
                engine
                    .daily_risk_from(&equity)
                    .map_err(|e| self.api_error(e))
            }
        }
    }

    /// Node's `currentProtection`: the protection in force, or `None` without an open position.
    pub(super) fn current_protection(
        &self,
        a: &rows::Agent,
        target: &ExecutionTarget,
    ) -> Result<Option<Protection>, ApiError> {
        match target {
            ExecutionTarget::Paper => {
                if self
                    .engine
                    .store()
                    .get_open_position(&a.account_id, &a.symbol)
                    .map_err(|e| self.api_error(e))?
                    .is_none()
                {
                    return Ok(None);
                }
                self.engine
                    .protection_of(&a.account_id, &a.symbol)
                    .map(Some)
                    .map_err(|e| self.api_error(e))
            }
            ExecutionTarget::Okx(engine) => Ok(engine
                .positions()
                .iter()
                .any(|p| p.position.symbol == a.symbol)
                .then(|| engine.protection_of(&a.symbol))),
        }
    }

    pub(super) fn assess(
        &self,
        a: &rows::Agent,
        account: &PaperAccount,
        d: &AiDecisionData,
        quote: Option<Quote>,
        protection: Option<Protection>,
        target: &ExecutionTarget,
    ) -> Result<RiskResult, ApiError> {
        let risk: RiskSettings =
            serde_json::from_str(a.risk.get()).map_err(|e| self.api_error(e))?;
        let daily = self.venue_daily_risk(target, &a.account_id)?;
        let verdict = evaluate_risk(
            &RiskInput {
                decision: d.clone(),
                symbol: a.symbol.clone(),
                settings: effective_risk(&risk, account.live_limits.as_ref()),
                quote,
                now: self.market_now(account.source),
                balance: self.venue_balance(target, &a.account_id)?,
                open_positions: self.venue_open_positions(target, &a.account_id)?,
                daily_pnl: daily.daily_pnl,
                day_start_equity: daily.day_start_equity,
                fees: account.config.clone(),
                allow_trailing_stop: a.allow_trailing_stop,
                protection,
            },
            self.engine.instruments(),
        );
        if let Some(c) = verdict.checks.iter().find(|c| c.id == "input" && !c.passed) {
            return Err(ApiError::bad_request(self.redact(&c.detail)));
        }
        Ok(verdict)
    }

    // ---- acting ---------------------------------------------------------------------------

    /// Node's `execute`: what an approved decision sends. A paper account's act runs here and
    /// the record is settled; an exchange's is returned for the caller to send off the task.
    pub(super) fn execute_decision(
        &mut self,
        a: &rows::Agent,
        d: &AiDecisionData,
        r: &mut Value,
        target: &ExecutionTarget,
    ) -> Result<Option<Outgoing>, ApiError> {
        let act = self.act_for(a, d, r, target)?;
        match target {
            ExecutionTarget::Paper => {
                let acted = self.act_now(act);
                settle_acted(r, acted);
                Ok(None)
            }
            ExecutionTarget::Okx(_) => Ok(Some((self.engine_for(target.clone()), act))),
        }
    }

    /// Node's `toOrder`, or the protection change of a PROTECT merged onto the snapshot it was
    /// judged against.
    fn act_for(
        &self,
        a: &rows::Agent,
        d: &AiDecisionData,
        r: &Value,
        target: &ExecutionTarget,
    ) -> Result<Act, ApiError> {
        let decision_id = r["id"].as_str().map(String::from);
        if d.action == AiAction::Protect {
            let before: Protection = serde_json::from_value(r["protectionBefore"].clone())
                .map_err(|e| self.api_error(e))?;
            return Ok(Act::Protect {
                account_id: a.account_id.clone(),
                symbol: a.symbol.clone(),
                target: target_protection(d, &before),
                meta: ProtectionMeta {
                    client_order_id: format!("protect:{}", r["id"].as_str().unwrap_or_default()),
                    agent_id: Some(a.id.clone()),
                    decision_id,
                },
            });
        }
        let mut request = OrderRequest {
            account_id: a.account_id.clone(),
            client_order_id: format!("agent:{}:{}", a.id, r["id"].as_str().unwrap()),
            symbol: a.symbol.clone(),
            side: if d.action == AiAction::Long {
                OrderSide::Buy
            } else {
                OrderSide::Sell
            },
            order_type: d.order_type,
            quantity: String::new(),
            price: if d.order_type == EntryOrderType::Limit {
                plain(d.entry_price)
            } else {
                None
            },
            leverage: 0,
            reduce_only: None,
            stop_loss: plain(d.stop_loss),
            take_profit: plain(d.take_profit),
            trailing_stop: d
                .trailing_stop
                .as_ref()
                .and_then(|t| t.as_ref())
                .map(trailing),
            agent_id: Some(a.id.clone()),
            decision_id,
        };
        if d.action == AiAction::Close {
            let p = self
                .venue_positions(target, &a.account_id)?
                .into_iter()
                .map(|view| view.position)
                .find(|p| p.symbol == a.symbol)
                .ok_or_else(|| {
                    ApiError::bad_request("The position changed while it was being checked")
                })?;
            request.side = if p.side == PositionSide::Long {
                OrderSide::Sell
            } else {
                OrderSide::Buy
            };
            request.order_type = EntryOrderType::Market;
            request.quantity = p.quantity;
            request.leverage = p.leverage;
            request.reduce_only = Some(true);
            request.price = None;
            request.stop_loss = None;
            request.take_profit = None;
            request.trailing_stop = None;
        } else {
            let risk: RiskResult =
                serde_json::from_value(r["risk"].clone()).map_err(|e| self.api_error(e))?;
            let sized = risk
                .sized
                .ok_or_else(|| ApiError::bad_request("Approved entry has no sizing"))?;
            request.quantity = sized.quantity;
            request.leverage = sized.leverage;
        }
        Ok(Act::Place(request))
    }

    /// A paper act, on the task.
    fn act_now(&mut self, act: Act) -> Acted {
        match act {
            Act::Place(request) => {
                let result = self.engine.place_order(request);
                Acted::Placed(result.map(Box::new).map_err(|e| self.api_error(e)))
            }
            Act::Protect {
                account_id,
                symbol,
                target,
                ..
            } => {
                let result = self.engine.set_protection(&account_id, &symbol, &target);
                // A paper change is one transaction: it is never refused part-way.
                Acted::Protected(result.map_err(|e| ProtectionFailure {
                    error: self.api_error(e),
                    after: None,
                }))
            }
        }
    }

    // ---- suggestions ----------------------------------------------------------------------

    pub fn dismiss_signal(&mut self, id: &str) -> Result<Value, ApiError> {
        let mut r = self.pending_signal(id)?;
        settle(&mut r, "SIGNAL_DISMISSED", Some("Dismissed by user".into()));
        rows::save_decision(self.connection(), &r).map_err(|e| self.api_error(e))?;
        self.invalidate(&[mochi_domain::Topic::Decisions]);
        Ok(r)
    }
    fn pending_signal(&self, id: &str) -> Result<Value, ApiError> {
        let r = rows::get_decision(self.connection(), id)
            .map_err(|e| self.api_error(e))?
            .ok_or_else(|| ApiError::bad_request("Unknown suggestion"))?;
        if r["outcome"] != "SIGNAL" {
            return Err(ApiError::bad_request(
                "This suggestion is no longer pending",
            ));
        }
        Ok(r)
    }

    /// Node's `executeSignal` up to its engine call. Nothing from the original analysis is
    /// trusted except the idea itself: the risk engine runs again on current prices and balances.
    pub(super) fn begin_signal(&mut self, id: &str) -> Result<SignalStep, ApiError> {
        let mut r = self.pending_signal(id)?;
        let a = self.require_agent(r["agentId"].as_str().unwrap_or_default())?;
        if self.is_agent_busy(&a.id) {
            return Err(ApiError::bad_request(
                "This agent is busy. Try again in a moment.",
            ));
        }
        let account = self.require_account(&a.account_id)?;
        let expired = if r["accountId"] != account.id || r["source"] != json!(account.source) {
            Some("Account or market source changed since this suggestion")
        } else {
            (r["signalExpiresAt"].as_i64().unwrap_or(0) <= self.market_now(account.source))
                .then_some("Expired before it was acted on")
        };
        if let Some(detail) = expired {
            settle(&mut r, "SIGNAL_EXPIRED", Some(detail.into()));
            return self.save_signal(r).map(SignalStep::Done);
        }
        // Node's `execution(accountId)`: a disconnected exchange account throws here.
        let target = self.execution(&account)?;
        let d: AiDecisionData =
            serde_json::from_value(r["decision"].clone()).map_err(|e| self.api_error(e))?;
        let protection = self.current_protection(&a, &target)?;
        let before = serde_json::from_value::<Protection>(r["protectionBefore"].clone()).ok();
        // A PROTECT was judged against the stops it saw; applied over newer ones it could undo
        // them.
        if d.action == AiAction::Protect
            && !matches!((&protection, &before), (Some(x), Some(y)) if same_protection(x, y))
        {
            settle(
                &mut r,
                "SIGNAL_EXPIRED",
                Some("Protection changed since this suggestion".into()),
            );
            return self.save_signal(r).map(SignalStep::Done);
        }
        let risk = self.assess(
            &a,
            &account,
            &d,
            self.market_quote(account.source, &a.symbol),
            protection,
            &target,
        )?;
        r["risk"] = json!(risk);
        if !risk.approved {
            settle(
                &mut r,
                "RISK_REJECTED",
                Some(risk.rejection_reasons.join(" | ")),
            );
            return self.save_signal(r).map(SignalStep::Done);
        }
        match self.execute_decision(&a, &d, &mut r, &target)? {
            None => self.save_signal(r).map(SignalStep::Done),
            Some(outgoing) => {
                let token = format!("signal_{}", uuid::Uuid::new_v4());
                self.agents.active.insert(a.id.clone(), token.clone());
                Ok(SignalStep::Send {
                    record: r,
                    agent_id: a.id,
                    token,
                    outgoing: Box::new(outgoing),
                })
            }
        }
    }

    /// Records the engine's answer to an accepted suggestion and frees the agent.
    pub(super) fn finish_signal(
        &mut self,
        mut record: Value,
        agent_id: &str,
        token: &str,
        acted: Acted,
    ) -> Result<Value, ApiError> {
        if self.agents.active.get(agent_id).map(String::as_str) == Some(token) {
            self.agents.active.remove(agent_id);
        }
        settle_acted(&mut record, acted);
        self.save_signal(record)
    }

    fn save_signal(&mut self, mut r: Value) -> Result<Value, ApiError> {
        r["signalExpiresAt"] = Value::Null;
        rows::save_decision(self.connection(), &r).map_err(|e| self.api_error(e))?;
        self.invalidate_cycle();
        Ok(r)
    }

    pub(super) fn retire_signals(
        &mut self,
        predicate: impl Fn(&Value) -> bool,
        detail: &str,
    ) -> Result<(), ApiError> {
        for raw in rows::list_pending_signals(self.connection()).map_err(|e| self.api_error(e))? {
            let mut r: Value = serde_json::from_str(raw.get()).map_err(|e| self.api_error(e))?;
            if predicate(&r) {
                settle(&mut r, "SIGNAL_EXPIRED", Some(detail.into()));
                rows::save_decision(self.connection(), &r).map_err(|e| self.api_error(e))?;
            }
        }
        self.invalidate(&[mochi_domain::Topic::Decisions]);
        Ok(())
    }
    pub fn retire_all_signals(&mut self) -> Result<(), ApiError> {
        self.retire_signals(|_| true, "Cleared by Stop All")
    }
    pub fn expire_signals(&mut self, source: DataSource) -> Result<(), ApiError> {
        let now = self.market_now(source);
        let src = json!(source);
        let pending =
            rows::list_pending_signals(self.connection()).map_err(|e| self.api_error(e))?;
        let due = pending
            .iter()
            .filter_map(|r| serde_json::from_str::<Value>(r.get()).ok())
            .any(|r| r["source"] == src && r["signalExpiresAt"].as_i64().unwrap_or(0) <= now);
        if !due {
            return Ok(());
        }
        self.retire_signals(
            |r| r["source"] == src && r["signalExpiresAt"].as_i64().unwrap_or(0) <= now,
            "Expired before it was acted on",
        )
    }
    pub(super) fn invalidate_cycle(&mut self) {
        self.invalidate(&[
            mochi_domain::Topic::Decisions,
            mochi_domain::Topic::Accounts,
            mochi_domain::Topic::Orders,
            mochi_domain::Topic::Positions,
            mochi_domain::Topic::Arena,
        ]);
    }
}
