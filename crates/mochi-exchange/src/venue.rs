//! What the trading engine asks of an exchange, in neutral terms.
//!
//! Every row field is an `Option<VenueField>` that keeps the JSON kind of what the exchange sent,
//! because the engine reads the same field in more than one way (Node's `??`, truthiness, a
//! strict "is it a string" check, or a whole-value compare) and each way treats the kinds apart.
//! A field is in one of four states:
//!
//! - `None`: the key was absent.
//! - `Some(VenueField::Null)`: the key was JSON `null`.
//! - `Some(VenueField::Text(t))`: a JSON string, `""` included.
//! - `Some(VenueField::Scalar(t))`: a JSON number or bool, as the text `String(value)` gives
//!   (serde_json's number text, `true`, `false`).
//!
//! Read a field with [`nullish`] (`x ?? fallback`), [`strict`] (`typeof x === 'string'`),
//! [`truthy`] (`x || fallback`), [`js_number`] (`Number(x)`) or [`plain_number`]; compare two
//! fields with `==`, which keeps absent apart from null like comparing two JSON values. Money and
//! quantities stay text; the engine parses them with `Dec` where it needs a number.

use std::future::Future;
use std::pin::Pin;

use mochi_decimal::Dec;
use mochi_domain::jsnum::js_string_to_number;
use mochi_domain::OrderSide;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One present field of an exchange row. See the module doc for the four states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VenueField {
    Null,
    /// A JSON string.
    Text(String),
    /// A JSON number or bool, as its text.
    Scalar(String),
}

/// `x ?? fallback`: absent and null are `None`; a string, number or bool is its text.
pub fn nullish(field: &Option<VenueField>) -> Option<&str> {
    match field {
        Some(VenueField::Text(text) | VenueField::Scalar(text)) => Some(text),
        None | Some(VenueField::Null) => None,
    }
}

/// `x` only when it is a JSON string, like `Value::as_str`. A number or bool never matches.
pub fn strict(field: &Option<VenueField>) -> Option<&str> {
    match field {
        Some(VenueField::Text(text)) => Some(text),
        _ => None,
    }
}

/// `x || fallback`: absent, null, `""`, `0` and `false` are `None`.
pub fn truthy(field: &Option<VenueField>) -> Option<&str> {
    match field {
        Some(VenueField::Text(text)) if !text.is_empty() => Some(text),
        Some(VenueField::Scalar(text)) => {
            let falsy = text == "false" || text.parse::<f64>().is_ok_and(|n| n == 0.0);
            (!falsy).then_some(text.as_str())
        }
        _ => None,
    }
}

/// Node's `Number(x)`: absent is NaN, null is 0, a bool is 0 or 1.
pub fn js_number(field: &Option<VenueField>) -> f64 {
    match field {
        None => f64::NAN,
        Some(VenueField::Null) => 0.0,
        Some(VenueField::Text(text)) => js_string_to_number(text),
        Some(VenueField::Scalar(text)) => match text.as_str() {
            "true" => 1.0,
            "false" => 0.0,
            number => number.parse().unwrap_or(f64::NAN),
        },
    }
}

/// A truthy field that is a plain unsigned decimal (digits, optionally `.` and digits), as fixed
/// text. Anything else is `None`, so exchange text that is not a number never passes on.
pub fn plain_number(field: &Option<VenueField>) -> Option<String> {
    let text = truthy(field)?;
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (text, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || fraction.is_some_and(|f| !digits(f)) {
        return None;
    }
    Dec::parse(text).ok().map(|d| d.to_fixed())
}

/// Why an exchange call failed. The texts are the adapter's own (for OKX, the client's).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VenueError {
    /// The exchange answered and refused. Safe to treat as "did not happen".
    #[error("{display}")]
    Rejection {
        code: String,
        message: String,
        /// The text shown and stored for the refusal, e.g. `OKX 51000: Parameter error`.
        display: String,
        /// What the engine must do differently for this refusal, when anything.
        kind: Option<RejectKind>,
    },
    /// No usable answer. The request may or may not have taken effect, so never blindly retry a
    /// write: resolve it by lookup.
    #[error("{0}")]
    Unknown(String),
    /// A request refused before sending, or an answer that could not be read at all.
    #[error("{0}")]
    Invalid(String),
}

/// Refusals the engine treats apart from the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectKind {
    /// The trigger price is already on the wrong side of the market, so the algo would fire at
    /// once. The engine waits for a fresh quote instead of rejecting the leg.
    TriggerWouldFireNow,
}

/// The pending-algo lists the engine reads: OKX's `conditional`, `oco` and `move_order_stop`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlgoKind {
    Conditional,
    Oco,
    Trailing,
}

/// The algo kinds the engine places. It never places an OCO, so there is none here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VenueAlgoOrderType {
    Conditional,
    Trailing,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VenueMarginMode {
    #[default]
    Isolated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VenueOrderType {
    Market,
    Limit,
}

/// What the connect checks need to know about the key and the account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueAccountConfig {
    pub uid: Option<String>,
    /// The main account's uid when the key belongs to a sub-account.
    pub master_uid: Option<String>,
    pub can_withdraw: bool,
    pub can_trade: bool,
    /// Net (one-way) position mode, the only one MochiPrep trades in.
    pub one_way_mode: bool,
    /// The account mode can trade perpetuals.
    pub account_mode_ok: bool,
}

/// A contract's size and step as the exchange reports them; an absent field is the empty string.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueContract {
    pub contract_value: String,
    pub lot_size: String,
}

/// The USDT balance. OKX: one `details` entry of `account/balance`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueBalance {
    pub equity: Option<VenueField>,
    pub unrealized_pnl: Option<VenueField>,
    pub available_equity: Option<VenueField>,
    pub available_balance: Option<VenueField>,
}

/// One open position. Sizes are contracts, signed in one-way mode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenuePosition {
    pub symbol: Option<VenueField>,
    pub size: Option<VenueField>,
    pub entry_price: Option<VenueField>,
    pub liquidation_price: Option<VenueField>,
    pub mark_price: Option<VenueField>,
    pub margin: Option<VenueField>,
    pub leverage: Option<VenueField>,
    pub unrealized_pnl: Option<VenueField>,
    pub realized_pnl: Option<VenueField>,
    pub position_id: Option<VenueField>,
    pub margin_mode: Option<VenueField>,
    pub position_side: Option<VenueField>,
    /// The last trade that changed the position.
    pub trade_id: Option<VenueField>,
    pub created_ms: Option<VenueField>,
    pub updated_ms: Option<VenueField>,
}

/// One order, as a lookup by client id reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueOrder {
    pub exchange_order_id: Option<VenueField>,
    pub state: Option<VenueField>,
    /// Contracts filled so far.
    pub filled_size: Option<VenueField>,
    pub average_price: Option<VenueField>,
    pub fee: Option<VenueField>,
    pub realized_pnl: Option<VenueField>,
    /// The order's last trade.
    pub trade_id: Option<VenueField>,
    pub fill_ms: Option<VenueField>,
}

/// One algo (conditional, OCO or trailing) order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueAlgo {
    pub symbol: Option<VenueField>,
    pub exchange_algo_id: Option<VenueField>,
    pub client_algo_id: Option<VenueField>,
    pub state: Option<VenueField>,
    pub order_type: Option<VenueField>,
    /// Contracts.
    pub size: Option<VenueField>,
    pub sl_trigger_price: Option<VenueField>,
    pub sl_order_price: Option<VenueField>,
    pub tp_trigger_price: Option<VenueField>,
    pub tp_order_price: Option<VenueField>,
    pub callback_ratio: Option<VenueField>,
    pub callback_spread: Option<VenueField>,
    pub activation_price: Option<VenueField>,
    /// A trailing stop's current trigger.
    pub move_trigger_price: Option<VenueField>,
}

/// One recent execution on an instrument, manual, liquidation and ADL fills included.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueFill {
    pub symbol: Option<VenueField>,
    pub exchange_order_id: Option<VenueField>,
    pub client_order_id: Option<VenueField>,
    pub position_side: Option<VenueField>,
    pub sub_type: Option<VenueField>,
    /// Contracts.
    pub fill_size: Option<VenueField>,
    pub trade_id: Option<VenueField>,
    pub fill_ms: Option<VenueField>,
}

/// One execution from the account's longer history, paged by bill id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueBill {
    pub bill_id: Option<VenueField>,
    pub symbol: Option<VenueField>,
    pub exchange_order_id: Option<VenueField>,
    pub position_side: Option<VenueField>,
    pub side: Option<VenueField>,
    pub trade_id: Option<VenueField>,
    pub fill_ms: Option<VenueField>,
}

/// One closed or partly closed position.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueClosedPosition {
    pub symbol: Option<VenueField>,
    pub position_id: Option<VenueField>,
    pub realized_pnl: Option<VenueField>,
    pub close_type: Option<VenueField>,
    pub margin_mode: Option<VenueField>,
    pub position_side: Option<VenueField>,
    pub direction: Option<VenueField>,
    pub created_ms: Option<VenueField>,
    pub updated_ms: Option<VenueField>,
}

/// One protective leg attached to an entry order. Fields are in the order the engine writes them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueAttachedAlgo {
    pub size: Option<String>,
    pub client_algo_id: Option<String>,
    pub sl_trigger_price: Option<String>,
    pub sl_order_price: Option<String>,
    pub tp_trigger_price: Option<String>,
    pub tp_order_price: Option<String>,
    pub callback_ratio: Option<String>,
    pub callback_spread: Option<String>,
    pub activation_price: Option<String>,
}

/// An order to place. Sizes and prices are decimal strings; the size is contracts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VenuePlaceOrder {
    pub symbol: String,
    pub margin_mode: VenueMarginMode,
    pub side: OrderSide,
    pub order_type: VenueOrderType,
    pub size: String,
    /// Already in the exchange's form ([`VenueAdapter::client_order_id`]).
    pub client_order_id: String,
    pub price: Option<String>,
    pub reduce_only: Option<bool>,
    pub attached_algos: Option<Vec<VenueAttachedAlgo>>,
}

/// A protective algo order to place. One-way mode is required at connect, so there is no
/// position side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VenuePlaceAlgo {
    pub symbol: String,
    pub margin_mode: VenueMarginMode,
    pub side: OrderSide,
    /// Already in the exchange's form ([`VenueAdapter::client_algo_id`]).
    pub client_algo_id: String,
    pub size: String,
    /// Always true: a protective algo only ever closes.
    pub reduce_only: bool,
    pub order_type: VenueAlgoOrderType,
    pub sl_trigger_price: Option<String>,
    pub sl_order_price: Option<String>,
    pub tp_trigger_price: Option<String>,
    pub tp_order_price: Option<String>,
    /// Exactly one of `callback_ratio` / `callback_spread` for a trailing stop.
    pub callback_ratio: Option<String>,
    pub callback_spread: Option<String>,
    pub activation_price: Option<String>,
}

/// One algo order to cancel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VenueCancelAlgo {
    pub symbol: String,
    pub exchange_algo_id: String,
}

/// One exchange account, as the trading engine uses it. Every call is one request (or one
/// lookup) on the exchange; the adapter never retries a write.
pub trait VenueAdapter: Send + Sync {
    /// Shown in texts the engine writes, e.g. "OKX".
    fn name(&self) -> &str;
    fn account_config(&self) -> BoxFuture<'_, Result<VenueAccountConfig, VenueError>>;
    fn contract(&self, symbol: &str) -> BoxFuture<'_, Result<VenueContract, VenueError>>;
    fn balance(&self) -> BoxFuture<'_, Result<Option<VenueBalance>, VenueError>>;
    fn positions(&self) -> BoxFuture<'_, Result<Vec<VenuePosition>, VenueError>>;
    /// Newest update first. `after` asks for those last updated before it (ms).
    fn positions_history(
        &self,
        after: Option<i64>,
    ) -> BoxFuture<'_, Result<Vec<VenueClosedPosition>, VenueError>>;
    /// Paged by bill id: `after` asks for bills older than it.
    fn execution_history(
        &self,
        after: Option<u64>,
    ) -> BoxFuture<'_, Result<Vec<VenueBill>, VenueError>>;
    fn recent_fills(&self, symbol: &str) -> BoxFuture<'_, Result<Vec<VenueFill>, VenueError>>;
    fn set_leverage(&self, symbol: &str, leverage: u32) -> BoxFuture<'_, Result<(), VenueError>>;
    /// The exchange's order id. A lost answer is `Unknown`: look the order up, never resend.
    fn place_order(&self, order: VenuePlaceOrder) -> BoxFuture<'_, Result<String, VenueError>>;
    /// `None` only when the exchange positively says the order does not exist.
    fn get_order(
        &self,
        symbol: &str,
        client_order_id: &str,
    ) -> BoxFuture<'_, Result<Option<VenueOrder>, VenueError>>;
    fn cancel_order(
        &self,
        symbol: &str,
        client_order_id: &str,
    ) -> BoxFuture<'_, Result<(), VenueError>>;
    /// The exchange's algo id. A lost answer is `Unknown`: look the algo up, never resend.
    fn place_algo(&self, algo: VenuePlaceAlgo) -> BoxFuture<'_, Result<String, VenueError>>;
    fn pending_algos(&self, kind: AlgoKind) -> BoxFuture<'_, Result<Vec<VenueAlgo>, VenueError>>;
    /// `None` only on an empty answer; every refusal is an error.
    fn get_algo(
        &self,
        client_algo_id: &str,
    ) -> BoxFuture<'_, Result<Option<VenueAlgo>, VenueError>>;
    fn cancel_algos(&self, items: Vec<VenueCancelAlgo>) -> BoxFuture<'_, Result<(), VenueError>>;
    /// The exchange's client-id form of our order id (OKX: `to_cl_ord_id`).
    fn client_order_id(&self, order_id: &str) -> String;
    fn client_algo_id(&self, order_id: &str) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_display_their_text() {
        let rejection = VenueError::Rejection {
            code: "51000".into(),
            message: "Parameter error".into(),
            display: "OKX 51000: Parameter error".into(),
            kind: None,
        };
        assert_eq!(rejection.to_string(), "OKX 51000: Parameter error");
        assert_eq!(VenueError::Unknown("lost".into()).to_string(), "lost");
        assert_eq!(VenueError::Invalid("bad".into()).to_string(), "bad");
    }

    fn text(t: &str) -> Option<VenueField> {
        Some(VenueField::Text(t.into()))
    }

    fn scalar(t: &str) -> Option<VenueField> {
        Some(VenueField::Scalar(t.into()))
    }

    #[test]
    fn readers_follow_the_js_reads() {
        let null = Some(VenueField::Null);
        assert_eq!(nullish(&None), None);
        assert_eq!(nullish(&null), None);
        assert_eq!(nullish(&text("")), Some(""));
        assert_eq!(nullish(&scalar("7")), Some("7"));

        assert_eq!(strict(&null), None);
        assert_eq!(strict(&text("")), Some(""));
        assert_eq!(strict(&scalar("7")), None);
        assert_eq!(strict(&scalar("true")), None);

        assert_eq!(truthy(&text("")), None);
        assert_eq!(truthy(&text("0")), Some("0"));
        assert_eq!(truthy(&scalar("0")), None);
        assert_eq!(truthy(&scalar("-0.0")), None);
        assert_eq!(truthy(&scalar("false")), None);
        assert_eq!(truthy(&scalar("true")), Some("true"));
        assert_eq!(truthy(&scalar("1.5")), Some("1.5"));
        assert_eq!(truthy(&null), None);

        assert!(js_number(&None).is_nan());
        assert_eq!(js_number(&null), 0.0);
        assert_eq!(js_number(&text("")), 0.0);
        assert_eq!(js_number(&text(" 5 ")), 5.0);
        assert_eq!(js_number(&scalar("true")), 1.0);
        assert_eq!(js_number(&scalar("false")), 0.0);
        assert_eq!(js_number(&scalar("2.5")), 2.5);

        assert_eq!(plain_number(&text("1.50")).as_deref(), Some("1.5"));
        assert_eq!(plain_number(&scalar("3")).as_deref(), Some("3"));
        assert_eq!(plain_number(&text("-1")), None);
        assert_eq!(plain_number(&text("1e3")), None);
        assert_eq!(plain_number(&text("0")), Some("0".into()));
        assert_eq!(plain_number(&scalar("0")), None);
    }

    #[test]
    fn absent_and_null_compare_apart() {
        assert_ne!(None, Some(VenueField::Null));
        assert_ne!(text("7"), scalar("7"));
        assert_eq!(text("7"), text("7"));
    }
}
