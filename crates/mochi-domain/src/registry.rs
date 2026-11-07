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
