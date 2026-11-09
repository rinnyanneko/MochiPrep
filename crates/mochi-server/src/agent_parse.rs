//! Strict model JSON parsing. Only the runner may execute the validated result.
use mochi_domain::{jsnum::js_trim, AiDecisionData};
use serde_json::Value;

/// Accept bare JSON or one fenced block; return only schema-validated model data.
/// Numeric overflow, isolated surrogate escapes and excessive JSON depth are safely rejected.
pub fn parse_decision(text: &str) -> Result<AiDecisionData, String> {
    let trimmed = js_trim(text);
    // The one accepted fence is anchored, optionally labelled json, case insensitive.
    let source = if let Some(body) = trimmed
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
    };
    let value: Value =
        serde_json::from_str(source).map_err(|_| "Response is not valid JSON".to_string())?;
    let checked = crate::validate::check_ai_decision(&value)?;
    serde_json::from_value(checked).map_err(|_| "Response is not valid JSON".to_string())
}
