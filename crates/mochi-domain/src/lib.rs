//! Domain types shared by every MochiPrep crate. They mirror `packages/shared/src/domain.ts`,
//! and the golden fixtures in `crates/golden` keep the two in step.

mod instruments;
pub mod jsnum;
mod market;

pub use instruments::{
    default_instrument, default_instruments, InstrumentSpecification, DEFAULT_SYMBOLS,
};
pub use market::{Candle, DataSource, Quote, Timeframe};
