//! One-time import of an existing MochiPrep data folder (for example the Node backend's) into the
//! launcher's data dir.

use rusqlite::{backup::Backup, Connection, OpenFlags};
use std::fmt;
use std::path::Path;
use std::time::Duration;

const DB_NAME: &str = "mochiprep.db";
const PARTIAL_NAME: &str = "mochiprep.db.importing";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    NoDatabase,
    TargetExists,
    SameFolder,
    NotMochiPrep,
    Io(String),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDatabase => write!(f, "That folder has no mochiprep.db."),
            Self::TargetExists => write!(
                f,
                "MochiPrep already has data here, so nothing was imported."
            ),
            Self::SameFolder => write!(f, "That is MochiPrep's own data folder."),
            Self::NotMochiPrep => write!(f, "mochiprep.db is not a MochiPrep database."),
            Self::Io(e) => write!(f, "Import failed: {e}"),
        }
    }
}

impl std::error::Error for ImportError {}

fn io<E: fmt::Display>(e: E) -> ImportError {
    ImportError::Io(e.to_string())
}

/// Copies `source_dir/mochiprep.db` into `target_dir` as a consistent snapshot. Never overwrites
/// existing data; the new file appears under its final name only once complete.
pub fn import_data(source_dir: &Path, target_dir: &Path) -> Result<(), ImportError> {
    import_with(source_dir, target_dir, |from, to| std::fs::rename(from, to))
}

/// `finalize` is the last step (the rename into place); it is a parameter so tests can make it
/// fail after the partial file exists and check the cleanup.
fn import_with(
    source_dir: &Path,
    target_dir: &Path,
    finalize: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), ImportError> {
    if let (Ok(a), Ok(b)) = (source_dir.canonicalize(), target_dir.canonicalize()) {
        if a == b {
            return Err(ImportError::SameFolder);
        }
    }
    let source_db = source_dir.join(DB_NAME);
    if !source_db.is_file() {
        return Err(ImportError::NoDatabase);
    }
    let final_path = target_dir.join(DB_NAME);
    if final_path.exists() {
        return Err(ImportError::TargetExists);
    }
    create_private_dir(target_dir)?;
    let partial = target_dir.join(PARTIAL_NAME);
    let result = copy_snapshot(&source_db, &partial)
        .and_then(|()| finalize(&partial, &final_path).map_err(io));
    if result.is_err() {
        // The side files exist only if the partial was ever opened in WAL mode; remove them too.
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(target_dir.join(format!("{PARTIAL_NAME}{suffix}")));
        }
    }
    result
}

fn copy_snapshot(source_db: &Path, partial: &Path) -> Result<(), ImportError> {
    // Read-only, so a database the Node backend still has open is never modified; the backup API
    // reads through its WAL for a consistent view.
    let src =
        Connection::open_with_flags(source_db, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(io)?;
    let probe = src.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
        [],
        |_| Ok(()),
    );
    match probe {
        Ok(()) => {}
        Err(rusqlite::Error::QueryReturnedNoRows) => return Err(ImportError::NotMochiPrep),
        // A file that is not SQLite at all is just as foreign as a database without the table;
        // but busy, locked, I/O and permission failures say nothing about what the file is.
        Err(rusqlite::Error::SqliteFailure(e, _))
            if matches!(
                e.code,
                rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt
            ) =>
        {
            return Err(ImportError::NotMochiPrep)
        }
        Err(other) => return Err(io(other)),
    }
    let _ = std::fs::remove_file(partial);
    create_private_file(partial)?;
    let mut dst = Connection::open(partial).map_err(io)?;
    let backup = Backup::new(&src, &mut dst).map_err(io)?;
    backup
        .run_to_completion(256, Duration::from_millis(0), None)
        .map_err(io)?;
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<(), ImportError> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(io)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<(), ImportError> {
    std::fs::create_dir_all(dir).map_err(io)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<(), ImportError> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map(drop)
        .map_err(io)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> Result<(), ImportError> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(drop)
        .map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::Path;

    fn make_source(dir: &Path) -> Connection {
        let c = Connection::open(dir.join("mochiprep.db")).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        c.execute_batch(
            "CREATE TABLE schema_migrations (id TEXT PRIMARY KEY); INSERT INTO schema_migrations VALUES ('001');",
        )
        .unwrap();
        c
    }

    #[test]
    fn copies_a_mochiprep_database() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        // Held open, as the Node backend would: the row lives only in the uncheckpointed WAL.
        let _open = make_source(src.path());
        import_data(src.path(), dst.path()).unwrap();
        let c = Connection::open(dst.path().join("mochiprep.db")).unwrap();
        let id: String = c
            .query_row("SELECT id FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(id, "001");
        assert!(!dst.path().join("mochiprep.db.importing").exists());
    }

    #[test]
    fn creates_the_target_dir() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let target = dst.path().join("new").join("data");
        drop(make_source(src.path()));
        import_data(src.path(), &target).unwrap();
        assert!(target.join("mochiprep.db").exists());
    }

    #[test]
    fn refuses_when_target_has_data() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        drop(make_source(src.path()));
        std::fs::write(dst.path().join("mochiprep.db"), b"existing").unwrap();
        assert_eq!(
            import_data(src.path(), dst.path()),
            Err(ImportError::TargetExists)
        );
        assert_eq!(
            std::fs::read(dst.path().join("mochiprep.db")).unwrap(),
            b"existing"
        );
    }

    #[test]
    fn refuses_a_folder_without_db() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        assert_eq!(
            import_data(src.path(), dst.path()),
            Err(ImportError::NoDatabase)
        );
        assert!(!dst.path().join("mochiprep.db").exists());
    }

    #[test]
    fn refuses_the_target_itself() {
        let dir = tempfile::tempdir().unwrap();
        drop(make_source(dir.path()));
        assert_eq!(
            import_data(dir.path(), dir.path()),
            Err(ImportError::SameFolder)
        );
    }

    #[test]
    fn refuses_a_non_mochiprep_sqlite_file() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let c = Connection::open(src.path().join("mochiprep.db")).unwrap();
        c.execute_batch("CREATE TABLE other (x INTEGER);").unwrap();
        drop(c);
        assert_eq!(
            import_data(src.path(), dst.path()),
            Err(ImportError::NotMochiPrep)
        );
        assert!(!dst.path().join("mochiprep.db").exists());
    }

    #[test]
    fn leaves_no_partial_file_on_failure() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        drop(make_source(src.path()));
        // Fail the final rename, after the partial file is complete.
        let r = import_with(src.path(), dst.path(), |from, _| {
            assert!(from.exists());
            Err(std::io::Error::other("boom"))
        });
        assert_eq!(r, Err(ImportError::Io("boom".into())));
        assert!(!dst.path().join("mochiprep.db").exists());
        let left: Vec<_> = std::fs::read_dir(dst.path()).unwrap().collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }

    #[test]
    fn a_text_file_is_not_a_mochiprep_database() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("mochiprep.db"),
            b"this is not sqlite at all, just text, padded out well past the sqlite header size.....",
        )
        .unwrap();
        assert_eq!(
            import_data(src.path(), dst.path()),
            Err(ImportError::NotMochiPrep)
        );
        assert!(std::fs::read_dir(dst.path()).unwrap().next().is_none());
    }

    #[test]
    fn error_texts() {
        assert_eq!(
            ImportError::NoDatabase.to_string(),
            "That folder has no mochiprep.db."
        );
        assert_eq!(ImportError::Io("x".into()).to_string(), "Import failed: x");
    }
}
