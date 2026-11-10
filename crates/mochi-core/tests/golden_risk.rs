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

#[test]
fn nonfinite_inputs_and_timestamp_overflow_fail_closed_even_for_hold() {
    let cases = golden();
    let base: RiskInput = serde_json::from_value(cases[0]["input"].clone()).unwrap();
    let instruments = Instruments::with_defaults();
    for field in 0..12 {
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut input = base.clone();
            input.decision.action = mochi_domain::AiAction::Hold;
            match field {
                0 => input.settings.max_leverage = invalid,
                1 => input.settings.max_risk_per_trade_pct = invalid,
                2 => input.settings.max_daily_loss_pct = invalid,
                3 => input.settings.max_open_positions = invalid,
                4 => input.settings.max_quote_age_sec = invalid,
                5 => input.settings.max_notional = invalid,
                6 => input.decision.confidence = invalid,
                7 => input.decision.entry_price = Some(invalid),
                8 => input.decision.stop_loss = Some(invalid),
                9 => input.decision.take_profit = Some(invalid),
                10 => {
                    input.decision.trailing_stop = Some(Some(mochi_domain::AiTrailingStop {
                        kind: mochi_domain::TrailingKind::Ratio,
                        callback: invalid,
                        activation_price: None,
                    }))
                }
                _ => {
                    input.decision.trailing_stop = Some(Some(mochi_domain::AiTrailingStop {
                        kind: mochi_domain::TrailingKind::Ratio,
                        callback: 0.01,
                        activation_price: Some(invalid),
                    }))
                }
            }
            let result = evaluate_risk(&input, &instruments);
            assert!(!result.approved, "field {field}");
            assert!(result.checks.iter().any(|c| c.id == "input" && !c.passed));
        }
    }
    let mut input = base;
    input.now = i64::MAX;
    input.quote.as_mut().unwrap().ts = i64::MIN;
    let result = evaluate_risk(&input, &instruments);
    assert!(!result.approved);
    assert_eq!(result.checks.last().unwrap().id, "input");
}
