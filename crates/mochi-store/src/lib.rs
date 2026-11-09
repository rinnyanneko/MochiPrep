//! SQLite store shared with the Node backend: same pragmas, same migration files, same
//! `schema_migrations` bookkeeping, so either backend can open a database the other created.

mod paper;
pub mod rows;
mod settings;

pub use paper::SqlitePaperStore;
pub use settings::{
    delete_instrument, get_setting, has_vault, list_instruments, save_instrument, set_setting,
};

use rusqlite::Connection;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The Node backend's migration files, embedded so the Rust binary needs no files at runtime.
/// `migration_list_matches_directory` fails if a file is added without being listed here.
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "001_init.sql",
        include_str!("../../../apps/server/src/db/migrations/001_init.sql"),
    ),
    (
        "002_live_and_keys.sql",
        include_str!("../../../apps/server/src/db/migrations/002_live_and_keys.sql"),
    ),
    (
        "003_instruments.sql",
        include_str!("../../../apps/server/src/db/migrations/003_instruments.sql"),
    ),
    (
        "004_execution_mode.sql",
        include_str!("../../../apps/server/src/db/migrations/004_execution_mode.sql"),
    ),
    (
        "005_provider_headers.sql",
        include_str!("../../../apps/server/src/db/migrations/005_provider_headers.sql"),
    ),
    (
        "006_agent_news.sql",
        include_str!("../../../apps/server/src/db/migrations/006_agent_news.sql"),
    ),
    (
        "007_vault.sql",
        include_str!("../../../apps/server/src/db/migrations/007_vault.sql"),
    ),
    (
        "008_trailing_stop.sql",
        include_str!("../../../apps/server/src/db/migrations/008_trailing_stop.sql"),
    ),
];

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A stored JSON value that does not parse as the type asked for, or a value that cannot be
    /// written as JSON.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("migration {name} failed: {source}")]
    Migration {
        name: String,
        source: rusqlite::Error,
    },
    /// Another connection held a read, so the WAL could not be truncated and the old page
    /// images, possibly plaintext secrets, are still in it. Retry once that reader is done.
    #[error(
        "WAL checkpoint was blocked by another connection; deleted data may remain in the WAL"
    )]
    CheckpointBlocked,
}

// secure_delete zeroes freed content, so a key removed when sealing does not linger in free pages.
const PRAGMAS: &str = "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; \
                       PRAGMA synchronous = NORMAL; PRAGMA secure_delete = ON;";

/// Opens the database, restricts its permissions and applies pending migrations in order.
pub fn open_database(path: &Path) -> Result<Connection, StoreError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        create_private_dir(dir)?;
    }
    // Create the file 0600 before SQLite does: SQLite gives -wal/-shm the main file's mode, so
    // nothing is ever readable by other users, not even between creation and chmod.
    create_private_file(path)?;
    let conn = Connection::open(path)?;
    conn.execute_batch(PRAGMAS)?;
    restrict_files(path)?;
    migrate_with(&conn, MIGRATIONS)?;
    Ok(conn)
}

/// A migrated in-memory database, for tests.
pub fn open_in_memory() -> Result<Connection, StoreError> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(PRAGMAS)?;
    migrate_with(&conn, MIGRATIONS)?;
    Ok(conn)
}

/// Folds the WAL into the main file and truncates it, which makes deleted plaintext actually
/// gone: until then old page images still sit in the WAL. Callers only call it outside a
/// transaction, since a checkpoint cannot truncate while one is open.
pub fn scrub(conn: &Connection) -> Result<(), StoreError> {
    // The pragma reports (busy, log, checkpointed) as a row instead of failing: busy = 1 means
    // a reader stopped the truncation, and claiming success then would hide lingering plaintext.
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
    if busy != 0 {
        return Err(StoreError::CheckpointBlocked);
    }
    Ok(())
}

/// Applies every migration not yet recorded in `schema_migrations`. Each runs in its own
/// transaction together with its bookkeeping row, so a failure leaves no partial schema.
#[doc(hidden)]
pub fn migrate_with(conn: &Connection, migrations: &[(&str, &str)]) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations \
         (name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL)",
    )?;
    for (name, sql) in migrations {
        let already: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE name = ?1)",
            [name],
            |r| r.get(0),
        )?;
        if already {
            continue;
        }
        conn.execute_batch("BEGIN")?;
        if let Err(source) = apply(conn, name, sql) {
            // The original error matters more than a failed rollback.
            let _ = conn.execute_batch("ROLLBACK");
            return Err(StoreError::Migration {
                name: (*name).to_string(),
                source,
            });
        }
    }
    Ok(())
}

fn apply(conn: &Connection, name: &str, sql: &str) -> rusqlite::Result<()> {
    conn.execute_batch(sql)?;
    conn.execute(
        "INSERT INTO schema_migrations (name, applied_at) VALUES (?1, ?2)",
        (name, unix_ms()),
    )?;
    conn.execute_batch("COMMIT")
}

pub(crate) fn unix_ms() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before 1970");
    i64::try_from(since_epoch.as_millis()).expect("timestamp fits in i64")
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::DirBuilderExt;
    // Mode only applies to directories this call creates; an existing one is left alone.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(dir)?;
    Ok(())
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::OpenOptionsExt;
    // No truncate: an existing database is opened and left as it is; mode only applies on creation.
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_file(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(unix)]
fn restrict_files(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let file = std::path::PathBuf::from(file);
        if file.exists() {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_files(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}
