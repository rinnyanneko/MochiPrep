//! One-time import of an existing MochiPrep data folder (for example the Node backend's) into the
//! launcher's data dir.

use rusqlite::{
    backup::{Backup, StepResult},
    Connection, OpenFlags,
};
use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

const DB_NAME: &str = "mochiprep.db";
const PARTIAL_NAME: &str = "mochiprep.db.importing";
/// SQLite restarts a backup whenever another connection commits to the source between steps, so
/// a source a running backend keeps writing to may never finish copying. The import window stays
/// locked while a copy runs, so the copy gives up instead of holding it forever.
const COPY_DEADLINE: Duration = Duration::from_secs(60);
const PAGES_PER_STEP: i32 = 256;
/// The pause after a busy or locked step, so a source held by a writer is not polled hot.
const BUSY_PAUSE: Duration = Duration::from_millis(50);

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
    import_with(source_dir, target_dir, COPY_DEADLINE, publish)
}

/// Gives the complete partial file its final name without ever replacing a file already there.
/// `rename` would replace one (on Windows too, as std calls it with REPLACE_EXISTING), and a
/// database created after the check above is the owner's data. A hard link fails instead when
/// the name is taken; the partial name is then dropped.
fn publish(partial: &Path, final_path: &Path) -> Result<(), ImportError> {
    match std::fs::hard_link(partial, final_path) {
        Ok(()) => {
            // A leftover partial is harmless: the next import removes it before copying.
            let _ = std::fs::remove_file(partial);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(ImportError::TargetExists)
        }
        Err(error) => Err(io(error)),
    }
}

/// `finalize` is the last step (publishing under the final name); it is a parameter so tests
/// can make it fail after the partial file exists and check the cleanup.
fn import_with(
    source_dir: &Path,
    target_dir: &Path,
    deadline: Duration,
    finalize: impl FnOnce(&Path, &Path) -> Result<(), ImportError>,
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
    let result = copy_snapshot(&source_db, &partial, deadline)
        .and_then(|()| finalize(&partial, &final_path));
    if result.is_err() {
        // The side files exist only if the partial was ever opened in WAL mode; remove them too.
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(target_dir.join(format!("{PARTIAL_NAME}{suffix}")));
        }
    }
    result
}

fn copy_snapshot(source_db: &Path, partial: &Path, deadline: Duration) -> Result<(), ImportError> {
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
    let started = Instant::now();
    loop {
        match backup.step(PAGES_PER_STEP).map_err(io)? {
            StepResult::Done => return Ok(()),
            StepResult::More => {}
            StepResult::Busy | StepResult::Locked => std::thread::sleep(BUSY_PAUSE),
            // Not a variant rusqlite 0.37 returns; treated as "not finished yet".
            _ => std::thread::sleep(BUSY_PAUSE),
        }
        if started.elapsed() >= deadline {
            return Err(ImportError::Io(format!(
                "the source database is busy: it kept changing for {} s, so its copy never                  finished. Close whatever is using it (such as another MochiPrep) and try again.",
                deadline.as_secs()
            )));
        }
    }
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
        let r = import_with(src.path(), dst.path(), COPY_DEADLINE, |from, _| {
            assert!(from.exists());
            Err(ImportError::Io("boom".into()))
        });
        assert_eq!(r, Err(ImportError::Io("boom".into())));
        assert!(!dst.path().join("mochiprep.db").exists());
        let left: Vec<_> = std::fs::read_dir(dst.path()).unwrap().collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }

    /// A database that appears in the target while the copy runs (a backend started meanwhile)
    /// is kept, and the import reports that data exists instead of replacing it.
    #[test]
    fn never_replaces_a_database_created_during_the_copy() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        drop(make_source(src.path()));
        let r = import_with(src.path(), dst.path(), COPY_DEADLINE, |from, to| {
            std::fs::write(to, b"created meanwhile").unwrap();
            publish(from, to)
        });
        assert_eq!(r, Err(ImportError::TargetExists));
        assert_eq!(
            std::fs::read(dst.path().join("mochiprep.db")).unwrap(),
            b"created meanwhile"
        );
        assert!(!dst.path().join("mochiprep.db.importing").exists());
    }

    /// A copy that cannot finish in time (here: a zero deadline and more pages than one step
    /// copies) gives up with an error and removes its partial file.
    #[test]
    fn a_copy_past_its_deadline_gives_up_and_cleans_up() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let c = make_source(src.path());
        c.execute("CREATE TABLE filler (b BLOB); ", []).unwrap();
        // Well over PAGES_PER_STEP pages, so the first step cannot finish the copy.
        c.execute("INSERT INTO filler VALUES (zeroblob(4 * 1024 * 1024))", [])
            .unwrap();
        drop(c);
        let r = import_with(src.path(), dst.path(), Duration::ZERO, |_, _| {
            panic!("an unfinished copy must not be published")
        });
        match r {
            Err(ImportError::Io(message)) => assert!(message.contains("busy"), "{message}"),
            other => panic!("expected a busy error, got {other:?}"),
        }
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
