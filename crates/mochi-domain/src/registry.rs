use std::collections::HashMap;

use crate::instruments::{default_instruments, InstrumentSpecification, DEFAULT_SYMBOLS};

/// The instruments known to this process. TypeScript keeps them in a global; here the owner
/// passes the registry explicitly, so tests and accounts cannot leak instruments into each other.
#[derive(Debug, Clone)]
pub struct Instruments {
    by_symbol: HashMap<String, InstrumentSpecification>,
}

impl Instruments {
    pub fn with_defaults() -> Self {
        let by_symbol = default_instruments()
            .into_iter()
            .map(|spec| (spec.symbol.clone(), spec))
            .collect();
        Self { by_symbol }
    }

    pub fn get(&self, symbol: &str) -> Option<&InstrumentSpecification> {
        self.by_symbol.get(symbol)
    }

    /// Adds an instrument read from OKX, or replaces one with the same symbol.
    pub fn register(&mut self, spec: InstrumentSpecification) {
        self.by_symbol.insert(spec.symbol.clone(), spec);
    }

    /// Default symbols are always available (the replay tape only has those), so they stay.
    pub fn unregister(&mut self, symbol: &str) {
        if !DEFAULT_SYMBOLS.contains(&symbol) {
            self.by_symbol.remove(symbol);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_replaces_a_default_spec() {
        let mut instruments = Instruments::with_defaults();
        let mut btc = instruments.get("BTCUSDT").unwrap().clone();
        assert_ne!(btc.max_leverage, 20);
        btc.max_leverage = 20;
        instruments.register(btc.clone());
        assert_eq!(instruments.get("BTCUSDT"), Some(&btc));
        // Replacing a default does not make it removable.
        instruments.unregister("BTCUSDT");
        assert_eq!(instruments.get("BTCUSDT"), Some(&btc));
    }

    #[test]
    fn register_adds_and_unregister_removes_a_new_symbol() {
        let mut instruments = Instruments::with_defaults();
        let mut doge = instruments.get("SOLUSDT").unwrap().clone();
        doge.symbol = "DOGEUSDT".into();
        instruments.register(doge.clone());
        assert_eq!(instruments.get("DOGEUSDT"), Some(&doge));
        instruments.unregister("DOGEUSDT");
        assert_eq!(instruments.get("DOGEUSDT"), None);
    }
}
