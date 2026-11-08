//! The paper engine and what it persists through.

mod engine;
mod fills;
mod market;
mod protection;
mod store;
mod trailing;
mod types;
mod views;

pub use engine::PaperExecutionEngine;
pub use store::{PaperStore, StoreFailure, StoreFailureKind};
pub use trailing::{advance_extreme, trailing_trigger, validate_trailing};
pub use types::{
    AccountConfigOverrides, Clock, CreateAccountArgs, EngineError, EngineEvent, EventSink,
    IdSource, PaperEngineDeps, ProtectionResult, QuoteLookup,
};
pub use views::DailyRisk;
