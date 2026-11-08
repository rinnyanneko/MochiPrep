//! Request validation that answers exactly as Node does: Fastify's body parser, its query string
//! parser, and the Zod v4 schemas in `packages/shared/src/schemas.ts` and `apps/server/src/app.ts`.
//!
//! The web app shows a 400's text verbatim, so the wording, the order of the issues and even
//! Zod's quirks (length checks that still run after a type error) are part of the API.
//! `crates/golden/validation.json` holds what Node answers for each case and
//! `tests/golden_validation.rs` checks this module against it.
//!
//! Each schema is a plain function below ([`create_account`] and so on) that reads like the Zod
//! source. Every message text lives in [`msg`].

use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING};
use axum::http::{HeaderMap, StatusCode};
use mochi_domain::jsnum::js_number_string;
use mochi_domain::{DataSource, Timeframe, TrailingStopSpec};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Map, Value};

use crate::error::ApiError;

/// Every message text, in one place so it can be audited against Zod and Fastify.
mod msg {
    // Custom messages from packages/shared/src/schemas.ts.
    pub const DECIMAL: &str = "expected a positive decimal string";
    pub const SYMBOL: &str = "expected a pair like DOGEUSDT";

    // Zod v4's English locale (zod/v4/locales/en.js).
    pub const URL: &str = "Invalid URL";

    pub fn invalid_type(expected: &str, received: &str) -> String {
        format!("Invalid input: expected {expected}, received {received}")
    }

    /// Zod quotes each option with plain `"…"`, not JSON escaping.
    pub fn one_of(options: &[&str]) -> String {
        let quoted: Vec<String> = options.iter().map(|o| format!("\"{o}\"")).collect();
        format!("Invalid option: expected one of {}", quoted.join("|"))
    }

    /// `word` is "Too small" or "Too big"; `bound` is e.g. ">=1" or "<=60". Strings and arrays
    /// get a unit, every other origin ("number", "int", "unknown") does not.
    pub fn bound(word: &str, origin: &str, bound: &str) -> String {
        match origin {
            "string" => format!("{word}: expected string to have {bound} characters"),
            "array" => format!("{word}: expected array to have {bound} items"),
            _ => format!("{word}: expected {origin} to be {bound}"),
        }
    }

    pub fn unrecognized(keys: &[String]) -> String {
        let quoted: Vec<String> = keys.iter().map(|k| format!("\"{k}\"")).collect();
        let plural = if keys.len() > 1 { "s" } else { "" };
        format!("Unrecognized key{plural}: {}", quoted.join(", "))
    }

    // Fastify 5's content-type parser (fastify/lib/errors.js).
    pub const EMPTY_JSON_BODY: &str =
        "Body cannot be empty when content-type is set to 'application/json'";
    pub const INVALID_JSON_BODY: &str =
        "Body is not valid JSON but content-type is set to 'application/json'";
    pub const UNSUPPORTED_MEDIA_TYPE: &str = "Unsupported Media Type";
}

/// The schemas the ported routes use. Names follow the TS constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schema {
    CreateAccount,
    PlaceOrder,
    ClosePosition,
    Protection,
    AddInstrument,
    NewsSourceInput,
    NewsSourceToggle,
    ReplayControl,
    CandleQuery,
    SourceQuery,
    LimitQuery,
    DecisionQuery,
    /// `GET /api/news`: `z.object({ symbol: SymbolSchema })`.
    NewsQuery,
    /// `SymbolSchema` on its own, for a `:symbol` path param.
    Symbol,
}

// ---- typed results ---------------------------------------------------------------------------
// What a route gets after validation. `PlaceOrder` parses into `mochi_domain::OrderRequest`.

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAccountInput {
    pub name: String,
    pub source: DataSource,
    pub initial_balance: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosePositionInput {
    pub fraction: f64,
    pub client_order_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtectionInput {
    pub stop_loss: Option<String>,
    pub take_profit: Option<String>,
    /// None: the key was absent, so the route keeps the current trailing stop.
    /// Some(None): explicit null, which removes it.
    #[serde(default, deserialize_with = "present")]
    pub trailing_stop: Option<Option<TrailingStopSpec>>,
}

/// Only called when the key is present, so a present `null` becomes `Some(None)`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AddInstrumentInput {
    pub symbol: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NewsSourceInput {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NewsSourceToggle {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReplayControl {
    pub playing: Option<bool>,
    pub speed: Option<u32>,
    pub steps: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CandleQuery {
    pub source: DataSource,
    pub symbol: String,
    pub timeframe: Timeframe,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SourceQuery {
    pub source: DataSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LimitQuery {
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionQuery {
    pub limit: usize,
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NewsQuery {
    pub symbol: String,
}

// ---- entry points ----------------------------------------------------------------------------

/// `Schema.parse(body)` for a JSON body.
pub fn parse_body<T: DeserializeOwned>(schema: Schema, value: &Value) -> Result<T, ApiError> {
    parse_input(schema, Some(value))
}

/// `Schema.parse(request.body)`, where `None` is JS `undefined`: a request that sent no body.
pub fn parse_input<T: DeserializeOwned>(
    schema: Schema,
    input: Option<&Value>,
) -> Result<T, ApiError> {
    let value = check(schema, input)?;
    serde_json::from_value(value).map_err(|error| {
        // Only reachable if a typed result above disagrees with its schema: a bug, not bad input.
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Validated input did not fit its type: {error}"),
        )
    })
}

/// `Schema.parse(request.query)` for the raw query string (the part after `?`, or "").
pub fn parse_query<T: DeserializeOwned>(schema: Schema, raw_query: &str) -> Result<T, ApiError> {
    parse_input(schema, Some(&query_object(raw_query)))
}

/// `SymbolSchema.parse(request.params.symbol)` for an already decoded path param.
pub fn parse_symbol(param: &str) -> Result<String, ApiError> {
    parse_input(Schema::Symbol, Some(&Value::String(param.to_string())))
}

/// What Zod's parse returns (defaults filled in, strings trimmed, unknown query keys dropped),
/// or the 400 Node's error handler sends for its issues.
pub fn check(schema: Schema, input: Option<&Value>) -> Result<Value, ApiError> {
    let result = match schema {
        Schema::CreateAccount => create_account(input),
        Schema::PlaceOrder => place_order(input),
        Schema::ClosePosition => close_position(input),
        Schema::Protection => protection(input),
        Schema::AddInstrument => add_instrument(input),
        Schema::NewsSourceInput => news_source_input(input),
        Schema::NewsSourceToggle => news_source_toggle(input),
        Schema::ReplayControl => replay_control(input),
        Schema::CandleQuery => candle_query(input),
        Schema::SourceQuery => source_query(input),
        Schema::LimitQuery => limit_query(input),
        Schema::DecisionQuery => decision_query(input),
        Schema::NewsQuery => news_query(input),
        Schema::Symbol => check_string(input, &SYMBOL),
    };
    result.map_err(|issues| ApiError::bad_request(format_issues(&issues)))
}

// ---- schemas ---------------------------------------------------------------------------------

const SOURCES: &[&str] = &["REPLAY", "LIVE"];
const SIDES: &[&str] = &["BUY", "SELL"];
const ENTRY_TYPES: &[&str] = &["MARKET", "LIMIT"];
const TRAILING_KINDS: &[&str] = &["RATIO", "SPREAD"];
const TIMEFRAMES: &[&str] = &["1m", "5m", "15m", "1h", "4h"];

/// `decimalString`: `z.string().regex(/^\d+(\.\d+)?$/, …).max(40)`.
const DECIMAL: StringRule = StringRule {
    format: Format::Decimal,
    max: Some(40),
    ..StringRule::PLAIN
};
/// `SymbolSchema`: `z.string().regex(/^[A-Z0-9]{1,24}USDT$/, …)`.
const SYMBOL: StringRule = StringRule {
    format: Format::Symbol,
    ..StringRule::PLAIN
};
/// `clientOrderId`: `z.string().min(1).max(128)`.
const CLIENT_ORDER_ID: StringRule = StringRule {
    min: Some(1),
    max: Some(128),
    ..StringRule::PLAIN
};

fn decimal(input: Input) -> Checked {
    check_string(input, &DECIMAL)
}

/// `TrailingStopSchema`.
fn trailing_stop(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("kind", |v| check_enum(v, TRAILING_KINDS));
    fields.required("callback", decimal);
    fields.nullable("activationPrice", decimal);
    fields.strict()
}

fn create_account(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("name", |v| check_string(v, &StringRule::trimmed(60)));
    fields.required("source", |v| check_enum(v, SOURCES));
    fields.defaulted("initialBalance", json!("10000"), decimal);
    fields.strict()
}

fn place_order(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("accountId", |v| {
        check_string(
            v,
            &StringRule {
                min: Some(1),
                ..StringRule::PLAIN
            },
        )
    });
    fields.required("clientOrderId", |v| check_string(v, &CLIENT_ORDER_ID));
    fields.required("symbol", |v| check_string(v, &SYMBOL));
    fields.required("side", |v| check_enum(v, SIDES));
    fields.required("type", |v| check_enum(v, ENTRY_TYPES));
    fields.required("quantity", decimal);
    fields.nullish("price", decimal);
    fields.required("leverage", |v| {
        check_number(v, &NumberRule::int(1.0, 100.0))
    });
    fields.optional("reduceOnly", check_bool);
    fields.nullish("stopLoss", decimal);
    fields.nullish("takeProfit", decimal);
    fields.nullish("trailingStop", trailing_stop);
    fields.strict()
}

fn protection(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.nullable("stopLoss", decimal);
    fields.nullable("takeProfit", decimal);
    // `.nullable().optional()` behaves as `.nullish()`.
    fields.nullish("trailingStop", trailing_stop);
    fields.strict()
}

fn close_position(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    let fraction = NumberRule {
        int: false,
        min: 0.0,
        min_inclusive: false,
        max: 1.0,
    };
    fields.required("fraction", |v| check_number(v, &fraction));
    fields.required("clientOrderId", |v| check_string(v, &CLIENT_ORDER_ID));
    fields.strict()
}

fn add_instrument(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("symbol", |v| check_string(v, &SYMBOL));
    fields.strict()
}

fn news_source_input(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("name", |v| check_string(v, &StringRule::trimmed(40)));
    let url = StringRule {
        format: Format::Url,
        max: Some(300),
        ..StringRule::PLAIN
    };
    fields.required("url", |v| check_string(v, &url));
    fields.strict()
}

fn news_source_toggle(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("enabled", check_bool);
    fields.strict()
}

fn replay_control(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.optional("playing", check_bool);
    fields.optional("speed", |v| check_number(v, &NumberRule::int(1.0, 600.0)));
    fields.optional("steps", |v| check_number(v, &NumberRule::int(1.0, 1440.0)));
    fields.strict()
}

// Query schemas are `z.object` (not strict): unknown keys are dropped, not refused.

fn candle_query(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("source", |v| check_enum(v, SOURCES));
    fields.required("symbol", |v| check_string(v, &SYMBOL));
    fields.required("timeframe", |v| check_enum(v, TIMEFRAMES));
    fields.defaulted("limit", json!(300), |v| {
        check_coerced_number(v, &NumberRule::int(1.0, 1000.0))
    });
    fields.strip()
}

fn source_query(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("source", |v| check_enum(v, SOURCES));
    fields.strip()
}

fn limit_field(fields: &mut Fields) {
    fields.defaulted("limit", json!(200), |v| {
        check_coerced_number(v, &NumberRule::int(1.0, 2000.0))
    });
}

fn limit_query(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    limit_field(&mut fields);
    fields.strip()
}

fn decision_query(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    limit_field(&mut fields);
    fields.optional("agentId", |v| check_string(v, &StringRule::PLAIN));
    fields.strip()
}

fn news_query(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("symbol", |v| check_string(v, &SYMBOL));
    fields.strip()
}

// ---- issues ----------------------------------------------------------------------------------

/// A Zod issue: where (keys from the top) and what.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Issue {
    path: Vec<String>,
    message: String,
}

impl Issue {
    fn new(message: impl Into<String>) -> Self {
        Self {
            path: Vec::new(),
            message: message.into(),
        }
    }
}

/// `issues.map(i => `${i.path.join('.') || 'input'}: ${i.message}`).join('; ')`, as app.ts does.
fn format_issues(issues: &[Issue]) -> String {
    let parts: Vec<String> = issues
        .iter()
        .map(|issue| {
            let path = if issue.path.is_empty() {
                "input".to_string()
            } else {
                issue.path.join(".")
            };
            format!("{path}: {}", issue.message)
        })
        .collect();
    parts.join("; ")
}

/// The value being checked. `None` is JS `undefined`: an absent key or an absent body.
type Input<'a> = Option<&'a Value>;
/// Zod's output for one value, or every issue it found there.
type Checked = Result<Value, Vec<Issue>>;

/// Zod's `parsedType`, as it names the received type in messages.
fn received(input: Input) -> &'static str {
    match input {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

// ---- objects ---------------------------------------------------------------------------------

/// One object being checked, key by key in the schema's order, as Zod's object parser runs.
/// Issues from a key's value get the key prefixed to their path.
struct Fields<'a> {
    input: &'a Map<String, Value>,
    known: Vec<&'static str>,
    output: Map<String, Value>,
    issues: Vec<Issue>,
}

impl<'a> Fields<'a> {
    fn new(input: Input<'a>) -> Result<Self, Vec<Issue>> {
        match input {
            Some(Value::Object(map)) => Ok(Self {
                input: map,
                known: Vec::new(),
                output: Map::new(),
                issues: Vec::new(),
            }),
            other => Err(vec![Issue::new(msg::invalid_type(
                "object",
                received(other),
            ))]),
        }
    }

    fn record(&mut self, key: &'static str, result: Checked) {
        match result {
            Ok(value) => {
                self.output.insert(key.to_string(), value);
            }
            Err(issues) => {
                for mut issue in issues {
                    issue.path.insert(0, key.to_string());
                    self.issues.push(issue);
                }
            }
        }
    }

    /// A plain key: absent is checked as `undefined`, which the value's check refuses.
    fn required(&mut self, key: &'static str, check: impl Fn(Input) -> Checked) {
        self.known.push(key);
        let result = check(self.input.get(key));
        self.record(key, result);
    }

    /// `.optional()`: absent is left out of the output; anything present (null too) is checked.
    fn optional(&mut self, key: &'static str, check: impl Fn(Input) -> Checked) {
        self.known.push(key);
        if let Some(value) = self.input.get(key) {
            self.record(key, check(Some(value)));
        }
    }

    /// `.nullable()`: null passes; absent is checked as `undefined`, so it is still refused.
    fn nullable(&mut self, key: &'static str, check: impl Fn(Input) -> Checked) {
        self.known.push(key);
        let result = match self.input.get(key) {
            Some(Value::Null) => Ok(Value::Null),
            other => check(other),
        };
        self.record(key, result);
    }

    /// `.nullish()`: absent is left out, null is kept as null.
    fn nullish(&mut self, key: &'static str, check: impl Fn(Input) -> Checked) {
        self.known.push(key);
        match self.input.get(key) {
            None => {}
            Some(Value::Null) => self.record(key, Ok(Value::Null)),
            Some(value) => self.record(key, check(Some(value))),
        }
    }

    /// `.default(x)`: absent becomes `x` without being checked, as Zod does.
    fn defaulted(&mut self, key: &'static str, default: Value, check: impl Fn(Input) -> Checked) {
        self.known.push(key);
        let result = match self.input.get(key) {
            None => Ok(default),
            Some(value) => check(Some(value)),
        };
        self.record(key, result);
    }

    /// `z.strictObject`: unknown keys are one more issue, at the object's own path, after the
    /// issues of its declared keys.
    fn strict(mut self) -> Checked {
        let mut unknown: Vec<String> = self
            .input
            .keys()
            .filter(|key| !self.known.contains(&key.as_str()))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            js_key_order(&mut unknown);
            self.issues.push(Issue::new(msg::unrecognized(&unknown)));
        }
        self.strip()
    }

    /// `z.object`: unknown keys are dropped.
    fn strip(self) -> Checked {
        if self.issues.is_empty() {
            Ok(Value::Object(self.output))
        } else {
            Err(self.issues)
        }
    }
}

/// The order `for…in` lists a parsed JSON object's keys in: integer-like keys ascending first,
/// then the rest as the client sent them (the map keeps insertion order: `preserve_order`).
fn js_key_order(keys: &mut [String]) {
    // A stable sort leaves the non-index keys in their original order.
    keys.sort_by_key(|key| match array_index(key) {
        Some(index) => (0, index),
        None => (1, 0),
    });
}

/// A canonical array index: "0", or digits without a leading zero, below 2^32 - 1.
fn array_index(key: &str) -> Option<u64> {
    let canonical = key == "0"
        || (!key.is_empty()
            && !key.starts_with('0')
            && key.len() <= 10
            && key.bytes().all(|b| b.is_ascii_digit()));
    if !canonical {
        return None;
    }
    key.parse::<u64>().ok().filter(|&n| n < 4_294_967_295)
}

// ---- primitives ------------------------------------------------------------------------------

fn check_enum(input: Input, options: &[&str]) -> Checked {
    match input {
        Some(Value::String(s)) if options.contains(&s.as_str()) => Ok(Value::String(s.clone())),
        // Zod's enum check is the same for every wrong type, undefined included.
        _ => Err(vec![Issue::new(msg::one_of(options))]),
    }
}

fn check_bool(input: Input) -> Checked {
    match input {
        Some(Value::Bool(b)) => Ok(Value::Bool(*b)),
        other => Err(vec![Issue::new(msg::invalid_type(
            "boolean",
            received(other),
        ))]),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    None,
    Decimal,
    Symbol,
    Url,
}

/// A `z.string()` chain. Every chain in the schemas runs its checks in this order:
/// `.trim()`, then the format (regex or url), then `.min()`, then `.max()`.
#[derive(Debug, Clone, Copy)]
struct StringRule {
    trim: bool,
    format: Format,
    /// In code points, as Zod v4 counts.
    min: Option<usize>,
    max: Option<usize>,
}

impl StringRule {
    /// A bare `z.string()`.
    const PLAIN: StringRule = StringRule {
        trim: false,
        format: Format::None,
        min: None,
        max: None,
    };

    /// `z.string().trim().min(1).max(max)`.
    const fn trimmed(max: usize) -> StringRule {
        StringRule {
            trim: true,
            format: Format::None,
            min: Some(1),
            max: Some(max),
        }
    }
}

fn check_string(input: Input, rule: &StringRule) -> Checked {
    let Some(Value::String(raw)) = input else {
        let mut issues = vec![Issue::new(msg::invalid_type("string", received(input)))];
        // Zod quirk: its length checks run even after the type error, on any value that has a
        // `.length`. An array counts items; another object's `length` key is compared as JS
        // would (`"0x29" <= 40` is false, and NaN fails every comparison), and reported as an
        // "unknown" origin.
        if let Some((length, origin)) = js_length(input) {
            if let Some(min) = rule.min {
                if length.is_nan() || length < min as f64 {
                    issues.push(Issue::new(msg::bound(
                        "Too small",
                        origin,
                        &format!(">={min}"),
                    )));
                }
            }
            if let Some(max) = rule.max {
                if length.is_nan() || length > max as f64 {
                    issues.push(Issue::new(msg::bound(
                        "Too big",
                        origin,
                        &format!("<={max}"),
                    )));
                }
            }
        }
        return Err(issues);
    };

    let mut value = if rule.trim {
        js_trim(raw).to_string()
    } else {
        raw.clone()
    };
    // A failed format check does not stop the length checks (Zod sets `continue`).
    let mut issues = Vec::new();
    match rule.format {
        Format::None => {}
        Format::Decimal => {
            if !is_decimal(&value) {
                issues.push(Issue::new(msg::DECIMAL));
            }
        }
        Format::Symbol => {
            if !is_symbol(&value) {
                issues.push(Issue::new(msg::SYMBOL));
            }
        }
        Format::Url => {
            // Zod trims, asks the WHATWG parser whether it parses, and on success keeps the
            // trimmed text minus tabs and newlines (the parser drops those itself).
            let trimmed = js_trim(&value);
            if url::Url::parse(trimmed).is_ok() {
                value = trimmed.replace(['\t', '\n', '\r'], "");
            } else {
                issues.push(Issue::new(msg::URL));
            }
        }
    }
    let length = value.chars().count();
    if let Some(min) = rule.min {
        if length < min {
            issues.push(Issue::new(msg::bound(
                "Too small",
                "string",
                &format!(">={min}"),
            )));
        }
    }
    if let Some(max) = rule.max {
        if length > max {
            issues.push(Issue::new(msg::bound(
                "Too big",
                "string",
                &format!("<={max}"),
            )));
        }
    }
    if issues.is_empty() {
        Ok(Value::String(value))
    } else {
        Err(issues)
    }
}

/// `/^\d+(\.\d+)?$/`. JS `\d` is ASCII only, and `$` does not accept a trailing newline.
fn is_decimal(s: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    match s.split_once('.') {
        Some((whole, fraction)) => digits(whole) && digits(fraction),
        None => digits(s),
    }
}

/// `/^[A-Z0-9]{1,24}USDT$/`.
fn is_symbol(s: &str) -> bool {
    let Some(base) = s.strip_suffix("USDT") else {
        return false;
    };
    (1..=24).contains(&base.len())
        && base
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// The `.length` Zod's length checks read from a non-string, with the origin it reports.
fn js_length(input: Input) -> Option<(f64, &'static str)> {
    match input {
        Some(Value::Array(items)) => Some((items.len() as f64, "array")),
        Some(Value::Object(map)) => map
            .get("length")
            .map(|length| (js_to_number(length), "unknown")),
        _ => None,
    }
}

/// A `z.number()` chain: `.int()` (optional), then a lower and an upper bound.
#[derive(Debug, Clone, Copy)]
struct NumberRule {
    int: bool,
    min: f64,
    /// false for `.gt(min)`.
    min_inclusive: bool,
    max: f64,
}

impl NumberRule {
    /// `z.number().int().min(min).max(max)`.
    const fn int(min: f64, max: f64) -> NumberRule {
        NumberRule {
            int: true,
            min,
            min_inclusive: true,
            max,
        }
    }
}

/// Largest integer JS represents exactly (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// `z.number()` on a JSON value. JSON numbers are always finite.
fn check_number(input: Input, rule: &NumberRule) -> Checked {
    match input {
        Some(Value::Number(n)) => check_number_value(n.as_f64().unwrap_or(f64::NAN), rule),
        other => Err(vec![Issue::new(msg::invalid_type(
            "number",
            received(other),
        ))]),
    }
}

/// `z.coerce.number()`: JS `Number(value)` first. Query values are strings or string arrays.
fn check_coerced_number(input: Input, rule: &NumberRule) -> Checked {
    let x = input.map_or(f64::NAN, js_to_number);
    if x.is_nan() {
        return Err(vec![Issue::new(msg::invalid_type("number", "NaN"))]);
    }
    if x.is_infinite() {
        let received = if x > 0.0 { "Infinity" } else { "-Infinity" };
        return Err(vec![Issue::new(msg::invalid_type("number", received))]);
    }
    check_number_value(x, rule)
}

/// `x` is finite here: both callers refuse NaN and infinities first.
fn check_number_value(x: f64, rule: &NumberRule) -> Checked {
    let mut issues = Vec::new();
    if rule.int {
        // A fraction stops the chain; an unsafe integer does not (Zod sets `continue`).
        if x.fract() != 0.0 {
            return Err(vec![Issue::new(msg::invalid_type("int", "number"))]);
        }
        if x > MAX_SAFE_INTEGER {
            issues.push(Issue::new(msg::bound(
                "Too big",
                "int",
                "<=9007199254740991",
            )));
        } else if x < -MAX_SAFE_INTEGER {
            issues.push(Issue::new(msg::bound(
                "Too small",
                "int",
                ">=-9007199254740991",
            )));
        }
    }
    let min = js_number_string(rule.min);
    if rule.min_inclusive && x < rule.min {
        issues.push(Issue::new(msg::bound(
            "Too small",
            "number",
            &format!(">={min}"),
        )));
    } else if !rule.min_inclusive && x <= rule.min {
        issues.push(Issue::new(msg::bound(
            "Too small",
            "number",
            &format!(">{min}"),
        )));
    }
    if x > rule.max {
        let max = js_number_string(rule.max);
        issues.push(Issue::new(msg::bound(
            "Too big",
            "number",
            &format!("<={max}"),
        )));
    }
    if issues.is_empty() {
        Ok(js_number_value(x))
    } else {
        Err(issues)
    }
}

/// The JSON value JSON.stringify would write for `x`: 5, not 5.0, and 0 for -0.
fn js_number_value(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() <= MAX_SAFE_INTEGER {
        Value::from(x as i64)
    } else {
        serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
    }
}

// ---- JS semantics ----------------------------------------------------------------------------

/// The characters JS `String.prototype.trim` removes (WhiteSpace and LineTerminator). Rust's
/// `trim` uses a different set: it keeps U+FEFF and removes U+0085.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// JS `Number(value)` for a JSON value.
fn js_to_number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => js_string_to_number(s),
        // An array converts through its `join(',')` text, so `["5"]` is 5 and `["5","6"]` NaN.
        Value::Array(_) => js_string_to_number(&js_to_string(value)),
        Value::Object(_) => f64::NAN,
    }
}

/// JS `String(value)` for a JSON value.
fn js_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            // `join` writes null (and undefined) elements as empty text.
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_to_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// JS `Number(string)`: trimmed; empty is 0; `Infinity`; `0x`/`0o`/`0b` integers; otherwise a
/// decimal literal with optional sign, fraction and exponent, or NaN.
fn js_string_to_number(s: &str) -> f64 {
    let text = js_trim(s);
    if text.is_empty() {
        return 0.0;
    }
    match text {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = match text.get(..2) {
        Some("0x" | "0X") => Some(16),
        Some("0o" | "0O") => Some(8),
        Some("0b" | "0B") => Some(2),
        _ => None,
    };
    if let Some(radix) = radix {
        let digits = &text[2..];
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return f64::NAN;
        }
        // Exact while it fits u128; beyond that the value is far past every bound checked here.
        return match u128::from_str_radix(digits, radix) {
            Ok(n) => n as f64,
            Err(_) => digits.chars().fold(0.0, |acc, c| {
                acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            }),
        };
    }
    if is_js_decimal_literal(text) {
        // Rust parses this grammar with the same correct rounding (and overflow to infinity).
        text.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// `[+-]? (digits [. digits?] | . digits) ([eE] [+-]? digits)?`. Checked first because Rust's
/// parser also accepts words like "inf" and "nan", which JS does not.
fn is_js_decimal_literal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let count_digits = |from: usize| {
        bytes[from..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    let whole = count_digits(i);
    i += whole;
    let mut fraction = 0;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        fraction = count_digits(i);
        i += fraction;
    }
    if whole == 0 && fraction == 0 {
        return false;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exponent = count_digits(i);
        if exponent == 0 {
            return false;
        }
        i += exponent;
    }
    i == bytes.len()
}

// ---- Fastify's body parser -------------------------------------------------------------------

/// What Fastify makes of a request body before the route runs (`lib/content-type-parser.js`),
/// for every method that may carry one (POST, PUT, DELETE, …; never GET). `None` is a request
/// with no body at all, which Zod then sees as `undefined`.
///
/// - No content-type: no body is fine (an empty POST from the web app); a declared body is 415.
/// - `application/json` (any parameters): empty is 400, invalid JSON is 400, and so is a
///   `__proto__` key or a `constructor.prototype` anywhere (secure-json-parse).
/// - `text/plain`: the body as a string, which a schema then refuses as "received string".
/// - Anything else, or a malformed content-type: 415.
pub fn read_body(headers: &HeaderMap, bytes: &[u8]) -> Result<Option<Value>, ApiError> {
    let unsupported = || {
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            msg::UNSUPPORTED_MEDIA_TYPE,
        )
    };
    let Some(content_type) = headers.get(CONTENT_TYPE) else {
        let no_length = headers
            .get(CONTENT_LENGTH)
            .is_none_or(|length| length.as_bytes() == b"0");
        return if no_length && !headers.contains_key(TRANSFER_ENCODING) {
            Ok(None)
        } else {
            Err(unsupported())
        };
    };
    match media_type(content_type.as_bytes()).as_deref() {
        Some("application/json") => parse_json(bytes).map(Some),
        Some("text/plain") => Ok(Some(Value::String(
            String::from_utf8_lossy(bytes).into_owned(),
        ))),
        _ => Err(unsupported()),
    }
}

/// `type/subtype` of a content-type, lowercased, or None when Fastify's `ContentType` calls the
/// header invalid. Parameters never matter for choosing the parser.
fn media_type(header: &[u8]) -> Option<String> {
    // Node reads header bytes as latin1.
    let text: String = header.iter().map(|&b| char::from(b)).collect();
    if text.is_empty() || text == "undefined" {
        return None;
    }
    let media = text.split(';').next().unwrap_or("");
    let (kind, subtype) = media.split_once('/')?;
    let kind = kind.trim_start_matches(is_js_space).to_ascii_lowercase();
    let subtype = subtype.trim_end_matches(is_js_space).to_ascii_lowercase();
    // RFC 9110 token characters, as Fastify's regexes allow.
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+.^_`|~-".contains(&b))
    };
    (token(&kind) && token(&subtype)).then(|| format!("{kind}/{subtype}"))
}

fn parse_json(bytes: &[u8]) -> Result<Value, ApiError> {
    if bytes.is_empty() {
        return Err(ApiError::bad_request(msg::EMPTY_JSON_BODY));
    }
    let invalid = || ApiError::bad_request(msg::INVALID_JSON_BODY);
    // Fastify decodes the body as UTF-8 with replacement before parsing, and drops one BOM.
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    let value: Value = serde_json::from_str(text).map_err(|_| invalid())?;
    if has_prototype_key(&value) {
        return Err(invalid());
    }
    Ok(value)
}

/// secure-json-parse refuses a `__proto__` key, or a `constructor` object with a `prototype`
/// key, at any depth.
fn has_prototype_key(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key("__proto__")
                || matches!(map.get("constructor"), Some(Value::Object(c)) if c.contains_key("prototype"))
                || map.values().any(has_prototype_key)
        }
        Value::Array(items) => items.iter().any(has_prototype_key),
        _ => false,
    }
}

// ---- Fastify's query parser ------------------------------------------------------------------

/// `request.query` as Fastify's default parser (fast-querystring) builds it from the text after
/// `?`: `+` is a space, `%XX` is decoded as UTF-8 (kept raw when that fails), a key without `=`
/// has the value "", and a repeated key becomes an array of strings.
pub fn query_object(raw_query: &str) -> Value {
    let mut map = Map::new();
    for pair in raw_query.split('&') {
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None if pair.is_empty() => continue,
            None => (pair, ""),
        };
        let key = query_decode(key);
        let value = Value::String(query_decode(value));
        match map.get_mut(&key) {
            None => {
                map.insert(key, value);
            }
            Some(Value::Array(items)) => items.push(value),
            Some(first) => {
                let first = first.take();
                map.insert(key, Value::Array(vec![first, value]));
            }
        }
    }
    Value::Object(map)
}

fn query_decode(part: &str) -> String {
    let spaced = part.replace('+', " ");
    if !spaced.contains('%') {
        return spaced;
    }
    percent_decode(&spaced).unwrap_or(spaced)
}

/// Every `%XX` must be two hex digits and the result valid UTF-8, or the whole text is refused
/// (fast-decode-uri-component returns null and fast-querystring keeps the raw text).
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            // Checked first: `from_str_radix` would also take a sign, as in "%+f".
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_number_conversion() {
        assert_eq!(js_string_to_number(" 12 "), 12.0);
        assert_eq!(js_string_to_number(""), 0.0);
        assert_eq!(js_string_to_number("0x1F"), 31.0);
        assert!(js_string_to_number("-0x1").is_nan());
        assert!(js_string_to_number("inf").is_nan());
        assert!(js_string_to_number("1e").is_nan());
        assert!(js_string_to_number(".").is_nan());
        assert_eq!(js_string_to_number("-.5e1"), -5.0);
        assert_eq!(js_to_number(&json!(["7"])), 7.0);
        assert_eq!(js_to_number(&json!([])), 0.0);
        assert!(js_to_number(&json!({})).is_nan());
    }

    #[test]
    fn array_index_keys() {
        assert_eq!(array_index("0"), Some(0));
        assert_eq!(array_index("10"), Some(10));
        assert_eq!(array_index("01"), None);
        assert_eq!(array_index("4294967295"), None);
        assert_eq!(array_index("-1"), None);
    }
}
