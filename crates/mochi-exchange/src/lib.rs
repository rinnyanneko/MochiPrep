//! The exchange-neutral side of live trading: the rows and requests every exchange adapter speaks,
//! the [`venue::VenueAdapter`] trait, and the trading engine ([`engine::ExchangeEngine`]) that
//! calls it. OKX is the only adapter (`mochi_okx::OkxAdapter`).

pub mod engine;
pub mod venue;
