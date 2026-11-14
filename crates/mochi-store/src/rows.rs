//! The tables later plans own (agents, AI providers, decisions, saved OKX keys), read as the
//! Node store reads them (`apps/server/src/db/store.ts`), so the server can answer the web app's
//! refetches with what the database really holds.
//!
//! Internal configuration/key access and writes are re-exported from `ai`; browser-facing
//! provider views below deliberately omit all credential values.

use mochi_domain::jsnum::js_number_raw;
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

pub use crate::ai::*;
use serde_json::value::RawValue;
use serde_json::Value;

use crate::StoreError;

/// The built-in provider every database has (`MOCK_PROVIDER_ID`).
pub const MOCK_PROVIDER_ID: &str = "mock";

/// An agent as `toAgent` maps its row, keys in that order. `risk` is the stored JSON as written:
/// Node parses and re-serialises the text it wrote with `JSON.stringify`, which gives it back.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub provider_id: String,
    pub model: String,
    pub symbol: String,
    pub timeframe: String,
    pub strategy: String,
    pub account_id: String,
    pub risk: Box<RawValue>,
    pub interval_sec: i64,
    pub execution_mode: String,
    pub use_news: bool,
    pub allow_trailing_stop: bool,
    pub status: String,
    pub running: bool,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub status_detail: Option<String>,
    pub consecutive_errors: i64,
    pub is_demo: bool,
    pub created_at: i64,
}

/// `providerViews()`: the provider config without its extra-header values (they can hold
/// credentials), plus the header names and whether a key is saved. Keys in Node's order.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub base_url: Option<String>,
    pub default_model: String,
    /// REAL columns, written as JS writes the number (`10`, not `10.0`).
    pub input_price_per_m_tok: Option<Box<RawValue>>,
    pub output_price_per_m_tok: Option<Box<RawValue>>,
    pub created_at: i64,
    pub header_names: Vec<String>,
    pub requires_key: bool,
    pub has_key: bool,
}

/// `decisionStats(agentId)`.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionStats {
    pub count: usize,
    /// The JS-number sum of every `estimatedCost` that is not null; `None` when all are null.
    pub estimated_cost: Option<f64>,
}

/// Decode an agent row using the persisted JSON contracts shared with Node.
pub(crate) fn agent_from_row(row: &Row<'_>) -> rusqlite::Result<Agent> {
    let risk = RawValue::from_string(row.get("risk")?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
    })?;
    Ok(Agent {
        id: row.get("id")?,
        name: row.get("name")?,
        provider_id: row.get("provider_id")?,
        model: row.get("model")?,
        symbol: row.get("symbol")?,
        timeframe: row.get("timeframe")?,
        strategy: row.get("strategy")?,
        account_id: row.get("account_id")?,
        risk,
        interval_sec: row.get("interval_sec")?,
        execution_mode: row.get("execution_mode")?,
        use_news: row.get::<_, i64>("use_news")? != 0,
        allow_trailing_stop: row.get::<_, i64>("allow_trailing_stop")? != 0,
        status: row.get("status")?,
        running: row.get::<_, i64>("running")? != 0,
        next_run_at: row.get("next_run_at")?,
        last_run_at: row.get("last_run_at")?,
        status_detail: row.get("status_detail")?,
        consecutive_errors: row.get("consecutive_errors")?,
        is_demo: row.get::<_, i64>("is_demo")? != 0,
        created_at: row.get("created_at")?,
    })
}

/// `listAgents()`: oldest first.
pub fn list_agents(conn: &Connection) -> Result<Vec<Agent>, StoreError> {
    let mut stmt = conn.prepare("SELECT * FROM agents ORDER BY created_at, id")?;
    let agents = stmt.query_map([], agent_from_row)?;
    Ok(agents.collect::<Result<_, _>>()?)
}

/// `providerViews()`. `has_vault` decides what "has a key" means: Node loads plaintext keys at
/// start only when no master password is set, so with a vault only the mock provider (which
/// needs none) has a key here. While unlocked the runtime marks the keys it has loaded.
pub fn list_provider_views(
    conn: &Connection,
    has_vault: bool,
) -> Result<Vec<ProviderView>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, p.kind, p.base_url, p.default_model, p.input_price, p.output_price,
                p.extra_headers, p.created_at,
                EXISTS(SELECT 1 FROM provider_keys k WHERE k.provider_id = p.id) AS has_plain_key
         FROM providers p ORDER BY p.created_at, p.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>("id")?,
            row.get::<_, String>("name")?,
            row.get::<_, String>("kind")?,
            row.get::<_, Option<String>>("base_url")?,
            row.get::<_, String>("default_model")?,
            row.get::<_, Option<f64>>("input_price")?,
            row.get::<_, Option<f64>>("output_price")?,
            row.get::<_, String>("extra_headers")?,
            row.get::<_, i64>("created_at")?,
            row.get::<_, bool>("has_plain_key")?,
        ))
    })?;
    let mut views = Vec::new();
    for row in rows {
        let (id, name, kind, base_url, default_model, input, output, headers, created_at, plain) =
            row?;
        let is_mock = kind == "MOCK";
        views.push(ProviderView {
            header_names: header_names(&headers)?,
            requires_key: !is_mock,
            has_key: is_mock || (plain && !has_vault),
            id,
            name,
            kind,
            base_url,
            default_model,
            input_price_per_m_tok: input.map(js_number_raw),
            output_price_per_m_tok: output.map(js_number_raw),
            created_at,
        });
    }
    Ok(views)
}

/// `Object.keys(extraHeaders)`, in the order they were saved. (JS would list integer-like names
/// first; header names are tokens the schema lowercases, and none of the ones gateways use are
/// numbers.)
fn header_names(json: &str) -> Result<Vec<String>, StoreError> {
    let headers: serde_json::Map<String, Value> = serde_json::from_str(json)?;
    Ok(headers.keys().cloned().collect())
}

/// Each `body` is the record Node wrote with `JSON.stringify`, so it goes out as it is.
fn bodies(
    stmt: &mut rusqlite::Statement<'_>,
    params: impl rusqlite::Params,
) -> Result<Vec<Box<RawValue>>, StoreError> {
    let texts = stmt.query_map(params, |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for text in texts {
        out.push(RawValue::from_string(text?)?);
    }
    Ok(out)
}

/// `listPendingSignals()`: suggestions waiting for the owner, newest first.
pub fn list_pending_signals(conn: &Connection) -> Result<Vec<Box<RawValue>>, StoreError> {
    let mut stmt =
        conn.prepare("SELECT body FROM decisions WHERE outcome='SIGNAL' ORDER BY ts DESC")?;
    bodies(&mut stmt, [])
}

/// `listDecisions({ agentId?, limit })`: newest first.
pub fn list_decisions(
    conn: &Connection,
    agent_id: Option<&str>,
    limit: usize,
) -> Result<Vec<Box<RawValue>>, StoreError> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    match agent_id {
        Some(agent_id) => {
            let mut stmt = conn.prepare(
                "SELECT body FROM decisions WHERE agent_id=? ORDER BY ts DESC, rowid DESC LIMIT ?",
            )?;
            bodies(&mut stmt, rusqlite::params![agent_id, limit])
        }
        None => {
            let mut stmt =
                conn.prepare("SELECT body FROM decisions ORDER BY ts DESC, rowid DESC LIMIT ?")?;
            bodies(&mut stmt, [limit])
        }
    }
}

/// `decisionStats(agentId)`: how many decisions, and their summed cost as JS sums it
/// (`cost += Number(estimatedCost)`, in floating point, as the arena shows it).
pub fn decision_stats(conn: &Connection, agent_id: &str) -> Result<DecisionStats, StoreError> {
    let mut stmt = conn.prepare("SELECT body FROM decisions WHERE agent_id=?")?;
    let texts = stmt.query_map([agent_id], |row| row.get::<_, String>(0))?;
    let mut count = 0;
    let mut cost: Option<f64> = None;
    for text in texts {
        count += 1;
        let record: Value = serde_json::from_str(&text?)?;
        // Node skips only an explicit null; anything else goes through Number(), so a missing
        // cost is NaN there too (records always have the key).
        let value = match record.get("estimatedCost") {
            Some(Value::Null) => continue,
            Some(Value::String(s)) => js_number_from_string(s),
            Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
            _ => f64::NAN,
        };
        cost = Some(cost.unwrap_or(0.0) + value);
    }
    Ok(DecisionStats {
        count,
        estimated_cost: cost,
    })
}

/// Stored costs use Node's `Number(text)` rules, including JS whitespace and radix prefixes.
fn js_number_from_string(text: &str) -> f64 {
    mochi_domain::jsnum::js_string_to_number(text)
}

/// `hasSealedExchangeCredentials(accountId)`: the owner saved this account's OKX keys, sealed by
/// the master password. Only the row's presence is read.
pub fn has_sealed_exchange_credentials(
    conn: &Connection,
    account_id: &str,
) -> Result<bool, StoreError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sealed_exchange_credentials WHERE account_id=?",
            [account_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Node's `seedMockProvider`: the offline demo provider, added once if it is missing.
pub fn seed_mock_provider(conn: &Connection, created_at: i64) -> Result<(), StoreError> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM providers WHERE id=?",
            [MOCK_PROVIDER_ID],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO providers (id,name,kind,base_url,default_model,input_price,output_price,extra_headers,created_at)
         VALUES (?,?,?,?,?,?,?,?,?)",
        rusqlite::params![
            MOCK_PROVIDER_ID,
            "Mock AI (offline demo)",
            "MOCK",
            None::<String>,
            // MOCK_MODELS.momentum in packages/ai-providers/src/mock.ts.
            "mock-momentum-v1",
            None::<f64>,
            None::<f64>,
            "{}",
            created_at
        ],
    )?;
    Ok(())
}

/// `runner.disarmAfterRestart()`: agents never survive the process, so one that was scheduled
/// or mid-cycle when it stopped is paused until the owner starts it again. Returns how many.
pub fn pause_agents_after_restart(conn: &Connection) -> Result<usize, StoreError> {
    Ok(conn.execute(
        "UPDATE agents SET running=0, status='PAUSED', next_run_at=NULL,
                status_detail='Paused because the backend restarted'
         WHERE running<>0 OR status IN ('ANALYZING','EXECUTING')",
        [],
    )?)
}
