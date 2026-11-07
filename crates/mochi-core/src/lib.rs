//! Pure trading math: perpetual formulas, the fill model, the ledger fold and audit, and
//! performance stats. Everything here is a function of its inputs, so the paper and risk engines
//! can call it without touching a store or a clock.

mod error;
pub mod fill_model;
pub mod ledger;
pub mod performance;
pub mod perp;

pub use error::CoreError;
