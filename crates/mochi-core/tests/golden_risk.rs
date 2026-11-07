use mochi_core::risk::{evaluate_risk, RiskInput};
use mochi_domain::registry::Instruments;
use serde_json::Value;

fn golden() -> Vec<Value> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/risk.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn verdicts_match_typescript() {
    let instruments = Instruments::with_defaults();
    let cases = golden();
    assert!(cases.len() >= 100, "the fixture should be broad");
    for (index, case) in cases.iter().enumerate() {
        let input: RiskInput = serde_json::from_value(case["input"].clone()).unwrap();
        let got = serde_json::to_value(evaluate_risk(&input, &instruments)).unwrap();
        // Comparing whole values checks every detail string and the order of `checks`.
        assert_eq!(got, case["output"], "case {index}: {}", case["input"]);
    }
}
