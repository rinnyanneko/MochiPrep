//! What the trading engine asks of an exchange, in neutral terms.
//!
//! Every row field holds the exchange's raw text, so the engine can keep reading it exactly as it
//! read the exchange's JSON: `None` means the field was absent or JSON `null`, and `Some("")`
//! stays `Some("")` (Node's `??` and truthiness tell the two apart). Money and quantities stay
//! text; the engine parses them with `Dec` where it needs a number.

use std::future::Future;
use std::pin::Pin;

use mochi_domain::OrderSide;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

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
    pub equity: Option<String>,
    pub unrealized_pnl: Option<String>,
    pub available_equity: Option<String>,
    pub available_balance: Option<String>,
}

/// One open position. Sizes are contracts, signed in one-way mode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenuePosition {
    /// Empty when the exchange named no instrument.
    pub symbol: String,
    pub size: Option<String>,
    pub entry_price: Option<String>,
    pub liquidation_price: Option<String>,
    pub mark_price: Option<String>,
    pub margin: Option<String>,
    pub leverage: Option<String>,
    pub unrealized_pnl: Option<String>,
    pub realized_pnl: Option<String>,
    pub position_id: Option<String>,
    pub margin_mode: Option<String>,
    pub position_side: Option<String>,
    /// The last trade that changed the position.
    pub trade_id: Option<String>,
    pub created_ms: Option<String>,
    pub updated_ms: Option<String>,
}

/// One order, as a lookup by client id reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueOrder {
    pub exchange_order_id: Option<String>,
    pub state: Option<String>,
    /// Contracts filled so far.
    pub filled_size: Option<String>,
    pub average_price: Option<String>,
    pub fee: Option<String>,
    pub realized_pnl: Option<String>,
    /// The order's last trade.
    pub trade_id: Option<String>,
    pub fill_ms: Option<String>,
}

/// One algo (conditional, OCO or trailing) order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueAlgo {
    pub symbol: Option<String>,
    pub exchange_algo_id: Option<String>,
    pub client_algo_id: Option<String>,
    pub state: Option<String>,
    pub order_type: Option<String>,
    /// Contracts.
    pub size: Option<String>,
    pub sl_trigger_price: Option<String>,
    pub sl_order_price: Option<String>,
    pub tp_trigger_price: Option<String>,
    pub tp_order_price: Option<String>,
    pub callback_ratio: Option<String>,
    pub callback_spread: Option<String>,
    pub activation_price: Option<String>,
    /// A trailing stop's current trigger.
    pub move_trigger_price: Option<String>,
}

/// One recent execution on an instrument, manual, liquidation and ADL fills included.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueFill {
    pub symbol: Option<String>,
    pub exchange_order_id: Option<String>,
    pub client_order_id: Option<String>,
    pub position_side: Option<String>,
    pub sub_type: Option<String>,
    /// Contracts.
    pub fill_size: Option<String>,
    pub trade_id: Option<String>,
    pub fill_ms: Option<String>,
}

/// One execution from the account's longer history, paged by bill id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueBill {
    pub bill_id: Option<String>,
    pub symbol: Option<String>,
    pub exchange_order_id: Option<String>,
    pub position_side: Option<String>,
    pub side: Option<String>,
    pub trade_id: Option<String>,
    pub fill_ms: Option<String>,
}

/// One closed or partly closed position.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VenueClosedPosition {
    pub symbol: Option<String>,
    pub position_id: Option<String>,
    pub realized_pnl: Option<String>,
    pub close_type: Option<String>,
    pub margin_mode: Option<String>,
    pub position_side: Option<String>,
    pub direction: Option<String>,
    pub created_ms: Option<String>,
    pub updated_ms: Option<String>,
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
}
