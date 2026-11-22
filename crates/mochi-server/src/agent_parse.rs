//! Strict model JSON parsing. Only the runner may execute the validated result.
use mochi_domain::{jsnum::js_trim, AiDecisionData};
use serde_json::Value;

/// Accept bare JSON or one fenced block; return only schema-validated model data.
/// Numeric overflow, isolated surrogate escapes and excessive JSON depth are safely rejected.
pub fn parse_decision(text: &str) -> Result<AiDecisionData, String> {
    let value: Value = serde_json::from_str(unfenced(text))
        .map_err(|_| "Response is not valid JSON".to_string())?;
    checked_decision(&value)
}

/// The answer for an agent that trades several pairs: `{"decisions": [...]}` and nothing else,
/// one decision per pair. The outer `Err` refuses the whole answer. Otherwise each pair, in
/// `symbols` order, gets its own validated decision or the reason it has none: missing, given
/// more than once, or failing the same schema [`parse_decision`] applies. A decision for a pair
/// the agent was not asked about is never used.
pub fn parse_decisions(
    text: &str,
    symbols: &[String],
) -> Result<Vec<Result<AiDecisionData, String>>, String> {
    let value: Value = serde_json::from_str(unfenced(text))
        .map_err(|_| "Response is not valid JSON".to_string())?;
    let items = match &value {
        Value::Object(map) if map.len() == 1 => match map.get("decisions") {
            Some(Value::Array(items)) => items,
            _ => return Err(ONLY_DECISIONS.to_string()),
        },
        _ => return Err(ONLY_DECISIONS.to_string()),
    };
    // Extra entries can only be duplicates or other pairs; a bound keeps a runaway answer cheap.
    if items.len() > symbols.len() * 2 {
        return Err(format!(
            "Response has {} decisions for {} pairs",
            items.len(),
            symbols.len()
        ));
    }
    let mut found: Vec<Option<Result<AiDecisionData, String>>> =
        symbols.iter().map(|_| None).collect();
    for item in items {
        let Some(index) = item
            .get("symbol")
            .and_then(Value::as_str)
            .and_then(|symbol| symbols.iter().position(|s| s == symbol))
        else {
            continue;
        };
        // Two answers for one pair: neither is trusted, however many more follow.
        found[index] = Some(if found[index].is_some() {
            Err(format!("More than one decision for {}", symbols[index]))
        } else {
            checked_decision(item)
        });
    }
    Ok(found
        .into_iter()
        .zip(symbols)
        .map(|(decision, symbol)| {
            decision.unwrap_or_else(|| Err(format!("No decision for {symbol}")))
        })
        .collect())
}

const ONLY_DECISIONS: &str = "Response must be an object whose only key is a \"decisions\" array";

fn checked_decision(value: &Value) -> Result<AiDecisionData, String> {
    let checked = crate::validate::check_ai_decision(value)?;
    serde_json::from_value(checked).map_err(|_| "Response is not valid JSON".to_string())
}

/// Bare JSON, or the body of one fence: anchored, optionally labelled json, case insensitive.
fn unfenced(text: &str) -> &str {
    let trimmed = js_trim(text);
    if let Some(body) = trimmed
        .strip_prefix("```")
        .and_then(|s| s.strip_suffix("```"))
    {
        let body = if body
            .get(..4)
            .is_some_and(|s| s.eq_ignore_ascii_case("json"))
        {
            &body[4..]
        } else {
            body
        };
        js_trim(body)
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hold(symbol: &str) -> Value {
        json!({"symbol":symbol,"action":"HOLD","orderType":"MARKET","entryPrice":null,
               "stopLoss":null,"takeProfit":null,"reason":"wait","confidence":0.5,
               "expiresAt":null})
    }
    fn pairs() -> Vec<String> {
        vec!["BTCUSDT".into(), "ETHUSDT".into()]
    }

    #[test]
    fn each_pair_gets_its_own_decision_in_the_agents_order() {
        let text = json!({"decisions":[hold("ETHUSDT"), hold("BTCUSDT")]}).to_string();
        let parsed = parse_decisions(&text, &pairs()).unwrap();
        assert_eq!(parsed[0].as_ref().unwrap().symbol, "BTCUSDT");
        assert_eq!(parsed[1].as_ref().unwrap().symbol, "ETHUSDT");
    }

    #[test]
    fn a_missing_duplicate_or_invalid_decision_fails_only_its_pair() {
        let mut bad = hold("ETHUSDT");
        bad["action"] = json!("BUY_EVERYTHING");
        let text = json!({"decisions":[hold("BTCUSDT"), bad, hold("SOLUSDT")]}).to_string();
        let parsed = parse_decisions(&text, &pairs()).unwrap();
        assert!(parsed[0].is_ok());
        assert!(parsed[1].is_err());

        let text = json!({"decisions":[hold("BTCUSDT"), hold("BTCUSDT"), hold("BTCUSDT")]});
        let parsed = parse_decisions(&text.to_string(), &pairs()).unwrap();
        assert_eq!(
            parsed[0].as_ref().unwrap_err(),
            "More than one decision for BTCUSDT"
        );
        assert_eq!(parsed[1].as_ref().unwrap_err(), "No decision for ETHUSDT");
    }

    #[test]
    fn anything_but_one_decisions_array_is_refused_whole() {
        for text in [
            json!([hold("BTCUSDT")]).to_string(),
            hold("BTCUSDT").to_string(),
            json!({"decisions":[hold("BTCUSDT")],"note":"x"}).to_string(),
            json!({"decisions":{"BTCUSDT":hold("BTCUSDT")}}).to_string(),
        ] {
            assert_eq!(
                parse_decisions(&text, &pairs()).unwrap_err(),
                ONLY_DECISIONS,
                "{text}"
            );
        }
        assert_eq!(
            parse_decisions("not json", &pairs()).unwrap_err(),
            "Response is not valid JSON"
        );
        let many: Vec<Value> = (0..5).map(|_| hold("SOLUSDT")).collect();
        let text = json!({ "decisions": many }).to_string();
        assert_eq!(
            parse_decisions(&text, &pairs()).unwrap_err(),
            "Response has 5 decisions for 2 pairs"
        );
        let fenced = format!("```json\n{}\n```", json!({"decisions":[hold("BTCUSDT")]}));
        assert!(parse_decisions(&fenced, &pairs()).unwrap()[0].is_ok());
    }
}
