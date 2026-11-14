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

use super::{
    align, align_trailing, dec, is_leg, side_text, to_contracts, EngineError, LiveRows, OkxEngine,
    Registry, NEVER_ARRIVED_AFTER_MS,
};
use crate::client::{
    nullish_text, truthy_text, AlgoOrdType, AlgoOrderBody, CancelAlgo, OkxError, Row, Side, TdMode,
};
use crate::ids::{to_algo_cl_ord_id, to_inst_id};

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

/// Rows for what a filled entry attached (wholly, or partly before a cancel). Before that no rows
/// exist, so cleanup can never touch a resting entry's protection. The ids are fixed by the
/// entry's attachAlgoClOrdId tags. Runs inside one store turn, after the entry is saved.
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
        let (account_id, symbol) = (account_id.to_string(), symbol.to_string());
        self.queued(move |engine| {
            Box::pin(async move {
                engine
                    .apply_protection(account_id, symbol, protection, agent_id, decision_id)
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
        agent_id: Option<String>,
        decision_id: Option<String>,
    ) -> Result<ProtectionResult, EngineError> {
        let position = self
            .positions()
            .into_iter()
            .find(|p| p.position.symbol == symbol)
            .ok_or_else(|| EngineError::message(format!("No open {symbol} position")))?;
        let context = self.context().await?;
        let tick = dec(&context.registry.get(&symbol)?.price_tick)?;
        let target = Protection {
            stop_loss: align(&requested.stop_loss, &tick)?,
            take_profit: align(&requested.take_profit, &tick)?,
            trailing_stop: requested
                .trailing_stop
                .map(|spec| align_trailing(spec, &tick))
                .transpose()?,
        };
        let before = self.protection_of(&symbol);
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
            // A sync above may already have retired a superseded row; it is not cancelled twice.
            let Some(previous) = old.leg(*leg) else {
                continue;
            };
            let id = previous.id.clone();
            let row = self.rows(move |rows| Ok(rows.get_order(&id)?)).await?;
            if let Some(row) = row.filter(|r| r.status == OrderStatus::Open) {
                let reason = if leg.is_set(&target) {
                    REPLACED
                } else {
                    REMOVED
                };
                self.cancel_rows(vec![row], Some(reason)).await?;
            }
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
        let healthy = self.healthy();
        let (has_balance, sync_error) = {
            let state = self.state();
            (state.balance.is_some(), state.sync_error.clone())
        };
        if !healthy || !has_balance {
            let why = sync_error.unwrap_or_else(|| "no recent sync with OKX".into());
            return Ok(Some(format!("EXCHANGE_NOT_SYNCED: {why}")));
        }
        let limits = &context.limits;
        let Some(quote) = (self.deps.get_quote)(symbol) else {
            return Ok(Some("NO_MARKET_DATA: no quote".into()));
        };
        let age = (self.now() - quote.ts) as f64 / 1000.0;
        if age > limits.max_quote_age_sec {
            return Ok(Some(format!(
                "MARKET_DATA_STALE: quote is {}s old",
                mochi_domain::jsnum::js_to_fixed(age, 0)
            )));
        }

        if limits.require_stop_loss && before.stop_loss.is_some() && target.stop_loss.is_none() {
            return Ok(Some(
                "LIMIT_STOP_REQUIRED: positions must keep a stop loss".into(),
            ));
        }
        let last = dec(&quote.last)?;
        let long = side == PositionSide::Long;
        // A stop must sit below the price for a long and above it for a short; a target the
        // other way round.
        let wrong_side = |price: &str, below: bool| -> Result<bool, EngineError> {
            let price = dec(price)?;
            Ok(if below {
                price.gte(&last)
            } else {
                price.lte(&last)
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

    /// Called only for a row absent from the pending lists, so a refused lookup is read as "not
    /// live": the pending list is the evidence, and the lookup can only show the algo is still
    /// alive. Any other failure aborts the sync.
    async fn lookup_algo(&self, row: &Order) -> Result<Option<Row>, EngineError> {
        match self.deps.client.get_algo(&to_algo_cl_ord_id(&row.id)).await {
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
    pub(super) async fn reconcile_protection(
        &self,
        algos: Vec<Row>,
        held: &HashMap<String, PositionSide>,
    ) -> Result<ProtectionMap, EngineError> {
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
        let ours: HashMap<String, usize> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| (to_algo_cl_ord_id(&row.id), i))
            .collect();

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

        // Our open rows OKX did not list: lagging, finished, or never arrived.
        for (i, slot) in rows.iter_mut().enumerate().take(open_count) {
            if alive.iter().any(|(a, _)| *a == i) {
                continue;
            }
            let remote = self.lookup_algo(slot).await?;
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

        // Compared by instId, which every row maps to, so a symbol missing from the registry
        // still counts as held.
        let is_held = |row: &Order| held.contains_key(&to_inst_id(&row.symbol));
        let closes_held = |row: &Order| {
            held.get(&to_inst_id(&row.symbol))
                .is_some_and(|side| (*side == PositionSide::Long) == (row.side == OrderSide::Sell))
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
        // later position.
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
                    cancel.push((i, algo, None));
                }
            } else if closed_for_good || other_side(row) || groups.iter().any(|(k, _)| *k == key) {
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

        for (_, members) in &groups {
            let first = &rows[members[0].0];
            if !is_held(first) {
                if position_gone(first) {
                    for (i, algo) in members {
                        cancel.push((*i, algo.clone(), Some(POSITION_CLOSED)));
                    }
                }
                continue;
            }
            let (last, older) = members.split_last().expect("a group has a row");
            for (i, algo) in older {
                cancel.push((*i, algo.clone(), Some(REPLACED)));
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
        Ok(result)
    }

    async fn close_row(&self, row: &mut Order, reason: &str) -> Result<(), EngineError> {
        row.status = OrderStatus::Canceled;
        row.reject_reason = Some(reason.into());
        row.updated_at = self.now();
        self.save(row.clone()).await
    }
}
