//! Bringing local rows and the cached account in line with OKX.

use mochi_domain::jsnum::js_string_to_number;
use std::collections::HashMap;

use mochi_domain::{EquityPoint, Fill, Liquidity, Order, OrderStatus, PositionSide};
use serde_json::Value;

use super::protection::record_attached;
use super::{
    dec, is_leg, EngineError, OkxEngine, Registry, EQUITY_EVERY_MS, NEVER_ARRIVED_AFTER_MS,
};
use crate::client::{nullish_text, truthy_text, PendingAlgoType, Row};
use crate::ids::{to_cl_ord_id, to_inst_id};

/// Node's `Number(value)` for an OKX field.
pub(super) fn js_number(value: Option<&Value>) -> f64 {
    match value {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(flag)) => f64::from(u8::from(*flag)),
        Some(Value::Number(number)) => number.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(text)) => js_string_to_number(text),
        Some(_) => f64::NAN,
    }
}

/// `d(value || 0)`.
fn dec_or_zero(value: Option<&Value>) -> Result<mochi_decimal::Dec, EngineError> {
    dec(&truthy_text(value).unwrap_or_else(|| "0".into()))
}

impl OkxEngine {
    /// Brings one local order in line with OKX. A lookup that fails is an error: the order is
    /// left as it is, never given up on.
    pub(super) async fn sync_order(
        &self,
        order: &mut Order,
        registry: &Registry,
    ) -> Result<(), EngineError> {
        let remote = self
            .deps
            .client
            .get_order(
                &to_inst_id(&order.symbol),
                &to_cl_ord_id(&order.client_order_id),
            )
            .await?;
        let at = self.now();
        let Some(remote) = remote else {
            // Only OKX's own "order does not exist", and only once the order has had time to
            // arrive, closes it.
            if at - order.created_at >= NEVER_ARRIVED_AFTER_MS {
                order.status = OrderStatus::Canceled;
                order.reject_reason = Some("OKX has no record of this order".into());
                order.updated_at = at;
                let row = order.clone();
                self.rows(move |rows| Ok(rows.save_order(&row)?)).await?;
            }
            return Ok(());
        };
        let spec = registry.get(&order.symbol)?;
        order.exchange_order_id = nullish_text(remote.get("ordId"));
        order.reject_reason = None;
        let filled = dec_or_zero(remote.get("accFillSz"))?.mul(&dec(&spec.venue_contract_value)?);
        order.filled_quantity = filled.to_fixed();
        if let Some(average) = truthy_text(remote.get("avgPx")) {
            order.avg_fill_price = Some(average);
        }
        order.updated_at = at;
        match remote.get("state").and_then(Value::as_str) {
            Some("filled") => order.status = OrderStatus::Filled,
            Some("canceled" | "mmp_canceled") => order.status = OrderStatus::Canceled,
            // Live or partly filled: still working on OKX.
            _ => {}
        }
        let row = order.clone();
        // As in Node, the order is saved even when its fill then cannot be recorded.
        let fill = fill_of(order, &remote, &filled, at);
        let attached =
            row.status != OrderStatus::Open && filled.gt(&super::zero()) && !row.reduce_only;
        self.rows(move |rows| {
            rows.save_order(&row)?;
            if attached {
                record_attached(rows, &row, &filled, at)?;
            }
            if let Some(fill) = fill {
                if rows.list_fills_by_order(&row.id)?.is_empty() {
                    rows.insert_fill(&fill?)?;
                }
            }
            Ok(())
        })
        .await
    }

    /// Full reconciliation, inside the queue. On failure the error is kept and the account
    /// reads as unhealthy until a sync succeeds.
    pub(super) async fn sync_now(&self) -> Result<(), EngineError> {
        let result = self.reconcile().await;
        if let Err(error) = &result {
            self.state().sync_error = Some(error.to_string());
        }
        result
    }

    async fn reconcile(&self) -> Result<(), EngineError> {
        let account_id = self.deps.account_id.clone();
        let (open, registry) = {
            let account_id = account_id.clone();
            self.rows(move |rows| {
                Ok((
                    rows.list_open_orders(&account_id)?,
                    Registry(rows.instruments()),
                ))
            })
            .await?
        };
        // Orders first, so an entry seen filling here has its protection rows before algos are
        // matched.
        for mut order in open.into_iter().filter(|o| !is_leg(o)) {
            self.sync_order(&mut order, &registry).await?;
        }
        let client = &self.deps.client;
        let (usdt, remote, conditional, oco, trailing) = tokio::try_join!(
            client.get_usdt_balance(),
            client.get_positions(),
            client.get_pending_algos(PendingAlgoType::Conditional),
            client.get_pending_algos(PendingAlgoType::Oco),
            client.get_pending_algos(PendingAlgoType::MoveOrderStop),
        )?;
        let open: Vec<Row> = remote
            .into_iter()
            .filter(|p| js_number(p.get("pos")) != 0.0)
            .collect();
        let held: HashMap<String, PositionSide> = open
            .iter()
            .map(|p| {
                let side = if js_number(p.get("pos")) > 0.0 {
                    PositionSide::Long
                } else {
                    PositionSide::Short
                };
                (nullish_text(p.get("instId")).unwrap_or_default(), side)
            })
            .collect();
        let algos = conditional.into_iter().chain(oco).chain(trailing).collect();
        let protection = self.reconcile_protection(algos, &held).await?;
        self.state().protection = protection.clone();
        let (positions, balance) = {
            let account_id = account_id.clone();
            let now = self.now();
            self.rows(move |rows| {
                let registry = Registry(rows.instruments());
                let mut positions = Vec::new();
                for p in &open {
                    positions.extend(Self::to_view(
                        p,
                        rows,
                        &registry,
                        &account_id,
                        now,
                        &protection,
                    )?);
                }
                let balance = Self::to_balance(usdt.as_ref(), rows, &account_id)?;
                Ok((positions, balance))
            })
            .await?
        };
        let at = self.now();
        let point = {
            let mut state = self.state();
            state.positions = positions;
            state.balance = Some(balance.clone());
            state.last_sync_at = Some(at);
            state.sync_error = None;
            (at - state.last_equity_at >= EQUITY_EVERY_MS).then(|| {
                state.last_equity_at = at;
                EquityPoint {
                    account_id: account_id.clone(),
                    ts: at,
                    equity: balance.equity.clone(),
                    wallet: balance.wallet_balance.clone(),
                    unrealized: balance.unrealized_pnl.clone(),
                }
            })
        };
        if let Some(point) = point {
            self.rows(move |rows| Ok(rows.upsert_equity_point(&point)?))
                .await?;
        }
        Ok(())
    }
}

/// The fill for an order that is done and filled at least partly, `None` otherwise. Whether one
/// is already recorded is checked against the store.
fn fill_of(
    order: &Order,
    remote: &Row,
    filled: &mochi_decimal::Dec,
    at: i64,
) -> Option<Result<Fill, EngineError>> {
    if order.status == OrderStatus::Open || !filled.gt(&super::zero()) {
        return None;
    }
    Some(build_fill(order, remote, filled, at))
}

fn build_fill(
    order: &Order,
    remote: &Row,
    filled: &mochi_decimal::Dec,
    at: i64,
) -> Result<Fill, EngineError> {
    let ord_id = nullish_text(remote.get("ordId")).unwrap_or_else(|| "undefined".into());
    // Node binds `remote.avgPx` as is; SQLite cannot store an absent price.
    let price = nullish_text(remote.get("avgPx"))
        .ok_or_else(|| EngineError::Message("OKX reported a fill without a price".into()))?;
    let fee = dec_or_zero(remote.get("fee"))?;
    let fee = if fee.is_negative() { fee.neg() } else { fee };
    Ok(Fill {
        id: format!("fill_okx_{ord_id}"),
        order_id: order.id.clone(),
        account_id: order.account_id.clone(),
        symbol: order.symbol.clone(),
        side: order.side,
        quantity: filled.to_fixed(),
        price,
        fee: fee.to_fixed(),
        realized_pnl: truthy_text(remote.get("pnl")).unwrap_or_else(|| "0".into()),
        liquidity: Liquidity::Taker,
        ts: at,
    })
}
