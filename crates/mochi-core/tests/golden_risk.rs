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

#[test]
fn a_confidence_floor_refuses_only_new_positions_below_it() {
    let instruments = Instruments::with_defaults();
    let cases = golden();
    let approved_entry = cases
        .iter()
        .map(|case| serde_json::from_value::<RiskInput>(case["input"].clone()).unwrap())
        .find(|input| {
            matches!(
                input.decision.action,
                mochi_domain::AiAction::Long | mochi_domain::AiAction::Short
            ) && evaluate_risk(input, &instruments).approved
        })
        .expect("the fixture has an approved entry");
    let confidence = approved_entry.decision.confidence;

    let mut input = approved_entry.clone();
    input.settings.min_confidence = confidence;
    let result = evaluate_risk(&input, &instruments);
    assert!(result.approved, "a confidence at the floor passes");
    assert!(result
        .checks
        .iter()
        .any(|c| c.id == "confidence" && c.passed));

    input.decision.confidence = confidence - 0.01;
    let result = evaluate_risk(&input, &instruments);
    assert!(!result.approved);
    assert!(result
        .rejection_reasons
        .iter()
        .any(|r| r.starts_with("Confidence threshold: model says")));

    // Exits never wait on the model being sure.
    let close = cases
        .iter()
        .map(|case| serde_json::from_value::<RiskInput>(case["input"].clone()).unwrap())
        .find(|input| {
            input.decision.action == mochi_domain::AiAction::Close
                && evaluate_risk(input, &instruments).approved
        })
        .expect("the fixture has an approved close");
    let mut input = close;
    input.settings.min_confidence = 1.0;
    input.decision.confidence = 0.0;
    let result = evaluate_risk(&input, &instruments);
    assert!(result.approved);
    assert!(result.checks.iter().all(|c| c.id != "confidence"));
}

#[test]
fn settings_without_a_floor_serialize_as_before() {
    let settings = mochi_domain::default_risk_settings();
    let text = serde_json::to_string(&settings).unwrap();
    assert!(!text.contains("minConfidence"), "{text}");
    let floor = mochi_domain::RiskSettings {
        min_confidence: 0.65,
        ..settings
    };
    let value = serde_json::to_value(&floor).unwrap();
    assert_eq!(value["minConfidence"], serde_json::json!(0.65));
}

fn approved(instruments: &Instruments, pick: impl Fn(&RiskInput) -> bool) -> RiskInput {
    golden()
        .iter()
        .map(|case| serde_json::from_value::<RiskInput>(case["input"].clone()).unwrap())
        .find(|input| pick(input) && evaluate_risk(input, instruments).approved)
        .expect("the fixture has such a case")
}

#[test]
fn trailing_instead_of_take_profit_needs_an_activated_trail_and_no_target() {
    let instruments = Instruments::with_defaults();
    let entry = approved(&instruments, |input| {
        input.decision.action == mochi_domain::AiAction::Long
            && input.decision.order_type == mochi_domain::EntryOrderType::Market
    });
    let ask: f64 = entry.quote.as_ref().unwrap().ask.parse().unwrap();
    let mut input = entry.clone();
    input.settings.trail_instead_of_take_profit = true;
    // The setting turns trailing stops on, whatever the agent's own switch says.
    input.allow_trailing_stop = false;
    let reasons = |input: &RiskInput| evaluate_risk(input, &instruments).rejection_reasons;

    input.decision.take_profit = Some(ask * 1.05);
    input.decision.trailing_stop = None;
    assert!(reasons(&input)
        .iter()
        .any(|r| r == "Trailing exit: this agent exits by trailing stop: no take profit"));

    input.decision.take_profit = None;
    assert!(reasons(&input)
        .iter()
        .any(|r| r.contains("needs a trailing stop with an activation price")));

    let trail = |activation: Option<f64>| {
        Some(Some(mochi_domain::AiTrailingStop {
            kind: mochi_domain::TrailingKind::Ratio,
            callback: 0.01,
            activation_price: activation,
        }))
    };
    input.decision.trailing_stop = trail(None);
    assert!(!evaluate_risk(&input, &instruments).approved);

    input.decision.trailing_stop = trail(Some(ask * 1.01));
    let result = evaluate_risk(&input, &instruments);
    assert!(result.approved, "{:?}", result.rejection_reasons);
    assert!(result
        .checks
        .iter()
        .any(|c| c.id == "exit-style" && c.passed));

    // An activation price behind the entry is refused by the trailing rule itself.
    input.decision.trailing_stop = trail(Some(ask * 0.99));
    assert!(!evaluate_risk(&input, &instruments).approved);

    // Exits are never held back by it.
    let mut close = approved(&instruments, |input| {
        input.decision.action == mochi_domain::AiAction::Close
    });
    close.settings.trail_instead_of_take_profit = true;
    assert!(evaluate_risk(&close, &instruments).approved);
}

#[test]
fn trailing_instead_of_take_profit_lets_protect_keep_but_not_set_a_target() {
    let instruments = Instruments::with_defaults();
    let protect = approved(&instruments, |input| {
        input.decision.action == mochi_domain::AiAction::Protect
            && input.decision.take_profit.is_some()
            && input.protection.is_some()
    });
    let target = protect.decision.take_profit.unwrap();
    let mut input = protect.clone();
    input.settings.trail_instead_of_take_profit = true;
    input.protection.as_mut().unwrap().take_profit = Some(target.to_string());
    let result = evaluate_risk(&input, &instruments);
    assert!(result.approved, "{:?}", result.rejection_reasons);

    input.protection.as_mut().unwrap().take_profit = None;
    assert!(evaluate_risk(&input, &instruments)
        .rejection_reasons
        .iter()
        .any(|r| r.contains("no new take profit")));

    input.decision.take_profit = None;
    assert!(evaluate_risk(&input, &instruments).approved);
}

#[test]
fn settings_without_trailing_exits_serialize_as_before() {
    let settings = mochi_domain::default_risk_settings();
    assert!(!serde_json::to_string(&settings)
        .unwrap()
        .contains("trailInsteadOfTakeProfit"));
    let on = mochi_domain::RiskSettings {
        trail_instead_of_take_profit: true,
        ..settings
    };
    assert_eq!(
        serde_json::to_value(&on).unwrap()["trailInsteadOfTakeProfit"],
        serde_json::json!(true)
    );
}

#[test]
fn trailing_instead_of_take_profit_keeps_the_stop_loss_required() {
    let instruments = Instruments::with_defaults();
    let mut input = approved(&instruments, |input| {
        input.decision.action == mochi_domain::AiAction::Long
            && input.decision.order_type == mochi_domain::EntryOrderType::Market
    });
    let ask: f64 = input.quote.as_ref().unwrap().ask.parse().unwrap();
    input.settings.require_stop_loss = false;
    input.settings.trail_instead_of_take_profit = true;
    input.decision.take_profit = None;
    input.decision.stop_loss = None;
    input.decision.trailing_stop = Some(Some(mochi_domain::AiTrailingStop {
        kind: mochi_domain::TrailingKind::Ratio,
        callback: 0.01,
        activation_price: Some(ask * 1.01),
    }));
    assert!(evaluate_risk(&input, &instruments)
        .rejection_reasons
        .iter()
        .any(|r| r.contains("must carry a stop loss")));
}
