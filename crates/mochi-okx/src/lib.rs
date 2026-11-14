//! OKX private trading for MochiPrep: the signed REST client, byte-compatible with
//! `packages/exchanges/src/okx-client.ts`, and (behind the `fake` feature) an in-memory OKX for
//! tests. Real money: see docs/OKX_TRADING.md.

pub mod client;
#[cfg(feature = "fake")]
pub mod fake;
pub mod ids;

pub use client::*;
pub use ids::{iso_string, sign, to_algo_cl_ord_id, to_cl_ord_id, to_inst_id};
