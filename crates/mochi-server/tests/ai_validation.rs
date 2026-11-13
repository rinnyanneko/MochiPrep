use mochi_server::{
    agent_parse::parse_decision,
    validate::{self, Schema},
};
use serde_json::{json, Value};
#[test]
fn ai_validation_matches_node() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../golden/ai-validation.json")).unwrap();
    for case in fixture["api"].as_array().unwrap() {
        let schema = match case["schema"].as_str().unwrap() {
            "ProviderInput" => Schema::ProviderInput,
            "ProviderKey" => Schema::ProviderKey,
            "ProviderTest" => Schema::ProviderTest,
            "AgentInput" => Schema::AgentInput,
            _ => unreachable!(),
        };
        let actual = validate::check(schema, case.get("input"));
        if let Some(expected) = case.get("error") {
            assert_eq!(
                actual.unwrap_err().message(),
                expected.as_str().unwrap(),
                "{case}"
            );
        } else {
            assert_eq!(
                serde_json::to_string(&actual.unwrap()).unwrap(),
                serde_json::to_string(&case["output"]).unwrap(),
                "{case}"
            );
        }
    }
    for case in fixture["decisions"].as_array().unwrap() {
        let result = match parse_decision(case["text"].as_str().unwrap()) {
            Ok(decision) => json!({"ok":true,"decision":decision}),
            Err(error) => json!({"ok":false,"error":error}),
        };
        assert_eq!(result, case["result"], "{case}");
    }
}

#[test]
fn typed_inputs_keep_header_omission_and_signal_defaults() {
    use validate::{AgentInput, ProviderInput, ProviderKeyInput, ProviderTestInput};
    let fixture: Value =
        serde_json::from_str(include_str!("../../golden/ai-validation.json")).unwrap();
    for case in fixture["api"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c.get("error").is_none())
    {
        let input = &case["input"];
        match case["schema"].as_str().unwrap() {
            "ProviderInput" => {
                let value: ProviderInput =
                    validate::parse_body(Schema::ProviderInput, input).unwrap();
                assert_eq!(
                    value.extra_headers.is_some(),
                    input.get("extraHeaders").is_some()
                );
            }
            "AgentInput" => {
                let value: AgentInput = validate::parse_body(Schema::AgentInput, input).unwrap();
                assert_eq!(
                    value.execution_mode,
                    case["output"]["executionMode"].as_str().unwrap()
                );
            }
            "ProviderKey" => {
                let _: ProviderKeyInput = validate::parse_body(Schema::ProviderKey, input).unwrap();
            }
            "ProviderTest" => {
                let _: ProviderTestInput =
                    validate::parse_body(Schema::ProviderTest, input).unwrap();
            }
            _ => unreachable!(),
        }
    }
}
#[test]
fn isolated_surrogate_escapes_are_rejected_without_replacement() {
    let text = r#"{"symbol":"DOGEUSDT","action":"HOLD","orderType":"MARKET","entryPrice":null,"stopLoss":null,"takeProfit":null,"reason":"\ud800","confidence":0.5,"expiresAt":null}"#;
    assert_eq!(
        parse_decision(text).unwrap_err(),
        "Response is not valid JSON"
    );
    assert_eq!(
        parse_decision(&text.replace("\\ud800", "\\udc00")).unwrap_err(),
        "Response is not valid JSON"
    );
    let paired = parse_decision(&text.replace("\\ud800", "\\ud83d\\ude00")).unwrap();
    assert_eq!(paired.reason, "😀");
}

#[test]
fn deeply_nested_untrusted_json_is_rejected_without_panicking() {
    let text = format!("{}0{}", "[".repeat(200), "]".repeat(200));
    assert_eq!(
        parse_decision(&text).unwrap_err(),
        "Response is not valid JSON"
    );
}

#[test]
fn overflow_numbers_fail_closed_without_changing_workspace_json_semantics() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../golden/ai-validation.json")).unwrap();
    for case in fixture["safeRejections"].as_array().unwrap() {
        assert_eq!(
            parse_decision(case["text"].as_str().unwrap()).unwrap_err(),
            case["rustError"].as_str().unwrap()
        );
        assert!(!case["nodeResult"]["ok"].as_bool().unwrap());
    }
}
