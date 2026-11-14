//! `classify_protection_change` against `crates/golden/protection-classify.json`, which Node's
//! `classifyProtectionChange` wrote over every `protection.test.ts` case plus edge cases.

use mochi_core::protection_change::{classify_protection_change, ProtectionChangeClass};
use mochi_domain::{PositionSide, Protection};
use serde_json::Value;

fn golden() -> Vec<Value> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../golden/protection-classify.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn classifies_every_case_as_node_does() {
    let cases = golden();
    assert!(cases.len() >= 27, "the golden lost its cases");
    for case in cases {
        let label = case["label"].as_str().unwrap();
        let side: PositionSide = serde_json::from_value(case["side"].clone()).unwrap();
        let before: Protection = serde_json::from_value(case["before"].clone()).unwrap();
        let after: Protection = serde_json::from_value(case["after"].clone()).unwrap();
        let expected: ProtectionChangeClass =
            serde_json::from_value(case["expected"].clone()).unwrap();
        let got = classify_protection_change(side, &before, &after).unwrap();
        assert_eq!(got, expected, "{label} on a {:?}", side);
    }
}

#[test]
fn an_unreadable_price_is_an_error_not_a_verdict() {
    let before = Protection {
        stop_loss: Some("95".into()),
        take_profit: None,
        trailing_stop: None,
    };
    let after = Protection {
        stop_loss: Some("not a price".into()),
        ..before.clone()
    };
    assert!(classify_protection_change(PositionSide::Long, &before, &after).is_err());
}
