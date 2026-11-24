use std::collections::HashMap;

use crate::instruments::{default_instrument, InstrumentSpecification, DEFAULT_SYMBOLS};

/// The instruments known to this process. TypeScript keeps them in a global; here the owner
/// passes the registry explicitly, so tests and accounts cannot leak instruments into each other.
#[derive(Debug, Clone)]
pub struct Instruments {
    by_symbol: HashMap<String, InstrumentSpecification>,
    /// The pairs that cannot be removed, because the replay tape has them.
    builtin: Vec<&'static str>,
}

impl Instruments {
    pub fn with_defaults() -> Self {
        Self::with_builtin(&DEFAULT_SYMBOLS)
    }

    /// Only some of the default pairs built in. Tests that replay recordings from the Node
    /// backend, which shipped three pairs, use this to see the registry Node had.
    pub fn with_builtin(symbols: &[&'static str]) -> Self {
        let by_symbol = symbols
            .iter()
            .map(|&symbol| {
                let spec = default_instrument(symbol).expect("a built-in pair is a default pair");
                (spec.symbol.clone(), spec)
            })
            .collect();
        Self {
            by_symbol,
            builtin: symbols.to_vec(),
        }
    }

    pub fn get(&self, symbol: &str) -> Option<&InstrumentSpecification> {
        self.by_symbol.get(symbol)
    }

    /// Adds an instrument read from OKX, or replaces one with the same symbol.
    pub fn register(&mut self, spec: InstrumentSpecification) {
        self.by_symbol.insert(spec.symbol.clone(), spec);
    }

    /// Built-in pairs are always available (the replay tape only has those), so they stay.
    pub fn unregister(&mut self, symbol: &str) {
        if !self.builtin.contains(&symbol) {
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
        let mut ada = instruments.get("SOLUSDT").unwrap().clone();
        ada.symbol = "ADAUSDT".into();
        instruments.register(ada.clone());
        assert_eq!(instruments.get("ADAUSDT"), Some(&ada));
        instruments.unregister("ADAUSDT");
        assert_eq!(instruments.get("ADAUSDT"), None);
    }
}
