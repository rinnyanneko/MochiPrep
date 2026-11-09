//! AI API schemas and the untrusted model contract, sharing the server's Zod-compatible checks.
use super::*;
use indexmap::IndexMap;
use mochi_ai::ProviderKind;
use mochi_domain::{default_risk_settings, RiskSettings};
use regex::Regex;
use std::sync::LazyLock;

// Header values and keys can hold credentials; these inputs intentionally do not implement Debug.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInput {
    pub name: String,
    pub kind: ProviderKind,
    /// None keeps saved headers; an explicit empty map clears them.
    pub extra_headers: Option<IndexMap<String, String>>,
    pub base_url: Option<String>,
    pub default_model: String,
    #[serde(rename = "inputPricePerMTok")]
    pub input_price_per_m_tok: Option<f64>,
    #[serde(rename = "outputPricePerMTok")]
    pub output_price_per_m_tok: Option<f64>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderKeyInput {
    pub api_key: String,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ProviderTestInput {
    pub model: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInput {
    pub name: String,
    pub provider_id: String,
    pub model: String,
    pub symbol: String,
    pub timeframe: mochi_domain::Timeframe,
    pub strategy: String,
    pub account_id: String,
    pub risk: RiskSettings,
    pub interval_sec: i64,
    pub execution_mode: String,
    pub use_news: bool,
    pub allow_trailing_stop: bool,
}
const POSITIVE: NumberRule = NumberRule {
    int: false,
    min: 0.0,
    min_inclusive: false,
    max: f64::INFINITY,
};
fn number(input: Input, min: f64, max: f64) -> Checked {
    check_number(
        input,
        &NumberRule {
            int: false,
            min,
            min_inclusive: true,
            max,
        },
    )
}
fn bounded_string(input: Input, min: usize, max: usize) -> Checked {
    check_string(
        input,
        &StringRule {
            min: Some(min),
            max: Some(max),
            ..StringRule::PLAIN
        },
    )
}
fn nonempty(input: Input) -> Checked {
    check_string(
        input,
        &StringRule {
            min: Some(1),
            ..StringRule::PLAIN
        },
    )
}
pub(super) fn provider_key(input: Input) -> Checked {
    let mut f = Fields::new(input)?;
    f.required("apiKey", |v| check_string(v, &StringRule::trimmed(1000)));
    f.strict()
}
pub(super) fn provider_test(input: Input) -> Checked {
    let mut f = Fields::new(input)?;
    f.optional("model", |v| check_string(v, &StringRule::PLAIN));
    f.strip()
}
pub(super) fn provider_input(input: Input) -> Checked {
    let mut f = Fields::new(input)?;
    f.required("name", |v| check_string(v, &StringRule::trimmed(60)));
    f.required("kind", |v| {
        check_enum(
            v,
            &[
                "OPENAI_COMPATIBLE",
                "OPENAI",
                "ANTHROPIC",
                "GEMINI",
                "DEEPSEEK",
                "OPENCODE",
            ],
        )
    });
    f.optional("extraHeaders", extra_headers);
    f.nullable("baseUrl", |v| {
        check_string(
            v,
            &StringRule {
                format: Format::Url,
                max: Some(300),
                ..StringRule::PLAIN
            },
        )
    });
    f.required("defaultModel", |v| {
        check_string(v, &StringRule::trimmed(120))
    });
    f.nullable("inputPricePerMTok", |v| number(v, 0.0, f64::INFINITY));
    f.nullable("outputPricePerMTok", |v| number(v, 0.0, f64::INFINITY));
    f.strict()
}
fn extra_headers(input: Input) -> Checked {
    let Some(Value::Object(headers)) = input else {
        return Err(vec![Issue::new(msg::invalid_type(
            "record",
            received(input),
        ))]);
    };
    let mut names: Vec<String> = headers.keys().cloned().collect();
    js_key_order(&mut names);
    let mut issues = Vec::new();
    let mut output = Map::new();
    let mut aborted = false;
    for name in &names {
        if name.is_empty()
            || name.len() > 100
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+.^_`|~-".contains(&b))
        {
            issues.push(Issue {
                path: vec![name.clone()],
                message: "Invalid key in record".into(),
            });
            aborted = true;
            continue;
        }
        let value = headers.get(name);
        let mut checked = bounded_string(value, 1, 2000);
        if let Some(Value::String(value)) = value {
            if value.contains(['\r', '\n']) {
                match &mut checked {
                    Ok(_) => {
                        checked = Err(vec![Issue::new("header values cannot contain line breaks")])
                    }
                    Err(errors) => {
                        errors.push(Issue::new("header values cannot contain line breaks"))
                    }
                }
            }
        } else {
            aborted = true;
        }
        match checked {
            Ok(value) => {
                output.insert(name.to_ascii_lowercase(), value);
            }
            Err(errors) => {
                for mut error in errors {
                    error.path.insert(0, name.clone());
                    issues.push(error);
                }
            }
        }
    }
    if !aborted {
        if names.len() > 20 {
            issues.push(Issue::new("at most 20 headers"));
        }
        for name in names {
            if [
                "authorization",
                "x-api-key",
                "x-goog-api-key",
                "anthropic-version",
                "content-type",
                "content-length",
                "host",
                "connection",
                "transfer-encoding",
            ]
            .contains(&name.to_ascii_lowercase().as_str())
            {
                issues.push(Issue::new(format!(
                    "{name} is set by the provider and cannot be overridden"
                )));
            }
        }
    }
    if issues.is_empty() {
        Ok(Value::Object(output))
    } else {
        Err(issues)
    }
}
fn risk(input: Input) -> Checked {
    let mut f = Fields::new(input)?;
    f.required("maxLeverage", |v| number(v, 1.0, 100.0));
    f.required("maxRiskPerTradePct", |v| {
        check_number(
            v,
            &NumberRule {
                max: 100.0,
                ..POSITIVE
            },
        )
    });
    f.required("maxDailyLossPct", |v| {
        check_number(
            v,
            &NumberRule {
                max: 100.0,
                ..POSITIVE
            },
        )
    });
    f.required("maxOpenPositions", |v| {
        check_number(v, &NumberRule::int(1.0, 50.0))
    });
    f.required("requireStopLoss", check_bool);
    f.required("maxQuoteAgeSec", |v| number(v, 1.0, 3600.0));
    f.required("maxNotional", |v| check_number(v, &POSITIVE));
    f.strict()
}
pub(super) fn agent_input(input: Input) -> Checked {
    let mut f = Fields::new(input)?;
    f.required("name", |v| check_string(v, &StringRule::trimmed(60)));
    f.required("providerId", nonempty);
    f.required("model", |v| check_string(v, &StringRule::trimmed(120)));
    f.required("symbol", |v| check_string(v, &SYMBOL));
    f.required("timeframe", |v| check_enum(v, TIMEFRAMES));
    f.required("strategy", |v| {
        check_string(
            v,
            &StringRule {
                max: Some(8000),
                ..StringRule::PLAIN
            },
        )
    });
    f.required("accountId", nonempty);
    f.defaulted(
        "risk",
        serde_json::to_value(default_risk_settings()).expect("finite default risk"),
        risk,
    );
    f.required("intervalSec", |v| {
        check_number(v, &NumberRule::int(10.0, 86400.0))
    });
    f.defaulted("executionMode", json!("SIGNAL"), |v| {
        check_enum(v, &["AUTO", "SIGNAL"])
    });
    f.defaulted("useNews", json!(false), check_bool);
    f.defaulted("allowTrailingStop", json!(false), check_bool);
    f.strict()
}
fn ai_trailing(input: Input) -> Checked {
    let mut f = Fields::new(input)?;
    f.required("kind", |v| check_enum(v, TRAILING_KINDS));
    f.required("callback", |v| check_number(v, &POSITIVE));
    f.nullable("activationPrice", |v| check_number(v, &POSITIVE));
    f.strict()
}
fn datetime(input: Input) -> Checked {
    let value = check_string(input, &StringRule::PLAIN)?;
    // Zod's ISO grammar includes calendar leap years and mandatory qualified seconds.
    static ISO: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(r"\A(?:(?:[0-9][0-9][2468][048]|[0-9][0-9][13579][26]|[0-9][0-9]0[48]|[02468][048]00|[13579][26]00)-02-29|[0-9]{4}-(?:(?:0[13578]|1[02])-(?:0[1-9]|[12][0-9]|3[01])|(?:0[469]|11)-(?:0[1-9]|[12][0-9]|30)|02-(?:0[1-9]|1[0-9]|2[0-8])))",r"T(?:[01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9](?:\.[0-9]+)?(?:Z|[+-](?:[01][0-9]|2[0-3]):[0-5][0-9])\z")).expect("Zod datetime regex")
    });
    if ISO.is_match(value.as_str().expect("checked string")) {
        Ok(value)
    } else {
        Err(vec![Issue::new("Invalid ISO datetime")])
    }
}
pub(super) fn ai_decision(input: Input) -> Checked {
    let mut f = Fields::new(input)?;
    f.required("symbol", |v| bounded_string(v, 1, 32));
    f.required("action", |v| {
        check_enum(v, &["LONG", "SHORT", "CLOSE", "HOLD", "PROTECT"])
    });
    f.required("orderType", |v| check_enum(v, ENTRY_TYPES));
    f.nullable("entryPrice", |v| check_number(v, &POSITIVE));
    f.nullable("stopLoss", |v| check_number(v, &POSITIVE));
    f.nullable("takeProfit", |v| check_number(v, &POSITIVE));
    f.required("reason", |v| bounded_string(v, 1, 4000));
    f.required("confidence", |v| number(v, 0.0, 1.0));
    f.nullable("expiresAt", datetime);
    f.nullish("trailingStop", ai_trailing);
    let needs_price = f.output.get("orderType") == Some(&json!("LIMIT"))
        && f.output.get("entryPrice") == Some(&Value::Null)
        && matches!(
            f.output.get("action").and_then(Value::as_str),
            Some("LONG" | "SHORT")
        );
    // Refinements run after continuable constraints (including unknown keys), but not type/enum errors.
    let mut result = f.strict();
    if needs_price {
        let aborted = match &result {
            Ok(_) => false,
            Err(issues) => issues.iter().any(|i| {
                i.message.starts_with("Invalid input") || i.message.starts_with("Invalid option")
            }),
        };
        if !aborted {
            let issue = Issue {
                path: vec!["entryPrice".into()],
                message: "LIMIT orders need entryPrice".into(),
            };
            match &mut result {
                Ok(_) => result = Err(vec![issue]),
                Err(issues) => issues.push(issue),
            }
        }
    }
    result
}
