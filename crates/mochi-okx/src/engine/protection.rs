//! Protection on OKX: stop-loss, take-profit and trailing stop as reduce-only OKX algo orders, a
//! port of the protection half of `packages/exchanges/src/okx-engine.ts`.
//!
//! Two rules hold everywhere here. A position is never left without its stop: every new algo is
//! placed, and accepted, before any old one is cancelled. And an algo MochiPrep did not place (one
//! from the OKX app) is only ever reported, never cancelled.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use mochi_core::paper::{validate_trailing, ProtectionResult};
use mochi_core::protection_change::{
    classify_protection_change, same_price, same_trailing, ProtectionChangeClass,
};
use mochi_decimal::Dec;
use mochi_domain::{
    default_live_limits, ExternalProtection, ExternalProtectionKind, Order, OrderSide, OrderStatus,
    OrderType, PositionSide, PositionView, Protection, RiskSettings, TrailingStopSpec,
    CANCEL_PENDING_PREFIX,
};
use serde_json::Value;

use super::sync::{isolated_net, PositionSnapshot};
use super::{
    align, align_trailing, dec, is_leg, side_text, to_body, to_contracts, EngineError, LiveRows,
    OkxEngine, Registry, NEVER_ARRIVED_AFTER_MS,
};
use crate::client::{
    nullish_text, truthy_text, AlgoOrdType, AlgoOrderBody, CancelAlgo, OkxError, Row, Side, TdMode,
    RECENT_FILLS_LIMIT,
};
use crate::ids::{to_algo_cl_ord_id, to_cl_ord_id, to_inst_id};

/// How far back sync looks for our own algo ids. Older ones read as external, which is the safe
/// side.
const RECENT_ORDERS: usize = 500;
/// How long a position must stay missing before our protection on it is cancelled.
const POSITION_MISSING_MS: i64 = 10_000;
/// Algo states that can still trigger. Anything else, including states OKX may add later, cannot.
const ALIVE_STATES: [&str; 2] = ["live", "partially_effective"];
const REPLACED: &str = "Replaced";
const REMOVED: &str = "Removed";
const POSITION_CLOSED: &str = "Position closed";
pub(super) const REACHED_EXIT_UNCONFIRMED: &str =
    "OKX has no record of this market take profit; check its outcome in OKX";
// Durable outbox state, only written together with a new parent. Clearing it BEFORE sending
// is the at-most-once boundary: a crash thereafter permits lookup only, never another send.
const DEFERRED_TRAILING: &str = "Pending entry fill: trailing stop not yet sent";

const DEFERRED_TP: &str = "Pending entry fill: take profit not yet sent";

/// Fields read from the current position inside the same queued step as the edit.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProtectionRetention {
    pub take_profit: bool,
    pub trailing_stop: bool,
}

const PER_ENTRY_TARGET_SUFFIX: &str = ":deferred:tp";

fn deferred_parent_id(row: &Order, leg: Leg) -> Option<&str> {
    row.id
        .strip_suffix(PER_ENTRY_TARGET_SUFFIX)
        .or_else(|| row.id.strip_suffix(&format!(":{}", leg.tag())))
}

pub(super) fn is_deferred(row: &Order) -> bool {
    matches!(
        row.reject_reason.as_deref(),
        Some(DEFERRED_TRAILING | DEFERRED_TP)
    )
}

pub(super) fn defer_tp(entry: &Order) -> bool {
    !entry.reduce_only
        && entry
            .attached_stop_loss
            .as_deref()
            .is_some_and(|p| !p.is_empty())
        && entry
            .attached_take_profit
            .as_deref()
            .is_some_and(|p| !p.is_empty())
}

pub(super) fn defer_trailing(entry: &Order) -> bool {
    !entry.reduce_only
        && entry.attached_trailing_stop.is_some()
        && [
            entry.attached_stop_loss.as_deref(),
            entry.attached_take_profit.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|p| !p.is_empty())
}

pub(super) fn record_deferred_protection(
    rows: &dyn LiveRows,
    entry: &Order,
) -> Result<(), EngineError> {
    if !defer_trailing(entry) && !defer_tp(entry) {
        return Ok(());
    }
    let source = Source {
        account_id: entry.account_id.clone(),
        symbol: entry.symbol.clone(),
        side: if entry.side == OrderSide::Buy {
            PositionSide::Long
        } else {
            PositionSide::Short
        },
        quantity: entry.quantity.clone(),
        leverage: entry.leverage,
        agent_id: entry.agent_id.clone(),
        decision_id: entry.decision_id.clone(),
    };
    let target = Protection {
        stop_loss: None,
        take_profit: entry.attached_take_profit.clone(),
        trailing_stop: entry.attached_trailing_stop.clone(),
    };
    for (defer, leg, marker) in [
        (defer_tp(entry), Leg::Tp, DEFERRED_TP),
        (defer_trailing(entry), Leg::Trail, DEFERRED_TRAILING),
    ] {
        if defer {
            let mut row = protection_row(
                if leg == Leg::Tp {
                    format!("{}{PER_ENTRY_TARGET_SUFFIX}", entry.id)
                } else {
                    format!("{}:{}", entry.id, leg.tag())
                },
                &source,
                leg,
                &target,
                entry.created_at,
            );
            row.reject_reason = Some(marker.into());
            rows.insert_order(&row)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Leg {
    Sl,
    Tp,
    Trail,
}

const LEGS: [Leg; 3] = [Leg::Sl, Leg::Tp, Leg::Trail];

impl Leg {
    fn tag(self) -> &'static str {
        match self {
            Leg::Sl => "sl",
            Leg::Tp => "tp",
            Leg::Trail => "trail",
        }
    }

    fn order_type(self) -> OrderType {
        match self {
            Leg::Sl => OrderType::StopMarket,
            Leg::Tp => OrderType::TakeProfitMarket,
            Leg::Trail => OrderType::TrailingStopMarket,
        }
    }

    fn is_set(self, protection: &Protection) -> bool {
        match self {
            Leg::Sl => protection.stop_loss.is_some(),
            Leg::Tp => protection.take_profit.is_some(),
            Leg::Trail => protection.trailing_stop.is_some(),
        }
    }
}

pub(crate) fn leg_of(order: &Order) -> Option<Leg> {
    if !is_leg(order) {
        return None;
    }
    LEGS.into_iter()
        .find(|leg| leg.order_type() == order.order_type)
}

/// The protection rows in force for one symbol, read back from OKX on the last sync.
#[derive(Clone, Debug, Default)]
pub(crate) struct SymbolProtection {
    sl: Option<Order>,
    tp: Option<Order>,
    trail: Option<Order>,
    pub(crate) trail_trigger: Option<String>,
    pub(crate) external: Vec<ExternalProtection>,
}

impl SymbolProtection {
    fn leg(&self, leg: Leg) -> Option<&Order> {
        match leg {
            Leg::Sl => self.sl.as_ref(),
            Leg::Tp => self.tp.as_ref(),
            Leg::Trail => self.trail.as_ref(),
        }
    }

    fn set(&mut self, leg: Leg, row: Order) {
        match leg {
            Leg::Sl => self.sl = Some(row),
            Leg::Tp => self.tp = Some(row),
            Leg::Trail => self.trail = Some(row),
        }
    }
}

pub(crate) type ProtectionMap = HashMap<String, SymbolProtection>;

/// A position OKX listed on this sync: its side and its size in contracts, never negative.
pub(crate) struct Held {
    pub side: PositionSide,
    pub contracts: Dec,
    pub trade_id: Option<u64>,
}

impl PartialEq for Held {
    fn eq(&self, other: &Self) -> bool {
        self.side == other.side
            && self.trade_id == other.trade_id
            && self.contracts.eq_value(&other.contracts)
    }
}

pub(super) fn last_trade_id(row: &Row) -> Option<u64> {
    row.get("tradeId")
        .and_then(Value::as_str)?
        .parse()
        .ok()
        .filter(|id| *id > 0)
}

fn fill_time(row: &Row) -> Option<i64> {
    row.get("fillTime")?
        .as_str()?
        .parse()
        .ok()
        .filter(|at| *at > 0)
}

type ReachedExits = HashMap<String, String>;

pub(super) fn is_reached_exit(row: &Order) -> bool {
    row.order_type == OrderType::Market
        && row.reduce_only
        && row.trigger_price.is_some()
        && deferred_parent_id(row, Leg::Tp).is_some()
}

fn remember_reached_exit(exits: &mut ReachedExits, row: &Order) {
    if !is_reached_exit(row) || row.status == OrderStatus::Rejected {
        return;
    }
    if let Some((parent, exchange_id)) = deferred_parent_id(row, Leg::Tp)
        .zip(row.exchange_order_id.as_ref().filter(|id| !id.is_empty()))
    {
        exits.insert(exchange_id.clone(), parent.to_owned());
    }
}

struct PositionHistory<'a> {
    fills: &'a [Row],
    trade: u64,
    side: PositionSide,
    contracts: &'a Dec,
    reached_exits: &'a ReachedExits,
}

enum EntryInterval {
    Different,
    Continuous { own_reduction: Dec },
}

impl PositionHistory<'_> {
    fn entry_interval(&self, first_trade: u64, parent: &str) -> Option<EntryInterval> {
        let trade_time = |id| {
            self.fills
                .iter()
                .find(|row| last_trade_id(row) == Some(id))
                .and_then(fill_time)
        };
        let (first, last) = trade_time(first_trade).zip(trade_time(self.trade))?;
        if first > last {
            return None;
        }
        let (open_type, close_type) = match self.side {
            PositionSide::Long => ("3", "5"),
            PositionSide::Short => ("4", "6"),
        };
        let mut changes = Vec::new();
        let mut has_exit = false;
        for row in self.fills {
            let at = fill_time(row)?;
            let trade = last_trade_id(row);
            // Negative liquidation and ADL IDs cannot order fills within a millisecond.
            if at < first
                || at > last
                || trade.is_some_and(|id| {
                    (at == first && id < first_trade) || (at == last && id > self.trade)
                })
            {
                continue;
            }
            if row.get("posSide").and_then(Value::as_str) != Some("net") {
                return Some(EntryInterval::Different);
            }
            let opening = row.get("subType").and_then(Value::as_str) == Some(open_type);
            let own_exit = if opening {
                false
            } else {
                let source = row
                    .get("ordId")
                    .and_then(Value::as_str)
                    .and_then(|id| self.reached_exits.get(id));
                let Some(source) = source
                    .filter(|_| row.get("subType").and_then(Value::as_str) == Some(close_type))
                else {
                    return Some(EntryInterval::Different);
                };
                has_exit = true;
                source == parent
            };
            changes.push((trade?, row, opening, own_exit));
        }
        let mut own_reduction = super::zero();
        if has_exit {
            // A confirmed target reduced its own entry allocation. Walk back from the
            // observed size and reject a chain that contains a close and reopen.
            changes.sort_unstable_by_key(|(trade, _, _, _)| *trade);
            if changes.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                return None;
            }
            let mut contracts = self.contracts.clone();
            for (_, row, opening, own_exit) in changes.into_iter().rev() {
                if !contracts.gt(&super::zero()) {
                    return Some(EntryInterval::Different);
                }
                let size = dec(row.get("fillSz")?.as_str()?).ok()?;
                if !size.gt(&super::zero()) {
                    return None;
                }
                if own_exit {
                    own_reduction = own_reduction.add(&size);
                }
                contracts = if opening {
                    contracts.sub(&size)
                } else {
                    contracts.add(&size)
                };
            }
            if contracts.is_negative() {
                return Some(EntryInterval::Different);
            }
        }
        Some(EntryInterval::Continuous { own_reduction })
    }
}

/// Count only the parent's fills that still belong to the observed position. The order's
/// cumulative size also includes executions that may have been closed before a later fill.
fn proven_entry_contracts(
    history: &PositionHistory<'_>,
    order_id: &str,
    parent_id: &str,
    entry_trade: u64,
    expected: &Dec,
) -> Option<Dec> {
    if history.fills.len() >= RECENT_FILLS_LIMIT {
        return None;
    }
    let mut total = super::zero();
    let mut remaining = super::zero();
    let mut own_reduction = super::zero();
    let mut trades = HashSet::new();
    for fill in history.fills {
        if fill.get("ordId").and_then(Value::as_str) != Some(order_id) {
            continue;
        }
        let trade = last_trade_id(fill)?;
        if trade > entry_trade || !trades.insert(trade) {
            return None;
        }
        let size = dec(fill.get("fillSz")?.as_str()?).ok()?;
        if !size.gt(&super::zero()) {
            return None;
        }
        total = total.add(&size);
        if let EntryInterval::Continuous {
            own_reduction: reduced,
        } = history.entry_interval(trade, parent_id)?
        {
            remaining = remaining.add(&size);
            // The intervals overlap. The oldest proven entry interval includes all
            // confirmed reductions for this parent since that execution.
            own_reduction = own_reduction.max(&reduced);
        }
    }
    (trades.contains(&entry_trade) && total.eq_value(expected))
        .then(|| remaining.sub(&own_reduction).max(&super::zero()))
}

pub(super) fn isolated_net_position<'a>(
    positions: &'a [Row],
    inst_id: &str,
) -> Option<(&'a Row, Dec)> {
    positions.iter().find_map(|position| {
        if position.get("instId").and_then(Value::as_str) != Some(inst_id)
            || !isolated_net(position)
        {
            return None;
        }
        let size = dec(position.get("pos")?.as_str()?).ok()?;
        (!size.is_zero()).then_some((position, size))
    })
}

/// Consecutive syncs that found our protection but no position, and the first such sync.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Miss {
    count: u32,
    since: i64,
}

/// A trailing row's spec. Every trailing row MochiPrep writes carries one; a row without it is
/// read as no trailing stop rather than invented.
fn spec_of(row: &Order) -> Option<TrailingStopSpec> {
    Some(TrailingStopSpec {
        kind: row.trailing_kind?,
        callback: row.trailing_callback.clone()?,
        activation_price: row.trailing_activation_price.clone(),
    })
}

/// Node's `effectiveOf`: our protection OKX listed as pending on the last sync. External algos are
/// not included.
pub(crate) fn effective_of(map: &ProtectionMap, symbol: &str) -> Protection {
    let entry = map.get(symbol);
    let leg = |leg: Leg| entry.and_then(|e| e.leg(leg));
    Protection {
        stop_loss: leg(Leg::Sl).and_then(|row| row.trigger_price.clone()),
        take_profit: leg(Leg::Tp).and_then(|row| row.trigger_price.clone()),
        trailing_stop: leg(Leg::Trail).and_then(spec_of),
    }
}

fn decimal_error(_: mochi_decimal::DecError) -> EngineError {
    EngineError::message("[DecimalError] Invalid argument")
}

fn same_leg(leg: Leg, a: &Protection, b: &Protection) -> Result<bool, EngineError> {
    match leg {
        Leg::Sl => same_price(a.stop_loss.as_deref(), b.stop_loss.as_deref()),
        Leg::Tp => same_price(a.take_profit.as_deref(), b.take_profit.as_deref()),
        Leg::Trail => same_trailing(a.trailing_stop.as_ref(), b.trailing_stop.as_ref()),
    }
    .map_err(decimal_error)
}

/// Exchange text never reaches the model: only fields that parse as plain numbers are kept.
fn num(value: Option<&serde_json::Value>) -> Option<String> {
    let text = truthy_text(value)?;
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (text.as_str(), None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || fraction.is_some_and(|f| !digits(f)) {
        return None;
    }
    Dec::parse(&text).ok().map(|d| d.to_fixed())
}

/// What an algo is, for display, built only from its numeric fields.
pub(crate) fn external_of(algo: &Row) -> Vec<ExternalProtection> {
    if algo.get("ordType").and_then(|v| v.as_str()) == Some("move_order_stop") {
        let ratio = num(algo.get("callbackRatio"));
        let spread = num(algo.get("callbackSpread"));
        let from = num(algo.get("activePx"));
        let distance = match ratio {
            Some(ratio) => {
                let hundred = Dec::parse("100").expect("100 is a decimal");
                let percent = Dec::parse(&ratio).map(|r| r.mul(&hundred).to_fixed());
                format!("{}%", percent.unwrap_or(ratio))
            }
            None => spread.unwrap_or_else(|| "unknown".into()),
        };
        let from = from.map(|f| format!(" from {f}")).unwrap_or_default();
        return vec![ExternalProtection {
            kind: ExternalProtectionKind::Trailing,
            trigger_price: num(algo.get("moveTriggerPx")),
            detail: format!("trailing {distance}{from}"),
        }];
    }
    let mut found = Vec::new();
    let legs = [
        (ExternalProtectionKind::StopLoss, "SL", "slTriggerPx"),
        (ExternalProtectionKind::TakeProfit, "TP", "tpTriggerPx"),
    ];
    for (kind, label, field) in legs {
        if truthy_text(algo.get(field)).is_none() {
            continue;
        }
        let price = num(algo.get(field));
        let shown = price.clone().unwrap_or_else(|| "unknown".into());
        found.push(ExternalProtection {
            kind,
            trigger_price: price,
            detail: format!("{label} trigger {shown}"),
        });
    }
    found
}

/// Who and what a protection row is for.
pub(crate) struct Source {
    pub account_id: String,
    pub symbol: String,
    pub side: PositionSide,
    pub quantity: String,
    pub leverage: u32,
    pub agent_id: Option<String>,
    pub decision_id: Option<String>,
}

/// A local row for one protective algo. Its id is also its clientOrderId and the algoClOrdId
/// source.
pub(crate) fn protection_row(
    id: String,
    source: &Source,
    leg: Leg,
    protection: &Protection,
    now: i64,
) -> Order {
    let trail = match leg {
        Leg::Trail => protection.trailing_stop.as_ref(),
        _ => None,
    };
    Order {
        client_order_id: id.clone(),
        id,
        account_id: source.account_id.clone(),
        symbol: source.symbol.clone(),
        side: match source.side {
            PositionSide::Long => OrderSide::Sell,
            PositionSide::Short => OrderSide::Buy,
        },
        order_type: leg.order_type(),
        quantity: source.quantity.clone(),
        price: None,
        trigger_price: match leg {
            Leg::Sl => protection.stop_loss.clone(),
            Leg::Tp => protection.take_profit.clone(),
            Leg::Trail => None,
        },
        leverage: source.leverage,
        reduce_only: true,
        close_position: true,
        status: OrderStatus::Open,
        filled_quantity: "0".into(),
        avg_fill_price: None,
        reserved_margin: "0".into(),
        attached_stop_loss: None,
        attached_take_profit: None,
        trailing_kind: trail.map(|t| t.kind),
        trailing_callback: trail.map(|t| t.callback.clone()),
        trailing_activation_price: trail.and_then(|t| t.activation_price.clone()),
        trailing_extreme: None,
        attached_trailing_stop: None,
        reject_reason: None,
        exchange_order_id: None,
        agent_id: source.agent_id.clone(),
        decision_id: source.decision_id.clone(),
        created_at: now,
        updated_at: now,
    }
}

/// Track exchange-attached protection after a terminal entry with a fill. Existing
/// deferred rows keep their submission state. Runs inside the entry's store transaction.
pub(crate) fn record_attached(
    rows: &dyn LiveRows,
    entry: &Order,
    filled: &Dec,
    now: i64,
) -> Result<(), EngineError> {
    let protection = Protection {
        stop_loss: entry.attached_stop_loss.clone(),
        take_profit: entry.attached_take_profit.clone(),
        trailing_stop: entry.attached_trailing_stop.clone(),
    };
    let source = Source {
        account_id: entry.account_id.clone(),
        symbol: entry.symbol.clone(),
        side: match entry.side {
            OrderSide::Buy => PositionSide::Long,
            OrderSide::Sell => PositionSide::Short,
        },
        quantity: filled.to_fixed(),
        leverage: entry.leverage,
        agent_id: entry.agent_id.clone(),
        decision_id: entry.decision_id.clone(),
    };
    for leg in LEGS {
        if leg == Leg::Tp
            && rows
                .get_order(&format!("{}{PER_ENTRY_TARGET_SUFFIX}", entry.id))?
                .is_some()
        {
            continue;
        }
        let id = format!("{}:{}", entry.id, leg.tag());
        if !leg.is_set(&protection) || rows.get_order(&id)?.is_some() {
            continue;
        }
        rows.insert_order(&protection_row(id, &source, leg, &protection, now))?;
    }
    Ok(())
}

/// Net mode is required at connect, so no posSide, exactly as entry orders are sent.
fn algo_body(row: &Order, registry: &Registry) -> Result<AlgoOrderBody, EngineError> {
    let mut body = AlgoOrderBody {
        inst_id: to_inst_id(&row.symbol),
        td_mode: TdMode::Isolated,
        side: match row.side {
            OrderSide::Buy => Side::Buy,
            OrderSide::Sell => Side::Sell,
        },
        algo_cl_ord_id: to_algo_cl_ord_id(&row.id),
        sz: to_contracts(row, registry)?.to_fixed(),
        reduce_only: true,
        ord_type: AlgoOrdType::Conditional,
        sl_trigger_px: None,
        sl_ord_px: None,
        tp_trigger_px: None,
        tp_ord_px: None,
        callback_ratio: None,
        callback_spread: None,
        active_px: None,
    };
    match row.order_type {
        OrderType::StopMarket => {
            body.sl_trigger_px = row.trigger_price.clone();
            body.sl_ord_px = Some("-1".into());
        }
        OrderType::TakeProfitMarket => {
            body.tp_trigger_px = row.trigger_price.clone();
            body.tp_ord_px = Some("-1".into());
        }
        _ => {
            body.ord_type = AlgoOrdType::MoveOrderStop;
            // A row without a spec sends neither callback, which the client refuses before
            // anything is signed.
            if let Some(spec) = spec_of(row) {
                match spec.kind {
                    mochi_domain::TrailingKind::Ratio => body.callback_ratio = Some(spec.callback),
                    mochi_domain::TrailingKind::Spread => {
                        body.callback_spread = Some(spec.callback)
                    }
                }
                body.active_px = spec.activation_price.filter(|p| !p.is_empty());
            }
        }
    }
    Ok(body)
}

/// A row's size in contracts as OKX listed it in `algo`, and as the row records it (for an
/// attached row, what its entry filled), each when it reads.
fn sizes(row: &Order, algo: &Row, registry: &Registry) -> (Option<Dec>, Option<Dec>) {
    let listed = num(algo.get("sz")).and_then(|sz| Dec::parse(&sz).ok());
    (listed, to_contracts(row, registry).ok())
}

/// Whether `row`, whose algo OKX listed as `algo`, covers a position of `contracts`. It takes the
/// smaller of OKX's `sz` and the row's own size: older stops are retired on this, so a size OKX
/// reports above what was filled or sent is never trusted.
fn covers(row: &Order, algo: &Row, contracts: &Dec, registry: &Registry) -> bool {
    let size = match sizes(row, algo, registry) {
        (Some(listed), Some(own)) => Some(listed.min(&own)),
        (listed, own) => listed.or(own),
    };
    size.is_some_and(|size| size.gte(contracts))
}

/// A row attached to an entry, sized to that entry's fill alone. Other rows (a protection
/// change, a cover) were sized to the whole position when placed.
fn is_per_fill(row: &Order) -> bool {
    leg_of(row).is_some_and(|leg| row.id.ends_with(&format!(":{}", leg.tag())))
}

/// The contracts a group's live rows guard between them, oldest first: a whole-position row
/// counts as its own size, and each later per-fill row adds its fill. It takes the larger of
/// OKX's `sz` and the row's own, since a cover sized to it must not fall short. `None` when a
/// size does not read.
fn group_claim(members: &[(usize, Row)], rows: &[Order], registry: &Registry) -> Option<Dec> {
    let mut claim = super::zero();
    for (i, algo) in members {
        let size = match sizes(&rows[*i], algo, registry) {
            (Some(listed), Some(own)) => listed.max(&own),
            (listed, own) => listed.or(own)?,
        };
        claim = if is_per_fill(&rows[*i]) {
            claim.add(&size)
        } else {
            size
        };
    }
    Some(claim)
}

/// A stop must sit below the price for a long and above it for a short; a target the other way
/// round. Node's texts.
fn wrong_side_refusal(
    side: PositionSide,
    last: &Dec,
    target: &Protection,
) -> Result<Option<String>, EngineError> {
    let long = side == PositionSide::Long;
    let wrong_side = |price: &str, below: bool| -> Result<bool, EngineError> {
        let price = dec(price)?;
        Ok(if below {
            price.gte(last)
        } else {
            price.lte(last)
        })
    };
    if let Some(stop) = target.stop_loss.as_deref().filter(|s| !s.is_empty()) {
        if wrong_side(stop, long)? {
            return Ok(Some(format!(
                "INVALID_PROTECTION: stop {stop} is on the wrong side of {} for {}",
                last.to_fixed(),
                side_text(side)
            )));
        }
    }
    if let Some(goal) = target.take_profit.as_deref().filter(|s| !s.is_empty()) {
        if wrong_side(goal, !long)? {
            return Ok(Some(format!(
                "INVALID_PROTECTION: target {goal} is on the wrong side of {} for {}",
                last.to_fixed(),
                side_text(side)
            )));
        }
    }
    Ok(None)
}

/// The most protective level among a leg's live rows, the one closest to the price on its side:
/// for a long, the highest stop and the lowest target; for a short, the other way round. A
/// full-size cover at any other level would loosen the stop of some fill. Only SL and TP have a
/// level; `None` when no row carries one.
fn tightest_level<'a>(
    leg: Leg,
    side: PositionSide,
    rows: impl Iterator<Item = &'a Order>,
) -> Result<Option<String>, EngineError> {
    // Highest wins for a long's stop and a short's target.
    let highest = (leg == Leg::Sl) == (side == PositionSide::Long);
    let mut best: Option<(Dec, String)> = None;
    for row in rows {
        let Some(text) = row.trigger_price.as_deref().filter(|t| !t.is_empty()) else {
            continue;
        };
        let level = dec(text)?;
        let better = best.as_ref().is_none_or(|(current, _)| {
            if highest {
                level.gt(current)
            } else {
                level.lt(current)
            }
        });
        if better {
            best = Some((level, text.to_string()));
        }
    }
    Ok(best.map(|(_, text)| text))
}

/// What a protection change reads from the store before deciding.
struct Context {
    limits: RiskSettings,
    registry: Registry,
    open_orders: Vec<Order>,
}

impl OkxEngine {
    /// Our protection that OKX listed as pending on the last sync, as the runner and risk engine
    /// see it. External algos are not included.
    pub fn protection_of(&self, symbol: &str) -> Protection {
        effective_of(&self.state().protection, symbol)
    }

    /// Replaces the whole protection of an open position. A `None` leg removes that leg. Runs in
    /// the account's queue, like an order.
    pub async fn set_protection(
        self: &Arc<Self>,
        account_id: &str,
        symbol: &str,
        protection: Protection,
        agent_id: Option<String>,
        decision_id: Option<String>,
    ) -> Result<ProtectionResult, EngineError> {
        self.set_protection_with(
            account_id,
            symbol,
            protection,
            ProtectionRetention::default(),
            agent_id,
            decision_id,
        )
        .await
    }

    /// Preserve selected legs in the queued state, including deferred submissions.
    pub async fn set_protection_with(
        self: &Arc<Self>,
        account_id: &str,
        symbol: &str,
        protection: Protection,
        retain: ProtectionRetention,
        agent_id: Option<String>,
        decision_id: Option<String>,
    ) -> Result<ProtectionResult, EngineError> {
        let (account_id, symbol) = (account_id.to_string(), symbol.to_string());
        self.queued(move |engine| {
            Box::pin(async move {
                engine
                    .apply_protection(
                        account_id,
                        symbol,
                        protection,
                        retain,
                        agent_id,
                        decision_id,
                    )
                    .await
            })
        })
        .await
    }

    pub(super) async fn save(&self, row: Order) -> Result<(), EngineError> {
        self.rows(move |rows| Ok(rows.save_order(&row)?)).await
    }

    async fn context(&self) -> Result<Context, EngineError> {
        let account_id = self.deps.account_id.clone();
        self.rows(move |rows| {
            Ok(Context {
                limits: rows
                    .get_account(&account_id)?
                    .and_then(|account| account.live_limits)
                    .unwrap_or_else(default_live_limits),
                registry: Registry(rows.instruments()),
                open_orders: rows.list_open_orders(&account_id)?,
            })
        })
        .await
    }

    /// Every changed leg is placed as a new algo first, and old or removed algos are cancelled
    /// only once OKX has accepted all of them, so the position is never left without a stop. Two
    /// reduce-only algos may briefly coexist; sync keeps the newest. A refused or unanswered
    /// placement stops here and cancels nothing.
    async fn apply_protection(
        &self,
        account_id: String,
        symbol: String,
        requested: Protection,
        retain: ProtectionRetention,
        agent_id: Option<String>,
        decision_id: Option<String>,
    ) -> Result<ProtectionResult, EngineError> {
        let position = self
            .positions()
            .into_iter()
            .find(|p| p.position.symbol == symbol && p.close_supported == Some(true))
            .ok_or_else(|| EngineError::message(format!("No open {symbol} position")))?;
        let context = self.context().await?;
        let tick = dec(&context.registry.get(&symbol)?.price_tick)?;
        let before = self.protection_of(&symbol);
        let target = Protection {
            stop_loss: align(&requested.stop_loss, &tick)?,
            // Retained legs keep their exchange prices and sizes even if the tick changed.
            take_profit: if retain.take_profit {
                before.take_profit.clone()
            } else {
                align(&requested.take_profit, &tick)?
            },
            trailing_stop: if retain.trailing_stop {
                before.trailing_stop.clone()
            } else {
                requested
                    .trailing_stop
                    .map(|spec| align_trailing(spec, &tick))
                    .transpose()?
            },
        };
        if let Some(refusal) = self.protection_refusal(&position, &before, &target, &context)? {
            return Err(EngineError::Message(refusal));
        }

        let mut changed = Vec::new();
        for leg in LEGS {
            if !same_leg(leg, &before, &target)? {
                changed.push(leg);
            }
        }
        let old = self
            .state()
            .protection
            .get(&symbol)
            .cloned()
            .unwrap_or_default();
        let source = Source {
            account_id,
            symbol: symbol.clone(),
            side: position.position.side,
            quantity: position.position.quantity.clone(),
            leverage: position.position.leverage,
            agent_id,
            decision_id,
        };
        // Every placement first. Nothing is cancelled unless all of them were accepted, so
        // swapping a stop for a trailing stop can never leave the position with neither.
        for leg in changed.iter().copied().filter(|leg| leg.is_set(&target)) {
            let mut row =
                protection_row((self.deps.new_id)("ord"), &source, leg, &target, self.now());
            // Written before anything is sent, so an unanswered request can be looked up by
            // algoClOrdId.
            let inserted = row.clone();
            self.rows(move |rows| Ok(rows.insert_order(&inserted)?))
                .await?;
            match self.place(&mut row, &context.registry).await {
                Ok(()) => {}
                // Unknown outcome: never placed again. A sync looks it up by algoClOrdId; if OKX
                // shows it live, the placement is known to have succeeded and the change carries
                // on.
                Err(EngineError::Okx(OkxError::Unknown(message))) => {
                    row.reject_reason = Some(format!("Awaiting confirmation: {message}"));
                    self.save(row.clone()).await?;
                    let _ = self.sync_now().await;
                    let id = row.id.clone();
                    let resolved = self.rows(move |rows| Ok(rows.get_order(&id)?)).await?;
                    let confirmed = resolved.is_some_and(|r| {
                        r.status == OrderStatus::Open
                            && r.exchange_order_id.is_some_and(|id| !id.is_empty())
                    });
                    if confirmed {
                        continue;
                    }
                    // Still unknown: the old algos stay until sync resolves it.
                    return Ok(ProtectionResult {
                        before,
                        after: self.protection_of(&symbol),
                        pending: true,
                    });
                }
                // A refusal, or Node's plain Error (a request refused before sending, an answer
                // that could not be read): this leg did not happen.
                Err(error) => {
                    let message = error.to_string();
                    row.status = OrderStatus::Rejected;
                    row.reject_reason = Some(message.clone());
                    row.updated_at = self.now();
                    self.save(row).await?;
                    // Legs placed before this one stay; sync retires the algos they superseded.
                    let confirmed = self.sync_now().await.is_ok();
                    // Without the sync, `after` is the last view, which may lack legs OKX already
                    // accepted.
                    let note = if confirmed {
                        ""
                    } else {
                        "; current protection not yet confirmed from OKX"
                    };
                    return Err(EngineError::ProtectionChange {
                        message: format!("{message}{note}"),
                        after: Box::new(self.protection_of(&symbol)),
                    });
                }
            }
        }
        for leg in &changed {
            if !leg.is_set(&target) {
                // More than one per-entry order can share a leg. An explicit removal
                // clears all of them, while a preserved field never reaches this branch.
                let account = self.deps.account_id.clone();
                let symbol = symbol.clone();
                let leg = *leg;
                let closing_side = if position.position.side == PositionSide::Long {
                    OrderSide::Sell
                } else {
                    OrderSide::Buy
                };
                let active = self
                    .rows(move |rows| {
                        Ok(rows
                            .list_open_orders(&account)?
                            .into_iter()
                            .filter(|row| {
                                row.symbol == symbol
                                    && row.side == closing_side
                                    && leg_of(row) == Some(leg)
                                    && row
                                        .exchange_order_id
                                        .as_deref()
                                        .is_some_and(|id| !id.is_empty())
                            })
                            .collect())
                    })
                    .await?;
                self.cancel_rows(active, Some(REMOVED)).await?;
                continue;
            }
            // A sync above may already have retired a superseded row; it is not cancelled twice.
            let Some(previous) = old.leg(*leg) else {
                continue;
            };
            let id = previous.id.clone();
            let row = self.rows(move |rows| Ok(rows.get_order(&id)?)).await?;
            if let Some(row) = row.filter(|r| r.status == OrderStatus::Open) {
                self.cancel_rows(vec![row], Some(REPLACED)).await?;
            }
        }

        // A position edit only supersedes deferred exits of settled entries. Resting
        // entries may still fill later and must retain the exits requested with them.
        for mut pending in context
            .open_orders
            .into_iter()
            .filter(|row| row.symbol == symbol && is_deferred(row))
        {
            let leg = match leg_of(&pending) {
                Some(Leg::Trail) if !retain.trailing_stop => Leg::Trail,
                Some(Leg::Tp) if !retain.take_profit => Leg::Tp,
                _ => continue,
            };
            let Some(parent_id) = deferred_parent_id(&pending, leg).map(str::to_string) else {
                continue;
            };
            let parent = self
                .rows(move |rows| Ok(rows.get_order(&parent_id)?))
                .await?;
            if !parent.is_some_and(|parent| parent.status != OrderStatus::Open) {
                continue;
            }
            let reason = if leg.is_set(&target) {
                REPLACED
            } else {
                REMOVED
            };
            self.close_row(&mut pending, reason).await?;
        }

        let confirmed = self.sync_now().await.is_ok();
        // Every placement was accepted; if the view could not be refreshed, `after` may still be
        // the old protection, which must not read as "nothing changed".
        Ok(ProtectionResult {
            before,
            after: self.protection_of(&symbol),
            pending: !confirmed,
        })
    }

    /// Node's `try` block for one leg: the body, the placement and the saved algoId.
    async fn place(&self, row: &mut Order, registry: &Registry) -> Result<(), EngineError> {
        let body = algo_body(row, registry)?;
        row.exchange_order_id = Some(self.deps.client.place_algo(&body).await?);
        row.updated_at = self.now();
        self.save(row.clone()).await
    }

    /// The gate for protection changes. A change that only lowers risk needs no arming, like an
    /// exit. The checks run in Node's order and the texts are Node's, word for word.
    fn protection_refusal(
        &self,
        position: &PositionView,
        before: &Protection,
        target: &Protection,
        context: &Context,
    ) -> Result<Option<String>, EngineError> {
        let symbol = &position.position.symbol;
        let side = position.position.side;
        let unresolved = context.open_orders.iter().any(|o| {
            &o.symbol == symbol
                && is_leg(o)
                && !is_deferred(o)
                && o.exchange_order_id.as_deref().is_none_or(str::is_empty)
        });
        if unresolved {
            return Ok(Some(
                "PROTECTION_PENDING: an earlier protection change is awaiting confirmation from OKX"
                    .into(),
            ));
        }
        if !(self.deps.is_armed)()
            && classify_protection_change(side, before, target).map_err(decimal_error)?
                == ProtectionChangeClass::NeedsArmed
        {
            return Ok(Some("NOT_ARMED: this change increases risk".into()));
        }
        let limits = &context.limits;
        let last = match self.market_refusal(symbol, limits)? {
            Ok(last) => last,
            Err(refusal) => return Ok(Some(refusal)),
        };
        if limits.require_stop_loss && before.stop_loss.is_some() && target.stop_loss.is_none() {
            return Ok(Some(
                "LIMIT_STOP_REQUIRED: positions must keep a stop loss".into(),
            ));
        }
        if let Some(refusal) = wrong_side_refusal(side, &last, target)? {
            return Ok(Some(refusal));
        }
        // A kept trailing stop is not re-checked: its activation may rightly be behind the market
        // by now.
        if let Some(spec) = &target.trailing_stop {
            if !same_trailing(before.trailing_stop.as_ref(), Some(spec)).map_err(decimal_error)? {
                let tick = dec(&context.registry.get(symbol)?.price_tick)?;
                let refused = validate_trailing(side, &last, spec, &tick)
                    .map_err(|e| EngineError::Message(e.to_string()))?;
                if let Some(refused) = refused {
                    return Ok(Some(format!("INVALID_PROTECTION: {refused}")));
                }
            }
        }
        Ok(None)
    }

    /// The market checks any placement of protection passes, a protection change or a full-size
    /// cover: a recent healthy sync with a balance, and a fresh quote. Answers the last price, or
    /// the refusal in Node's words.
    fn market_refusal(
        &self,
        symbol: &str,
        limits: &RiskSettings,
    ) -> Result<Result<Dec, String>, EngineError> {
        let healthy = self.healthy();
        let (has_balance, sync_error) = {
            let state = self.state();
            (state.balance.is_some(), state.sync_error.clone())
        };
        if !healthy || !has_balance {
            let why = sync_error.unwrap_or_else(|| "no recent sync with OKX".into());
            return Ok(Err(format!("EXCHANGE_NOT_SYNCED: {why}")));
        }
        let Some(quote) = (self.deps.get_quote)(symbol) else {
            return Ok(Err("NO_MARKET_DATA: no quote".into()));
        };
        let age = (self.now() - quote.ts) as f64 / 1000.0;
        if age > limits.max_quote_age_sec {
            return Ok(Err(format!(
                "MARKET_DATA_STALE: quote is {}s old",
                mochi_domain::jsnum::js_to_fixed(age, 0)
            )));
        }
        Ok(Ok(dec(&quote.last)?))
    }

    /// Cancels the algos behind these rows and marks the rows closed. The cancel answer is not
    /// trusted either way: the next sync sees whether each algo is still pending and cancels it
    /// again if so. `reason` None keeps a row's existing reason.
    pub(super) async fn cancel_rows(
        &self,
        rows: Vec<Order>,
        reason: Option<&'static str>,
    ) -> Result<Option<String>, EngineError> {
        let sent: Vec<CancelAlgo> = rows
            .iter()
            .filter_map(|row| {
                let algo_id = row.exchange_order_id.clone().filter(|id| !id.is_empty())?;
                Some(CancelAlgo {
                    inst_id: to_inst_id(&row.symbol),
                    algo_id,
                })
            })
            .collect();
        let mut failure = None;
        for batch in sent.chunks(10) {
            if let Err(error) = self.deps.client.cancel_algos(batch).await {
                failure = Some(format!("Could not cancel an OKX algo order: {error}"));
            }
        }
        let now = self.now();
        let rows: Vec<Order> = rows
            .into_iter()
            .map(|mut row| {
                if row.status == OrderStatus::Open {
                    row.status = OrderStatus::Canceled;
                }
                if let Some(reason) = reason {
                    row.reject_reason = Some(reason.into());
                }
                row.updated_at = now;
                row
            })
            .collect();
        self.rows(move |store| {
            for row in &rows {
                store.save_order(row)?;
            }
            Ok(())
        })
        .await?;
        // Shown with the account status (redacted there). It does not make the account
        // unhealthy: the algo is only an extra reduce-only order, and sync keeps retrying.
        if let Some(failure) = &failure {
            self.state().cancel_error = Some(failure.clone());
        }
        Ok(failure)
    }

    async fn submit_deferred_protection(
        &self,
        row: &mut Order,
        registry: &Registry,
        snapshot: &mut PositionSnapshot,
        reached_exits: &ReachedExits,
    ) -> Result<(), EngineError> {
        let leg =
            leg_of(row).ok_or_else(|| EngineError::message("Invalid deferred protection leg"))?;
        let parent_id = deferred_parent_id(row, leg)
            .ok_or_else(|| EngineError::message("Invalid deferred protection id"))?
            .to_string();
        let parent = self
            .rows(move |rows| Ok(rows.get_order(&parent_id)?))
            .await?;
        let Some(parent) = parent else {
            return self
                .close_row(row, "Deferred protection canceled: parent missing")
                .await;
        };
        if parent.status == OrderStatus::Open {
            return Ok(());
        }
        if !dec(&parent.filled_quantity)?.gt(&super::zero()) {
            return self
                .close_row(row, "Deferred protection canceled: entry did not fill")
                .await;
        }
        let inst_id = to_inst_id(&row.symbol);
        let confirming = self.now().saturating_sub(parent.updated_at) < POSITION_MISSING_MS;
        let context = self.context().await?;
        if let Some(replacement) = context.open_orders.iter().find(|other| {
            other.symbol == row.symbol
                && other.created_at >= parent.updated_at
                && !is_per_fill(other)
                && leg_of(other) == Some(leg)
        }) {
            if replacement
                .exchange_order_id
                .as_deref()
                .is_some_and(|id| !id.is_empty())
            {
                return self.close_row(row, REPLACED).await;
            }
            // A manual replacement of unknown outcome must resolve before this intent sends.
            return Ok(());
        }
        let remote = self
            .deps
            .client
            .get_order(&inst_id, &to_cl_ord_id(&parent.client_order_id))
            .await?;
        if parent.status == OrderStatus::Canceled && parent.attached_stop_loss.is_some() {
            // Single attached stops also apply to a partial fill when the remainder is
            // canceled. Confirm the fixed stop appeared before placing optional exits.
            let stop = self.lookup_algo(&format!("{}:sl", parent.id)).await?;
            if !stop
                .as_ref()
                .and_then(|algo| algo.get("state"))
                .and_then(Value::as_str)
                .is_some_and(|state| ALIVE_STATES.contains(&state))
            {
                if confirming {
                    return Ok(());
                }
                return self.close_row(row, "Deferred protection canceled: OKX did not confirm the fixed stop for the partial fill").await;
            }
        }
        // Read fresh after the parent/stop calls, which may have outlived the earlier snapshot.
        *snapshot = PositionSnapshot::new(self.deps.client.get_positions().await?)?;
        let Some((position, mut signed_size)) = isolated_net_position(&snapshot.open, &inst_id)
        else {
            if confirming {
                return Ok(());
            }
            return self
                .close_row(
                    row,
                    "Deferred protection canceled: entry position is no longer present",
                )
                .await;
        };
        let short = signed_size.is_negative();
        let side = if short {
            PositionSide::Short
        } else {
            PositionSide::Long
        };
        let closes = (row.side == OrderSide::Sell) != short;
        let order_trade = remote.as_ref().and_then(last_trade_id);
        let (Some(entry_trade), Some(position_trade)) = (order_trade, last_trade_id(position))
        else {
            if confirming {
                return Ok(());
            }
            return self
                .close_row(
                    row,
                    "Deferred protection canceled: entry fill cannot be confirmed",
                )
                .await;
        };
        if !closes {
            return self
                .close_row(row, "Deferred protection canceled: position changed side")
                .await;
        }
        if position_trade < entry_trade {
            if confirming {
                return Ok(());
            }
            return self
                .close_row(
                    row,
                    "Deferred protection canceled: latest position trade is older than the entry",
                )
                .await;
        }
        let remote = remote
            .as_ref()
            .expect("the order trade came from a remote order");
        let expected = dec(&parent.filled_quantity)?;
        let contract_value = dec(&registry.get(&row.symbol)?.venue_contract_value)?;
        let expected = expected.div(&contract_value).map_err(decimal_error)?;
        let last_size = remote
            .get("fillSz")
            .and_then(Value::as_str)
            .and_then(|size| dec(size).ok());
        let filled = if position_trade == entry_trade
            && last_size
                .as_ref()
                .is_some_and(|size| size.eq_value(&expected))
        {
            // The whole parent filled in this one trade, which is still the position's latest.
            expected
        } else {
            let Some(order_id) = remote
                .get("ordId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            else {
                return self
                    .close_row(
                        row,
                        "Deferred protection canceled: entry order ID is missing",
                    )
                    .await;
            };
            let fills = self.deps.client.recent_fills(&inst_id).await?;
            let unresolved: HashSet<_> = context
                .open_orders
                .iter()
                .filter(|other| {
                    other.symbol == row.symbol
                        && is_reached_exit(other)
                        && other.exchange_order_id.as_deref().is_none_or(str::is_empty)
                })
                .map(|other| to_cl_ord_id(&other.client_order_id))
                .collect();
            let first_entry_trade = fills
                .iter()
                .filter(|fill| fill.get("ordId").and_then(Value::as_str) == Some(order_id))
                .filter_map(last_trade_id)
                .min()
                .unwrap_or(entry_trade);
            // Only a reduction in this entry's interval can make the unanswered exit
            // relevant. Local submission time and exchange fill time can disagree.
            let close_subtype = if short { "6" } else { "5" };
            let awaiting_sibling = !unresolved.is_empty()
                && fills.iter().any(|fill| {
                    last_trade_id(fill)
                        .is_some_and(|id| (first_entry_trade..=position_trade).contains(&id))
                        && fill.get("subType").and_then(Value::as_str) == Some(close_subtype)
                        && fill
                            .get("clOrdId")
                            .and_then(Value::as_str)
                            .is_none_or(|id| id.is_empty() || unresolved.contains(id))
                });
            let observed_contracts = if short {
                signed_size.neg()
            } else {
                signed_size.clone()
            };
            let history = PositionHistory {
                fills: &fills,
                trade: position_trade,
                side,
                contracts: &observed_contracts,
                reached_exits,
            };
            let filled = match proven_entry_contracts(
                &history,
                order_id,
                &parent.id,
                entry_trade,
                &expected,
            ) {
                Some(quantity) if quantity.gt(&super::zero()) => quantity,
                // An unattributed reduction could belong to our own unanswered exit.
                // Let lookup identify it before rejecting another entry's intent.
                _ if awaiting_sibling => return Ok(()),
                // The position and parent can become visible before the fills endpoint catches up.
                None if confirming => return Ok(()),
                _ => {
                    return self
                        .close_row(
                            row,
                            "Deferred protection canceled: entry position lifetime cannot be confirmed",
                        )
                        .await;
                }
            };
            *snapshot = PositionSnapshot::new(self.deps.client.get_positions().await?)?;
            let same_trade = isolated_net_position(&snapshot.open, &inst_id).filter(|(p, size)| {
                last_trade_id(p) == Some(position_trade) && size.eq_value(&signed_size)
            });
            let Some((_, size)) = same_trade else {
                return Ok(());
            };
            signed_size = size;
            filled
        };
        let contracts = if short {
            signed_size.neg()
        } else {
            signed_size
        };
        row.quantity = filled.min(&contracts).mul(&contract_value).to_fixed();
        let Some(quote) = (self.deps.get_quote)(&row.symbol) else {
            return Ok(());
        };
        if (self.now() - quote.ts) as f64 / 1000.0 > context.limits.max_quote_age_sec {
            return Ok(());
        }
        let last = dec(&quote.last)?;
        let after_fill = fill_time(remote).is_some_and(|at| quote.ts > at);
        let reached = |target: &Dec| match side {
            PositionSide::Long => last.gte(target),
            PositionSide::Short => last.lte(target),
        };
        if leg == Leg::Tp {
            let target = dec(row.trigger_price.as_deref().unwrap_or_default())?;
            if reached(&target) {
                // OKX timestamps are in milliseconds. Equality doesn't establish
                // that the ticker price was observed after the last execution.
                if !after_fill {
                    return Ok(());
                }
                return self
                    .submit_reached_target(row, &inst_id, registry, side, snapshot)
                    .await;
            }
        }
        if let Some(mut spec) = spec_of(row) {
            // This pass already fetched balance/positions/algos successfully. The cached health
            // flag may still be false on restart, so use the fresh quote directly, not last sync.
            if let Some(activation) = spec.activation_price.as_deref() {
                if reached(&dec(activation)?) {
                    if !after_fill {
                        return Ok(());
                    }
                    // OKX starts tracking immediately when no activation price is passed.
                    spec.activation_price = None;
                }
            }
            let tick = dec(&registry.get(&row.symbol)?.price_tick)?;
            if let Some(reason) = validate_trailing(side, &last, &spec, &tick)
                .map_err(|e| EngineError::message(e.to_string()))?
            {
                return self
                    .close_row(row, &format!("Deferred protection canceled: {reason}"))
                    .await;
            }
            row.trailing_activation_price = spec.activation_price;
        }
        let body = algo_body(row, registry)?;
        row.reject_reason = Some(format!(
            "Awaiting confirmation: deferred {} submission started",
            leg.tag()
        ));
        row.created_at = self.now();
        row.updated_at = self.now();
        self.save(row.clone()).await?;
        match self.deps.client.place_algo(&body).await {
            Ok(id) => {
                row.exchange_order_id = Some(id);
                row.reject_reason = None;
            }
            // These explicit price refusals mean no algo was accepted. Let the next fresh
            // quote decide whether the target has triggered while the request was in flight.
            Err(OkxError::Rejection { code, .. })
                if leg == Leg::Tp
                    && matches!(code.as_str(), "51050" | "51052" | "51277" | "51279") =>
            {
                row.reject_reason = Some(DEFERRED_TP.into());
            }
            Err(OkxError::Rejection { code, message }) => {
                row.status = OrderStatus::Rejected;
                row.reject_reason = Some(OkxError::Rejection { code, message }.to_string());
            }
            Err(error) => {
                row.reject_reason = Some(format!("Awaiting confirmation: {error}"));
            }
        }
        self.save(row.clone()).await
    }

    /// The target already triggered on a fresh quote. Convert its unsent intent to a normal
    /// reduce-only market order so sync uses regular order lookup, even after a lost response.
    async fn submit_reached_target(
        &self,
        row: &mut Order,
        inst_id: &str,
        registry: &Registry,
        side: PositionSide,
        snapshot: &mut PositionSnapshot,
    ) -> Result<(), EngineError> {
        let prepared = self.prepare(&row.account_id, &row.client_order_id).await?;
        if let Some(reason) = self.pre_trade_refusal(row, Some(side), &prepared)? {
            return self.close_row(row, &reason).await;
        }
        let contracts = to_contracts(row, registry)?;
        if contracts.is_zero() {
            return self
                .close_row(row, "Deferred take profit canceled: below one lot")
                .await;
        }
        let contract_value = dec(&registry.get(&row.symbol)?.venue_contract_value)?;
        row.quantity = contracts.mul(&contract_value).to_fixed();
        row.order_type = OrderType::Market;
        row.close_position = false;
        row.reject_reason =
            Some("Awaiting confirmation: reached take profit submission started".into());
        row.created_at = self.now();
        row.updated_at = self.now();
        self.save(row.clone()).await?;
        let client_id = to_cl_ord_id(&row.client_order_id);
        let body = to_body(row, inst_id, &client_id, contracts.to_fixed());
        match self.send(row, inst_id, &body).await {
            Err(error @ EngineError::Okx(OkxError::Rejection { .. })) => {
                row.status = OrderStatus::Rejected;
                row.reject_reason = Some(error.to_string());
                return self.save(row.clone()).await;
            }
            result => {
                // An unanswered market order may already have changed the position.
                snapshot.forget(inst_id);
                self.state()
                    .positions
                    .retain(|p| p.position.symbol != row.symbol);
                if let Err(error) = result {
                    row.reject_reason = Some(format!("Awaiting confirmation: {error}"));
                    self.save(row.clone()).await?;
                }
            }
        }
        self.sync_order(row, registry).await?;
        *snapshot = PositionSnapshot::new(self.deps.client.get_positions().await?)?;
        Ok(())
    }

    /// Called only for a row absent from the pending lists, so a refused lookup is read as "not
    /// live": the pending list is the evidence, and the lookup can only show the algo is still
    /// alive. Any other failure aborts the sync.
    async fn lookup_algo(&self, id: &str) -> Result<Option<Row>, EngineError> {
        match self.deps.client.get_algo(&to_algo_cl_ord_id(id)).await {
            Ok(found) => Ok(found),
            Err(OkxError::Rejection { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// A row OKX lists live: it carries the algoId and no stale reason.
    async fn confirm(
        &self,
        row: &mut Order,
        algo: &Row,
        at: i64,
        alive: &mut Vec<(usize, Row)>,
        index: usize,
    ) -> Result<(), EngineError> {
        let algo_id = nullish_text(algo.get("algoId"));
        if row.exchange_order_id != algo_id
            || row.reject_reason.as_deref().is_some_and(|r| !r.is_empty())
        {
            row.exchange_order_id = algo_id;
            row.reject_reason = None;
            row.updated_at = at;
            self.save(row.clone()).await?;
        }
        match alive.iter_mut().find(|(i, _)| *i == index) {
            Some(entry) => entry.1 = algo.clone(),
            None => alive.push((index, algo.clone())),
        }
        Ok(())
    }

    /// Matches pending algos to our protection rows by algoClOrdId. OKX is the truth: a row counts
    /// only once its algo is seen live, and an algo we did not place is reported as external and
    /// never touched. Per (symbol, leg) the newest live row is in force and older ones are
    /// cancelled; rows on a symbol whose position is missing on two syncs in a row, over at least
    /// 10 s, are cancelled. An algo still pending after a cancel attempt stays on the view,
    /// marked, and the cancel is retried each sync. A row belongs to the held position only if
    /// its side closes it (SELL a long, BUY a short): after a quick re-entry on the other side,
    /// the previous position's algos are cancelled, never put in force.
    ///
    /// Older rows of a leg are cancelled only once the newest, a row sized to the whole position,
    /// covers it. Two entries filling into one position each attach an algo sized to their own
    /// fill, so retiring the older one would leave the rest of the position without that leg on
    /// OKX. A newest per-fill row never retires older ones: the position list can lag behind the
    /// fills this sync confirmed and make it look as if it covers. Until then every row stays
    /// live and, armed, one SL or legacy attached TP algo is placed at the most protective level among the live
    /// rows, never a looser one, sized to the larger of the position and what the rows guard
    /// between them. The sync after OKX lists it retires the rest (placed before cancelled).
    /// Deferred per-entry targets and trailing stops stay distinct.
    pub(super) async fn reconcile_protection(
        &self,
        algos: Vec<Row>,
        snapshot: &mut PositionSnapshot,
        balance: &mut Option<Row>,
    ) -> Result<ProtectionMap, EngineError> {
        match self
            .reconcile_protection_pass(algos, snapshot, true)
            .await?
        {
            Some(result) => Ok(result),
            None => {
                // A market exit can retire algos or outlive a closed position. The original
                // protection snapshot cannot be associated with the newer positions.
                let refreshed = self.pending_protection().await;
                let post_exit = PositionSnapshot::new(self.deps.client.get_positions().await?)?;
                let position_changed = snapshot.held != post_exit.held;
                *snapshot = post_exit;
                let refreshed = if position_changed {
                    let refreshed = self.pending_protection().await?;
                    let latest = PositionSnapshot::new(self.deps.client.get_positions().await?)?;
                    let changed_again = snapshot.held != latest.held;
                    *snapshot = latest;
                    if changed_again {
                        return Err(EngineError::message(
                            "Position changed during protection readback",
                        ));
                    }
                    refreshed
                } else {
                    refreshed?
                };
                *balance = self.deps.client.get_usdt_balance().await?;
                self.reconcile_protection_pass(refreshed, snapshot, false)
                    .await?
                    .ok_or_else(|| EngineError::message("Protection changed during readback"))
            }
        }
    }

    async fn reconcile_protection_pass(
        &self,
        algos: Vec<Row>,
        snapshot: &mut PositionSnapshot,
        submit_deferred: bool,
    ) -> Result<Option<ProtectionMap>, EngineError> {
        let at = self.now();
        let account_id = self.deps.account_id.clone();
        let (open, recent, registry) = self
            .rows(move |rows| {
                Ok((
                    rows.list_open_orders(&account_id)?,
                    rows.list_orders(&account_id, RECENT_ORDERS)?,
                    Registry(rows.instruments()),
                ))
            })
            .await?;
        // Open rows first, in listOpenOrders order (oldest first, its contract), so an index is
        // also a row's rank: a later index is a newer row.
        let mut rows: Vec<Order> = open.into_iter().filter(is_leg).collect();
        let open_count = rows.len();
        let open_ids: HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
        let mut reached_exits = ReachedExits::new();
        for row in &recent {
            remember_reached_exit(&mut reached_exits, row);
        }
        rows.extend(
            recent
                .into_iter()
                .filter(|r| is_leg(r) && !open_ids.contains(&r.id)),
        );
        let mut ours: HashMap<String, usize> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| (to_algo_cl_ord_id(&row.id), i))
            .collect();
        // A live algo with a client id but no row in that window may be ours from further back,
        // most often a cancel OKX refused and newer orders have since pushed out. It is looked up
        // by algoId, so it is still cancelled rather than shown as someone else's.
        let unmatched: Vec<String> = algos
            .iter()
            .filter(|algo| {
                truthy_text(algo.get("algoClOrdId")).is_some_and(|id| !ours.contains_key(&id))
            })
            .filter_map(|algo| truthy_text(algo.get("algoId")))
            .collect();
        if !unmatched.is_empty() {
            let account_id = self.deps.account_id.clone();
            let older = self
                .rows(move |rows| Ok(rows.find_orders_by_exchange_ids(&account_id, &unmatched)?))
                .await?;
            for row in older.into_iter().filter(is_leg) {
                let key = to_algo_cl_ord_id(&row.id);
                if let std::collections::hash_map::Entry::Vacant(slot) = ours.entry(key) {
                    slot.insert(rows.len());
                    rows.push(row);
                }
            }
        }

        let mut result = ProtectionMap::new();
        let mut alive: Vec<(usize, Row)> = Vec::new();
        let mut strays: Vec<(usize, Row)> = Vec::new();
        for algo in algos {
            let index = truthy_text(algo.get("algoClOrdId")).and_then(|id| ours.get(&id).copied());
            match index {
                None => {
                    let inst_id = nullish_text(algo.get("instId")).unwrap_or_default();
                    if let Some(spec) = registry.symbol_of_inst_id(&inst_id) {
                        result
                            .entry(spec.symbol.clone())
                            .or_default()
                            .external
                            .extend(external_of(&algo));
                    }
                }
                Some(i) if rows[i].status == OrderStatus::Open => {
                    let mut row = rows[i].clone();
                    self.confirm(&mut row, &algo, at, &mut alive, i).await?;
                    rows[i] = row;
                }
                Some(i) => strays.push((i, algo)),
            }
        }

        // The pending lists have already confirmed any matching submitted algo. An unsent
        // deferred row has no exchange order to reconcile until its one permitted placement.
        let mut market_exit = false;
        for (i, slot) in rows.iter_mut().enumerate().take(open_count) {
            if slot.status != OrderStatus::Open || alive.iter().any(|(a, _)| *a == i) {
                continue;
            }
            if is_deferred(slot) {
                if !submit_deferred {
                    continue;
                }
                self.submit_deferred_protection(slot, &registry, snapshot, &reached_exits)
                    .await?;
                market_exit |=
                    slot.order_type == OrderType::Market && slot.status != OrderStatus::Rejected;
                remember_reached_exit(&mut reached_exits, slot);
                if is_leg(slot) && slot.status == OrderStatus::Open && !is_deferred(slot) {
                    if let Some(remote) = self.lookup_algo(&slot.id).await? {
                        if remote
                            .get("state")
                            .and_then(Value::as_str)
                            .is_some_and(|s| ALIVE_STATES.contains(&s))
                        {
                            self.confirm(slot, &remote, at, &mut alive, i).await?;
                        }
                    }
                }
                continue;
            }
            let remote = self.lookup_algo(&slot.id).await?;
            let live = remote.as_ref().is_some_and(|r| {
                r.get("state")
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| ALIVE_STATES.contains(&s))
            });
            let row = slot;
            if let (true, Some(remote)) = (live, &remote) {
                self.confirm(row, remote, at, &mut alive, i).await?;
            } else if remote.is_some()
                || row
                    .exchange_order_id
                    .as_deref()
                    .is_some_and(|id| !id.is_empty())
            {
                self.close_row(row, "Gone from OKX").await?;
            } else if at - row.created_at >= NEVER_ARRIVED_AFTER_MS {
                self.close_row(row, "OKX has no record of this algo")
                    .await?;
            }
        }
        if market_exit {
            return Ok(None);
        }

        let held = &snapshot.held;
        // Compared by instId, which every row maps to, so a symbol missing from the registry
        // still counts as held.
        let is_held = |row: &Order| held.contains_key(&to_inst_id(&row.symbol));
        let closes_held = |row: &Order| {
            held.get(&to_inst_id(&row.symbol)).is_some_and(|held| {
                (held.side == PositionSide::Long) == (row.side == OrderSide::Sell)
            })
        };
        let other_side = |row: &Order| is_held(row) && !closes_held(row);
        let key_of = |row: &Order| format!("{}:{:?}", row.symbol, leg_of(row));

        let mut cancel: Vec<(usize, Row, Option<&'static str>)> = Vec::new();
        let mut groups: Vec<(String, Vec<(usize, Row)>)> = Vec::new();
        let mut ranked = alive.clone();
        ranked.sort_by_key(|(i, _)| *i);
        for (i, algo) in ranked {
            if other_side(&rows[i]) {
                cancel.push((i, algo, Some(POSITION_CLOSED)));
                continue;
            }
            let key = key_of(&rows[i]);
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, members)) => members.push((i, algo)),
                None => groups.push((key, vec![(i, algo)])),
            }
        }

        // A position must be missing on two syncs in a row, and for at least
        // POSITION_MISSING_MS since the first miss, before its rows are cancelled: syncs can be
        // milliseconds apart, and the position list may lag behind a fill.
        let mut missing: HashMap<String, Miss> = HashMap::new();
        {
            let prior = self.state().missing_position.clone();
            for i in alive.iter().chain(strays.iter()).map(|(i, _)| *i) {
                let inst_id = to_inst_id(&rows[i].symbol);
                if held.contains_key(&inst_id) || missing.contains_key(&inst_id) {
                    continue;
                }
                let before = prior.get(&inst_id);
                missing.insert(
                    inst_id,
                    Miss {
                        count: before.map_or(0, |m| m.count) + 1,
                        since: before.map_or(at, |m| m.since),
                    },
                );
            }
            let mut state = self.state();
            state.leftovers = missing.keys().cloned().collect();
            state.missing_position = missing.clone();
        }
        let position_gone = |row: &Order| {
            missing
                .get(&to_inst_id(&row.symbol))
                .is_some_and(|m| m.count >= 2 && at - m.since >= POSITION_MISSING_MS)
        };

        // A closed row whose algo is still live: cancel it again, unless it is the only stop for
        // that leg. A row closed with its position, or one for the other side, never protects a
        // later position: those are marked POSITION_CLOSED, so a cancel that fails again cannot
        // let a later same-side position take the algo up.
        for (i, algo) in strays {
            rows[i].exchange_order_id = nullish_text(algo.get("algoId"));
            let key = key_of(&rows[i]);
            let row = &rows[i];
            let closed_for_good = matches!(
                row.reject_reason.as_deref(),
                Some(REMOVED | POSITION_CLOSED)
            );
            if !is_held(row) {
                if position_gone(row) {
                    cancel.push((i, algo, Some(POSITION_CLOSED)));
                }
            } else if other_side(row) {
                cancel.push((i, algo, Some(POSITION_CLOSED)));
            } else if closed_for_good || groups.iter().any(|(k, _)| *k == key) {
                cancel.push((i, algo, None));
            } else {
                let mut row = row.clone();
                row.status = OrderStatus::Open;
                row.reject_reason = None;
                row.updated_at = at;
                self.save(row.clone()).await?;
                rows[i] = row;
                groups.push((key, vec![(i, algo)]));
            }
        }

        let mut uncovered: Vec<(Vec<usize>, Option<Dec>)> = Vec::new();
        for (_, members) in &groups {
            let first = &rows[members[0].0];
            let Some(position) = held.get(&to_inst_id(&first.symbol)) else {
                if position_gone(first) {
                    for (i, algo) in members {
                        cancel.push((*i, algo.clone(), Some(POSITION_CLOSED)));
                    }
                }
                continue;
            };
            let (last, older) = members.split_last().expect("a group has a row");
            if !is_per_fill(&rows[last.0])
                && covers(&rows[last.0], &last.1, &position.contracts, &registry)
            {
                for (i, algo) in older {
                    cancel.push((*i, algo.clone(), Some(REPLACED)));
                }
            } else if !older.is_empty() {
                uncovered.push((
                    members.iter().map(|(i, _)| *i).collect(),
                    group_claim(members, &rows, &registry),
                ));
            }
            let effective = &rows[last.0];
            let Some(leg) = leg_of(effective) else {
                continue;
            };
            let entry = result.entry(effective.symbol.clone()).or_default();
            entry.set(leg, effective.clone());
            if leg == Leg::Trail {
                entry.trail_trigger = num(last.1.get("moveTriggerPx"));
            }
        }

        // A full-size cover changes which algo guards the older fills, so it needs the account
        // armed, read before each placement so that a disarm mid-loop stops the rest. Disarmed,
        // every fill keeps its own algo, which between them still cover the whole position.
        // A placement still awaiting OKX's answer (a protection change, or an earlier cover) may
        // already be the replacement, so nothing more is sent until it is resolved.
        let mut limits: Option<RiskSettings> = None;
        for (members, claim) in uncovered {
            if !(self.deps.is_armed)() {
                break;
            }
            let newest = &rows[*members.last().expect("a group has a row")];
            let Some(leg) = leg_of(newest) else {
                continue;
            };
            // Deferred targets retain their entry-specific prices and sizes. Legacy attached
            // targets still use the whole-position cover policy. Trails always keep their history.
            if leg == Leg::Trail
                || (leg == Leg::Tp
                    && members
                        .iter()
                        .any(|&i| rows[i].id.ends_with(PER_ENTRY_TARGET_SUFFIX)))
            {
                continue;
            }
            let awaiting = rows.iter().take(open_count).any(|r| {
                r.status == OrderStatus::Open
                    && r.symbol == newest.symbol
                    && r.exchange_order_id.as_deref().is_none_or(str::is_empty)
            });
            if awaiting {
                continue;
            }
            let Some(position) = held.get(&to_inst_id(&newest.symbol)) else {
                continue;
            };
            // A size that does not read leaves the per-fill rows as they are.
            let Some(claim) = claim else {
                continue;
            };
            let cover = Held {
                side: position.side,
                contracts: position.contracts.max(&claim),
                trade_id: position.trade_id,
            };
            let group = members.iter().map(|i| &rows[*i]);
            let Some(level) = tightest_level(leg, position.side, group)? else {
                continue;
            };
            if limits.is_none() {
                limits = Some(self.context().await?.limits);
            }
            let limits = limits.as_ref().expect("read above");
            self.cover_position(newest, leg, level, &cover, &registry, limits)
                .await?;
        }

        // Still live on OKX as of this sync, so still shown, marked, until a later sync no longer
        // lists it.
        for (i, algo, _) in &cancel {
            let row = &rows[*i];
            if !is_held(row) {
                continue;
            }
            let marked = external_of(algo).into_iter().map(|e| ExternalProtection {
                detail: format!("{CANCEL_PENDING_PREFIX} {}", e.detail),
                ..e
            });
            result
                .entry(row.symbol.clone())
                .or_default()
                .external
                .extend(marked);
        }
        let mut reasons: Vec<Option<&'static str>> = Vec::new();
        for (_, _, reason) in &cancel {
            if !reasons.contains(reason) {
                reasons.push(*reason);
            }
        }
        let mut failure = None;
        for reason in reasons {
            let batch: Vec<Order> = cancel
                .iter()
                .filter(|(_, _, r)| *r == reason)
                .map(|(i, _, _)| rows[*i].clone())
                .collect();
            if let Some(failed) = self.cancel_rows(batch, reason).await? {
                failure = Some(failed);
            }
        }
        self.state().cancel_error = failure;
        Ok(Some(result))
    }

    /// Places one algo for `newest`'s leg at `level`, the most protective in its group, on
    /// `cover`'s side and sized to its contracts, at least the whole position. It first passes the market checks a protection
    /// change gets (a healthy recent sync, a fresh quote, the level on the right side of the
    /// price); refused, nothing is written or sent, the per-fill algos stay, and a later sync
    /// tries again. Its id is fixed by the row and the size, so it is sent at most once: an
    /// unknown outcome is resolved by lookup on a later sync like any other row, and an OKX
    /// refusal is not retried (the older algos then simply stay). Only an OKX answer is absorbed
    /// here; a store failure aborts the sync.
    async fn cover_position(
        &self,
        newest: &Order,
        leg: Leg,
        level: String,
        cover: &Held,
        registry: &Registry,
        limits: &RiskSettings,
    ) -> Result<(), EngineError> {
        let id = format!("{}:cover:{}", newest.id, cover.contracts.to_fixed());
        let lookup = id.clone();
        if self
            .rows(move |rows| Ok(rows.get_order(&lookup)?))
            .await?
            .is_some()
        {
            return Ok(());
        }
        let Ok(spec) = registry.get(&newest.symbol) else {
            return Ok(());
        };
        let protection = Protection {
            stop_loss: (leg == Leg::Sl).then(|| level.clone()),
            take_profit: (leg == Leg::Tp).then_some(level),
            trailing_stop: None,
        };
        let last = match self.market_refusal(&newest.symbol, limits)? {
            Ok(last) => last,
            Err(_) => return Ok(()),
        };
        if wrong_side_refusal(cover.side, &last, &protection)?.is_some() {
            return Ok(());
        }
        let source = Source {
            account_id: newest.account_id.clone(),
            symbol: newest.symbol.clone(),
            side: cover.side,
            quantity: cover
                .contracts
                .mul(&dec(&spec.venue_contract_value)?)
                .to_fixed(),
            leverage: newest.leverage,
            agent_id: newest.agent_id.clone(),
            decision_id: newest.decision_id.clone(),
        };
        let mut row = protection_row(id, &source, leg, &protection, self.now());
        // Written before anything is sent, so an unanswered request can be looked up by
        // algoClOrdId.
        let inserted = row.clone();
        self.rows(move |rows| Ok(rows.insert_order(&inserted)?))
            .await?;
        match self.place(&mut row, registry).await {
            Ok(()) => Ok(()),
            // Unknown outcome: never placed again; a later sync looks it up.
            Err(EngineError::Okx(OkxError::Unknown(message))) => {
                row.reject_reason = Some(format!("Awaiting confirmation: {message}"));
                self.save(row).await
            }
            Err(error @ (EngineError::Okx(_) | EngineError::Message(_))) => {
                row.status = OrderStatus::Rejected;
                row.reject_reason = Some(error.to_string());
                row.updated_at = self.now();
                self.save(row).await
            }
            Err(error) => Err(error),
        }
    }

    async fn close_row(&self, row: &mut Order, reason: &str) -> Result<(), EngineError> {
        row.status = OrderStatus::Canceled;
        row.reject_reason = Some(reason.into());
        row.updated_at = self.now();
        self.save(row.clone()).await
    }
}
