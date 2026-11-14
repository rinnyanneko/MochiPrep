//! Node's `preTradeRefusal`: the last line of defence. Every order passes here, from the UI or
//! from an agent. The checks run in Node's order and the texts are Node's, word for word.

use mochi_core::paper::validate_trailing;
use mochi_decimal::Dec;
use mochi_domain::jsnum::{js_number_string, js_to_fixed};
use mochi_domain::{Order, OrderSide, OrderType, PositionSide, PositionView};

use super::{day_start_equity, dec, is_leg, side_text, EngineError, OkxEngine, Prepared};
use crate::ids::to_inst_id;

impl OkxEngine {
    pub(super) fn pre_trade_refusal(
        &self,
        order: &Order,
        position: Option<&PositionView>,
        prepared: &Prepared,
    ) -> Result<Option<String>, EngineError> {
        // An exit only needs a position to reduce: no arming, sync, quote or limit checks, so a
        // disarmed or unhealthy account can always get out.
        if order.reduce_only {
            let same_side = position.is_some_and(|p| {
                (p.position.side == PositionSide::Long) == (order.side == OrderSide::Buy)
            });
            if position.is_none() || same_side {
                return Ok(Some("REDUCE_ONLY_NO_POSITION: nothing to reduce".into()));
            }
            return Ok(None);
        }
        let limits = &prepared.limits;
        if !(self.deps.is_armed)() {
            return Ok(Some(
                "NOT_ARMED: real-money orders are switched off for this account".into(),
            ));
        }
        let (healthy, balance, sync_error, open_positions) = {
            let healthy = self.healthy();
            let state = self.state();
            (
                healthy,
                state.balance.clone(),
                state.sync_error.clone(),
                state.positions.len(),
            )
        };
        let Some(balance) = balance.filter(|_| healthy) else {
            let why = sync_error.unwrap_or_else(|| "no recent sync with OKX".into());
            return Ok(Some(format!("EXCHANGE_NOT_SYNCED: {why}")));
        };

        let Some(quote) = (self.deps.get_quote)(&order.symbol) else {
            return Ok(Some("NO_MARKET_DATA: no quote".into()));
        };
        let age = (self.now() - quote.ts) as f64 / 1000.0;
        if age > limits.max_quote_age_sec {
            return Ok(Some(format!(
                "MARKET_DATA_STALE: quote is {}s old",
                js_to_fixed(age, 0)
            )));
        }

        if f64::from(order.leverage) > limits.max_leverage {
            return Ok(Some(format!(
                "LIMIT_LEVERAGE: {}x exceeds {}x",
                order.leverage,
                js_number_string(limits.max_leverage)
            )));
        }
        if let Some(position) = position {
            return Ok(Some(format!(
                "LIMIT_EXISTING_POSITION: already {} {}",
                side_text(position.position.side),
                order.symbol
            )));
        }
        // The previous position's algos would otherwise count as the new one's protection, or
        // close it.
        let leftover = self.state().leftovers.contains(&to_inst_id(&order.symbol))
            || prepared
                .open_orders
                .iter()
                .any(|o| o.symbol == order.symbol && is_leg(o));
        if leftover {
            return Ok(Some(
                "PROTECTION_PENDING: protection from the previous position is still being cleaned up"
                    .into(),
            ));
        }
        if open_positions as f64 >= limits.max_open_positions {
            return Ok(Some(format!(
                "LIMIT_POSITIONS: {open_positions} open, limit {}",
                js_number_string(limits.max_open_positions)
            )));
        }

        let reference = match (order.order_type, order.side) {
            (OrderType::Limit, _) => dec(order.price.as_deref().unwrap_or("null"))?,
            (_, OrderSide::Buy) => dec(&quote.ask)?,
            (_, OrderSide::Sell) => dec(&quote.bid)?,
        };
        let stop = order
            .attached_stop_loss
            .as_deref()
            .filter(|s| !s.is_empty());
        if stop.is_none() && limits.require_stop_loss {
            return Ok(Some(
                "LIMIT_STOP_REQUIRED: new positions must carry a stop loss".into(),
            ));
        }
        if let Some(stop) = stop {
            let value = dec(stop)?;
            let wrong = match order.side {
                OrderSide::Buy => value.gte(&reference),
                OrderSide::Sell => value.lte(&reference),
            };
            if wrong {
                return Ok(Some(format!(
                    "LIMIT_STOP_INVALID: stop {stop} is on the wrong side of {}",
                    reference.to_fixed()
                )));
            }
        }
        if let Some(trailing) = &order.attached_trailing_stop {
            let side = match order.side {
                OrderSide::Buy => PositionSide::Long,
                OrderSide::Sell => PositionSide::Short,
            };
            let tick = dec(&prepared.registry.get(&order.symbol)?.price_tick)?;
            let refused = validate_trailing(side, &reference, trailing, &tick)
                .map_err(|e| EngineError::Message(e.to_string()))?;
            if let Some(refused) = refused {
                return Ok(Some(format!("INVALID_PROTECTION: {refused}")));
            }
        }

        let notional = dec(&order.quantity)?.mul(&reference);
        if notional.gt(&Dec::from_js_number(limits.max_notional)) {
            return Ok(Some(format!(
                "LIMIT_NOTIONAL: {} USDT exceeds {} USDT",
                notional.to_fixed_dp(2),
                js_number_string(limits.max_notional)
            )));
        }
        let margin = notional
            .div(&dec(&order.leverage.to_string())?)
            .map_err(|_| EngineError::Message("[DecimalError] Division by zero".into()))?;
        if margin.gt(&dec(&balance.available_balance)?) {
            return Ok(Some(format!(
                "INSUFFICIENT_MARGIN: needs {} USDT",
                margin.to_fixed_dp(2)
            )));
        }

        let equity = dec(&balance.equity)?;
        let base = dec(&day_start_equity(
            &prepared.equity,
            self.now(),
            &balance.equity,
        ))?;
        let floor = base
            .mul(&Dec::from_js_number(limits.max_daily_loss_pct))
            .div(&dec("-100")?)
            .map_err(|_| EngineError::Message("[DecimalError] Division by zero".into()))?;
        if equity.sub(&base).lte(&floor) {
            return Ok(Some(format!(
                "LIMIT_DAILY_LOSS: down {} USDT today",
                base.sub(&equity).to_fixed_dp(2)
            )));
        }
        Ok(None)
    }
}
