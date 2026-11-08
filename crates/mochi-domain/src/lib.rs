//! Domain types shared by every MochiPrep crate. They mirror `packages/shared/src/domain.ts`,
//! and the golden fixtures in `crates/golden` keep the two in step.

mod accounting;
mod instruments;
pub mod jsnum;
mod market;
pub mod registry;

pub use accounting::{
    default_account_config, default_live_limits, default_risk_settings, AccountBalance,
    AccountVenue, AiAction, AiDecisionData, AiTrailingStop, EntryOrderType, EquityPoint,
    ExternalProtection, ExternalProtectionKind, Fill, LedgerEvent, LedgerEventType, Liquidity,
    Order, OrderRequest, OrderResult, OrderSide, OrderStatus, OrderType, PaperAccount,
    PaperAccountConfig, Position, PositionSide, PositionStatus, PositionView, Protection,
    RiskCheck, RiskResult, RiskSettings, SizedTrade, TrailingKind, TrailingLimits,
    TrailingStopSpec, TrailingStopView, TRAILING_LIMITS,
};
pub use instruments::{
    default_instrument, default_instruments, InstrumentSpecification, DEFAULT_SYMBOLS,
};
pub use market::{Candle, DataSource, Quote, Timeframe};
