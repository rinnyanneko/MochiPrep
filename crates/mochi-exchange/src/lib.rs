//! The exchange-neutral side of live trading: the rows and requests every exchange adapter speaks,
//! and the [`venue::VenueAdapter`] trait the trading engine calls. OKX is the only adapter
//! (`mochi_okx::OkxAdapter`).

pub mod venue;
