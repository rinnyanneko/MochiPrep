//! Bringing local rows and the cached account in line with OKX.

use mochi_domain::jsnum::js_string_to_number;
use std::collections::{HashMap, HashSet};

use mochi_domain::{EquityPoint, Fill, Liquidity, Order, OrderStatus, PositionSide};
use serde_json::Value;

use super::protection::{
    is_reached_exit, last_trade_id, record_attached, Held, REACHED_EXIT_UNCONFIRMED,
};
use super::{
    dec, is_leg, EngineError, OkxEngine, Registry, EQUITY_EVERY_MS, NEVER_ARRIVED_AFTER_MS,
};
use crate::client::{nullish_text, truthy_text, PendingAlgoType, Row, RECENT_FILLS_LIMIT};
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

#[derive(Clone)]
pub(super) struct PositionSnapshot {
    pub(super) open: Vec<Row>,
    pub(super) held: HashMap<String, Held>,
    pub(super) post_exit_unverified: bool,
}

impl PositionSnapshot {
    pub(super) fn update_positions(&mut self, snapshot: Self) {
        self.open = snapshot.open;
        self.held = snapshot.held;
    }

    pub(super) fn new(rows: Vec<Row>) -> Result<Self, EngineError> {
        let mut open = Vec::new();
        let mut held = HashMap::new();
        for row in rows {
            let size = dec_or_zero(row.get("pos"))?;
            if size.is_zero() {
                continue;
            }
            let (side, contracts) = if size.is_negative() {
                (PositionSide::Short, size.neg())
            } else {
                (PositionSide::Long, size)
            };
            let inst_id = nullish_text(row.get("instId")).unwrap_or_default();
            if isolated_net(&row) {
                held.insert(
                    inst_id,
                    Held {
                        side,
                        contracts,
                        trade_id: last_trade_id(&row),
                        position_id: super::views::position_id(&row, side),
                        updated_at: nullish_text(row.get("uTime")),
                    },
                );
            }
            open.push(row);
        }
        Ok(Self {
            open,
            held,
            post_exit_unverified: false,
        })
    }

    fn protection_keys(&self, registry: &Registry) -> HashMap<String, String> {
        let mut keys = HashMap::new();
        let mut seen = HashSet::new();
        for row in &self.open {
            if !isolated_net(row) {
                continue;
            }
            let inst_id = nullish_text(row.get("instId")).unwrap_or_default();
            let Some(spec) = registry.symbol_of_inst_id(&inst_id) else {
                continue;
            };
            if !seen.insert(&spec.symbol) {
                keys.remove(&spec.symbol);
            } else {
                if let Some(held) = self.held.get(&inst_id) {
                    if held.trade_id.is_some() {
                        keys.insert(spec.symbol.clone(), held.position_id.clone());
                    }
                }
            }
        }
        keys
    }
}

pub(super) fn isolated_net(row: &Row) -> bool {
    row.get("mgnMode").and_then(Value::as_str) == Some("isolated")
        && row.get("posSide").and_then(Value::as_str) == Some("net")
}

impl OkxEngine {
    pub(super) async fn pending_protection(&self) -> Result<Vec<Row>, EngineError> {
        let client = &self.deps.client;
        let (conditional, oco, trailing) = tokio::try_join!(
            client.get_pending_algos(PendingAlgoType::Conditional),
            client.get_pending_algos(PendingAlgoType::Oco),
            client.get_pending_algos(PendingAlgoType::MoveOrderStop),
        )?;
        Ok(conditional.into_iter().chain(oco).chain(trailing).collect())
    }

    /// Brings one local order in line with OKX. A lookup that fails is an error: the order is
    /// left as it is, never given up on.
    pub(super) async fn sync_order(
        &self,
        order: &mut Order,
        registry: &Registry,
    ) -> Result<(), EngineError> {
        let inst_id = to_inst_id(&order.symbol);
        let client_id = to_cl_ord_id(&order.client_order_id);
        let remote = self.deps.client.get_order(&inst_id, &client_id).await?;
        let at = self.now();
        let Some(remote) = remote else {
            // Only OKX's own "order does not exist", and only once the order has had time to
            // arrive, closes it.
            let age = at.saturating_sub(order.created_at);
            if age >= NEVER_ARRIVED_AFTER_MS && order.status == OrderStatus::Open {
                if is_reached_exit(order) {
                    // A fill can be visible before the original order lookup catches up.
                    // Require complete recent evidence before retiring a never-seen exit.
                    const RECENT_FILL_WINDOW: i64 = 3 * 24 * 60 * 60 * 1_000;
                    if order
                        .exchange_order_id
                        .as_deref()
                        .is_some_and(|id| !id.is_empty())
                    {
                        return Ok(());
                    }
                    if age < RECENT_FILL_WINDOW {
                        let fills =
                            self.deps
                                .client
                                .recent_fills(&inst_id)
                                .await
                                .map_err(|error| {
                                    EngineError::message(format!(
                                        "Could not verify deferred market exit: {error}"
                                    ))
                                })?;
                        if fills.len() >= RECENT_FILLS_LIMIT
                            || fills.iter().any(|row| {
                                row.get("clOrdId").and_then(Value::as_str)
                                    == Some(client_id.as_str())
                            })
                        {
                            return Ok(());
                        }
                    }
                }
                let reason = if is_reached_exit(order) {
                    REACHED_EXIT_UNCONFIRMED
                } else {
                    "OKX has no record of this order"
                };
                self.expire_unconfirmed_order(order, reason, at).await?;
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
            _ => order.status = OrderStatus::Open,
        }
        let execution = if filled.gt(&super::zero()) && order.agent_id.is_some() {
            super::history::execution_of(&remote)
        } else {
            None
        };
        let row = order.clone();
        let attached =
            row.status != OrderStatus::Open && filled.gt(&super::zero()) && !row.reduce_only;
        // As in Node, the order is saved even when its fill then cannot be built.
        let (fill, unbuilt) = match fill_of(order, &remote, &filled, at) {
            Some(Ok(fill)) => (Some(fill), None),
            Some(Err(error)) => (None, Some(error)),
            None => (None, None),
        };
        // One transaction: a final status saved without its fill or protection rows would never
        // be looked at again, since only OPEN orders are synced.
        self.rows(move |rows| {
            rows.transaction(&mut || {
                rows.save_order(&row)?;
                if let Some((trade_id, at)) = &execution {
                    rows.record_exchange_execution(&row.id, trade_id, *at)?;
                }
                if attached {
                    record_attached(rows, &row, &filled, at)?;
                }
                if let Some(fill) = &fill {
                    if rows.list_fills_by_order(&row.id)?.is_empty() {
                        rows.insert_fill(fill)?;
                    }
                }
                Ok(())
            })?;
            unbuilt.map_or(Ok(()), Err)
        })
        .await
    }

    pub(super) async fn expire_unconfirmed_order(
        &self,
        row: &mut Order,
        reason: &'static str,
        at: i64,
    ) -> Result<(), EngineError> {
        let observed = row.clone();
        let current = self
            .rows(move |rows| {
                let mut current = None;
                rows.transaction(&mut || {
                    current = rows.get_order(&observed.id)?;
                    if let Some(order) = current.as_mut().filter(|order| **order == observed) {
                        // A connection's readback can overlap the active session restoring
                        // a known-unsent order. Its late lookup may only expire what it saw.
                        order.status = OrderStatus::Canceled;
                        order.reject_reason = Some(reason.into());
                        order.updated_at = at;
                        rows.save_order(order)?;
                    }
                    Ok(())
                })?;
                Ok(current)
            })
            .await?;
        if let Some(current) = current {
            *row = current;
        }
        Ok(())
    }

    /// Full reconciliation, inside the queue. On failure the error is kept and the account
    /// reads as unhealthy until a sync succeeds.
    pub(super) async fn sync_now(&self) -> Result<(), EngineError> {
        self.sync_with_deferred_protection(true).await
    }

    /// Read back an unanswered placement during an edit, or take a connection's
    /// first snapshot, without dispatching deferred exits.
    pub(super) async fn sync_without_deferred_orders(&self) -> Result<(), EngineError> {
        self.sync_with_deferred_protection(false).await
    }

    async fn sync_with_deferred_protection(
        &self,
        submit_deferred: bool,
    ) -> Result<(), EngineError> {
        let result = self.reconcile(submit_deferred).await;
        if let Err(error) = &result {
            self.state().sync_error = Some(error.to_string());
        }
        result
    }

    async fn reconcile(&self, submit_deferred: bool) -> Result<(), EngineError> {
        let account_id = self.deps.account_id.clone();
        let (open, registry) = {
            let account_id = account_id.clone();
            self.rows(move |rows| {
                let mut pending = rows.list_open_orders(&account_id)?;
                // A locally retired target stays lookup-only. Protection can
                // wait on matching fill evidence while lookup catches up.
                const RECOVERY_BATCH: usize = 10;
                pending.extend(rows.list_canceled_orders_for_recovery(
                    &account_id,
                    REACHED_EXIT_UNCONFIRMED,
                    RECOVERY_BATCH,
                )?);
                Ok((pending, Registry(rows.instruments())))
            })
            .await?
        };
        // Orders first, so an entry seen filling here has its protection rows before algos are
        // matched. An order whose answer cannot be recorded (its fill write keeps failing, say)
        // stays OPEN and is tried again next sync, nothing resent; it fails this sync, but not
        // before the other orders, positions and protection are brought in line. Only an OKX or
        // shutdown failure stops the pass at once.
        let mut order_error =
            self.dispatch().restore_order.is_some().then(|| {
                EngineError::message("An unsent OKX protection order is awaiting recovery")
            });
        for mut order in open.into_iter().filter(|o| !is_leg(o)) {
            let reached_exit = is_reached_exit(&order);
            let recovering = order.status == OrderStatus::Canceled
                && order.reject_reason.as_deref() == Some(REACHED_EXIT_UNCONFIRMED);
            match self.sync_order(&mut order, &registry).await {
                Ok(()) => {}
                Err(error @ EngineError::Stopped) => return Err(error),
                Err(error @ EngineError::Okx(_)) if !reached_exit => return Err(error),
                Err(error) => {
                    order_error.get_or_insert(EngineError::Message(format!(
                        "Could not record OKX order {}: {error}",
                        order.id
                    )));
                }
            }
            if recovering {
                // Move even an unanswered lookup behind orders that have not had a turn.
                // Keep this checkpoint across restarts without changing the order's status.
                let checked_at = self.now();
                if let Err(error) = self
                    .rows(move |rows| {
                        Ok(rows.mark_order_recovery_checked(
                            &order.id,
                            REACHED_EXIT_UNCONFIRMED,
                            checked_at,
                        )?)
                    })
                    .await
                {
                    if matches!(error, EngineError::Stopped) {
                        return Err(error);
                    }
                    // A checkpoint failure must not hide a position that has already closed.
                    order_error.get_or_insert(error);
                }
            }
        }
        let client = &self.deps.client;
        let (usdt, remote, algos) = tokio::join!(
            client.get_usdt_balance(),
            client.get_positions(),
            self.pending_protection(),
        );
        let mut usdt = usdt?;
        let mut snapshot = PositionSnapshot::new(remote?)?;
        let pre_protection = snapshot.clone();
        let protection = match algos {
            Ok(algos) => {
                self.reconcile_protection(algos, &mut snapshot, &mut usdt, submit_deferred)
                    .await
            }
            Err(error) => Err(error),
        };
        // A market exit can change both balance and positions. If either
        // readback failed, keep the coherent pre-exit view and report it stale.
        if snapshot.post_exit_unverified {
            snapshot = pre_protection;
        }
        let keys = snapshot.protection_keys(&registry);
        let protection = match protection {
            Ok(protection) => protection,
            Err(error) => {
                // A failed protection read does not invalidate the new positions. Last known
                // protection belongs only to the exact same observed trade and position side.
                order_error.get_or_insert(error);
                let state = self.state();
                state
                    .protection
                    .iter()
                    .filter(|(symbol, _)| {
                        keys.get(*symbol)
                            .is_some_and(|key| state.protection_keys.get(*symbol) == Some(key))
                    })
                    .map(|(symbol, protection)| (symbol.clone(), protection.clone()))
                    .collect()
            }
        };
        let saved_protection = protection.clone();
        let (positions, open_results, balance) = {
            let account_id = account_id.clone();
            let now = self.now();
            self.rows(move |rows| {
                let registry = Registry(rows.instruments());
                let mut positions = Vec::new();
                let mut open_results = Vec::new();
                for p in &snapshot.open {
                    open_results.extend(super::history::open_result_of(p, &registry)?);
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
                Ok((positions, open_results, balance))
            })
            .await?
        };
        let at = self.now();
        let point = {
            let mut state = self.state();
            state.positions = positions;
            state.protection = saved_protection;
            state.protection_keys = keys;
            state.open_results = open_results;
            state.balance = Some(balance.clone());
            state.last_sync_at = Some(at);
            // Set here, in the same step, so the account never reads as healthy in between.
            state.sync_error = order_error.as_ref().map(ToString::to_string);
            (order_error.is_none() && at - state.last_equity_at >= EQUITY_EVERY_MS).then(|| {
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
        order_error.map_or(Ok(()), Err)
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
