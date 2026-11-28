//! What the exchange reports, as MochiPrep's balance and position views.

use mochi_domain::{
    AccountBalance, OrderStatus, Position, PositionSide, PositionStatus, PositionView,
    TrailingStopView,
};

use crate::venue::{
    js_number, nullish, strict, truthy, VenueAdapter, VenueBalance, VenueField, VenuePosition,
};

use super::protection::{effective_of, ProtectionMap};
use super::sync::isolated_net;
use super::{dec, zero, EngineError, ExchangeEngine, LiveRows, Registry};

/// Node's `Number(p.lever) || 1`. Leverage is whole in MochiPrep, so a fractional OKX value is
/// rounded down, and never below 1.
pub(super) fn leverage_of(position: &VenuePosition) -> u32 {
    let lever = js_number(&position.leverage);
    if lever.is_nan() || lever < 1.0 {
        1
    } else {
        lever.min(f64::from(u32::MAX)) as u32
    }
}

/// posId can be reused after a close. Include the observed trade and direction so a stale
/// close request cannot select a newer position on the same instrument and margin mode.
pub(super) fn position_id(
    venue: &dyn VenueAdapter,
    row: &VenuePosition,
    side: PositionSide,
) -> String {
    let field = |field| strict(field).unwrap_or_default();
    let side = super::side_text(side);
    format!(
        "okx:{}:{}:{side}:{}:{}",
        strict(&row.symbol)
            .map(|id| venue.instrument_id(id))
            .unwrap_or_default(),
        field(&row.margin_mode),
        field(&row.position_id),
        field(&row.trade_id),
    )
}

pub(super) fn position_side(row: &VenuePosition, negative: bool) -> PositionSide {
    match strict(&row.position_side) {
        Some("short") => PositionSide::Short,
        Some("long") => PositionSide::Long,
        _ if negative => PositionSide::Short,
        _ => PositionSide::Long,
    }
}

impl ExchangeEngine {
    /// One open OKX position. A pair missing from the registry is not shown.
    pub(super) fn to_view(
        venue: &dyn VenueAdapter,
        p: &VenuePosition,
        rows: &dyn LiveRows,
        registry: &Registry,
        account_id: &str,
        now: i64,
        protection: &ProtectionMap,
    ) -> Result<Vec<PositionView>, EngineError> {
        let inst_id = nullish(&p.symbol)
            .map(|id| venue.instrument_id(id))
            .unwrap_or_default();
        let Some(spec) = registry.symbol_of_inst_id(venue, &inst_id) else {
            return Ok(Vec::new());
        };
        let symbol = spec.symbol.clone();
        let read_back = isolated_net(p).then(|| protection.get(&symbol)).flatten();
        let in_force = if read_back.is_some() {
            effective_of(protection, &symbol)
        } else {
            mochi_domain::Protection {
                stop_loss: None,
                take_profit: None,
                trailing_stop: None,
            }
        };
        let size = dec(nullish(&p.size).unwrap_or("undefined"))?;
        let side = position_side(p, size.is_negative());
        let size = if size.is_negative() { size.neg() } else { size };
        let quantity = size.mul(&dec(&spec.venue_contract_value)?).to_fixed();
        let entry = rows
            .list_orders(account_id, 200)?
            .into_iter()
            .find(|o| o.symbol == symbol && !o.reduce_only && o.status == OrderStatus::Filled);
        Ok(vec![PositionView {
            position: Position {
                id: position_id(venue, p, side),
                account_id: account_id.to_string(),
                symbol,
                side,
                quantity: quantity.clone(),
                entry_price: nullish(&p.entry_price).unwrap_or_default().to_owned(),
                leverage: leverage_of(p),
                margin: truthy(&p.margin).unwrap_or("0").to_owned(),
                liquidation_price: truthy(&p.liquidation_price).unwrap_or("0").to_owned(),
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
            margin_mode: nullish(&p.margin_mode).map(str::to_owned),
            close_supported: Some(isolated_net(p)),
            mark_price: truthy(&p.mark_price).map(str::to_owned),
            unrealized_pnl: truthy(&p.unrealized_pnl).unwrap_or("0").to_owned(),
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
        usdt: Option<&VenueBalance>,
        rows: &dyn LiveRows,
        account_id: &str,
    ) -> Result<AccountBalance, EngineError> {
        fn field(
            usdt: Option<&VenueBalance>,
            read: fn(&VenueBalance) -> &Option<VenueField>,
        ) -> Option<&str> {
            usdt.and_then(|row| truthy(read(row)))
        }
        let equity = dec(field(usdt, |b| &b.equity).unwrap_or("0"))?;
        let unrealized = dec(field(usdt, |b| &b.unrealized_pnl).unwrap_or("0"))?;
        let available = dec(field(usdt, |b| &b.available_equity)
            .or_else(|| field(usdt, |b| &b.available_balance))
            .unwrap_or("0"))?;
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
