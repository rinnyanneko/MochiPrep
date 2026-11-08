//! The quote-driven half of the paper engine: funding, liquidation, protection triggers and
//! resting limit fills. A port of `onQuote` and its helpers in `packages/core/src/paper/engine.ts`.
//!
//! As in `engine.rs`, each function keeps the TypeScript order of operations and of id draws,
//! because the rows and events must match the Node engine's.

use mochi_decimal::Dec;
use mochi_domain::{
    EntryOrderType, Fill, LedgerEventType, Liquidity, Order, OrderRequest, OrderStatus, OrderType,
    PaperAccount, Position, PositionSide, PositionStatus, Quote,
};

use super::engine::{
    closing_side, dec, math_error, money, zero, Attempt, EngineError, EngineEvent,
    PaperExecutionEngine, Posting,
};
use super::store::PaperStore;
use crate::{fill_model, perp};

/// `d(value!)` on an optional string. decimal.js throws exactly this for `null`.
fn required(value: &Option<String>) -> Result<Dec, EngineError> {
    dec(value.as_deref().unwrap_or("null"))
}

/// Stops first, then targets, then limits: the pessimistic order `onQuote` evaluates them in.
fn rank(order_type: OrderType) -> u8 {
    match order_type {
        OrderType::StopMarket | OrderType::TrailingStopMarket => 0,
        OrderType::TakeProfitMarket => 1,
        OrderType::Limit => 2,
        OrderType::Market => 3,
    }
}

impl<S: PaperStore> PaperExecutionEngine<S> {
    /// Advances the simulation by one observed quote. Order of evaluation is fixed and
    /// pessimistic: funding, liquidation, stop-loss, take-profit, resting limits. Gaps between
    /// quotes are never interpolated, so a stop fills at the first quote past it.
    ///
    /// On an error, events already queued by committed steps stay queued until the next flush,
    /// as they do in TypeScript.
    pub fn on_quote(&mut self, quote: &Quote) -> Result<(), EngineError> {
        let instrument = self.instrument(&quote.symbol)?;
        let interval = Self::funding_interval_ms(instrument)?;

        for position in self
            .store
            .list_open_positions_for_market(quote.source, &quote.symbol)?
        {
            self.apply_funding(position, quote, interval)?;
        }

        for position in self
            .store
            .list_open_positions_for_market(quote.source, &quote.symbol)?
        {
            let mark = dec(&quote.mark)?;
            let threshold = dec(&position.liquidation_price)?;
            let liquidated = match position.side {
                PositionSide::Long => mark.lte(&threshold),
                PositionSide::Short => mark.gte(&threshold),
            };
            if liquidated {
                self.liquidate(position, quote)?;
            }
        }

        let mut open = self
            .store
            .list_open_orders_for_market(quote.source, &quote.symbol)?;
        // A stable sort, like Array.prototype.sort, so equal keys keep the store's order.
        open.sort_by_key(|o| (rank(o.order_type), o.created_at));
        for candidate in open {
            // An earlier order in this pass may have closed the position and canceled this one.
            if let Some(order) = self.store.get_order(&candidate.id)? {
                if order.status == OrderStatus::Open {
                    self.evaluate_order(order, quote)?;
                }
            }
        }
        self.flush();
        Ok(())
    }

    /// Pays or receives funding for every boundary passed since the last one, at this quote's
    /// rate and mark. Without a rate the boundary still passes, with nothing paid.
    fn apply_funding(
        &self,
        mut position: Position,
        quote: &Quote,
        interval: i64,
    ) -> Result<(), EngineError> {
        let mut next = position.last_funding_time + interval;
        if next > quote.ts {
            return Ok(());
        }
        let mut account = self.require_account(&position.account_id)?;
        self.store.transaction(|| {
            while next <= quote.ts {
                let payment = match &quote.funding_rate {
                    None => zero(),
                    Some(rate) => money(&perp::funding(
                        position.side,
                        &dec(&quote.mark)?,
                        &dec(&position.quantity)?,
                        &dec(rate)?,
                    )),
                };
                let memo = format!(
                    "Funding at rate {}, mark {}",
                    quote.funding_rate.as_deref().unwrap_or("unavailable"),
                    quote.mark
                );
                let mut funding = Posting::new(LedgerEventType::Funding, memo, next);
                funding.amount = payment.neg();
                funding.symbol = Some(position.symbol.clone());
                funding.position_id = Some(position.id.clone());
                self.post(&mut account, funding)?;
                position.funding_paid = dec(&position.funding_paid)?.add(&payment).to_fixed();
                position.last_funding_time = next;
                if !payment.is_zero() {
                    self.queue(EngineEvent::Funding {
                        account_id: account.id.clone(),
                        symbol: position.symbol.clone(),
                        amount: payment.neg().to_fixed(),
                    });
                }
                next += interval;
            }
            self.store.save_position(&position)?;
            self.store.save_account(&account)?;
            Ok(())
        })
    }

    /// Simplified isolated liquidation: the position is force-closed at its liquidation price
    /// and the whole isolated margin is forfeited. Loss is capped at that margin, even through
    /// a gap.
    fn liquidate(&self, mut position: Position, quote: &Quote) -> Result<(), EngineError> {
        let mut account = self.require_account(&position.account_id)?;
        let instrument = self.instrument(&position.symbol)?;
        let ts = quote.ts;
        let margin = dec(&position.margin)?;
        let quantity = dec(&position.quantity)?;
        let price = dec(&position.liquidation_price)?
            .round_to_step(&dec(&instrument.price_tick)?)
            .map_err(|e| math_error("the liquidation fill price", e))?;
        let pnl = money(&perp::pnl(
            position.side,
            &dec(&position.entry_price)?,
            &price,
            &quantity,
        ))
        .max(&margin.neg());
        let closing = closing_side(position.side);

        self.store.transaction(|| {
            let request = OrderRequest {
                account_id: account.id.clone(),
                client_order_id: format!("liquidation:{}", position.id),
                symbol: position.symbol.clone(),
                side: closing,
                order_type: EntryOrderType::Market,
                quantity: position.quantity.clone(),
                price: None,
                leverage: position.leverage,
                reduce_only: Some(true),
                stop_loss: None,
                take_profit: None,
                trailing_stop: None,
                agent_id: None,
                decision_id: None,
            };
            let order = Order {
                status: OrderStatus::Filled,
                filled_quantity: position.quantity.clone(),
                avg_fill_price: Some(price.to_fixed()),
                ..self.build_order(&request, ts)
            };
            self.store.insert_order(&order)?;
            let fill = Fill {
                id: self.next_id("fill"),
                order_id: order.id.clone(),
                account_id: account.id.clone(),
                symbol: position.symbol.clone(),
                side: closing,
                quantity: position.quantity.clone(),
                price: price.to_fixed(),
                fee: "0".to_string(),
                realized_pnl: margin.neg().to_fixed(),
                liquidity: Liquidity::Liquidation,
                ts,
            };
            self.store.insert_fill(&fill)?;

            let posting = |event_type, memo: String| Posting {
                symbol: Some(position.symbol.clone()),
                order_id: Some(order.id.clone()),
                fill_id: Some(fill.id.clone()),
                position_id: Some(position.id.clone()),
                ..Posting::new(event_type, memo, ts)
            };
            let memo = format!("Liquidated @ {} (mark {})", price.to_fixed(), quote.mark);
            let mut realized = posting(LedgerEventType::RealizedPnl, memo);
            realized.amount = pnl.clone();
            let memo = "Remaining isolated margin forfeited".to_string();
            let mut penalty = posting(LedgerEventType::LiquidationPenalty, memo);
            penalty.amount = margin.add(&pnl).neg();
            let memo = "Margin released on liquidation".to_string();
            let mut released = posting(LedgerEventType::MarginRelease, memo);
            released.margin_delta = margin.neg();
            self.post(&mut account, realized)?;
            self.post(&mut account, penalty)?;
            self.post(&mut account, released)?;

            position.status = PositionStatus::Liquidated;
            position.quantity = "0".to_string();
            position.margin = "0".to_string();
            position.realized_pnl = dec(&position.realized_pnl)?.sub(&margin).to_fixed();
            position.exit_price = Some(price.to_fixed());
            position.closed_at = Some(ts);
            self.store.save_position(&position)?;
            self.cancel_protection(&account.id, &position.symbol, None, ts)?;
            self.store.save_account(&account)?;
            self.queue(EngineEvent::Fill { fill, order });
            self.queue(EngineEvent::Liquidation {
                position: position.clone(),
            });
            Ok(())
        })
    }

    /// Fills a resting limit that this quote reaches, or a protection order it triggers.
    fn evaluate_order(&self, mut order: Order, quote: &Quote) -> Result<(), EngineError> {
        let mut account = self.require_account(&order.account_id)?;
        let instrument = self.instrument(&order.symbol)?;
        let position = self
            .store
            .get_open_position(&order.account_id, &order.symbol)?;

        if order.order_type == OrderType::Limit {
            let limit = required(&order.price)?;
            let fills = fill_model::is_resting_limit_filled(order.side, &limit, quote)
                .map_err(|e| math_error("whether the limit fills", e))?;
            if !fills {
                return Ok(());
            }
            let quantity = if order.reduce_only {
                let held = match &position {
                    Some(p) => dec(&p.quantity)?,
                    None => zero(),
                };
                dec(&order.quantity)?.min(&held)
            } else {
                dec(&order.quantity)?
            };
            return self.fill_resting(
                &mut account,
                order,
                &quantity,
                &limit,
                Liquidity::Maker,
                quote.ts,
            );
        }

        let Some(position) = position else {
            self.close_order(&mut order, Some("Position closed"), quote.ts)?;
            return Ok(());
        };
        let is_long = position.side == PositionSide::Long;
        let trigger = required(&order.trigger_price)?;
        let last = dec(&quote.last)?;
        let triggered = match order.order_type {
            OrderType::StopMarket => fill_model::is_stop_triggered(is_long, &trigger, &last),
            // TS's ternary sends everything that is not a stop to the take-profit test; LIMIT
            // returned above, and a MARKET order never rests.
            _ => fill_model::is_take_profit_triggered(is_long, &trigger, &last),
        };
        if !triggered {
            return Ok(());
        }
        let quantity = dec(&position.quantity)?;
        let price = fill_model::market_fill_price(
            order.side,
            &quantity,
            quote,
            &account.config,
            instrument,
        )
        .map_err(|e| math_error("the market fill price", e))?;
        self.fill_resting(
            &mut account,
            order,
            &quantity,
            &price,
            Liquidity::Taker,
            quote.ts,
        )
    }

    /// Fills an order that was resting. A rejection rolls the fill back and cancels the order
    /// with the reason; any other failure propagates.
    fn fill_resting(
        &self,
        account: &mut PaperAccount,
        mut order: Order,
        quantity: &Dec,
        price: &Dec,
        liquidity: Liquidity,
        ts: i64,
    ) -> Result<(), EngineError> {
        let outcome = self.store.transaction(|| -> Result<(), Attempt> {
            order.reserved_margin = "0".to_string();
            order.quantity = quantity.to_fixed();
            self.store.save_order(&order)?;
            self.fill_order(account, &mut order, quantity, price, liquidity, ts)?;
            Ok(())
        });
        let Err(failure) = outcome else {
            return Ok(());
        };
        // The rolled-back fill never happened.
        self.queued.borrow_mut().retain(
            |e| !matches!(e, EngineEvent::Fill { order: filled, .. } if filled.id == order.id),
        );
        let rejection = match failure {
            Attempt::Rejected(rejection) => rejection,
            Attempt::Failed(e) => return Err(e),
        };
        let mut fresh = self
            .store
            .get_order(&order.id)?
            .ok_or_else(|| EngineError::Invalid(format!("Unknown order {}", order.id)))?;
        self.close_order(&mut fresh, Some(&rejection.message()), ts)?;
        self.queue(EngineEvent::Order { order: fresh });
        Ok(())
    }
}
