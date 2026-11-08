//! The small tables the server runtime reads outside the paper engine: `settings` (one JSON
//! value per key), the `instruments` the owner added on top of the built-ins, and whether a
//! master password exists. The SQL is `apps/server/src/db/store.ts`'s, since Node opens the same
//! file.

use mochi_domain::InstrumentSpecification;
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::{unix_ms, StoreError};

/// `getSetting`: the value stored under `key`, or `None` when the key was never set. A value of
/// another type than `T` is an error rather than a silent default.
pub fn get_setting<T: DeserializeOwned>(
    conn: &Connection,
    key: &str,
) -> Result<Option<T>, StoreError> {
    let text: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE key=?", [key], |r| {
            r.get(0)
        })
        .optional()?;
    match text {
        Some(text) => Ok(Some(serde_json::from_str(&text)?)),
        None => Ok(None),
    }
}

/// `setSetting`: stores `value` as JSON, replacing any earlier value.
pub fn set_setting<T: Serialize + ?Sized>(
    conn: &Connection,
    key: &str,
    value: &T,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO settings (key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, serde_json::to_string(value)?],
    )?;
    Ok(())
}

/// The added pairs, by symbol.
pub fn list_instruments(conn: &Connection) -> Result<Vec<InstrumentSpecification>, StoreError> {
    let mut stmt = conn.prepare("SELECT spec FROM instruments ORDER BY symbol")?;
    let specs = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for spec in specs {
        out.push(serde_json::from_str(&spec?)?);
    }
    Ok(out)
}

/// Adds a pair, or replaces the spec of one already added (keeping when it was first added).
pub fn save_instrument(
    conn: &Connection,
    spec: &InstrumentSpecification,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO instruments (symbol,spec,added_at) VALUES (?,?,?)
         ON CONFLICT(symbol) DO UPDATE SET spec=excluded.spec",
        params![spec.symbol, serde_json::to_string(spec)?, unix_ms()],
    )?;
    Ok(())
}

pub fn delete_instrument(conn: &Connection, symbol: &str) -> Result<(), StoreError> {
    conn.execute("DELETE FROM instruments WHERE symbol=?", [symbol])?;
    Ok(())
}

/// `hasVault`: a master password is set when `vault_keys` has any row.
pub fn has_vault(conn: &Connection) -> Result<bool, StoreError> {
    Ok(conn
        .query_row("SELECT 1 FROM vault_keys LIMIT 1", [], |_| Ok(()))
        .optional()?
        .is_some())
}
