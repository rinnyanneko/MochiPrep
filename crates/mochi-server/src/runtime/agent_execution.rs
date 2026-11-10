//! The only AI-to-order path: validated decisions and a current-state risk verdict.
use super::Runtime;
use crate::error::ApiError;
use mochi_core::{
    paper::PaperStore,
    risk::{evaluate_risk, RiskInput},
};
use mochi_decimal::Dec;
use mochi_domain::{
    AiAction, AiDecisionData, AiTrailingStop, DataSource, EntryOrderType, OrderRequest, OrderSide,
    OrderStatus, PaperAccount, PositionSide, Protection, Quote, RiskResult, RiskSettings,
    TrailingStopSpec,
};
use mochi_store::rows;
use serde_json::{json, Value};

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
impl Runtime {
    pub(super) fn current_protection(
        &self,
        a: &rows::Agent,
    ) -> Result<Option<Protection>, ApiError> {
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
    pub(super) fn assess(
        &self,
        a: &rows::Agent,
        account: &PaperAccount,
        d: &AiDecisionData,
        quote: Option<Quote>,
        protection: Option<Protection>,
    ) -> Result<RiskResult, ApiError> {
        let risk: RiskSettings =
            serde_json::from_str(a.risk.get()).map_err(|e| self.api_error(e))?;
        let daily = self
            .engine
            .daily_risk(&a.account_id)
            .map_err(|e| self.api_error(e))?;
        let verdict = evaluate_risk(
            &RiskInput {
                decision: d.clone(),
                symbol: a.symbol.clone(),
                settings: effective_risk(&risk, account.live_limits.as_ref()),
                quote,
                now: self.market_now(account.source),
                balance: self
                    .engine
                    .balance_of(&a.account_id)
                    .map_err(|e| self.api_error(e))?,
                open_positions: self
                    .engine
                    .store()
                    .list_open_positions(&a.account_id)
                    .map_err(|e| self.api_error(e))?,
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
    pub(super) fn execute_decision(
        &mut self,
        a: &rows::Agent,
        d: &AiDecisionData,
        r: &mut Value,
    ) -> Result<(), ApiError> {
        self.assert_can_run(a)?;
        if d.action == AiAction::Protect {
            let before: Protection = serde_json::from_value(r["protectionBefore"].clone())
                .map_err(|e| self.api_error(e))?;
            let target = target_protection(d, &before);
            match self
                .engine
                .set_protection(&a.account_id, &a.symbol, &target)
            {
                Err(e) => settle(r, "ORDER_REJECTED", Some(self.redact(&e.to_string()))),
                Ok(result) => {
                    r["protectionBefore"] = json!(result.before);
                    r["protectionAfter"] = json!(result.after);
                    if same_protection(&result.before, &result.after) {
                        settle(r, "HOLD", Some("Protection already as requested".into()));
                    } else {
                        settle(r, "EXECUTED", Some("Protection updated".into()));
                    }
                }
            }
            return Ok(());
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
            decision_id: r["id"].as_str().map(String::from),
        };
        if d.action == AiAction::Close {
            let p = self
                .engine
                .store()
                .get_open_position(&a.account_id, &a.symbol)
                .map_err(|e| self.api_error(e))?
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
        match self.engine.place_order(request) {
            Err(e) => settle(r, "ORDER_REJECTED", Some(self.redact(&e.to_string()))),
            Ok(result) => {
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
        }
        Ok(())
    }
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
    pub fn execute_signal(&mut self, id: &str) -> Result<Value, ApiError> {
        let mut r = self.pending_signal(id)?;
        let a = self.require_agent(r["agentId"].as_str().unwrap_or_default())?;
        if self.is_agent_busy(&a.id) {
            return Err(ApiError::bad_request(
                "This agent is busy. Try again in a moment.",
            ));
        }
        let account = self.require_account(&a.account_id)?;
        self.assert_can_run(&a)?;
        let d: AiDecisionData =
            serde_json::from_value(r["decision"].clone()).map_err(|e| self.api_error(e))?;
        if r["signalExpiresAt"].as_i64().unwrap_or(0) <= self.market_now(account.source) {
            settle(
                &mut r,
                "SIGNAL_EXPIRED",
                Some("Expired before it was acted on".into()),
            );
        } else {
            let protection = self.current_protection(&a)?;
            let before = serde_json::from_value::<Protection>(r["protectionBefore"].clone()).ok();
            if d.action == AiAction::Protect
                && !matches!((&protection,&before),(Some(x),Some(y)) if same_protection(x,y))
            {
                settle(
                    &mut r,
                    "SIGNAL_EXPIRED",
                    Some("Protection changed since this suggestion".into()),
                );
            } else {
                let risk = self.assess(
                    &a,
                    &account,
                    &d,
                    self.market_quote(account.source, &a.symbol),
                    protection,
                )?;
                r["risk"] = json!(risk);
                if !risk.approved {
                    settle(
                        &mut r,
                        "RISK_REJECTED",
                        Some(risk.rejection_reasons.join(" | ")),
                    );
                } else {
                    self.execute_decision(&a, &d, &mut r)?;
                }
            }
        }
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
