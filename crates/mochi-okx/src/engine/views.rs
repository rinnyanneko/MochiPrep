//! What OKX reports, as MochiPrep's balance and position views.

use mochi_domain::{
    AccountBalance, OrderStatus, Position, PositionSide, PositionStatus, PositionView,
    TrailingStopView,
};

use super::protection::{effective_of, ProtectionMap};
use super::sync::js_number;
use super::{dec, zero, EngineError, LiveRows, OkxEngine, Registry};
use crate::client::{nullish_text, truthy_text, Row};

/// Node's `Number(p.lever) || 1`. Leverage is whole in MochiPrep, so a fractional OKX value is
/// rounded down, and never below 1.
fn leverage_of(position: &Row) -> u32 {
    let lever = js_number(position.get("lever"));
    if lever.is_nan() || lever < 1.0 {
        1
    } else {
        lever.min(f64::from(u32::MAX)) as u32
    }
}

impl OkxEngine {
    /// One open OKX position. A pair missing from the registry is not shown.
    pub(super) fn to_view(
        p: &Row,
        rows: &dyn LiveRows,
        registry: &Registry,
        account_id: &str,
        now: i64,
        protection: &ProtectionMap,
    ) -> Result<Vec<PositionView>, EngineError> {
        let inst_id = nullish_text(p.get("instId")).unwrap_or_default();
        let Some(spec) = registry.symbol_of_inst_id(&inst_id) else {
            return Ok(Vec::new());
        };
        let symbol = spec.symbol.clone();
        let in_force = effective_of(protection, &symbol);
        let read_back = protection.get(&symbol);
        let pos = nullish_text(p.get("pos")).unwrap_or_else(|| "undefined".into());
        let size = dec(&pos)?;
        let size = if size.is_negative() { size.neg() } else { size };
        let quantity = size.mul(&dec(&spec.venue_contract_value)?).to_fixed();
        let entry = rows
            .list_orders(account_id, 200)?
            .into_iter()
            .find(|o| o.symbol == symbol && !o.reduce_only && o.status == OrderStatus::Filled);
        let side = if js_number(p.get("pos")) > 0.0 {
            PositionSide::Long
        } else {
            PositionSide::Short
        };
        Ok(vec![PositionView {
            position: Position {
                id: format!("okx:{inst_id}"),
                account_id: account_id.to_string(),
                symbol,
                side,
                quantity: quantity.clone(),
                entry_price: nullish_text(p.get("avgPx")).unwrap_or_default(),
                leverage: leverage_of(p),
                margin: truthy_text(p.get("margin")).unwrap_or_else(|| "0".into()),
                liquidation_price: truthy_text(p.get("liqPx")).unwrap_or_else(|| "0".into()),
                status: PositionStatus::Open,
                realized_pnl: "0".into(),
                fees_paid: "0".into(),
                funding_paid: "0".into(),
                max_quantity: quantity,
                exit_price: None,
                last_funding_time: now,
                opened_at: entry.map_or(now, |o| o.created_at),
                closed_at: None,
            },
            mark_price: truthy_text(p.get("markPx")),
            unrealized_pnl: truthy_text(p.get("upl")).unwrap_or_else(|| "0".into()),
            stop_loss: in_force.stop_loss,
            take_profit: in_force.take_profit,
            // OKX does not report the tracked extreme, only the current trigger.
            trailing_stop: in_force.trailing_stop.map(|spec| TrailingStopView {
                spec,
                extreme: None,
                trigger_price: read_back.and_then(|e| e.trail_trigger.clone()),
            }),
            external_protection: read_back.map(|e| e.external.clone()).unwrap_or_default(),
        }])
    }

    /// The USDT balance OKX reports, with realized PnL and fees summed from our own fills.
    pub(super) fn to_balance(
        usdt: Option<&Row>,
        rows: &dyn LiveRows,
        account_id: &str,
    ) -> Result<AccountBalance, EngineError> {
        let field = |key: &str| usdt.and_then(|row| truthy_text(row.get(key)));
        let equity = dec(&field("eq").unwrap_or_else(|| "0".into()))?;
        let unrealized = dec(&field("upl").unwrap_or_else(|| "0".into()))?;
        let available = dec(&field("availEq")
            .or_else(|| field("availBal"))
            .unwrap_or_else(|| "0".into()))?;
        let fills = rows.list_fills(account_id, 5000)?;
        let (mut realized, mut fees) = (zero(), zero());
        for fill in &fills {
            realized = realized.add(&dec(&fill.realized_pnl)?);
            fees = fees.add(&dec(&fill.fee)?);
        }
        let initial_balance = rows
            .get_account(account_id)?
            .map_or_else(|| "0".to_string(), |a| a.initial_balance);
        Ok(AccountBalance {
            account_id: account_id.to_string(),
            initial_balance,
            wallet_balance: equity.sub(&unrealized).to_fixed(),
            available_balance: available.to_fixed(),
            used_margin: equity.sub(&available).to_fixed(),
            order_margin: "0".into(),
            equity: equity.to_fixed(),
            realized_pnl: realized.to_fixed(),
            unrealized_pnl: unrealized.to_fixed(),
            fees_paid: fees.to_fixed(),
            funding_paid: "0".into(),
        })
    }
}
