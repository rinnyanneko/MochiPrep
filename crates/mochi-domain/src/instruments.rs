use serde::{Deserialize, Serialize};

/// Always available, and the only pairs on the replay tape.
pub const DEFAULT_SYMBOLS: [&str; 3] = ["BTCUSDT", "ETHUSDT", "SOLUSDT"];

/// Decimal fields stay strings, as in the TypeScript contract, so no precision is lost in transit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstrumentSpecification {
    pub symbol: String,
    pub market_type: String,
    pub base_currency: String,
    pub quote_currency: String,
    pub settlement_currency: String,
    /// Base units per contract. Simulated quantities are in base units, so this is 1 here.
    pub contract_size: String,
    pub quantity_step: String,
    pub min_quantity: String,
    pub price_tick: String,
    pub max_leverage: u32,
    pub maintenance_margin_rate: String,
    pub funding_interval_hours: u32,
    /// Base units per OKX contract. Orders are sent in contracts, so this must match OKX exactly.
    pub venue_contract_value: String,
    /// Smallest contract increment on OKX.
    pub venue_lot_size: String,
}

fn perp(
    base: &str,
    quantity_step: &str,
    price_tick: &str,
    venue_contract_value: &str,
) -> InstrumentSpecification {
    InstrumentSpecification {
        symbol: format!("{base}USDT"),
        market_type: "CRYPTO_PERP".to_string(),
        base_currency: base.to_string(),
        quote_currency: "USDT".to_string(),
        settlement_currency: "USDT".to_string(),
        contract_size: "1".to_string(),
        quantity_step: quantity_step.to_string(),
        min_quantity: quantity_step.to_string(),
        price_tick: price_tick.to_string(),
        max_leverage: 100,
        maintenance_margin_rate: "0.005".to_string(),
        funding_interval_hours: 8,
        venue_contract_value: venue_contract_value.to_string(),
        venue_lot_size: "0.01".to_string(),
    }
}

/// Steps mirror OKX USDT swaps (ctVal x lotSz): 0.01 x 0.01 BTC, 0.1 x 0.01 ETH, 1 x 0.01 SOL.
/// The order matches `DEFAULT_SYMBOLS`.
pub fn default_instruments() -> Vec<InstrumentSpecification> {
    vec![
        perp("BTC", "0.0001", "0.1", "0.01"),
        perp("ETH", "0.001", "0.01", "0.1"),
        perp("SOL", "0.01", "0.01", "1"),
    ]
}

pub fn default_instrument(symbol: &str) -> Option<InstrumentSpecification> {
    default_instruments()
        .into_iter()
        .find(|spec| spec.symbol == symbol)
}
