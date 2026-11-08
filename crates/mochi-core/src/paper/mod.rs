//! The paper engine and what it persists through.

mod engine;
mod market;
mod store;
mod trailing;
mod views;

pub use engine::{
    AccountConfigOverrides, Clock, CreateAccountArgs, EngineError, EngineEvent, EventSink,
    IdSource, PaperEngineDeps, PaperExecutionEngine, ProtectionResult, QuoteLookup,
};
pub use store::{PaperStore, StoreFailure};
pub use trailing::{advance_extreme, trailing_trigger, validate_trailing};
pub use views::DailyRisk;
