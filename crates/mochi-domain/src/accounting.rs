//! Accounting types: orders, fills, positions, ledger events and risk results. They mirror
//! `packages/shared/src/domain.ts` field for field, in the same order, because serde writes
//! fields in declaration order and the JSON must match what the TypeScript backend sends.
//!
//! Decimal values stay `String`, as in TS. Symbols are plain strings because `SymbolId` is
//! open-ended (instruments come from OKX). Optional TS fields (`?`) are skipped when `None`;
//! `X | null` fields serialize as `null`.

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::jsnum::js_number_raw;
use crate::market::DataSource;

/// JSON.stringify writes 3, not 3.0, so f64 fields go through `js_number_raw`.
fn serialize_js_number<S: Serializer>(x: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    js_number_raw(*x).serialize(serializer)
}

fn serialize_js_number_or_null<S: Serializer>(
    x: &Option<f64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match x {
        Some(x) => js_number_raw(*x).serialize(serializer),
        None => serializer.serialize_none(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    Market,
    Limit,
    StopMarket,
    TakeProfitMarket,
    TrailingStopMarket,
}

/// RATIO: the callback is a fraction of the extreme price. SPREAD: it is a price distance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TrailingKind {
    Ratio,
    Spread,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrailingStopSpec {
    pub kind: TrailingKind,
    pub callback: String,
    /// The stop only starts trailing once the last price reaches this; None = active at once.
    pub activation_price: Option<String>,
}

/// What a position is protected by. None means that leg is not set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Protection {
    pub stop_loss: Option<String>,
    pub take_profit: Option<String>,
    pub trailing_stop: Option<TrailingStopSpec>,
}

/// A `TrailingStopSpec` plus its live state. TS uses `extends`, so the JSON is one flat object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrailingStopView {
    #[serde(flatten)]
    pub spec: TrailingStopSpec,
    /// Highest (long) or lowest (short) last price since activation; None until active.
    pub extreme: Option<String>,
    /// Current trigger price; None until active.
    pub trigger_price: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExternalProtectionKind {
    StopLoss,
    TakeProfit,
    Trailing,
}

/// Starts the detail of an external entry that is really MochiPrep's own algo whose cancel has
/// not gone through yet, so the model and the UI can tell the two apart.
pub const CANCEL_PENDING_PREFIX: &str = "ours, cancel pending:";

/// A protective order that lives on the exchange but was not placed through MochiPrep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalProtection {
    pub kind: ExternalProtectionKind,
    pub trigger_price: Option<String>,
    pub detail: String,
}

/// Bounds shared by the risk engine and engine validation, so both refuse the same callbacks.
pub struct TrailingLimits {
    pub min_ratio: &'static str,
    pub max_ratio: &'static str,
    pub max_spread_fraction: &'static str,
}

pub const TRAILING_LIMITS: TrailingLimits = TrailingLimits {
    min_ratio: "0.001",
    max_ratio: "0.3",
    max_spread_fraction: "0.3",
};

/// The two types a caller may request (`OrderRequest.type`, `AiDecisionData.orderType`);
/// stops and take-profits are only ever created by the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum EntryOrderType {
    Market,
    Limit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderStatus {
    Open,
    Filled,
    Canceled,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PositionSide {
    Long,
    Short,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PositionStatus {
    Open,
    Closed,
    Liquidated,
}

/// TS declares this inline on `Fill`. It has a third value, LIQUIDATION, which the paper
/// engine writes on forced closes, so leaving it out would fail to read those fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Liquidity {
    Maker,
    Taker,
    Liquidation,
}

/// The exchange an account trades on, by its lowercase id (`okx`). Open-ended, so an account
/// row can name an exchange this build has no support for; the server refuses to trade it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExchangeId(Arc<str>);

impl ExchangeId {
    /// Lowercased, so `OKX` and `okx` are the same exchange. Only nonempty ASCII letters and
    /// digits are accepted, so every id round-trips through `AccountVenue` text.
    pub fn new(id: &str) -> Option<Self> {
        let valid = !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric());
        valid.then(|| ExchangeId(Arc::from(id.to_ascii_lowercase())))
    }

    pub fn okx() -> Self {
        ExchangeId(Arc::from("okx"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Demo trades the exchange's virtual funds; Live is real money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Environment {
    Demo,
    Live,
}

/// PAPER is simulated locally; an exchange account is `<EXCHANGE>_DEMO` (virtual funds) or
/// `<EXCHANGE>_LIVE` (real money), so the stored and sent strings stay `OKX_DEMO`/`OKX_LIVE`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AccountVenue {
    Paper,
    Exchange {
        exchange: ExchangeId,
        environment: Environment,
    },
}

impl AccountVenue {
    pub fn okx_demo() -> Self {
        AccountVenue::Exchange {
            exchange: ExchangeId::okx(),
            environment: Environment::Demo,
        }
    }

    pub fn okx_live() -> Self {
        AccountVenue::Exchange {
            exchange: ExchangeId::okx(),
            environment: Environment::Live,
        }
    }

    /// Real money, on any exchange.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            AccountVenue::Exchange {
                environment: Environment::Live,
                ..
            }
        )
    }

    /// `PAPER`, or `<EXCHANGE upper>_<DEMO|LIVE>`.
    pub fn as_text(&self) -> String {
        match self {
            AccountVenue::Paper => "PAPER".to_string(),
            AccountVenue::Exchange {
                exchange,
                environment,
            } => {
                let environment = match environment {
                    Environment::Demo => "DEMO",
                    Environment::Live => "LIVE",
                };
                format!("{}_{environment}", exchange.as_str().to_ascii_uppercase())
            }
        }
    }

    /// `PAPER`, or a string matching `^[A-Z0-9]+_(DEMO|LIVE)$`; anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        if text == "PAPER" {
            return Some(AccountVenue::Paper);
        }
        let (exchange, environment) = text.split_once('_')?;
        let environment = match environment {
            "DEMO" => Environment::Demo,
            "LIVE" => Environment::Live,
            _ => return None,
        };
        let valid = !exchange.is_empty()
            && exchange
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
        valid.then(|| AccountVenue::Exchange {
            exchange: ExchangeId(Arc::from(exchange.to_ascii_lowercase())),
            environment,
        })
    }
}

impl Serialize for AccountVenue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_text())
    }
}

impl<'de> Deserialize<'de> for AccountVenue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        AccountVenue::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown account venue `{text}`, expected `PAPER` or `<EXCHANGE>_DEMO` or `<EXCHANGE>_LIVE`"
            ))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LedgerEventType {
    Deposit,
    TradeFee,
    RealizedPnl,
    Funding,
    LiquidationPenalty,
    MarginLock,
    MarginRelease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AiAction {
    Long,
    Short,
    Close,
    Hold,
    /// Sets the full protection of the open position: a value sets a leg, null removes it.
    Protect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderRequest {
    pub account_id: String,
    /// Idempotency key, unique per account. Replays return the original order.
    pub client_order_id: String,
    pub symbol: String,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: EntryOrderType,
    pub quantity: String,
    // TS allows these to be absent or null; both read as None, and None is left out when
    // writing, like JSON.stringify leaves out undefined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    pub leverage: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_loss: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take_profit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing_stop: Option<TrailingStopSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub id: String,
    pub account_id: String,
    pub client_order_id: String,
    pub symbol: String,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: OrderType,
    pub quantity: String,
    /// Limit price for LIMIT orders.
    pub price: Option<String>,
    /// Trigger for STOP_MARKET / TAKE_PROFIT_MARKET.
    pub trigger_price: Option<String>,
    pub leverage: u32,
    pub reduce_only: bool,
    /// Marks a protection order. Paper protection covers the remaining position, while
    /// OKX protection has an explicit quantity.
    pub close_position: bool,
    pub status: OrderStatus,
    pub filled_quantity: String,
    pub avg_fill_price: Option<String>,
    /// Margin and fee held back while a resting opening order waits.
    pub reserved_margin: String,
    pub attached_stop_loss: Option<String>,
    pub attached_take_profit: Option<String>,
    /// Set on a TRAILING_STOP_MARKET order.
    pub trailing_kind: Option<TrailingKind>,
    pub trailing_callback: Option<String>,
    pub trailing_activation_price: Option<String>,
    /// Best last price seen since activation. Stored, so a restart does not reset the trail.
    pub trailing_extreme: Option<String>,
    /// Trailing stop to arm when this entry order fills.
    pub attached_trailing_stop: Option<TrailingStopSpec>,
    pub reject_reason: Option<String>,
    /// OKX ordId once the exchange has acknowledged the order.
    pub exchange_order_id: Option<String>,
    pub agent_id: Option<String>,
    pub decision_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fill {
    pub id: String,
    pub order_id: String,
    pub account_id: String,
    pub symbol: String,
    pub side: OrderSide,
    pub quantity: String,
    pub price: String,
    pub fee: String,
    pub realized_pnl: String,
    pub liquidity: Liquidity,
    pub ts: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderResult {
    pub order: Order,
    pub fills: Vec<Fill>,
    /// True when the idempotency key matched an earlier order and nothing was executed.
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub id: String,
    pub account_id: String,
    pub symbol: String,
    pub side: PositionSide,
    pub quantity: String,
    pub entry_price: String,
    pub leverage: u32,
    /// Isolated margin currently committed.
    pub margin: String,
    pub liquidation_price: String,
    pub status: PositionStatus,
    pub realized_pnl: String,
    pub fees_paid: String,
    /// Positive = paid, negative = received.
    pub funding_paid: String,
    pub max_quantity: String,
    pub exit_price: Option<String>,
    pub last_funding_time: i64,
    pub opened_at: i64,
    pub closed_at: Option<i64>,
}

/// A `Position` plus live fields. TS uses `extends`, so the JSON is one flat object and the
/// position fields are flattened into it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionView {
    #[serde(flatten)]
    pub position: Position,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_supported: Option<bool>,
    pub mark_price: Option<String>,
    pub unrealized_pnl: String,
    pub stop_loss: Option<String>,
    pub take_profit: Option<String>,
    pub trailing_stop: Option<TrailingStopView>,
    pub external_protection: Vec<ExternalProtection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaperAccountConfig {
    pub taker_fee_rate: String,
    pub maker_fee_rate: String,
    /// Fixed slippage on market fills, basis points.
    pub slippage_bps: String,
    /// Extra slippage, basis points per 1,000,000 USDT of notional.
    pub impact_bps_per_million: String,
}

pub fn default_account_config() -> PaperAccountConfig {
    PaperAccountConfig {
        taker_fee_rate: "0.0005".to_string(),
        maker_fee_rate: "0.0002".to_string(),
        slippage_bps: "1".to_string(),
        impact_bps_per_million: "5".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskSettings {
    #[serde(serialize_with = "serialize_js_number")]
    pub max_leverage: f64,
    #[serde(serialize_with = "serialize_js_number")]
    pub max_risk_per_trade_pct: f64,
    #[serde(serialize_with = "serialize_js_number")]
    pub max_daily_loss_pct: f64,
    #[serde(serialize_with = "serialize_js_number")]
    pub max_open_positions: f64,
    pub require_stop_loss: bool,
    /// Market data older than this blocks new positions.
    #[serde(serialize_with = "serialize_js_number")]
    pub max_quote_age_sec: f64,
    #[serde(serialize_with = "serialize_js_number")]
    pub max_notional: f64,
    /// New positions need the model to state at least this confidence, 0 to 1. Zero is off, and
    /// is left out when written, so settings saved before it existed read and write unchanged.
    #[serde(
        default,
        serialize_with = "serialize_js_number",
        skip_serializing_if = "is_zero"
    )]
    pub min_confidence: f64,
    /// New positions exit by a trailing stop with an activation price instead of a take profit:
    /// an entry must carry one and no take profit, and PROTECT cannot set a new take profit. It
    /// turns trailing stops on for the agent. Off is left out when written, like `min_confidence`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub trail_instead_of_take_profit: bool,
}

fn is_zero(value: &f64) -> bool {
    *value == 0.0
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub fn default_risk_settings() -> RiskSettings {
    RiskSettings {
        max_leverage: 3.0,
        max_risk_per_trade_pct: 0.5,
        max_daily_loss_pct: 2.0,
        max_open_positions: 2.0,
        require_stop_loss: true,
        max_quote_age_sec: 30.0,
        max_notional: 100_000.0,
        min_confidence: 0.0,
        trail_instead_of_take_profit: false,
    }
}

/// Starting ceilings for exchange accounts: deliberately far below the paper defaults.
pub fn default_live_limits() -> RiskSettings {
    RiskSettings {
        max_quote_age_sec: 10.0,
        max_notional: 100.0,
        ..default_risk_settings()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaperAccount {
    pub id: String,
    pub name: String,
    pub source: DataSource,
    pub venue: AccountVenue,
    pub exchange_uid: Option<String>,
    /// Hard ceilings on every order for an exchange account, whoever places it.
    pub live_limits: Option<RiskSettings>,
    pub is_demo: bool,
    pub initial_balance: String,
    pub wallet_balance: String,
    pub used_margin: String,
    pub realized_pnl: String,
    pub fees_paid: String,
    /// Positive = paid, negative = received.
    pub funding_paid: String,
    pub config: PaperAccountConfig,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountBalance {
    pub account_id: String,
    pub initial_balance: String,
    pub wallet_balance: String,
    pub available_balance: String,
    pub used_margin: String,
    pub order_margin: String,
    pub equity: String,
    pub realized_pnl: String,
    pub unrealized_pnl: String,
    pub fees_paid: String,
    pub funding_paid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEvent {
    pub seq: i64,
    pub id: String,
    pub account_id: String,
    #[serde(rename = "type")]
    pub event_type: LedgerEventType,
    /// Signed change to wallet balance.
    pub amount: String,
    /// Signed change to used margin.
    pub margin_delta: String,
    pub wallet_after: String,
    pub symbol: Option<String>,
    pub order_id: Option<String>,
    pub fill_id: Option<String>,
    pub position_id: Option<String>,
    pub memo: String,
    pub ts: i64,
}

/// A resumable scan of execution bills, newest to oldest. The completed head advances only
/// once a scan reaches the previously completed bills or the end of the exchange's history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExchangeExecutionCursor {
    pub completed_bill: Option<String>,
    pub scan_head: Option<String>,
    pub scan_after: Option<String>,
}

/// One closed (or partly closed) position on an exchange account, as the exchange reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangePositionRecord {
    pub account_id: String,
    pub pos_id: String,
    pub opened_at: i64,
    /// The exchange's own instrument id (OKX `instId`).
    pub inst_id: String,
    /// When the exchange last updated it: the close, or the latest partial close.
    pub closed_at: i64,
    /// PnL net of fees, funding and any liquidation penalty.
    pub realized_pnl: String,
    /// The exchange's own code for how it closed (OKX `type`), kept for diagnosis.
    pub close_type: String,
    /// BUY or SELL for a confirmed isolated net position.
    pub opening_side: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EquityPoint {
    pub account_id: String,
    pub ts: i64,
    pub equity: String,
    pub wallet: String,
    pub unrealized: String,
}

/// Prices are plain numbers here because that is what the model returns; the risk engine
/// converts them to decimals before any accounting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiDecisionData {
    pub symbol: String,
    pub action: AiAction,
    pub order_type: EntryOrderType,
    #[serde(serialize_with = "serialize_js_number_or_null")]
    pub entry_price: Option<f64>,
    #[serde(serialize_with = "serialize_js_number_or_null")]
    pub stop_loss: Option<f64>,
    #[serde(serialize_with = "serialize_js_number_or_null")]
    pub take_profit: Option<f64>,
    pub reason: String,
    #[serde(serialize_with = "serialize_js_number")]
    pub confidence: f64,
    pub expires_at: Option<String>,
    /// The outer None is an absent key (keep the current trailing stop, or none); `Some(None)` is
    /// an explicit null (remove it). The two mean different things, so both must round-trip.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub trailing_stop: Option<Option<AiTrailingStop>>,
}

/// A trailing stop as the model writes it: numbers, converted to decimal strings before use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiTrailingStop {
    pub kind: TrailingKind,
    #[serde(serialize_with = "serialize_js_number")]
    pub callback: f64,
    #[serde(serialize_with = "serialize_js_number_or_null")]
    pub activation_price: Option<f64>,
}

/// Called only when the key is present, so a JSON null becomes `Some(None)`; `default` covers
/// the absent key.
fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskCheck {
    pub id: String,
    pub label: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SizedTrade {
    pub quantity: String,
    pub leverage: u32,
    pub notional: String,
    pub margin: String,
    pub risk_amount: String,
    pub reference_price: String,
    pub estimated_liquidation_price: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskResult {
    pub approved: bool,
    pub checks: Vec<RiskCheck>,
    pub rejection_reasons: Vec<String>,
    pub sized: Option<SizedTrade>,
}
