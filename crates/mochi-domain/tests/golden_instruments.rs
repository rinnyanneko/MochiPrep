use mochi_domain::{default_instruments, Timeframe, DEFAULT_SYMBOLS};

fn golden(name: &str) -> serde_json::Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/");
    serde_json::from_str(&std::fs::read_to_string(format!("{path}{name}")).unwrap()).unwrap()
}

/// `instruments.json` holds the three pairs the Node backend shipped; later built-in pairs
/// were copied from OKX afterwards and have no fixture.
#[test]
fn default_instruments_match_typescript() {
    let node: Vec<_> = default_instruments()
        .into_iter()
        .filter(|spec| ["BTCUSDT", "ETHUSDT", "SOLUSDT"].contains(&spec.symbol.as_str()))
        .collect();
    assert_eq!(
        serde_json::to_value(node).unwrap(),
        golden("instruments.json")
    );
}

#[test]
fn default_instruments_follow_default_symbols() {
    let symbols: Vec<_> = default_instruments()
        .into_iter()
        .map(|spec| spec.symbol)
        .collect();
    assert_eq!(symbols, DEFAULT_SYMBOLS);
}

#[test]
fn timeframe_ms_and_names() {
    assert_eq!(Timeframe::H4.ms(), 14_400_000);
    assert_eq!(serde_json::to_string(&Timeframe::M15).unwrap(), "\"15m\"");
}
