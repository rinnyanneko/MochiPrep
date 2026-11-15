//! Rows of the three master-password tables (`vault_keys`, `sealed_provider_keys`,
//! `sealed_exchange_credentials`). The SQL is `apps/server/src/db/store.ts`'s, since Node opens
//! the same file. Only sealed text and the password wrap are stored here: the wrap holds a
//! wrapped (not plain) data key, and nothing in this module ever sees the master password.
//!
//! Writes that touch more than one statement run in a SAVEPOINT, so they commit on their own
//! yet nest inside a caller's open transaction and never issue a raw BEGIN.

use mochi_vault::PasswordWrap;
use rusqlite::{params, Connection, OptionalExtension};

use crate::StoreError;

/// `getVaultKey`: the stored wrap under `id` (Node only ever writes `"password"`).
pub fn vault_key(conn: &Connection, id: &str) -> Result<Option<PasswordWrap>, StoreError> {
    let text: Option<String> = conn
        .query_row("SELECT params FROM vault_keys WHERE id=?", [id], |r| {
            r.get(0)
        })
        .optional()?;
    match text {
        Some(text) => Ok(Some(serde_json::from_str(&text)?)),
        None => Ok(None),
    }
}

/// `saveVaultKey`: the upsert replaces `params` only, so `kind` and `created_at` stay as first
/// written, exactly as Node's `ON CONFLICT` does.
pub fn save_vault_key(
    conn: &Connection,
    id: &str,
    kind: &str,
    wrap: &PasswordWrap,
    now: i64,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO vault_keys (id,kind,params,created_at) VALUES (?,?,?,?)
         ON CONFLICT(id) DO UPDATE SET params=excluded.params",
        params![id, kind, serde_json::to_string(wrap)?, now],
    )?;
    Ok(())
}

/// `deleteVault`: drops the vault and everything only it could open. Atomic, so a failure never
/// leaves sealed rows behind without the wrap (or the reverse).
pub fn delete_vault(conn: &Connection) -> Result<(), StoreError> {
    grouped(conn, |conn| {
        conn.execute("DELETE FROM sealed_exchange_credentials", [])?;
        conn.execute("DELETE FROM sealed_provider_keys", [])?;
        conn.execute("DELETE FROM vault_keys", [])?;
        Ok(())
    })
}

/// `getSealedProviderKeys`: `(provider id, sealed text)` pairs.
pub fn sealed_provider_keys(conn: &Connection) -> Result<Vec<(String, String)>, StoreError> {
    pairs(conn, "SELECT provider_id, sealed FROM sealed_provider_keys")
}

/// `saveSealedProviderKey`: upserts the sealed key and deletes the plaintext row in the same
/// savepoint, so a key is never stored in both forms and a failed write keeps the plaintext.
pub fn save_sealed_provider_key(
    conn: &Connection,
    provider_id: &str,
    sealed: &str,
    now: i64,
) -> Result<(), StoreError> {
    grouped(conn, |conn| {
        conn.execute(
            "INSERT INTO sealed_provider_keys (provider_id,sealed,updated_at) VALUES (?,?,?)
             ON CONFLICT(provider_id) DO UPDATE SET sealed=excluded.sealed, updated_at=excluded.updated_at",
            params![provider_id, sealed, now],
        )?;
        conn.execute(
            "DELETE FROM provider_keys WHERE provider_id=?",
            [provider_id],
        )?;
        Ok(())
    })
}

/// `listSealedExchangeCredentials`: `(account id, sealed text)` pairs.
pub fn list_sealed_exchange_credentials(
    conn: &Connection,
) -> Result<Vec<(String, String)>, StoreError> {
    pairs(
        conn,
        "SELECT account_id, sealed FROM sealed_exchange_credentials",
    )
}

pub fn save_sealed_exchange_credentials(
    conn: &Connection,
    account_id: &str,
    sealed: &str,
    now: i64,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO sealed_exchange_credentials (account_id,sealed,updated_at) VALUES (?,?,?)
         ON CONFLICT(account_id) DO UPDATE SET sealed=excluded.sealed, updated_at=excluded.updated_at",
        params![account_id, sealed, now],
    )?;
    Ok(())
}

pub fn delete_sealed_exchange_credentials(
    conn: &Connection,
    account_id: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "DELETE FROM sealed_exchange_credentials WHERE account_id=?",
        [account_id],
    )?;
    Ok(())
}

fn pairs(conn: &Connection, sql: &str) -> Result<Vec<(String, String)>, StoreError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Runs `write` in a savepoint: released on success, rolled back on error or panic, leaving an
/// enclosing transaction usable.
fn grouped(
    conn: &Connection,
    write: impl FnOnce(&Connection) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    conn.execute_batch("SAVEPOINT mochi_vault_write")?;
    let mut guard = Savepoint {
        conn,
        finished: false,
    };
    write(conn)?;
    conn.execute_batch("RELEASE mochi_vault_write")?;
    guard.finished = true;
    Ok(())
}

struct Savepoint<'a> {
    conn: &'a Connection,
    finished: bool,
}

impl Drop for Savepoint<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // The original error matters more than a failed rollback.
            let _ = self
                .conn
                .execute_batch("ROLLBACK TO mochi_vault_write; RELEASE mochi_vault_write");
        }
    }
}
