use mochi_ai::{
    build_system_prompt, build_user_prompt, indicator_snapshot, redact_secrets, AiProvider,
    AiRequest, MarketContext, MockProvider,
};
use serde_json::Value;
fn fixture() -> Value {
    serde_json::from_str(include_str!("../../golden/ai.json")).unwrap()
}
fn close(a: &Value, b: &Value) {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            assert!((a - b).abs() <= 1e-11 * a.abs().max(1.), "{a} != {b}");
        }
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(a.len(), b.len());
            for (k, v) in a {
                close(v, &b[k]);
            }
        }
        _ => assert_eq!(a, b),
    }
}
#[test]
fn indicator_parity() {
    for case in fixture()["indicators"].as_array().unwrap() {
        let candles: Vec<mochi_domain::Candle> =
            serde_json::from_value(case["candles"].clone()).unwrap();
        close(
            &serde_json::to_value(indicator_snapshot(&candles)).unwrap(),
            &case["expected"],
        );
    }
}
#[test]
fn complete_prompts_match_node_byte_for_byte() {
    for case in fixture()["prompts"].as_array().unwrap() {
        let context: MarketContext = serde_json::from_value(case["context"].clone()).unwrap();
        let trailing = case["allowTrailingStop"].as_bool().unwrap();
        assert_eq!(
            build_system_prompt(trailing),
            case["system"],
            "{}",
            case["name"]
        );
        assert_eq!(
            build_user_prompt(&context, trailing),
            case["user"],
            "{}",
            case["name"]
        );
    }
}
#[tokio::test]
async fn all_mock_strategy_branches_match_node() {
    let provider = MockProvider;
    for case in fixture()["mocks"].as_array().unwrap() {
        let request = AiRequest {
            model: case["model"].as_str().unwrap().into(),
            system: String::new(),
            user: String::new(),
            context: Some(serde_json::from_value(case["context"].clone()).unwrap()),
        };
        let response = provider.complete(&request).await.unwrap();
        assert_eq!(response.input_tokens, Some(0.));
        assert_eq!(response.output_tokens, Some(0.));
        let actual: Value = serde_json::from_str(&response.text).unwrap();
        assert_eq!(actual, case["expected"], "{}", case["name"]);
    }
}
#[test]
fn redaction_matches_node() {
    for case in fixture()["redactions"].as_array().unwrap() {
        let secrets: Vec<&str> = case["secrets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert_eq!(
            redact_secrets(case["text"].as_str().unwrap(), secrets),
            case["expected"]
        );
    }
}
#[tokio::test]
async fn mock_requires_context() {
    let e = MockProvider
        .complete(&AiRequest {
            model: String::new(),
            system: String::new(),
            user: String::new(),
            context: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, mochi_ai::ProviderErrorCode::BadResponse);
}

#[tokio::test]
async fn mock_rejects_precision_beyond_javascript_range_without_panicking() {
    let mut context: MarketContext =
        serde_json::from_value(fixture()["mocks"][0]["context"].clone()).unwrap();
    context.quote.last = format!("1000.{}", "0".repeat(101));
    let request = AiRequest {
        model: "mock-momentum-v1".into(),
        system: String::new(),
        user: String::new(),
        context: Some(context),
    };
    let error = MockProvider.complete(&request).await.unwrap_err();
    assert_eq!(error.code, mochi_ai::ProviderErrorCode::BadResponse);
    assert_eq!(
        error.message,
        "Mock quote precision exceeds 100 fractional digits"
    );
}
