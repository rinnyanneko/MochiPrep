//! Replacing and cancelling protection orders, and closing orders.

use mochi_domain::{
    EntryOrderType, Order, OrderRequest, OrderStatus, OrderType, Position, Protection,
    TrailingStopSpec,
};

use super::engine::PaperExecutionEngine;
use super::store::PaperStore;
use super::types::*;

impl<S: PaperStore> PaperExecutionEngine<S> {
    /// Cancels every protection order on the position and creates the requested ones. `last` is
    /// the last price of the quote in hand: a trailing stop without an activation price starts
    /// tracking there. An unchanged trailing stop keeps the extreme it had tracked, so restating it
    /// (to change only the stop-loss, say) never loosens its trigger or puts an activated one back
    /// to sleep.
    pub(super) fn replace_protection(
        &self,
        position: &Position,
        protection: &Protection,
        last: &str,
        ts: i64,
    ) -> Result<(), EngineError> {
        // Outer None: no matching trail rested (TS `undefined`); inner None: it was not active.
        let mut carried: Option<Option<String>> = None;
        for mut order in self.store.list_open_orders(&position.account_id)? {
            if order.symbol != position.symbol || !order.close_position {
                continue;
            }
            if let (OrderType::TrailingStopMarket, Some(wanted)) =
                (order.order_type, &protection.trailing_stop)
            {
                if same_trailing(&trailing_spec_of(&order)?, wanted)? {
                    carried = Some(order.trailing_extreme.clone());
                }
            }
            self.close_order(&mut order, None, ts)?;
        }
        let make = |order_type: OrderType,
                    trigger: Option<&str>,
                    trailing: Option<(&TrailingStopSpec, Option<String>)>|
         -> Result<(), EngineError> {
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
                trailing_stop: None,
                agent_id: None,
                decision_id: None,
            };
            let mut order = Order {
                order_type,
                trigger_price: trigger.map(str::to_string),
                close_position: true,
                ..self.build_order(&request, ts)
            };
            if let Some((spec, extreme)) = trailing {
                order.trailing_kind = Some(spec.kind);
                order.trailing_callback = Some(spec.callback.clone());
                order.trailing_activation_price = spec.activation_price.clone();
                order.trailing_extreme = extreme;
            }
            self.store.insert_order(&order)?;
            self.queue(EngineEvent::Order { order });
            Ok(())
        };
        if let Some(stop) = given(&protection.stop_loss) {
            make(OrderType::StopMarket, Some(stop), None)?;
        }
        if let Some(target) = given(&protection.take_profit) {
            make(OrderType::TakeProfitMarket, Some(target), None)?;
        }
        if let Some(spec) = &protection.trailing_stop {
            let extreme = match carried {
                Some(extreme) => extreme,
                None if spec.activation_price.is_none() => Some(dec(last)?.to_fixed()),
                None => None,
            };
            make(OrderType::TrailingStopMarket, None, Some((spec, extreme)))?;
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
}
