//! Agent templates: named sets of agent form settings. Applying one only pre-fills the agent
//! form in the web app, so nothing here can loosen a limit: the agent is still created through
//! the validated agent routes, and every order still passes the risk engine.
//!
//! Built-in presets are defined in code rather than seeded rows, so a release can improve them.
//! They are read-only (the web app offers Duplicate instead of Edit); deleting one only hides it,
//! and [`restore_builtin_agent_templates`] shows every hidden one again.

use mochi_domain::RiskSettings;
use rusqlite::types::Type;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::{unix_ms, StoreError};

/// Ids of built-in presets start with this, and user templates never do.
pub const BUILTIN_PREFIX: &str = "builtin_";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTemplate {
    pub id: String,
    pub name: String,
    pub description: String,
    pub built_in: bool,
    pub strategy: String,
    pub timeframe: String,
    pub interval_sec: i64,
    pub execution_mode: String,
    pub risk: RiskSettings,
    pub use_news: bool,
    pub allow_trailing_stop: bool,
    /// None for built-ins, which were never written to the database.
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
}

/// The fields a user sets; the store adds the id and timestamps.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTemplateFields {
    pub name: String,
    pub description: String,
    pub strategy: String,
    pub timeframe: String,
    pub interval_sec: i64,
    pub execution_mode: String,
    pub risk: RiskSettings,
    pub use_news: bool,
    pub allow_trailing_stop: bool,
}

struct Preset {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    strategy: &'static str,
    timeframe: &'static str,
    interval_sec: i64,
    allow_trailing_stop: bool,
    risk: RiskSettings,
}

/// Preset risk limits. A stop loss is always required: no preset may leave a position unprotected.
fn limits(
    max_leverage: f64,
    max_risk_per_trade_pct: f64,
    max_daily_loss_pct: f64,
    max_open_positions: f64,
    max_quote_age_sec: f64,
    max_notional: f64,
) -> RiskSettings {
    RiskSettings {
        max_leverage,
        max_risk_per_trade_pct,
        max_daily_loss_pct,
        max_open_positions,
        require_stop_loss: true,
        max_quote_age_sec,
        max_notional,
        min_confidence: 0.0,
    }
}

/// Every preset suggests rather than trades (SIGNAL), requires a stop loss and keeps leverage
/// and risk per trade at or below the paper defaults: a preset is a starting point, and the
/// owner opts into anything riskier by editing a copy. Strategy text names only what the prompt
/// carries: candles of the agent's own timeframe and the indicators computed from them (see
/// `mochi-ai`'s indicators). The model is never sent another timeframe to compare against.
fn presets() -> [Preset; 4] {
    [
        Preset {
            id: "builtin_conservative",
            name: "Conservative",
            description: "Few, small trades with tight limits. Suggests only.",
            strategy: "Be patient and selective. Only propose a trade when ema20 and ema50 agree \
                       on the trend and price has pulled back to support (for longs) or resistance \
                       (for shorts) in the recent candles. Always set a stop loss beyond the recent \
                       swing and a take profit at least 2x the stop distance. When the picture is \
                       unclear, HOLD.",
            timeframe: "1h",
            interval_sec: 3600,
            allow_trailing_stop: false,
            risk: limits(2.0, 0.25, 1.0, 1.0, 30.0, 5_000.0),
        },
        Preset {
            id: "builtin_balanced",
            name: "Balanced",
            description: "Pullback entries in the direction of the trend. Suggests only.",
            strategy: "Trade pullbacks in the direction of the trend, taking ema50 as the trend. \
                       Enter on a confirmation such as a higher low in an uptrend or a lower high \
                       in a downtrend. Set the stop loss beyond the pullback's extreme and the take \
                       profit near the previous swing. Skip trades during sharp, news-driven moves.",
            timeframe: "15m",
            interval_sec: 900,
            allow_trailing_stop: false,
            risk: limits(3.0, 0.5, 2.0, 2.0, 30.0, 20_000.0),
        },
        Preset {
            id: "builtin_trend",
            name: "Trend following",
            description: "Rides established trends with a trailing stop. Suggests only.",
            strategy: "Follow established trends. Go long only when price and ema20 are above a \
                       rising ema50, and short only when both are below a falling one. Enter on \
                       breakouts of recent ranges, set an initial stop loss on the other side of \
                       the range, and use a trailing stop to stay in the move. Close when the \
                       trend structure breaks.",
            timeframe: "1h",
            interval_sec: 3600,
            allow_trailing_stop: true,
            risk: limits(3.0, 0.5, 2.0, 2.0, 30.0, 20_000.0),
        },
        Preset {
            id: "builtin_range",
            name: "Range reversion",
            description: "Fades the edges of a clear range on short timeframes. Suggests only.",
            strategy: "Look for a clear sideways range in the recent candles. Propose a long near \
                       the range low or a short near the range high, with the stop loss just \
                       outside the range and the take profit near the middle or the opposite edge. \
                       Do not trade when the range is breaking or atr14 shows volatility expanding.",
            timeframe: "5m",
            interval_sec: 300,
            allow_trailing_stop: false,
            risk: limits(2.0, 0.25, 1.5, 1.0, 15.0, 10_000.0),
        },
    ]
}

/// Every built-in preset, hidden or not, in display order.
pub fn builtin_agent_templates() -> Vec<AgentTemplate> {
    presets()
        .into_iter()
        .map(|p| AgentTemplate {
            id: p.id.into(),
            name: p.name.into(),
            description: p.description.into(),
            built_in: true,
            strategy: p.strategy.into(),
            timeframe: p.timeframe.into(),
            interval_sec: p.interval_sec,
            execution_mode: "SIGNAL".into(),
            risk: p.risk,
            use_news: false,
            allow_trailing_stop: p.allow_trailing_stop,
            created_at: None,
            updated_at: None,
        })
        .collect()
}

fn template_from_row(row: &Row<'_>) -> rusqlite::Result<AgentTemplate> {
    let risk: String = row.get("risk")?;
    let risk = serde_json::from_str(&risk).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
    })?;
    Ok(AgentTemplate {
        id: row.get("id")?,
        name: row.get("name")?,
        description: row.get("description")?,
        built_in: false,
        strategy: row.get("strategy")?,
        timeframe: row.get("timeframe")?,
        interval_sec: row.get("interval_sec")?,
        execution_mode: row.get("execution_mode")?,
        risk,
        use_news: row.get::<_, i64>("use_news")? != 0,
        allow_trailing_stop: row.get::<_, i64>("allow_trailing_stop")? != 0,
        created_at: Some(row.get("created_at")?),
        updated_at: Some(row.get("updated_at")?),
    })
}

fn hidden_ids(conn: &Connection) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare("SELECT id FROM agent_template_hidden")?;
    let ids = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(ids.collect::<Result<_, _>>()?)
}

/// The visible built-ins in preset order, then the user's templates oldest first.
pub fn list_agent_templates(conn: &Connection) -> Result<Vec<AgentTemplate>, StoreError> {
    let hidden = hidden_ids(conn)?;
    let mut out: Vec<AgentTemplate> = builtin_agent_templates()
        .into_iter()
        .filter(|t| !hidden.contains(&t.id))
        .collect();
    let mut stmt = conn.prepare("SELECT * FROM agent_templates ORDER BY created_at, id")?;
    for row in stmt.query_map([], template_from_row)? {
        out.push(row?);
    }
    Ok(out)
}

/// A visible template by id; a hidden built-in counts as absent.
pub fn get_agent_template(
    conn: &Connection,
    id: &str,
) -> Result<Option<AgentTemplate>, StoreError> {
    if id.starts_with(BUILTIN_PREFIX) {
        if hidden_ids(conn)?.iter().any(|h| h == id) {
            return Ok(None);
        }
        return Ok(builtin_agent_templates().into_iter().find(|t| t.id == id));
    }
    Ok(conn
        .query_row(
            "SELECT * FROM agent_templates WHERE id=?",
            [id],
            template_from_row,
        )
        .optional()?)
}

/// Saves a new user template under a fresh id.
pub fn insert_agent_template(
    conn: &Connection,
    id: &str,
    fields: &AgentTemplateFields,
) -> Result<AgentTemplate, StoreError> {
    debug_assert!(!id.starts_with(BUILTIN_PREFIX));
    let now = unix_ms();
    conn.execute(
        "INSERT INTO agent_templates (id,name,description,strategy,timeframe,interval_sec,\
         execution_mode,risk,use_news,allow_trailing_stop,created_at,updated_at) \
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            id,
            fields.name,
            fields.description,
            fields.strategy,
            fields.timeframe,
            fields.interval_sec,
            fields.execution_mode,
            serde_json::to_string(&fields.risk)?,
            i64::from(fields.use_news),
            i64::from(fields.allow_trailing_stop),
            now,
            now,
        ],
    )?;
    get_agent_template(conn, id)?.ok_or(StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
}

/// Replaces a user template's fields. Returns None when no user template has that id; built-ins
/// are never updated here.
pub fn update_agent_template(
    conn: &Connection,
    id: &str,
    fields: &AgentTemplateFields,
) -> Result<Option<AgentTemplate>, StoreError> {
    let changed = conn.execute(
        "UPDATE agent_templates SET name=?,description=?,strategy=?,timeframe=?,interval_sec=?,\
         execution_mode=?,risk=?,use_news=?,allow_trailing_stop=?,updated_at=? WHERE id=?",
        params![
            fields.name,
            fields.description,
            fields.strategy,
            fields.timeframe,
            fields.interval_sec,
            fields.execution_mode,
            serde_json::to_string(&fields.risk)?,
            i64::from(fields.use_news),
            i64::from(fields.allow_trailing_stop),
            unix_ms(),
            id,
        ],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get_agent_template(conn, id)
}

/// Deletes a user template, or hides a built-in. False when no visible template has that id.
pub fn delete_agent_template(conn: &Connection, id: &str) -> Result<bool, StoreError> {
    if id.starts_with(BUILTIN_PREFIX) {
        if get_agent_template(conn, id)?.is_none() {
            return Ok(false);
        }
        conn.execute(
            "INSERT OR IGNORE INTO agent_template_hidden (id) VALUES (?)",
            [id],
        )?;
        return Ok(true);
    }
    Ok(conn.execute("DELETE FROM agent_templates WHERE id=?", [id])? > 0)
}

/// Shows every hidden built-in again; returns how many came back. User templates are untouched.
pub fn restore_builtin_agent_templates(conn: &Connection) -> Result<usize, StoreError> {
    Ok(conn.execute("DELETE FROM agent_template_hidden", [])?)
}
