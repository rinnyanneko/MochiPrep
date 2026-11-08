//! Fills and their ledger postings.

use mochi_decimal::Dec;
use mochi_domain::jsnum::js_number_string;
use mochi_domain::{
    Fill, LedgerEvent, LedgerEventType, Liquidity, Order, OrderStatus, PaperAccount, Position,
    PositionStatus,
};

use super::engine::PaperExecutionEngine;
use super::store::PaperStore;
use super::types::*;
use crate::perp;

impl<S: PaperStore> PaperExecutionEngine<S> {
    /// reject after writing, and only a rollback undoes those writes. `last` is the last price of
    /// the quote the fill came from; a trailing stop attached on fill starts there.
    // One more argument than clippy likes; each is a distinct input of the TS function.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fill_order(
        &self,
        account: &mut PaperAccount,
        order: &mut Order,
        quantity: &Dec,
        price: &Dec,
        liquidity: Liquidity,
        last: &str,
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
                || given(&order.attached_take_profit).is_some()
                || order.attached_trailing_stop.is_some();
            if let (Some(position), true) = (&position, protected) {
                self.replace_protection(position, &attached_protection(order), last, ts)?;
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
