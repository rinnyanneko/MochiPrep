//! Read models over the paper books: balance, position views, open orders, daily risk, and the
//! equity history point. A port of the read-model section of `packages/core/src/paper/engine.ts`.

use mochi_domain::{
    AccountBalance, EquityPoint, LedgerEventType, Order, OrderType, PositionView, TrailingStopView,
};
use serde::Serialize;

use super::engine::{
    dec, math_error, money, trailing_spec_of, zero, EngineError, PaperExecutionEngine,
};
use super::store::PaperStore;
use super::trailing::trailing_trigger;
use crate::perp;

const DAY_MS: i64 = 86_400_000;

/// Loss since 00:00 UTC and the equity the day started with, for the daily-loss limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyRisk {
    pub daily_pnl: String,
    pub day_start_equity: String,
}

impl<S: PaperStore> PaperExecutionEngine<S> {
    pub fn open_orders(&self, account_id: &str) -> Result<Vec<Order>, EngineError> {
        Ok(self.store.list_open_orders(account_id)?)
    }

    /// Balances with open PnL marked at the latest quote. A position with no quote counts as
    /// flat, since there is no price to mark it at.
    pub fn balance_of(&self, account_id: &str) -> Result<AccountBalance, EngineError> {
        let account = self.require_account(account_id)?;
        let mut order_margin = zero();
        for order in self.store.list_open_orders(account_id)? {
            order_margin = order_margin.add(&dec(&order.reserved_margin)?);
        }
        let mut unrealized = zero();
        for position in self.store.list_open_positions(account_id)? {
            if let Some(quote) = (self.get_quote)(account.source, &position.symbol) {
                let pnl = perp::pnl(
                    position.side,
                    &dec(&position.entry_price)?,
                    &dec(&quote.mark)?,
                    &dec(&position.quantity)?,
                );
                unrealized = unrealized.add(&pnl);
            }
        }
        let wallet = dec(&account.wallet_balance)?;
        let available = wallet.sub(&dec(&account.used_margin)?).sub(&order_margin);
        Ok(AccountBalance {
            account_id: account_id.to_string(),
            initial_balance: account.initial_balance,
            wallet_balance: account.wallet_balance,
            available_balance: available.to_fixed(),
            used_margin: account.used_margin,
            order_margin: order_margin.to_fixed(),
            equity: money(&wallet.add(&unrealized)).to_fixed(),
            realized_pnl: account.realized_pnl,
            unrealized_pnl: money(&unrealized).to_fixed(),
            fees_paid: account.fees_paid,
            funding_paid: account.funding_paid,
        })
    }

    /// Open positions with their mark, open PnL and the first stop, target and trailing stop
    /// protecting each.
    pub fn position_views(&self, account_id: &str) -> Result<Vec<PositionView>, EngineError> {
        let account = self.require_account(account_id)?;
        let orders = self.store.list_open_orders(account_id)?;
        let mut views = Vec::new();
        for position in self.store.list_open_positions(account_id)? {
            let quote = (self.get_quote)(account.source, &position.symbol);
            let protection = |order_type| {
                orders
                    .iter()
                    .find(|o| o.symbol == position.symbol && o.order_type == order_type)
                    .and_then(|o| o.trigger_price.clone())
            };
            let unrealized_pnl = match &quote {
                Some(quote) => money(&perp::pnl(
                    position.side,
                    &dec(&position.entry_price)?,
                    &dec(&quote.mark)?,
                    &dec(&position.quantity)?,
                ))
                .to_fixed(),
                None => "0".to_string(),
            };
            let stop_loss = protection(OrderType::StopMarket);
            let take_profit = protection(OrderType::TakeProfitMarket);
            let trailing = orders.iter().find(|o| {
                o.symbol == position.symbol && o.order_type == OrderType::TrailingStopMarket
            });
            let trailing_stop = match trailing {
                None => None,
                Some(order) => {
                    let spec = trailing_spec_of(order)?;
                    let extreme = order.trailing_extreme.clone();
                    let trigger_price = match &extreme {
                        None => None,
                        Some(extreme) => {
                            let tick = dec(&self.instrument(&position.symbol)?.price_tick)?;
                            let trigger =
                                trailing_trigger(position.side, &spec, &dec(extreme)?, &tick)
                                    .map_err(|e| math_error("the trailing trigger", e))?;
                            Some(trigger.to_fixed())
                        }
                    };
                    Some(TrailingStopView {
                        spec,
                        extreme,
                        trigger_price,
                    })
                }
            };
            views.push(PositionView {
                mark_price: quote.map(|q| q.mark),
                unrealized_pnl,
                stop_loss,
                take_profit,
                trailing_stop,
                external_protection: Vec::new(),
                position,
            });
        }
        Ok(views)
    }

    /// Loss since 00:00 UTC, from the ledger plus open PnL, for the daily-loss limit. Deposits
    /// are not trading results, so they are left out.
    pub fn daily_risk(&self, account_id: &str) -> Result<DailyRisk, EngineError> {
        let account = self.require_account(account_id)?;
        let now = (self.now)(account.source);
        // `Math.floor(now / 86_400_000) * 86_400_000`, in floating point as JS does.
        let day_start = (now as f64 / DAY_MS as f64).floor() as i64 * DAY_MS;
        let mut changed = zero();
        for event in self.store.list_ledger(account_id, Some(day_start))? {
            if event.event_type != LedgerEventType::Deposit {
                changed = changed.add(&dec(&event.amount)?);
            }
        }
        let balance = self.balance_of(account_id)?;
        Ok(DailyRisk {
            daily_pnl: changed.add(&dec(&balance.unrealized_pnl)?).to_fixed(),
            day_start_equity: dec(&account.wallet_balance)?.sub(&changed).to_fixed(),
        })
    }

    /// Writes this moment's equity to the history; a second call at the same `now` replaces it.
    pub fn record_equity(&mut self, account_id: &str) -> Result<(), EngineError> {
        let account = self.require_account(account_id)?;
        let balance = self.balance_of(account_id)?;
        self.store.upsert_equity_point(&EquityPoint {
            account_id: account_id.to_string(),
            ts: (self.now)(account.source),
            equity: balance.equity,
            wallet: balance.wallet_balance,
            unrealized: balance.unrealized_pnl,
        })?;
        Ok(())
    }
}
