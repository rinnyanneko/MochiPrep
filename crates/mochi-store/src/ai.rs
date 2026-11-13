//! Internal AI persistence. Secret-bearing configuration and keys never enter provider views.
use crate::{
    rows::{agent_from_row, Agent},
    StoreError,
};
use mochi_ai::ProviderConfig;
use rusqlite::{params, types::Type, Connection, OptionalExtension, Row};
use serde_json::{json, Value};
use std::collections::HashMap;

pub fn get_agent(conn: &Connection, id: &str) -> Result<Option<Agent>, StoreError> {
    Ok(conn
        .query_row("SELECT * FROM agents WHERE id=?", [id], agent_from_row)
        .optional()?)
}
pub fn insert_agent(conn: &Connection, a: &Agent) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO agents (id,name,provider_id,model,symbol,timeframe,strategy,account_id,risk,
            interval_sec,execution_mode,use_news,allow_trailing_stop,status,running,next_run_at,
            last_run_at,status_detail,consecutive_errors,is_demo,created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            a.id,
            a.name,
            a.provider_id,
            a.model,
            a.symbol,
            a.timeframe,
            a.strategy,
            a.account_id,
            a.risk.get(),
            a.interval_sec,
            a.execution_mode,
            a.use_news,
            a.allow_trailing_stop,
            a.status,
            a.running,
            a.next_run_at,
            a.last_run_at,
            a.status_detail,
            a.consecutive_errors,
            a.is_demo,
            a.created_at
        ],
    )?;
    Ok(())
}
/// Creation time and demo ownership are immutable, matching Node's UPDATE.
pub fn save_agent(conn: &Connection, a: &Agent) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE agents SET name=?,provider_id=?,model=?,symbol=?,timeframe=?,strategy=?,
            account_id=?,risk=?,interval_sec=?,execution_mode=?,use_news=?,allow_trailing_stop=?,
            status=?,running=?,next_run_at=?,last_run_at=?,status_detail=?,consecutive_errors=?
         WHERE id=?",
        params![
            a.name,
            a.provider_id,
            a.model,
            a.symbol,
            a.timeframe,
            a.strategy,
            a.account_id,
            a.risk.get(),
            a.interval_sec,
            a.execution_mode,
            a.use_news,
            a.allow_trailing_stop,
            a.status,
            a.running,
            a.next_run_at,
            a.last_run_at,
            a.status_detail,
            a.consecutive_errors,
            a.id
        ],
    )?;
    Ok(())
}
pub fn delete_agent(conn: &Connection, id: &str) -> Result<(), StoreError> {
    grouped_delete(conn, |conn| {
        conn.execute("DELETE FROM decisions WHERE agent_id=?", [id])?;
        conn.execute("DELETE FROM agents WHERE id=?", [id])?;
        Ok(())
    })
}
fn provider_from_row(row: &Row<'_>) -> rusqlite::Result<ProviderConfig> {
    let kind: String = row.get("kind")?;
    let headers: String = row.get("extra_headers")?;
    let parse_error =
        |error| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error));
    Ok(ProviderConfig {
        id: row.get("id")?,
        name: row.get("name")?,
        kind: serde_json::from_value(json!(kind)).map_err(parse_error)?,
        base_url: row.get("base_url")?,
        default_model: row.get("default_model")?,
        input_price_per_m_tok: row.get("input_price")?,
        output_price_per_m_tok: row.get("output_price")?,
        extra_headers: Some(serde_json::from_str(&headers).map_err(parse_error)?),
        created_at: row.get("created_at")?,
    })
}
pub fn get_provider(conn: &Connection, id: &str) -> Result<Option<ProviderConfig>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT * FROM providers WHERE id=?",
            [id],
            provider_from_row,
        )
        .optional()?)
}
pub fn list_providers(conn: &Connection) -> Result<Vec<ProviderConfig>, StoreError> {
    let mut stmt = conn.prepare("SELECT * FROM providers ORDER BY created_at,id")?;
    let rows = stmt.query_map([], provider_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}
pub fn insert_provider(conn: &Connection, p: &ProviderConfig) -> Result<(), StoreError> {
    let kind = serde_json::to_value(p.kind)?;
    let headers = serde_json::to_string(&p.extra_headers.as_ref().cloned().unwrap_or_default())?;
    conn.execute(
        "INSERT INTO providers (id,name,kind,base_url,default_model,input_price,output_price,
            extra_headers,created_at) VALUES (?,?,?,?,?,?,?,?,?)",
        params![
            p.id,
            p.name,
            kind.as_str(),
            p.base_url,
            p.default_model,
            p.input_price_per_m_tok,
            p.output_price_per_m_tok,
            headers,
            p.created_at
        ],
    )?;
    Ok(())
}
pub fn save_provider(conn: &Connection, p: &ProviderConfig) -> Result<(), StoreError> {
    let kind = serde_json::to_value(p.kind)?;
    let headers = serde_json::to_string(&p.extra_headers.as_ref().cloned().unwrap_or_default())?;
    conn.execute(
        "UPDATE providers SET name=?,kind=?,base_url=?,default_model=?,input_price=?,
            output_price=?,extra_headers=? WHERE id=?",
        params![
            p.name,
            kind.as_str(),
            p.base_url,
            p.default_model,
            p.input_price_per_m_tok,
            p.output_price_per_m_tok,
            headers,
            p.id
        ],
    )?;
    Ok(())
}
/// SQLite cascades both plaintext and sealed provider keys and refuses referenced providers.
pub fn delete_provider(conn: &Connection, id: &str) -> Result<(), StoreError> {
    conn.execute("DELETE FROM providers WHERE id=?", [id])?;
    Ok(())
}
/// Backend-only: these plaintext values must never enter API views or logs.
pub fn get_provider_keys(conn: &Connection) -> Result<HashMap<String, String>, StoreError> {
    let mut stmt = conn.prepare("SELECT provider_id,api_key FROM provider_keys")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}
pub fn save_provider_key(
    conn: &Connection,
    id: &str,
    key: &str,
    updated_at: i64,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO provider_keys (provider_id,api_key,updated_at) VALUES (?,?,?)
         ON CONFLICT(provider_id) DO UPDATE SET api_key=excluded.api_key,updated_at=excluded.updated_at",
        params![id,key,updated_at],
    )?;
    Ok(())
}
/// Delete both storage formats atomically; either may exist after a vault transition.
pub fn delete_provider_key(conn: &Connection, id: &str) -> Result<(), StoreError> {
    grouped_delete(conn, |conn| {
        conn.execute("DELETE FROM provider_keys WHERE provider_id=?", [id])?;
        conn.execute("DELETE FROM sealed_provider_keys WHERE provider_id=?", [id])?;
        Ok(())
    })
}

// Deserialize only the indexed fields, requiring a complete record before any SQL mutation.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DecisionIndex {
    id: String,
    agent_id: String,
    account_id: String,
    ts: i64,
    outcome: String,
}
pub fn insert_decision(conn: &Connection, record: &Value) -> Result<(), StoreError> {
    let i: DecisionIndex = serde_json::from_value(record.clone())?;
    conn.execute(
        "INSERT INTO decisions (id,agent_id,account_id,ts,outcome,body) VALUES (?,?,?,?,?,?)",
        params![
            i.id,
            i.agent_id,
            i.account_id,
            i.ts,
            i.outcome,
            serde_json::to_string(record)?
        ],
    )?;
    Ok(())
}
/// Node updates outcome/body only; ownership and sorting timestamp remain immutable indexes.
pub fn save_decision(conn: &Connection, record: &Value) -> Result<(), StoreError> {
    let i: DecisionIndex = serde_json::from_value(record.clone())?;
    conn.execute(
        "UPDATE decisions SET outcome=?,body=? WHERE id=?",
        params![i.outcome, serde_json::to_string(record)?, i.id],
    )?;
    Ok(())
}
pub fn get_decision(conn: &Connection, id: &str) -> Result<Option<Value>, StoreError> {
    let body: Option<String> = conn
        .query_row("SELECT body FROM decisions WHERE id=?", [id], |r| r.get(0))
        .optional()?;
    Ok(body.map(|v| serde_json::from_str(&v)).transpose()?)
}

// SAVEPOINT commits standalone groups while preserving an enclosing runtime transaction.
fn grouped_delete(
    conn: &Connection,
    delete: impl FnOnce(&Connection) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    conn.execute_batch("SAVEPOINT mochi_ai_delete")?;
    let mut guard = DeleteSavepoint {
        conn,
        finished: false,
    };
    delete(conn)?;
    conn.execute_batch("RELEASE mochi_ai_delete")?;
    guard.finished = true;
    Ok(())
}

struct DeleteSavepoint<'a> {
    conn: &'a Connection,
    finished: bool,
}

impl Drop for DeleteSavepoint<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // On failure or panic, roll back only this innermost group and keep its caller usable.
            let _ = self
                .conn
                .execute_batch("ROLLBACK TO mochi_ai_delete; RELEASE mochi_ai_delete");
        }
    }
}
