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
    default_live_limits, ExternalProtection, ExternalProtectionKind, InstrumentSpecification,
    Order, OrderSide, OrderStatus, OrderType, PositionSide, PositionView, Protection, RiskSettings,
    TrailingStopSpec, CANCEL_PENDING_PREFIX,
};
use serde_json::Value;

use super::sync::{isolated_net, PositionSnapshot};
use super::{
    align, align_trailing, dec, is_leg, side_text, to_body, to_contracts, EngineError, LiveRows,
    OkxEngine, Registry, NEVER_ARRIVED_AFTER_MS,
};
use crate::client::{
    nullish_text, truthy_text, AlgoOrdType, AlgoOrderBody, CancelAlgo, OkxError, Row, Side, TdMode,
    ORDER_NOT_FOUND, RECENT_FILLS_LIMIT,
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
pub(super) const ALGO_NOT_ARRIVED: &str = "OKX has no record of this algo";
pub(super) const REACHED_EXIT_UNCONFIRMED: &str =
    "OKX has no record of this market take profit; check its outcome in OKX";
// These durable markers identify an unsent intent. Clearing them before sending permits
// only lookup after a crash, never another send.
const DEFERRED_TRAILING: &str = "Pending entry fill: trailing stop not yet sent";

const DEFERRED_TP: &str = "Pending entry fill: take profit not yet sent";
const DEFERRED_POSITION_ADJUSTED: &str =
    "Pending protection: awaiting position adjustment evidence";

/// Fields read from the current position inside the same queued step as the edit.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProtectionRetention {
    pub take_profit: bool,
    pub trailing_stop: bool,
}

/// The position and protection used in an agent's risk calculation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedProtection {
    pub position_id: String,
    pub quantity: String,
    pub before: Protection,
}

pub struct ProtectionOrderOptions {
    pub retain: ProtectionRetention,
    pub agent_id: Option<String>,
    pub decision_id: Option<String>,
    pub checked: Option<CheckedProtection>,
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
        Some(DEFERRED_TRAILING | DEFERRED_TP | DEFERRED_POSITION_ADJUSTED)
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
#[derive(Clone)]
pub(crate) struct Held {
    pub side: PositionSide,
    pub contracts: Dec,
    pub trade_id: Option<u64>,
    pub position_id: String,
    pub updated_at: Option<String>,
}

impl PartialEq for Held {
    fn eq(&self, other: &Self) -> bool {
        self.same_position_size(other) && self.updated_at == other.updated_at
    }
}

impl Held {
    fn same_position_size(&self, other: &Self) -> bool {
        self.side == other.side
            && self.trade_id == other.trade_id
            && self.position_id == other.position_id
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
    exchange_time(row, "fillTime")
}

fn exchange_time(row: &Row, field: &str) -> Option<i64> {
    row.get(field)?.as_str()?.parse().ok().filter(|at| *at > 0)
}

pub(super) fn is_reached_exit(row: &Order) -> bool {
    row.order_type == OrderType::Market
        && row.reduce_only
        && row.trigger_price.is_some()
        && deferred_parent_id(row, Leg::Tp).is_some()
}

fn is_confirmed_reached_exit(order: &Order, symbol: &str, side: OrderSide) -> bool {
    order.symbol == symbol
        && order.side == side
        && order.status != OrderStatus::Rejected
        && is_reached_exit(order)
}

struct PositionHistory<'a> {
    inst_id: &'a str,
    fills: &'a [Row],
    trade: u64,
    updated_at: i64,
    side: PositionSide,
    contracts: &'a Dec,
    reached_exit_parents: &'a HashMap<String, String>,
}

enum EntryInterval {
    Different,
    Continuous { own_reduction: Dec },
}

impl PositionHistory<'_> {
    fn trade_row(&self, id: u64) -> Option<&Row> {
        self.fills.iter().find(|row| last_trade_id(row) == Some(id))
    }

    fn last_trade_accounts_for_update(&self) -> bool {
        // Fill records can be generated after later position changes. Only the
        // execution time can account for the update directly.
        self.trade_row(self.trade).and_then(fill_time) == Some(self.updated_at)
    }

    fn entry_interval(&self, first_trade: u64, parent: &str) -> Option<EntryInterval> {
        let first = fill_time(self.trade_row(first_trade)?)?;
        let last = fill_time(self.trade_row(self.trade)?)?;
        if first > last || last > self.updated_at {
            return None;
        }
        let (open_type, close_type) = match self.side {
            PositionSide::Long => ("3", "5"),
            PositionSide::Short => ("4", "6"),
        };
        let mut changes = Vec::new();
        for row in self.fills {
            let at = fill_time(row)?;
            let trade = last_trade_id(row);
            // Liquidation and ADL have negative IDs and can change the position
            // after its last ordinary trade without advancing that trade ID.
            if at < first
                || trade.is_some_and(|id| {
                    (at == first && id < first_trade) || at > last || id > self.trade
                })
            {
                continue;
            }
            if at > self.updated_at {
                // A system fill is visible before the position snapshot caught up.
                return None;
            }
            if row.get("instId").and_then(Value::as_str) != Some(self.inst_id)
                || row.get("posSide").and_then(Value::as_str) != Some("net")
            {
                return Some(EntryInterval::Different);
            }
            let opening = row.get("subType").and_then(Value::as_str) == Some(open_type);
            let own_exit = if opening {
                false
            } else {
                let source = row
                    .get("ordId")
                    .and_then(Value::as_str)
                    .and_then(|id| self.reached_exit_parents.get(id));
                let Some(source) = source
                    .filter(|_| row.get("subType").and_then(Value::as_str) == Some(close_type))
                else {
                    return Some(EntryInterval::Different);
                };
                source == parent
            };
            changes.push((trade?, row, opening, own_exit));
        }
        // Walk back from the observed size. Missing reductions can make the
        // chain inconsistent even when the position's trade ID is unchanged.
        changes.sort_unstable_by_key(|(trade, _, _, _)| *trade);
        if changes.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return None;
        }
        let mut own_reduction = super::zero();
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
            return None;
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
    let remaining = remaining.sub(&own_reduction).max(&super::zero());
    (trades.contains(&entry_trade) && total.eq_value(expected) && remaining.lte(history.contracts))
        .then_some(remaining)
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

fn live_trigger(row: &Order, algo: &Row) -> Option<String> {
    let field = match leg_of(row)? {
        Leg::Sl => "slTriggerPx",
        Leg::Tp => "tpTriggerPx",
        Leg::Trail => return None,
    };
    num(algo.get(field)).filter(|price| price != "0")
}

fn live_algo_matches(algo: &Row, leg: Leg, order: &Order) -> Result<bool, EngineError> {
    if !algo
        .get("state")
        .and_then(Value::as_str)
        .is_some_and(|state| ALIVE_STATES.contains(&state))
    {
        return Ok(false);
    }
    let price_matches = |field, expected| {
        let remote = truthy_text(algo.get(field)).filter(|value| value != "0");
        same_price(expected, remote.as_deref()).map_err(decimal_error)
    };
    match leg {
        Leg::Sl => price_matches("slTriggerPx", order.trigger_price.as_deref()),
        Leg::Tp => price_matches("tpTriggerPx", order.trigger_price.as_deref()),
        Leg::Trail => {
            let Some(spec) = spec_of(order) else {
                return Ok(false);
            };
            let field = match spec.kind {
                mochi_domain::TrailingKind::Ratio => "callbackRatio",
                mochi_domain::TrailingKind::Spread => "callbackSpread",
            };
            Ok(price_matches(field, Some(&spec.callback))?
                && price_matches("activePx", spec.activation_price.as_deref())?)
        }
    }
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

fn covers_with_reported_size(
    order: &Order,
    algo: &Row,
    contracts: &Dec,
    registry: &Registry,
) -> bool {
    let (live, recorded) = sizes(order, algo, registry);
    live.zip(recorded)
        .is_some_and(|(live, recorded)| live.min(&recorded).gte(contracts))
}

fn stop_precedes_liquidation(
    stop: &str,
    position: &Row,
    side: PositionSide,
) -> Result<bool, EngineError> {
    let stop = dec(stop)?;
    let liquidation = dec(&truthy_text(position.get("liqPx")).unwrap_or_else(|| "0".into()))?;
    Ok(liquidation.is_zero()
        || match side {
            PositionSide::Long => stop.gt(&liquidation),
            PositionSide::Short => stop.lt(&liquidation),
        })
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
        self.set_protection_order(
            account_id,
            symbol,
            protection,
            ProtectionOrderOptions {
                retain,
                agent_id,
                decision_id,
                checked: None,
            },
        )
        .await
    }

    pub async fn set_protection_order(
        self: &Arc<Self>,
        account_id: &str,
        symbol: &str,
        protection: Protection,
        options: ProtectionOrderOptions,
    ) -> Result<ProtectionResult, EngineError> {
        let (account_id, symbol) = (account_id.to_string(), symbol.to_string());
        self.queued(super::DeferredRestore::Required, move |engine| {
            Box::pin(async move {
                engine
                    .apply_protection(account_id, symbol, protection, options)
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
        options: ProtectionOrderOptions,
    ) -> Result<ProtectionResult, EngineError> {
        let ProtectionOrderOptions {
            retain,
            agent_id,
            decision_id,
            checked,
        } = options;
        let mut positions = self
            .positions()
            .into_iter()
            .filter(|p| p.position.symbol == symbol && p.close_supported == Some(true));
        let position = positions
            .next()
            .ok_or_else(|| EngineError::message(format!("No open {symbol} position")))?;
        if positions.next().is_some() {
            return Err(EngineError::message(format!(
                "Several isolated {symbol} positions match"
            )));
        }
        let context = self.context().await?;
        let spec = context.registry.get(&symbol)?;
        let tick = dec(&spec.price_tick)?;
        let before = self.protection_of(&symbol);
        let old = self
            .state()
            .protection
            .get(&symbol)
            .cloned()
            .unwrap_or_default();
        let position_changed = || {
            EngineError::message(if checked.is_some() {
                "The OKX position or protection changed after the agent's risk check"
            } else {
                "The OKX position changed while checking the stop loss"
            })
        };
        if let Some(checked) = &checked {
            let size = dec(&checked.quantity)?;
            if checked.position_id != position.position.id
                || !size.eq_value(&dec(&position.position.quantity)?)
                || checked.before != before
            {
                return Err(position_changed());
            }
            // An exchange-side edit can invalidate the stop used by the risk check
            // without changing the position's identity or quantity.
            for leg in LEGS {
                if let Some(order) = old.leg(leg) {
                    if !self.checked_algo_matches(leg, order).await? {
                        return Err(position_changed());
                    }
                }
            }
        }
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
            } else if let Some(tp) = old
                .leg(leg)
                .filter(|_| leg == Leg::Tp && !retain.take_profit)
            {
                let contracts = self
                    .check_protection_position(&position, spec, None, &position_changed)
                    .await?;
                let covered = self
                    .live_target_coverage(tp, &contracts, &context.registry)
                    .await?
                    .ok_or_else(|| {
                        EngineError::message("Could not verify the live take profit size")
                    })?;
                if covered {
                    self.check_protection_position(&position, spec, None, &position_changed)
                        .await?;
                } else {
                    changed.push(leg);
                }
            }
        }
        let new_stop = target
            .stop_loss
            .as_deref()
            .filter(|_| changed.contains(&Leg::Sl));
        let removing_trailing = changed.contains(&Leg::Trail) && target.trailing_stop.is_none();
        let source = Source {
            account_id,
            symbol: symbol.clone(),
            side: position.position.side,
            quantity: position.position.quantity.clone(),
            leverage: position.position.leverage,
            agent_id,
            decision_id,
        };
        // A partially filled entry already belongs to an explicit removal.
        let mut removals = Vec::new();
        if !retain.take_profit && target.take_profit.is_none() {
            removals.push((Leg::Tp, REMOVED));
        }
        if !retain.trailing_stop && target.trailing_stop.is_none() {
            removals.push((Leg::Trail, REMOVED));
        }
        let (removed_targets, removed_trailing): (Vec<_>, Vec<_>) = self
            .deferred_to_retire(&source, removals, true)
            .await?
            .into_iter()
            .partition(|(row, _)| leg_of(row) == Some(Leg::Tp));
        let replaced_targets = if changed.contains(&Leg::Tp) && target.take_profit.is_some() {
            self.deferred_to_retire(&source, vec![(Leg::Tp, REPLACED)], true)
                .await?
        } else {
            Vec::new()
        };
        let removes_trailing = removing_trailing || !removed_trailing.is_empty();
        let retained_stop = if removes_trailing && target.stop_loss.is_some() && new_stop.is_none()
        {
            Some(old.leg(Leg::Sl).ok_or_else(position_changed)?)
        } else {
            None
        };
        // Read the position after the entry lookups, before persisting changes or
        // submitting an algo based on the previously selected position.
        if let Some(fixed) = retained_stop {
            self.check_fixed_stop_coverage(fixed, &position, &context.registry, &position_changed)
                .await?;
        } else if checked.is_some()
            || changed.iter().any(|leg| leg.is_set(&target))
            || !removed_targets.is_empty()
            || !removed_trailing.is_empty()
        {
            self.check_protection_position(&position, spec, new_stop, &position_changed)
                .await?;
        }
        let mut retired_deferred = self.retire_deferred(removed_targets).await?;

        // Place new legs before cancelling live algos, so swapping a stop for a
        // trailing stop can never leave the position with neither.
        let fixed_is_only_placement = changed
            .iter()
            .all(|leg| *leg == Leg::Sl || !leg.is_set(&target));
        let mut needs_position_check = false;
        let mut placed_fixed = None;
        for leg in changed.iter().copied().filter(|leg| leg.is_set(&target)) {
            if needs_position_check {
                self.check_follow_up_position(&position, spec, &position_changed)
                    .await?;
            }
            let mut row =
                protection_row((self.deps.new_id)("ord"), &source, leg, &target, self.now());
            // Written before anything is sent, so an unanswered request can be looked up by
            // algoClOrdId.
            let inserted = row.clone();
            // Recovery must know which deferred exits this placement affects
            // even when its answer is lost before the edit can finish.
            let completes_trailing_removal = match leg {
                Leg::Sl => fixed_is_only_placement,
                Leg::Tp => placed_fixed.is_some() || retained_stop.is_some(),
                Leg::Trail => false,
            };
            let replaces = replaced_targets
                .iter()
                .filter(|_| leg == Leg::Tp)
                .chain(
                    removed_trailing
                        .iter()
                        .filter(|_| completes_trailing_removal),
                )
                .map(|(deferred, _)| deferred.id.clone())
                .collect::<Vec<_>>();
            self.rows(move |rows| {
                rows.transaction(&mut || {
                    rows.insert_order(&inserted)?;
                    for deferred in &replaces {
                        rows.link_target_replacement(deferred, &inserted.id)?;
                    }
                    Ok(())
                })
            })
            .await?;
            match self.place(&mut row, &context.registry).await {
                Ok(()) => {
                    if leg == Leg::Sl {
                        placed_fixed = Some(row);
                    }
                }
                Err(error @ EngineError::SessionRetired) => return Err(error),
                // Unknown outcome: never placed again. A sync looks it up by algoClOrdId; if OKX
                // shows it live, the placement is known to have succeeded and the change carries
                // on.
                Err(EngineError::Okx(OkxError::Unknown(message))) => {
                    row.reject_reason = Some(format!("Awaiting confirmation: {message}"));
                    self.save(row.clone()).await?;
                    let _ = self.sync_without_deferred_orders().await;
                    let id = row.id.clone();
                    let resolved = self.rows(move |rows| Ok(rows.get_order(&id)?)).await?;
                    let confirmed = resolved.filter(|r| {
                        r.status == OrderStatus::Open
                            && r.exchange_order_id
                                .as_deref()
                                .is_some_and(|id| !id.is_empty())
                    });
                    if let Some(confirmed) = confirmed {
                        self.check_follow_up_position(&position, spec, &position_changed)
                            .await?;
                        if leg == Leg::Sl {
                            placed_fixed = Some(confirmed);
                        }
                        needs_position_check = false;
                        continue;
                    }
                    // Still unknown: the old algos stay until sync resolves it.
                    return Ok(ProtectionResult {
                        before,
                        after: self.protection_of(&symbol),
                        pending: true,
                        retired_deferred,
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
                    return Err(self.protection_edit_error(&symbol, error).await);
                }
            }
            needs_position_check = true;
        }
        if needs_position_check && changed.contains(&Leg::Tp) && target.take_profit.is_some() {
            self.check_follow_up_position(&position, spec, &position_changed)
                .await?;
        }
        if let Some(fixed) = placed_fixed
            .as_ref()
            .filter(|_| removes_trailing)
            .or(retained_stop
                .filter(|_| changed.contains(&Leg::Tp) && target.take_profit.is_some()))
        {
            if let Err(error) = self
                .check_fixed_stop_coverage(fixed, &position, &context.registry, &position_changed)
                .await
            {
                return Err(self.protection_edit_error(&symbol, error).await);
            }
        }
        for leg in &changed {
            if !leg.is_set(&target) {
                // More than one per-entry order can share a leg. An explicit removal
                // clears all of them, while a preserved field never reaches this branch.
                let account = self.deps.account_id.clone();
                let order_symbol = symbol.clone();
                let leg = *leg;
                let closing_side = if position.position.side == PositionSide::Long {
                    OrderSide::Sell
                } else {
                    OrderSide::Buy
                };
                let active: Vec<Order> = self
                    .rows(move |rows| {
                        Ok(rows
                            .list_open_orders(&account)?
                            .into_iter()
                            .filter(|row| {
                                row.symbol == order_symbol
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
                if let Some(fixed) = placed_fixed
                    .as_ref()
                    .or(retained_stop)
                    .filter(|_| leg == Leg::Trail && !active.is_empty() && changed.len() > 1)
                {
                    // Other edits may have made exchange calls since preflight.
                    if let Err(error) = self
                        .check_fixed_stop_coverage(
                            fixed,
                            &position,
                            &context.registry,
                            &position_changed,
                        )
                        .await
                    {
                        return Err(self.protection_edit_error(&symbol, error).await);
                    }
                }
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

        // Keep the unsent trailing stop until any new protection has succeeded.
        retired_deferred |= self.retire_deferred(removed_trailing).await?;
        if !retain.trailing_stop && target.trailing_stop.is_some() {
            let replaced = self
                .deferred_to_retire(&source, vec![(Leg::Trail, REPLACED)], false)
                .await?;
            retired_deferred |= self.retire_deferred(replaced).await?;
        }
        // A replacement take profit is resolved in reconciliation. The accepted
        // size may be smaller than the position once another entry fills.
        let confirmed = self.sync_now().await.is_ok();
        // Every placement was accepted; if the view could not be refreshed, `after` may still be
        // the old protection, which must not read as "nothing changed".
        Ok(ProtectionResult {
            before,
            after: self.protection_of(&symbol),
            pending: !confirmed && !changed.is_empty(),
            retired_deferred,
        })
    }

    async fn check_protection_position(
        &self,
        position: &PositionView,
        spec: &InstrumentSpecification,
        stop: Option<&str>,
        changed: &impl Fn() -> EngineError,
    ) -> Result<Dec, EngineError> {
        let position = &position.position;
        let remote = self.deps.client.get_positions().await?;
        let (row, side, contracts) =
            super::select_close(&remote, &position.symbol, Some(&position.id))
                .map_err(|_| changed())?;
        let size = dec(&position.quantity)?;
        if !size.eq_value(&contracts.mul(&dec(&spec.venue_contract_value)?)) {
            return Err(changed());
        }
        if let Some(stop) = stop {
            if !stop_precedes_liquidation(stop, row, side)? {
                return Err(EngineError::message(
                    "Stop loss must trigger before liquidation",
                ));
            }
        }
        Ok(contracts)
    }

    async fn check_follow_up_position(
        &self,
        position: &PositionView,
        spec: &InstrumentSpecification,
        changed: &impl Fn() -> EngineError,
    ) -> Result<(), EngineError> {
        if let Err(error) = self
            .check_protection_position(position, spec, None, changed)
            .await
        {
            return Err(self
                .protection_edit_error(&position.position.symbol, error)
                .await);
        }
        Ok(())
    }

    async fn protection_edit_error(&self, symbol: &str, error: EngineError) -> EngineError {
        let confirmed = self.sync_without_deferred_orders().await.is_ok();
        let note = if confirmed {
            ""
        } else {
            "; current protection not yet confirmed from OKX"
        };
        EngineError::ProtectionChange {
            message: format!("{error}{note}"),
            after: confirmed.then(|| Box::new(self.protection_of(symbol))),
        }
    }

    async fn live_target_coverage(
        &self,
        target: &Order,
        contracts: &Dec,
        registry: &Registry,
    ) -> Result<Option<bool>, EngineError> {
        // An uncertain lookup must not be treated as proof that a live target is too small.
        let Some(algo) = self.lookup_algo(&target.id).await? else {
            return Ok(None);
        };
        let (Some(listed), Some(recorded)) = sizes(target, &algo, registry) else {
            return Ok(None);
        };
        Ok(Some(
            live_algo_matches(&algo, Leg::Tp, target)?
                && algo.get("tpOrdPx").and_then(Value::as_str) == Some("-1")
                && listed.min(&recorded).gte(contracts),
        ))
    }

    async fn live_fixed_stop_covers(
        &self,
        fixed: &Order,
        contracts: &Dec,
        registry: &Registry,
    ) -> Result<bool, EngineError> {
        let Some(algo) = self.lookup_algo(&fixed.id).await? else {
            return Ok(false);
        };
        Ok(live_algo_matches(&algo, Leg::Sl, fixed)?
            && algo.get("slOrdPx").and_then(Value::as_str) == Some("-1")
            && covers_with_reported_size(fixed, &algo, contracts, registry))
    }

    async fn refresh_matching_position(
        &self,
        snapshot: &mut PositionSnapshot,
        inst_id: &str,
    ) -> Result<bool, EngineError> {
        let fresh = PositionSnapshot::new(self.deps.client.get_positions().await?)?;
        let before = isolated_net_position(&snapshot.open, inst_id);
        let after = isolated_net_position(&fresh.open, inst_id);
        let same = before
            .zip(after)
            .is_some_and(|((before, size), (after, fresh_size))| {
                let side = if size.is_negative() {
                    PositionSide::Short
                } else {
                    PositionSide::Long
                };
                size.eq_value(&fresh_size)
                && super::views::position_id(before, side) == super::views::position_id(after, side)
                // Some system reductions do not change tradeId.
                && before.get("uTime") == after.get("uTime")
            });
        snapshot.update_positions(fresh);
        Ok(same)
    }

    async fn check_fixed_stop_coverage(
        &self,
        fixed: &Order,
        position: &PositionView,
        registry: &Registry,
        changed: &impl Fn() -> EngineError,
    ) -> Result<(), EngineError> {
        let spec = registry.get(&position.position.symbol)?;
        let contracts = self
            .check_protection_position(position, spec, fixed.trigger_price.as_deref(), changed)
            .await?;
        // Read size and price together after the position lookup. An unchanged
        // trigger alone does not establish full coverage.
        let algo = self.lookup_algo(&fixed.id).await?.ok_or_else(changed)?;
        if !live_algo_matches(&algo, Leg::Sl, fixed)? {
            return Err(changed());
        }
        if algo.get("slOrdPx").and_then(Value::as_str) != Some("-1") {
            return Err(EngineError::message(
                "The fixed stop loss must be a market order to remove the trailing stop",
            ));
        }
        if !covers_with_reported_size(fixed, &algo, &contracts, registry) {
            return Err(EngineError::message(
                "The fixed stop loss does not cover the entire OKX position",
            ));
        }
        self.check_protection_position(position, spec, fixed.trigger_price.as_deref(), changed)
            .await?;
        Ok(())
    }

    async fn deferred_to_retire(
        &self,
        source: &Source,
        legs: Vec<(Leg, &'static str)>,
        check_exchange: bool,
    ) -> Result<Vec<(Order, &'static str)>, EngineError> {
        if legs.is_empty() {
            return Ok(Vec::new());
        }
        let account = source.account_id.clone();
        let symbol = source.symbol.clone();
        let side = if source.side == PositionSide::Long {
            OrderSide::Sell
        } else {
            OrderSide::Buy
        };
        let candidates = self
            .rows(move |rows| {
                let mut candidates = Vec::new();
                for pending in rows
                    .list_open_orders(&account)?
                    .into_iter()
                    .filter(|row| row.symbol == symbol && row.side == side && is_deferred(row))
                {
                    let Some(&(leg, reason)) =
                        legs.iter().find(|(leg, _)| leg_of(&pending) == Some(*leg))
                    else {
                        continue;
                    };
                    let Some(parent_id) = deferred_parent_id(&pending, leg) else {
                        continue;
                    };
                    if let Some(parent) = rows.get_order(parent_id)? {
                        candidates.push((pending, parent, reason));
                    }
                }
                Ok(candidates)
            })
            .await?;
        let mut retired = Vec::new();
        for (pending, parent, reason) in candidates {
            let retire = if check_exchange && parent.status == OrderStatus::Open {
                // The current position may already contain some or all of this
                // order. Truly unfilled entries keep their own future take profit.
                let remote = self
                    .deps
                    .client
                    .get_order(
                        &to_inst_id(&parent.symbol),
                        &to_cl_ord_id(&parent.client_order_id),
                    )
                    .await?;
                let state = remote
                    .as_ref()
                    .and_then(|row| row.get("state"))
                    .and_then(Value::as_str);
                let filled = remote
                    .as_ref()
                    .and_then(|row| truthy_text(row.get("accFillSz")))
                    .map(|size| dec(&size))
                    .transpose()?;
                matches!(
                    state,
                    Some("partially_filled" | "filled" | "canceled" | "mmp_canceled")
                ) || filled.is_some_and(|size| size.gt(&super::zero()))
                    || dec(&parent.filled_quantity)?.gt(&super::zero())
            } else {
                parent.status != OrderStatus::Open
            };
            if retire {
                retired.push((pending, reason));
            }
        }
        Ok(retired)
    }

    async fn retire_deferred(
        &self,
        retired: Vec<(Order, &'static str)>,
    ) -> Result<bool, EngineError> {
        let changed = !retired.is_empty();
        for (mut pending, reason) in retired {
            self.close_row(&mut pending, reason).await?;
        }
        Ok(changed)
    }

    /// Node's `try` block for one leg: the body, the placement and the saved algoId.
    async fn place(&self, row: &mut Order, registry: &Registry) -> Result<(), EngineError> {
        let body = algo_body(row, registry)?;
        self.check_dispatch()?;
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
        let mut dispatched = HashSet::new();
        let mut retirement = None;
        for batch in sent.chunks(10) {
            if let Err(error) = self.check_dispatch() {
                if dispatched.is_empty() {
                    return Err(error);
                }
                retirement = Some(error);
                break;
            }
            dispatched.extend(batch.iter().map(|item| item.algo_id.as_str()));
            if let Err(error) = self.deps.client.cancel_algos(batch).await {
                failure = Some(format!("Could not cancel an OKX algo order: {error}"));
            }
        }
        let now = self.now();
        let rows: Vec<Order> = rows
            .into_iter()
            // A later batch can still be live when a previously sent batch is reconciled.
            .filter(|row| {
                retirement.is_none()
                    || row
                        .exchange_order_id
                        .as_deref()
                        .is_some_and(|id| dispatched.contains(id))
            })
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
        if let Some(error) = retirement {
            return Err(error);
        }
        Ok(failure)
    }

    /// True when the linked stop has resolved the removal or still needs confirmation.
    /// A known loss of coverage leaves the original trailing intent eligible to send.
    async fn resolve_deferred_trailing_removal(
        &self,
        row: &mut Order,
        linked_replacements: &[Order],
        registry: &Registry,
        snapshot: &mut PositionSnapshot,
    ) -> Result<bool, EngineError> {
        let Some(completion) = linked_replacements.iter().rev().find(|order| {
            order.status == OrderStatus::Open
                && order.symbol == row.symbol
                && order.side == row.side
        }) else {
            return Ok(false);
        };
        if completion
            .exchange_order_id
            .as_deref()
            .is_none_or(str::is_empty)
        {
            return Ok(true);
        }
        let fixed = match leg_of(completion) {
            Some(Leg::Sl) => completion.clone(),
            Some(Leg::Tp) => {
                let Some(confirmed) = self.lookup_algo(&completion.id).await? else {
                    return Ok(true);
                };
                if !live_algo_matches(&confirmed, Leg::Tp, completion)? {
                    return Ok(false);
                }
                // The edit already confirmed a new or retained fixed stop.
                // Recheck its current coverage before retiring the trail.
                let account = row.account_id.clone();
                let symbol = row.symbol.clone();
                let side = row.side;
                let fixed = self
                    .rows(move |rows| {
                        Ok(rows
                            .list_open_orders(&account)?
                            .into_iter()
                            .rev()
                            .find(|order| {
                                order.symbol == symbol
                                    && order.side == side
                                    && leg_of(order) == Some(Leg::Sl)
                                    && order
                                        .exchange_order_id
                                        .as_deref()
                                        .is_some_and(|id| !id.is_empty())
                            }))
                    })
                    .await?;
                let Some(fixed) = fixed else {
                    return Ok(false);
                };
                fixed
            }
            _ => return Ok(false),
        };
        let inst = to_inst_id(&row.symbol);
        let side = match fixed.side {
            OrderSide::Buy => PositionSide::Short,
            OrderSide::Sell => PositionSide::Long,
        };
        snapshot.update_positions(PositionSnapshot::new(
            self.deps.client.get_positions().await?,
        )?);
        let Some((position, original_size)) = isolated_net_position(&snapshot.open, &inst) else {
            return Ok(false);
        };
        if (side == PositionSide::Short) != original_size.is_negative() {
            return Ok(false);
        }
        let original_id = super::views::position_id(position, side);
        let Some(algo) = self.lookup_algo(&fixed.id).await? else {
            return Ok(true);
        };
        if !live_algo_matches(&algo, Leg::Sl, &fixed)?
            || algo.get("slOrdPx").and_then(Value::as_str) != Some("-1")
        {
            return Ok(false);
        }
        snapshot.update_positions(PositionSnapshot::new(
            self.deps.client.get_positions().await?,
        )?);
        let Some((position, signed_size)) = isolated_net_position(&snapshot.open, &inst) else {
            return Ok(false);
        };
        if !original_size.eq_value(&signed_size)
            || super::views::position_id(position, side) != original_id
        {
            return Ok(true);
        }
        let contracts = if side == PositionSide::Short {
            signed_size.neg()
        } else {
            signed_size
        };
        let Some(stop) = fixed.trigger_price.as_deref() else {
            return Ok(false);
        };
        if !covers_with_reported_size(&fixed, &algo, &contracts, registry)
            || !stop_precedes_liquidation(stop, position, side)?
        {
            return Ok(false);
        }
        self.close_row(row, REMOVED).await?;
        Ok(true)
    }

    async fn reached_exit_parents(
        &self,
        row: &Order,
        inst_id: &str,
        fills: &[Row],
        close_subtype: &str,
    ) -> Result<HashMap<String, String>, EngineError> {
        // The proof below requires a complete page. Only look up IDs from fills
        // that could belong to a reached exit on this instrument and side.
        if fills.len() >= RECENT_FILLS_LIMIT {
            return Ok(HashMap::new());
        }
        let mut ids: Vec<_> = fills
            .iter()
            .filter(|fill| {
                fill.get("instId").and_then(Value::as_str) == Some(inst_id)
                    && fill.get("posSide").and_then(Value::as_str) == Some("net")
                    && fill.get("subType").and_then(Value::as_str) == Some(close_subtype)
            })
            .filter_map(|fill| fill.get("ordId").and_then(Value::as_str))
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let account_id = row.account_id.clone();
        let symbol = row.symbol.clone();
        let side = row.side;
        self.rows(move |rows| {
            // A confirmed exit can be older than the recent local orders window.
            let orders = rows.find_orders_by_exchange_ids(&account_id, &ids)?;
            Ok(orders
                .into_iter()
                .filter(|order| is_confirmed_reached_exit(order, &symbol, side))
                .filter_map(|order| {
                    let parent = deferred_parent_id(&order, Leg::Tp)?.to_owned();
                    Some((order.exchange_order_id?, parent))
                })
                .collect())
        })
        .await
    }

    async fn awaiting_exit_attribution(
        &self,
        row: &Order,
        mut exchange_ids: Vec<String>,
        client_ids: HashSet<String>,
    ) -> Result<bool, EngineError> {
        if exchange_ids.is_empty() && client_ids.is_empty() {
            return Ok(false);
        }
        exchange_ids.sort_unstable();
        exchange_ids.dedup();
        let account_id = row.account_id.clone();
        let symbol = row.symbol.clone();
        let side = row.side;
        self.rows(move |rows| {
            // Retired targets can still fill. Their venue ID is a hash of the
            // local key, so compare it only among eligible recovery candidates.
            if !client_ids.is_empty()
                && rows.any_canceled_order_for_recovery(
                    &account_id,
                    &symbol,
                    side,
                    REACHED_EXIT_UNCONFIRMED,
                    &|order| {
                        is_reached_exit(order)
                            && client_ids.contains(&to_cl_ord_id(&order.client_order_id))
                    },
                )?
            {
                return Ok(true);
            }
            // Another engine may have confirmed the exit after our first
            // read and removed it from recovery. Retry attribution next sync.
            Ok(rows
                .find_orders_by_exchange_ids(&account_id, &exchange_ids)?
                .iter()
                .any(|order| is_confirmed_reached_exit(order, &symbol, side)))
        })
        .await
    }

    async fn submit_deferred_protection(
        &self,
        row: &mut Order,
        registry: &Registry,
        snapshot: &mut PositionSnapshot,
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
        let id = row.id.clone();
        let linked_replacements = self
            .rows(move |rows| Ok(rows.target_replacements(&id)?))
            .await?;
        if leg == Leg::Trail
            && self
                .resolve_deferred_trailing_removal(row, &linked_replacements, registry, snapshot)
                .await?
        {
            return Ok(());
        }
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
        let replacement = context.open_orders.iter().rev().find(|other| {
            other.symbol == row.symbol
                && other.side == row.side
                && !is_per_fill(other)
                && leg_of(other) == Some(leg)
                // Keep the timestamp fallback for orders written before replacement
                // links existed. A later partial fill changes the parent's timestamp.
                && (other.created_at >= parent.updated_at
                    || linked_replacements.iter().any(|linked| linked.id == other.id))
        });
        if let Some(replacement) = replacement {
            if replacement
                .exchange_order_id
                .as_deref()
                .is_none_or(str::is_empty)
            {
                // A manual replacement of unknown outcome must resolve before this intent sends.
                return Ok(());
            }
            if leg != Leg::Tp {
                return self.close_row(row, REPLACED).await;
            }
        }
        let remote = self
            .deps
            .client
            .get_order(&inst_id, &to_cl_ord_id(&parent.client_order_id))
            .await?;
        let needs_replacement_stop =
            if parent.status == OrderStatus::Canceled && parent.attached_stop_loss.is_some() {
                // Single attached stops also apply to a partial fill when the remainder is
                // canceled. Confirm the fixed stop appeared before placing optional exits.
                let stop = self.lookup_algo(&format!("{}:sl", parent.id)).await?;
                !stop
                    .as_ref()
                    .and_then(|algo| algo.get("state"))
                    .and_then(Value::as_str)
                    .is_some_and(|state| ALIVE_STATES.contains(&state))
            } else {
                false
            };
        // Read fresh after the parent/stop calls, which may have outlived the earlier snapshot.
        snapshot.update_positions(PositionSnapshot::new(
            self.deps.client.get_positions().await?,
        )?);
        let Some((position, signed_size)) = isolated_net_position(&snapshot.open, &inst_id) else {
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
        let (Some(entry_trade), Some(position_trade), Some(position_updated_at)) = (
            order_trade,
            last_trade_id(position),
            exchange_time(position, "uTime"),
        ) else {
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
        let observed = Held {
            side,
            contracts: if short {
                signed_size.neg()
            } else {
                signed_size.clone()
            },
            trade_id: Some(position_trade),
            position_id: super::views::position_id(position, side),
            updated_at: nullish_text(position.get("uTime")),
        };
        if needs_replacement_stop {
            // An SL-only edit may have superseded the attached stop while leaving
            // the entry's deferred exits in place.
            let replacement_stop = context.open_orders.iter().rev().find(|other| {
                other.symbol == row.symbol
                    && other.side == row.side
                    && !is_per_fill(other)
                    && leg_of(other) == Some(Leg::Sl)
                    && other
                        .exchange_order_id
                        .as_deref()
                        .is_some_and(|id| !id.is_empty())
            });
            let mut confirmed = false;
            if let Some((fixed, stop)) = replacement_stop
                .and_then(|fixed| fixed.trigger_price.as_deref().map(|stop| (fixed, stop)))
            {
                let contracts = if short {
                    signed_size.neg()
                } else {
                    signed_size.clone()
                };
                if self
                    .live_fixed_stop_covers(fixed, &contracts, registry)
                    .await?
                {
                    if !self.refresh_matching_position(snapshot, &inst_id).await? {
                        return Ok(());
                    }
                    let (position, _) = isolated_net_position(&snapshot.open, &inst_id)
                        .expect("the fresh position matched");
                    confirmed = stop_precedes_liquidation(stop, position, side)?;
                }
            }
            if !confirmed {
                if confirming {
                    return Ok(());
                }
                return self.close_row(row, "Deferred protection canceled: OKX did not confirm the fixed stop for the partial fill").await;
            }
        }
        if let Some(replacement) = replacement {
            let contracts = if short {
                signed_size.neg()
            } else {
                signed_size
            };
            if self
                .live_target_coverage(replacement, &contracts, registry)
                .await?
                == Some(true)
                && self.refresh_matching_position(snapshot, &inst_id).await?
            {
                return self.close_row(row, REPLACED).await;
            }
            // Until coverage is known, keep intent pending without sending a
            // second target that could overlap the already accepted replacement.
            return Ok(());
        }
        if leg == Leg::Tp
            && linked_replacements.iter().any(|replacement| {
                replacement
                    .exchange_order_id
                    .as_deref()
                    .is_some_and(|id| !id.is_empty())
            })
        {
            // Acceptance of the replacement superseded this price. A later fill
            // or cancellation of the replacement cannot authorize the old target.
            return Ok(());
        }
        let remote = remote
            .as_ref()
            .expect("the order trade came from a remote order");
        let expected = dec(&parent.filled_quantity)?;
        let contract_value = dec(&registry.get(&row.symbol)?.venue_contract_value)?;
        let expected = expected.div(&contract_value).map_err(decimal_error)?;
        let filled = {
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
                        && other.side == row.side
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
            let reached_exit_parents = self
                .reached_exit_parents(row, &inst_id, &fills, close_subtype)
                .await?;
            let unattributed_closings = fills.iter().filter(|fill| {
                last_trade_id(fill)
                    .is_some_and(|id| (first_entry_trade..=position_trade).contains(&id))
                    && fill.get("instId").and_then(Value::as_str) == Some(inst_id.as_str())
                    && fill.get("posSide").and_then(Value::as_str) == Some("net")
                    && fill.get("subType").and_then(Value::as_str) == Some(close_subtype)
                    && fill
                        .get("ordId")
                        .and_then(Value::as_str)
                        .is_none_or(|id| !reached_exit_parents.contains_key(id))
            });
            let awaiting_active = unattributed_closings.clone().any(|fill| {
                match fill.get("clOrdId").and_then(Value::as_str) {
                    Some(id) if !id.is_empty() => unresolved.contains(id),
                    // Only an active unresolved target can justify waiting on an
                    // anonymous reduction. Retired ones require a matching ID.
                    _ => !unresolved.is_empty(),
                }
            });
            let history = PositionHistory {
                inst_id: &inst_id,
                fills: &fills,
                trade: position_trade,
                updated_at: position_updated_at,
                side,
                contracts: &observed.contracts,
                reached_exit_parents: &reached_exit_parents,
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
                _ if awaiting_active => return Ok(()),
                // The position and parent can become visible before the fills endpoint catches up.
                None if confirming
                    || row.reject_reason.as_deref() == Some(DEFERRED_POSITION_ADJUSTED) =>
                {
                    return Ok(());
                }
                _ => {
                    let exchange_ids = if fills.len() < RECENT_FILLS_LIMIT {
                        unattributed_closings
                            .clone()
                            .filter_map(|fill| fill.get("ordId").and_then(Value::as_str))
                            .filter(|id| !id.is_empty())
                            .map(str::to_owned)
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let client_ids = unattributed_closings
                        .filter_map(|fill| fill.get("clOrdId").and_then(Value::as_str))
                        .filter(|id| {
                            id.len() == 32
                                && id.bytes().all(|byte| {
                                    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                                })
                        })
                        .map(str::to_owned)
                        .collect();
                    if self
                        .awaiting_exit_attribution(row, exchange_ids, client_ids)
                        .await?
                    {
                        // A client ID can justify waiting. Only order lookup can
                        // persist the exchange ID used to prove position lifetime.
                        return Ok(());
                    }
                    return self
                        .close_row(
                            row,
                            "Deferred protection canceled: entry position lifetime cannot be confirmed",
                        )
                        .await;
                }
            };
            let explained_by_trade = history.last_trade_accounts_for_update();
            let known_size = !explained_by_trade
                && self
                    .state()
                    .confirmed_entry_positions
                    .get(&inst_id)
                    .is_some_and(|confirmed| {
                        confirmed.same_position_size(&observed)
                            && confirmed
                                .updated_at
                                .as_deref()
                                .and_then(|at| at.parse::<i64>().ok())
                                .is_some_and(|at| at < position_updated_at)
                    });
            // Liquidation and ADL can leave tradeId unchanged and only reduce size.
            // The same previously confirmed size rules out a later system reduction.
            // A first observation after an adjustment cannot establish that by itself.
            if !explained_by_trade && !known_size {
                if row.reject_reason.as_deref() != Some(DEFERRED_POSITION_ADJUSTED) {
                    row.reject_reason = Some(DEFERRED_POSITION_ADJUSTED.into());
                    row.updated_at = self.now();
                    self.save(row.clone()).await?;
                }
                return Ok(());
            }
            if !self.refresh_matching_position(snapshot, &inst_id).await? {
                return Ok(());
            }
            if explained_by_trade {
                self.state()
                    .confirmed_entry_positions
                    .insert(inst_id.clone(), observed);
            }
            filled
        };
        let unsent = row.clone();
        row.quantity = filled.mul(&contract_value).to_fixed();
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
                    .submit_reached_target(row, unsent, &inst_id, registry, side, snapshot)
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
        let _staged = self.stage_order(super::UnsentOrder::Deferred {
            original: Box::new(unsent),
            marker: Box::new(row.clone()),
        })?;
        self.save(row.clone()).await?;
        if !self
            .recheck_deferred_dispatch(row, snapshot, &inst_id)
            .await?
        {
            return Ok(());
        }
        self.handoff_order(row)?;
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
        unsent: Order,
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
        let _staged = self.stage_order(super::UnsentOrder::Deferred {
            original: Box::new(unsent),
            marker: Box::new(row.clone()),
        })?;
        self.save(row.clone()).await?;
        let client_id = to_cl_ord_id(&row.client_order_id);
        let body = to_body(row, inst_id, &client_id, contracts.to_fixed());
        if !self
            .recheck_deferred_dispatch(row, snapshot, inst_id)
            .await?
        {
            return Ok(());
        }
        match self.send(row, inst_id, &body).await {
            Err(error @ EngineError::SessionRetired) => return Err(error),
            Err(error @ EngineError::Okx(OkxError::Rejection { .. })) => {
                row.status = OrderStatus::Rejected;
                row.reject_reason = Some(error.to_string());
                return self.save(row.clone()).await;
            }
            result => {
                // An unanswered market order may already have changed the position.
                snapshot.post_exit_unverified = true;
                if let Err(error) = result {
                    row.reject_reason = Some(format!("Awaiting confirmation: {error}"));
                    self.save(row.clone()).await?;
                }
            }
        }
        self.sync_order(row, registry).await?;
        snapshot.update_positions(PositionSnapshot::new(
            self.deps.client.get_positions().await?,
        )?);
        Ok(())
    }

    async fn recheck_deferred_dispatch(
        &self,
        row: &mut Order,
        snapshot: &mut PositionSnapshot,
        inst_id: &str,
    ) -> Result<bool, EngineError> {
        let current = self.refresh_matching_position(snapshot, inst_id).await;
        if !matches!(current, Ok(true)) {
            // The POST has not started. Restore the intent for fresh attribution.
            // A crash before this write leaves the marker eligible only for lookup.
            self.abandon_staged_order()?;
            if let Some(restored) = self.restore_unsent_order().await? {
                *row = restored;
            }
        }
        current
    }

    /// A temporary refusal cannot establish whether the algo is still live.
    async fn lookup_algo(&self, id: &str) -> Result<Option<Row>, EngineError> {
        match self.deps.client.get_algo(&to_algo_cl_ord_id(id)).await {
            Ok(found) => Ok(found),
            Err(OkxError::Rejection { code, .. }) if code == ORDER_NOT_FOUND => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn checked_algo_matches(&self, leg: Leg, order: &Order) -> Result<bool, EngineError> {
        let Some(algo) = self.lookup_algo(&order.id).await? else {
            return Ok(false);
        };
        live_algo_matches(&algo, leg, order)
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
        let trigger = live_trigger(row, algo);
        if row.exchange_order_id != algo_id
            || row.reject_reason.as_deref().is_some_and(|r| !r.is_empty())
            || trigger
                .as_ref()
                .is_some_and(|price| row.trigger_price.as_ref() != Some(price))
        {
            row.exchange_order_id = algo_id;
            row.reject_reason = None;
            if trigger.is_some() {
                row.trigger_price = trigger;
            }
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
        submit_deferred: bool,
    ) -> Result<ProtectionMap, EngineError> {
        match self
            .reconcile_protection_pass(algos, snapshot, submit_deferred)
            .await?
        {
            Some(result) => Ok(result),
            None => {
                // An exit or a position change can invalidate the earlier algo list.
                let refreshed = self.pending_protection().await;
                let post_exit = PositionSnapshot::new(self.deps.client.get_positions().await?)?;
                let position_changed = snapshot.held != post_exit.held;
                snapshot.update_positions(post_exit);
                let refreshed = if position_changed {
                    let refreshed = self.pending_protection().await?;
                    let latest = PositionSnapshot::new(self.deps.client.get_positions().await?)?;
                    let changed_again = snapshot.held != latest.held;
                    snapshot.update_positions(latest);
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
                snapshot.post_exit_unverified = false;
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
        let initial_held = snapshot.held.clone();
        let mut refresh_readback = false;
        for (i, slot) in rows.iter_mut().enumerate().take(open_count) {
            if slot.status != OrderStatus::Open || alive.iter().any(|(a, _)| *a == i) {
                continue;
            }
            if is_deferred(slot) {
                if !submit_deferred {
                    continue;
                }
                self.submit_deferred_protection(slot, &registry, snapshot)
                    .await?;
                refresh_readback |= snapshot.held != initial_held
                    || (slot.order_type == OrderType::Market
                        && slot.status != OrderStatus::Rejected);
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
                if let Some(id) = remote
                    .as_ref()
                    .and_then(|algo| truthy_text(algo.get("algoId")))
                {
                    row.exchange_order_id = Some(id);
                }
                self.close_row(row, "Gone from OKX").await?;
            } else if at - row.created_at >= NEVER_ARRIVED_AFTER_MS {
                self.expire_unconfirmed_order(row, ALGO_NOT_ARRIVED, self.now())
                    .await?;
            }
        }
        if refresh_readback || snapshot.post_exit_unverified {
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
                row.trigger_price = live_trigger(&row, &algo).or(row.trigger_price);
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
                    && !is_deferred(r)
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
                contracts: position.contracts.max(&claim),
                ..position.clone()
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
