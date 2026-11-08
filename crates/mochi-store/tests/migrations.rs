use mochi_store::{migrate_with, open_database, open_in_memory, scrub, StoreError, MIGRATIONS};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;

fn golden_path(name: &str) -> String {
    format!("{}/../golden/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

fn migration_names(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM schema_migrations ORDER BY name")
        .unwrap();
    let rows = stmt.query_map([], |r| r.get(0)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

#[test]
fn fresh_schema_matches_node() {
    let conn = open_in_memory().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT type, name, tbl_name, sql FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .unwrap();
    let objects: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "type": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "tbl_name": r.get::<_, String>(2)?,
                "sql": r.get::<_, Option<String>>(3)?,
            }))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let actual = json!({ "objects": objects, "migrations": migration_names(&conn) });
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(golden_path("schema.json")).unwrap())
            .unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn migration_list_matches_directory() {
    let dir = format!(
        "{}/../../apps/server/src/db/migrations",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut files: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".sql"))
        .collect();
    files.sort();
    let listed: Vec<&str> = MIGRATIONS.iter().map(|(name, _)| *name).collect();
    assert_eq!(listed, files);
}

#[test]
fn opens_node_database_without_reapplying() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.sqlite");
    std::fs::copy(golden_path("node-created.sqlite"), &path).unwrap();

    let applied_before = {
        let raw = Connection::open(&path).unwrap();
        applied_at(&raw)
    };
    let conn = open_database(&path).unwrap();
    assert_eq!(applied_at(&conn).len(), 7);
    assert_eq!(applied_at(&conn), applied_before);
    let name: String = conn
        .query_row(
            "SELECT name FROM accounts WHERE id = 'acct_golden'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(name, "Golden");
}

fn applied_at(conn: &Connection) -> Vec<(String, i64)> {
    let mut stmt = conn
        .prepare("SELECT name, applied_at FROM schema_migrations ORDER BY name")
        .unwrap();
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

#[test]
fn older_schema_gets_only_the_new_migration() {
    let conn = open_in_memory_bare();
    migrate_with(&conn, &MIGRATIONS[..6]).unwrap();
    conn.execute_batch(
        "INSERT INTO providers (id, name, kind, default_model, created_at) \
         VALUES ('p1', 'P', 'mock', 'm', 1)",
    )
    .unwrap();
    let before = migration_names(&conn);
    assert_eq!(before.len(), 6);

    migrate_with(&conn, MIGRATIONS).unwrap();
    let after = migration_names(&conn);
    let added: Vec<&String> = after.iter().filter(|n| !before.contains(n)).collect();
    assert_eq!(added, vec!["007_vault.sql"]);
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM providers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);
}

/// A connection with no migrations run, unlike `open_in_memory`.
fn open_in_memory_bare() -> Connection {
    Connection::open_in_memory().unwrap()
}

#[test]
fn failed_migration_leaves_nothing() {
    let conn = open_in_memory_bare();
    let result = migrate_with(
        &conn,
        &[("999_bad.sql", "CREATE TABLE t(x); SELECT * FROM nope;")],
    );
    match result {
        Err(StoreError::Migration { name, .. }) => assert_eq!(name, "999_bad.sql"),
        other => panic!("expected a Migration error, got {other:?}"),
    }
    // A rollback that left the transaction open would wedge every later write.
    assert!(conn.is_autocommit());
    assert!(!table_exists(&conn, "t"));
    assert!(migration_names(&conn).is_empty());
}

fn pragma_i64(conn: &Connection, name: &str) -> i64 {
    conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn pragmas_are_set() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open_database(&dir.path().join("sub").join("db.sqlite")).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    assert_eq!(pragma_i64(&conn, "foreign_keys"), 1);
    assert_eq!(pragma_i64(&conn, "secure_delete"), 1);
}

#[test]
fn scrub_empties_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    let conn = open_database(&path).unwrap();
    conn.execute_batch(
        "INSERT INTO providers (id, name, kind, default_model, created_at) \
         VALUES ('p1', 'P', 'mock', 'm', 1)",
    )
    .unwrap();
    let wal = wal_path(&path);
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    scrub(&conn).unwrap();
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
}

#[test]
fn scrub_reports_a_blocked_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    let conn = open_database(&path).unwrap();
    // No waiting: the test wants the busy answer, not a five-second retry.
    conn.busy_timeout(std::time::Duration::ZERO).unwrap();
    let insert = |id: &str| {
        conn.execute(
            "INSERT INTO providers (id, name, kind, default_model, created_at) \
             VALUES (?1, 'P', 'mock', 'm', 1)",
            [id],
        )
        .unwrap();
    };
    insert("p1");
    // A reader holding a snapshot pins the WAL, so it cannot be truncated.
    let reader = Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM providers", [], |r| r.get(0))
        .unwrap();
    insert("p2");
    assert!(matches!(scrub(&conn), Err(StoreError::CheckpointBlocked)));
    reader.execute_batch("COMMIT").unwrap();
    scrub(&conn).unwrap();
    assert_eq!(std::fs::metadata(wal_path(&path)).unwrap().len(), 0);
}

fn wal_path(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push("-wal");
    s.into()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
fn sibling(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    s.into()
}

#[cfg(unix)]
#[test]
fn new_database_files_are_private() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    let _conn = open_database(&path).unwrap();
    for file in [path.clone(), sibling(&path, "-wal"), sibling(&path, "-shm")] {
        assert_eq!(mode(&file), 0o600, "{}", file.display());
    }
}

// open_database relies on this: by pre-creating the main file 0600 it never has to chmod a
// -wal or -shm file that was briefly readable, because SQLite copies the main file's mode.
#[cfg(unix)]
#[test]
fn sqlite_gives_wal_and_shm_the_main_file_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    drop(open_database(&path).unwrap()); // closing removes -wal and -shm
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE IF NOT EXISTS t (x)")
        .unwrap();
    assert_eq!(mode(&sibling(&path, "-wal")), 0o640);
    assert_eq!(mode(&sibling(&path, "-shm")), 0o640);
}
