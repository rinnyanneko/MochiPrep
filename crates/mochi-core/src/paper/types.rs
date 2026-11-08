//! Events, errors, dependency types and the small helpers the engine modules share.

use mochi_decimal::Dec;
use mochi_domain::registry::Instruments;
use mochi_domain::{
    DataSource, Fill, LedgerEventType, Liquidity, Order, OrderSide, OrderStatus, Position,
    PositionSide, Protection, Quote, TrailingStopSpec,
};
use serde::{Deserialize, Serialize};

use super::store::{PaperStore, StoreFailure};
use crate::CoreError;

/// No fill may be priced off a quote older than this, whatever the caller's own limits are.
pub(super) const MAX_FILL_QUOTE_AGE_MS: i64 = 60_000;

/// What the engine reports after a change commits. Serialized as `{ kind, ... }` like the TS union.
// Events are built once and handed straight to `on_event`, so boxing the large variants would
// only make them harder to construct and match.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EngineEvent {
    Fill {
        fill: Fill,
        order: Order,
    },
    Liquidation {
        position: Position,
    },
    #[serde(rename_all = "camelCase")]
    Funding {
        account_id: String,
        symbol: String,
        amount: String,
    },
    Order {
        order: Order,
    },
}

/// The protection of a position just before and just after a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProtectionResult {
    pub before: Protection,
    /// What is actually in force now. While `pending`, the old legs are still in force.
    pub after: Protection,
    /// An exchange did not answer a placement; it is resolved by lookup, never resent. A paper
    /// change is applied in one transaction, so here it is always false.
    pub pending: bool,
}

/// A failure the caller sees. `Invalid` is what TypeScript throws as a plain `Error`, with the
/// same message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] StoreFailure),
}

// The callbacks are `Send` so the engine can move between async tasks with its owner.

/// The latest observed quote for a source and symbol, if any.
pub type QuoteLookup = Box<dyn Fn(DataSource, &str) -> Option<Quote> + Send>;
pub type Clock = Box<dyn Fn(DataSource) -> i64 + Send>;
/// Returns a fresh id for a prefix such as `ord` or `fill`.
pub type IdSource = Box<dyn FnMut(&str) -> String + Send>;
pub type EventSink = Box<dyn FnMut(EngineEvent) + Send>;

pub struct PaperEngineDeps<S: PaperStore> {
    pub store: S,
    pub instruments: Instruments,
    pub get_quote: QuoteLookup,
    pub now: Clock,
    pub new_id: IdSource,
    pub on_event: EventSink,
}

/// `Partial<PaperAccountConfig>`: fields left out keep the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountConfigOverrides {
    pub taker_fee_rate: Option<String>,
    pub maker_fee_rate: Option<String>,
    pub slippage_bps: Option<String>,
    pub impact_bps_per_million: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAccountArgs {
    pub name: String,
    pub source: DataSource,
    pub initial_balance: Option<String>,
    pub is_demo: Option<bool>,
    pub config: Option<AccountConfigOverrides>,
}

/// TS `OrderRejection`: the order is refused, and recorded as REJECTED with this reason.
pub(super) struct OrderRejection {
    code: &'static str,
    detail: String,
}

impl OrderRejection {
    pub(super) fn message(&self) -> String {
        format!("{}: {}", self.code, self.detail)
    }
}

/// How an order attempt ends when it does not succeed. TS tells the two apart with
/// `instanceof OrderRejection`.
pub(super) enum Attempt {
    Rejected(OrderRejection),
    Failed(EngineError),
}

impl From<EngineError> for Attempt {
    fn from(e: EngineError) -> Self {
        Attempt::Failed(e)
    }
}

impl From<StoreFailure> for Attempt {
    fn from(e: StoreFailure) -> Self {
        Attempt::Failed(EngineError::Store(e))
    }
}

impl Attempt {
    /// Outside an order attempt nothing catches a rejection, so TS surfaces it as an Error.
    pub(super) fn into_error(self) -> EngineError {
        match self {
            Attempt::Rejected(r) => EngineError::Invalid(r.message()),
            Attempt::Failed(e) => e,
        }
    }
}

pub(super) fn reject(code: &'static str, detail: impl Into<String>) -> Attempt {
    Attempt::Rejected(OrderRejection {
        code,
        detail: detail.into(),
    })
}

/// `d(s)` on a string. decimal.js throws exactly this for text that is not a number.
pub(super) fn dec(s: &str) -> Result<Dec, EngineError> {
    Dec::parse(s).map_err(|_| EngineError::Invalid(format!("[DecimalError] Invalid argument: {s}")))
}

/// A core-math failure TypeScript has no error for: decimal.js would return Infinity or NaN and
/// carry it into the books, so the port refuses instead.
pub(super) fn math_error(what: &str, e: impl Into<CoreError>) -> EngineError {
    EngineError::Invalid(format!("Cannot compute {what}: {}", e.into()))
}

pub(super) fn money(v: &Dec) -> Dec {
    v.to_decimal_places(8)
}

pub(super) fn zero() -> Dec {
    Dec::from_js_number(0.0)
}

/// JS truthiness of an optional string: `null`, `undefined` and `''` are all "not given".
pub(super) fn given(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|s| !s.is_empty())
}

pub(super) fn side_of(side: OrderSide) -> PositionSide {
    match side {
        OrderSide::Buy => PositionSide::Long,
        OrderSide::Sell => PositionSide::Short,
    }
}

pub(super) fn closing_side(side: PositionSide) -> OrderSide {
    match side {
        PositionSide::Long => OrderSide::Sell,
        PositionSide::Short => OrderSide::Buy,
    }
}

/// The spec a TRAILING_STOP_MARKET order was created from. TS asserts the fields with `!`; a row
/// without them is corrupt, so the port refuses it.
pub(super) fn trailing_spec_of(order: &Order) -> Result<TrailingStopSpec, EngineError> {
    match (order.trailing_kind, &order.trailing_callback) {
        (Some(kind), Some(callback)) => Ok(TrailingStopSpec {
            kind,
            callback: callback.clone(),
            activation_price: order.trailing_activation_price.clone(),
        }),
        _ => Err(EngineError::Invalid(format!(
            "Trailing order {} has no trailing spec",
            order.id
        ))),
    }
}

pub(super) fn same_trailing(
    a: &TrailingStopSpec,
    b: &TrailingStopSpec,
) -> Result<bool, EngineError> {
    if a.kind != b.kind || !dec(&a.callback)?.eq_value(&dec(&b.callback)?) {
        return Ok(false);
    }
    Ok(match (&a.activation_price, &b.activation_price) {
        (None, None) => true,
        (Some(x), Some(y)) => dec(x)?.eq_value(&dec(y)?),
        _ => false,
    })
}

pub(super) fn attached_protection(order: &Order) -> Protection {
    Protection {
        stop_loss: order.attached_stop_loss.clone(),
        take_profit: order.attached_take_profit.clone(),
        trailing_stop: order.attached_trailing_stop.clone(),
    }
}

pub(super) fn side_text(side: PositionSide) -> &'static str {
    match side {
        PositionSide::Long => "LONG",
        PositionSide::Short => "SHORT",
    }
}

pub(super) fn status_text(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::Open => "OPEN",
        OrderStatus::Filled => "FILLED",
        OrderStatus::Canceled => "CANCELED",
        OrderStatus::Rejected => "REJECTED",
    }
}

pub(super) fn liquidity_text(liquidity: Liquidity) -> &'static str {
    match liquidity {
        Liquidity::Maker => "MAKER",
        Liquidity::Taker => "TAKER",
        Liquidity::Liquidation => "LIQUIDATION",
    }
}

/// One ledger posting before `post` assigns its id and running wallet. Absent TS fields are
/// zero amounts and `None` references.
pub(super) struct Posting {
    pub(super) event_type: LedgerEventType,
    pub(super) amount: Dec,
    pub(super) margin_delta: Dec,
    pub(super) memo: String,
    pub(super) ts: i64,
    pub(super) symbol: Option<String>,
    pub(super) order_id: Option<String>,
    pub(super) fill_id: Option<String>,
    pub(super) position_id: Option<String>,
}

impl Posting {
    pub(super) fn new(event_type: LedgerEventType, memo: impl Into<String>, ts: i64) -> Self {
        Posting {
            event_type,
            amount: zero(),
            margin_delta: zero(),
            memo: memo.into(),
            ts,
            symbol: None,
            order_id: None,
            fill_id: None,
            position_id: None,
        }
    }
}
