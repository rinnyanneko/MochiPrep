//! Bringing local rows and the cached account in line with the exchange.

use std::collections::{HashMap, HashSet};

use crate::venue::{
    nullish, strict, truthy, AlgoKind, VenueAdapter, VenueAlgo, VenueField, VenueOrder,
    VenuePosition,
};
use mochi_domain::{EquityPoint, Fill, Liquidity, Order, OrderStatus, PositionSide};

use super::protection::{
    is_reached_exit, position_trade_id, reached_exit_unconfirmed, record_attached, Held,
};
use super::{
    dec, is_leg, EngineError, ExchangeEngine, Registry, EQUITY_EVERY_MS, NEVER_ARRIVED_AFTER_MS,
};

/// `d(value || 0)`.
fn dec_or_zero(field: &Option<VenueField>) -> Result<mochi_decimal::Dec, EngineError> {
    dec(truthy(field).unwrap_or("0"))
}

/// Which protection changes a reconciliation may send to OKX. Order lookups and
/// local bookkeeping happen in every mode.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum VenueWrites {
    /// Deferred exits, covering algos and cleanup cancels.
    All,
    /// Covering algos and cleanup cancels, but no deferred exit.
    NoDeferred,
    /// Nothing: the readback only observes.
    None,
}

impl VenueWrites {
    pub(super) fn submit_deferred(self) -> bool {
        self == Self::All
    }

    pub(super) fn maintain_algos(self) -> bool {
        self != Self::None
    }

    /// A second pass after an exit never submits another deferred exit.
    pub(super) fn without_deferred(self) -> Self {
        match self {
            Self::All => Self::NoDeferred,
            other => other,
        }
    }
}

#[derive(Clone)]
pub(super) struct PositionSnapshot {
    pub(super) open: Vec<VenuePosition>,
    pub(super) held: HashMap<String, Held>,
    pub(super) post_exit_unverified: bool,
}

impl PositionSnapshot {
    pub(super) fn update_positions(&mut self, snapshot: Self) {
        self.open = snapshot.open;
        self.held = snapshot.held;
    }

    pub(super) fn new(
        venue: &dyn VenueAdapter,
        rows: Vec<VenuePosition>,
    ) -> Result<Self, EngineError> {
        let mut open = Vec::new();
        let mut held = HashMap::new();
        for row in rows {
            let size = dec_or_zero(&row.size)?;
            if size.is_zero() {
                continue;
            }
            let (side, contracts) = if size.is_negative() {
                (PositionSide::Short, size.neg())
            } else {
                (PositionSide::Long, size)
            };
            let inst_id = nullish(&row.symbol)
                .map(|id| venue.instrument_id(id))
                .unwrap_or_default();
            if isolated_net(&row) {
                held.insert(
                    inst_id,
                    Held {
                        side,
                        contracts,
                        trade_id: position_trade_id(&row),
                        position_id: super::views::position_id(venue, &row, side),
                        updated_at: nullish(&row.updated_ms).map(str::to_owned),
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

    /// Every open position as it would be published, cross-margin ones included, in a
    /// stable order. Two readbacks with the same identity describe the same account.
    pub(super) fn identity(&self, venue: &dyn VenueAdapter) -> Vec<[Option<String>; 5]> {
        let mut identity: Vec<_> = self
            .open
            .iter()
            .map(|row| {
                [
                    nullish(&row.symbol).map(|id| venue.instrument_id(id)),
                    nullish(&row.margin_mode).map(str::to_owned),
                    nullish(&row.position_side).map(str::to_owned),
                    nullish(&row.size).map(str::to_owned),
                    nullish(&row.updated_ms).map(str::to_owned),
                ]
            })
            .collect();
        identity.sort();
        identity
    }

    fn protection_keys(
        &self,
        venue: &dyn VenueAdapter,
        registry: &Registry,
    ) -> HashMap<String, String> {
        let mut keys = HashMap::new();
        let mut seen = HashSet::new();
        for row in &self.open {
            if !isolated_net(row) {
                continue;
            }
            let inst_id = nullish(&row.symbol)
                .map(|id| venue.instrument_id(id))
                .unwrap_or_default();
            let Some(spec) = registry.symbol_of_inst_id(venue, &inst_id) else {
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

pub(super) fn isolated_net(row: &VenuePosition) -> bool {
    strict(&row.margin_mode) == Some("isolated") && strict(&row.position_side) == Some("net")
}

impl ExchangeEngine {
    pub(super) async fn pending_protection(&self) -> Result<Vec<VenueAlgo>, EngineError> {
        let venue = self.deps.venue.as_ref();
        let (conditional, oco, trailing) = tokio::try_join!(
            venue.pending_algos(AlgoKind::Conditional),
            venue.pending_algos(AlgoKind::Oco),
            venue.pending_algos(AlgoKind::Trailing),
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
        let client_id = self.deps.venue.client_order_id(&order.client_order_id);
        let remote = self.deps.venue.get_order(&order.symbol, &client_id).await?;
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
                                .venue
                                .recent_fills(&order.symbol)
                                .await
                                .map_err(|error| {
                                    EngineError::message(format!(
                                        "Could not verify deferred market exit: {error}"
                                    ))
                                })?;
                        if fills.len() >= self.deps.venue.recent_fills_page()
                            || fills
                                .iter()
                                .any(|row| strict(&row.client_order_id) == Some(client_id.as_str()))
                        {
                            return Ok(());
                        }
                    }
                }
                let name = self.deps.venue.name();
                let reason = if is_reached_exit(order) {
                    reached_exit_unconfirmed(name)
                } else {
                    super::order_unconfirmed(name)
                };
                self.expire_unconfirmed_order(order, reason, at).await?;
            }
            return Ok(());
        };
        let observed = order.clone();
        let mut row = observed.clone();
        let spec = registry.get(&row.symbol)?;
        row.exchange_order_id = nullish(&remote.exchange_order_id).map(str::to_owned);
        row.reject_reason = None;
        let filled = dec_or_zero(&remote.filled_size)?.mul(&dec(&spec.venue_contract_value)?);
        row.filled_quantity = filled.to_fixed();
        if let Some(average) = truthy(&remote.average_price) {
            row.avg_fill_price = Some(average.to_owned());
        }
        row.updated_at = at;
        match strict(&remote.state) {
            Some("filled") => row.status = OrderStatus::Filled,
            Some("canceled" | "mmp_canceled") => row.status = OrderStatus::Canceled,
            // Live or partly filled: still working on OKX.
            _ => row.status = OrderStatus::Open,
        }
        let execution = if filled.gt(&super::zero()) && row.agent_id.is_some() {
            super::history::order_execution(&remote)
        } else {
            None
        };
        let attached =
            row.status != OrderStatus::Open && filled.gt(&super::zero()) && !row.reduce_only;
        // A final status is only saved with its fill: only OPEN orders are synced, so a filled
        // order without its fill row would drop out of the ledger, History and Arena for good.
        // The order stays as it was and is looked up again on the next sync.
        let fill = match fill_of(self.deps.venue.as_ref(), &row, &remote, &filled, at) {
            Some(Ok(fill)) => Some(fill),
            Some(Err(error)) => return Err(error),
            None => None,
        };
        // One transaction: a final status saved without its fill or protection rows would never
        // be looked at again, since only OPEN orders are synced.
        let updated = self
            .rows(move |rows| {
                let mut current = None;
                rows.transaction(&mut || {
                    // While a connection candidate waits on OKX, the active session may
                    // already confirm this order. Keep the newer local result in that case.
                    current = rows.get_order(&row.id)?;
                    let unchanged = current.as_ref() == Some(&observed);
                    if unchanged {
                        rows.save_order(&row)?;
                        if let Some((trade_id, at)) = &execution {
                            rows.record_exchange_execution(&row.id, trade_id, *at)?;
                        }
                        if attached {
                            record_attached(rows, &row, &filled, at)?;
                        }
                    }
                    if let Some(fill) = &fill {
                        // The other session may have stored a terminal state without a
                        // usable fill. Complete it only if both views agree on the execution.
                        let same_execution = current.as_ref().is_some_and(|saved| {
                            saved.status == row.status
                                && saved.filled_quantity == row.filled_quantity
                                && row
                                    .exchange_order_id
                                    .as_deref()
                                    .is_some_and(|id| !id.is_empty())
                                && saved.exchange_order_id == row.exchange_order_id
                                // A known average price must also agree, or the fill
                                // would contradict the saved order.
                                && saved.avg_fill_price.as_deref().is_none_or(|saved| {
                                    row.avg_fill_price.as_deref().is_some_and(|seen| {
                                        dec(saved)
                                            .ok()
                                            .zip(dec(seen).ok())
                                            .is_some_and(|(saved, seen)| saved.eq_value(&seen))
                                    })
                                })
                        });
                        if (unchanged || same_execution)
                            && rows.list_fills_by_order(&row.id)?.is_empty()
                        {
                            rows.insert_fill(fill)?;
                        }
                    }
                    Ok(())
                })?;
                if current.as_ref() == Some(&observed) {
                    Ok(Some(row))
                } else {
                    Ok(current)
                }
            })
            .await?;
        if let Some(updated) = updated {
            *order = updated;
        }
        Ok(())
    }

    pub(super) async fn expire_unconfirmed_order(
        &self,
        row: &mut Order,
        reason: String,
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
                        order.reject_reason = Some(reason.clone());
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
        self.sync_with(VenueWrites::All).await
    }

    /// Read back an unanswered placement during an edit, or after a failed
    /// deferred recovery, without dispatching deferred exits.
    pub(super) async fn sync_without_deferred_orders(&self) -> Result<(), EngineError> {
        self.sync_with(VenueWrites::NoDeferred).await
    }

    /// A connection candidate's first snapshot. The current session still owns the
    /// account's algos, and a failed connect must leave them as they were.
    pub(super) async fn sync_read_only(&self) -> Result<(), EngineError> {
        self.sync_with(VenueWrites::None).await
    }

    async fn sync_with(&self, writes: VenueWrites) -> Result<(), EngineError> {
        let result = self.reconcile(writes).await;
        if let Err(error) = &result {
            self.state().sync_error = Some(error.to_string());
        }
        result
    }

    async fn reconcile(&self, writes: VenueWrites) -> Result<(), EngineError> {
        let _ = self.finalize_unsent_rejections().await;
        let account_id = self.deps.account_id.clone();
        let name = self.deps.venue.name();
        let (reached_unconfirmed, order_unconfirmed) = (
            reached_exit_unconfirmed(name),
            super::order_unconfirmed(name),
        );
        let (open, registry) = {
            let account_id = account_id.clone();
            let reasons = [reached_unconfirmed.clone(), order_unconfirmed.clone()];
            self.rows(move |rows| {
                let mut pending = rows.list_open_orders(&account_id)?;
                // A locally retired target stays lookup-only. Protection can
                // wait on matching fill evidence while lookup catches up.
                const RECOVERY_BATCH: usize = 10;
                for reason in &reasons {
                    pending.extend(rows.list_canceled_orders_for_recovery(
                        &account_id,
                        reason,
                        RECOVERY_BATCH,
                    )?);
                }
                Ok((pending, Registry(rows.instruments())))
            })
            .await?
        };
        // Orders first, so an entry seen filling here has its protection rows before algos are
        // matched. An order whose answer cannot be recorded (its fill write keeps failing, say)
        // stays OPEN and is tried again next sync, nothing resent; it fails this sync, but not
        // before the other orders, positions and protection are brought in line. Only an OKX or
        // shutdown failure stops the pass at once.
        let mut order_error = self.dispatch().restore_order.is_some().then(|| {
            EngineError::message(format!(
                "An unsent {name} protection order is awaiting recovery"
            ))
        });
        // An order refused before dispatch has nothing at OKX to look up.
        for mut order in open
            .into_iter()
            .filter(|o| !is_leg(o) && !self.is_unsent_rejection(&o.id))
        {
            let reached_exit = is_reached_exit(&order);
            let recovering = (order.status == OrderStatus::Canceled)
                .then_some(order.reject_reason.as_deref())
                .flatten()
                .filter(|reason| *reason == reached_unconfirmed || *reason == order_unconfirmed)
                .map(str::to_owned);
            let mut answered = false;
            match self.sync_order(&mut order, &registry).await {
                Ok(()) => answered = true,
                Err(error @ EngineError::Stopped) => return Err(error),
                // A recovery lookup must not stop the readback of orders still open.
                Err(error @ EngineError::Venue(_)) if !reached_exit && recovering.is_none() => {
                    return Err(error)
                }
                Err(error) => {
                    order_error.get_or_insert(EngineError::Message(format!(
                        "Could not record {name} order {}: {error}",
                        order.id
                    )));
                }
            }
            if let Some(reason) = recovering {
                let checked_at = self.now();
                // Only a lookup that OKX answered with "no such order" can end the
                // recovery: any record of the order would have changed this row. The
                // window starts at the local expiry, the row's last update.
                if answered
                    && order.status == OrderStatus::Canceled
                    && order.reject_reason.as_deref() == Some(order_unconfirmed.as_str())
                    && checked_at.saturating_sub(order.updated_at) >= super::UNCONFIRMED_LOOKUP_MS
                {
                    // Long past any delay in OKX's records: OKX never had this order.
                    if let Err(error) = self
                        .expire_unconfirmed_order(
                            &mut order,
                            super::order_not_arrived(name),
                            checked_at,
                        )
                        .await
                    {
                        if matches!(error, EngineError::Stopped) {
                            return Err(error);
                        }
                        order_error.get_or_insert(error);
                    }
                    continue;
                }
                // Move even an unanswered lookup behind orders that have not had a turn.
                // Keep this checkpoint across restarts without changing the order's status.
                if let Err(error) = self
                    .rows(move |rows| {
                        Ok(rows.mark_order_recovery_checked(&order.id, &reason, checked_at)?)
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
        let venue = self.deps.venue.as_ref();
        let (usdt, remote, algos) = tokio::join!(
            venue.balance(),
            venue.positions(),
            self.pending_protection(),
        );
        let mut usdt = usdt?;
        let mut snapshot = PositionSnapshot::new(venue, remote?)?;
        let pre_protection = snapshot.clone();
        let protection = match algos {
            Ok(algos) => {
                self.reconcile_protection(algos, &mut snapshot, &mut usdt, writes)
                    .await
            }
            Err(error) => Err(error),
        };
        // A market exit can change both balance and positions. If either
        // readback failed, keep the coherent pre-exit view and report it stale.
        if snapshot.post_exit_unverified {
            snapshot = pre_protection;
        }
        let keys = snapshot.protection_keys(venue, &registry);
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
            let shared = self.deps.venue.clone();
            self.rows(move |rows| {
                let venue = shared.as_ref();
                let registry = Registry(rows.instruments());
                let mut positions = Vec::new();
                let mut open_results = Vec::new();
                for p in &snapshot.open {
                    open_results.extend(super::history::open_result_of(venue, p, &registry)?);
                    positions.extend(Self::to_view(
                        venue,
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
    venue: &dyn VenueAdapter,
    order: &Order,
    remote: &VenueOrder,
    filled: &mochi_decimal::Dec,
    at: i64,
) -> Option<Result<Fill, EngineError>> {
    if order.status == OrderStatus::Open || !filled.gt(&super::zero()) {
        return None;
    }
    Some(build_fill(venue, order, remote, filled, at))
}

fn build_fill(
    venue: &dyn VenueAdapter,
    order: &Order,
    remote: &VenueOrder,
    filled: &mochi_decimal::Dec,
    at: i64,
) -> Result<Fill, EngineError> {
    let ord_id = nullish(&remote.exchange_order_id).unwrap_or("undefined");
    // OKX sends an empty avgPx until something fills; a fill must never get an invented or
    // empty price.
    let price = truthy(&remote.average_price)
        .filter(|price| dec(price).is_ok_and(|price| price.gt(&super::zero())))
        .map(str::to_owned)
        .ok_or_else(|| {
            EngineError::Message(format!("{} reported a fill without a price", venue.name()))
        })?;
    let fee = dec_or_zero(&remote.fee)?;
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
        realized_pnl: truthy(&remote.realized_pnl).unwrap_or("0").to_owned(),
        liquidity: Liquidity::Taker,
        ts: at,
    })
}
