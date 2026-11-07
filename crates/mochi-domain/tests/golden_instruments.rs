use mochi_domain::{default_instruments, Timeframe};

fn golden(name: &str) -> serde_json::Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/");
    serde_json::from_str(&std::fs::read_to_string(format!("{path}{name}")).unwrap()).unwrap()
}

#[test]
fn default_instruments_match_typescript() {
    assert_eq!(
        serde_json::to_value(default_instruments()).unwrap(),
        golden("instruments.json")
    );
}

#[test]
fn timeframe_ms_and_names() {
    assert_eq!(Timeframe::H4.ms(), 14_400_000);
    assert_eq!(serde_json::to_string(&Timeframe::M15).unwrap(), "\"15m\"");
}
