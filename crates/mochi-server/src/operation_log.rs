//! Local operation records. Callers supply route templates or application-defined event names,
//! never input bodies, tokens, provider responses or upstream error messages.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use rusqlite::Connection;
use tokio::sync::{oneshot, Mutex};

pub const SETTING: &str = "operationLogRetention";
pub const DEFAULT_RETENTION: i32 = 10;
const RETENTION_RETRY_DELAY: Duration = Duration::from_secs(60);

pub fn retention(db: &Connection) -> Result<i32, mochi_store::StoreError> {
    Ok(mochi_store::get_setting::<i32>(db, SETTING)?
        .unwrap_or(DEFAULT_RETENTION)
        .max(-1))
}

struct Writer {
    _lease: File,
    dir: PathBuf,
    archive_path: PathBuf,
    latest: File,
    archive: Option<File>,
}

pub struct OperationLog {
    commands: mpsc::Sender<Command>,
    worker: Option<thread::JoinHandle<()>>,
    pub(crate) settings: Mutex<()>,
}

enum Command {
    Record(String),
    Retain(i32, oneshot::Sender<io::Result<()>>),
    Flush(oneshot::Sender<()>),
    #[cfg(feature = "test-seams")]
    Pause(oneshot::Sender<()>, mpsc::Receiver<()>),
    Stop,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

impl OperationLog {
    pub fn open(data_dir: &Path, count: i32) -> io::Result<Self> {
        let dir = data_dir.join("logs");
        fs::create_dir_all(&dir)?;
        if !fs::symlink_metadata(&dir)?.is_dir() {
            return Err(io::Error::other("Expected a local logs directory"));
        }
        let lease = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(".writer.lock"))?;
        lease.try_lock().map_err(io::Error::from)?;
        let timestamp = mochi_okx::iso_string(now()).replace(':', "-");
        let archive_path = dir.join(format!(
            "session-{timestamp}-{}.log",
            uuid::Uuid::new_v4().simple()
        ));
        // Leave the archive outside the retention scan until latest.log is in place.
        let archive = (count != 0)
            .then(|| tempfile::NamedTempFile::new_in(&dir))
            .transpose()?;
        let latest = tempfile::NamedTempFile::new_in(&dir)?
            .persist(dir.join("latest.log"))
            .map_err(|error| error.error)?;
        let archive = archive
            .map(|file| {
                file.persist_noclobber(&archive_path)
                    .map_err(|error| error.error)
            })
            .transpose()?;
        let writer = Writer {
            _lease: lease,
            dir,
            archive_path,
            latest,
            archive,
        };
        let (commands, input) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("operation-log".into())
            .spawn(move || writer.run(input, count))?;
        let log = Self {
            commands,
            worker: Some(worker),
            settings: Mutex::new(()),
        };
        log.record(format_args!("backend started"));
        Ok(log)
    }

    pub(crate) async fn apply_retention(&self, count: i32) -> io::Result<()> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Retain(count, reply))
            .map_err(|_| io::Error::other("Log writer stopped"))?;
        result
            .await
            .map_err(|_| io::Error::other("Log writer stopped"))?
    }

    pub fn record(&self, event: fmt::Arguments<'_>) {
        let line = format!("{} {event}\n", mochi_okx::iso_string(now()));
        if self.commands.send(Command::Record(line)).is_err() {
            tracing::warn!("Operation log writer stopped");
        }
    }

    pub async fn flush(&self) {
        let (reply, done) = oneshot::channel();
        if self.commands.send(Command::Flush(reply)).is_ok() {
            let _ = done.await;
        }
    }

    #[cfg(feature = "test-seams")]
    pub async fn pause_writer_for_test(&self) -> mpsc::Sender<()> {
        let (paused, waiting) = oneshot::channel();
        let (release, resume) = mpsc::channel();
        self.commands.send(Command::Pause(paused, resume)).unwrap();
        waiting.await.unwrap();
        release
    }
}

impl Drop for OperationLog {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Writer {
    fn run(mut self, input: mpsc::Receiver<Command>, mut retention: i32) {
        let mut retry_at = Some(Instant::now());
        loop {
            let command = if let Some(at) = retry_at {
                let delay = at.saturating_duration_since(Instant::now());
                if delay.is_zero() {
                    retry_at = self.apply_retention(retention).err().map(|error| {
                        tracing::warn!(%error, "Could not apply operation log retention");
                        Instant::now() + RETENTION_RETRY_DELAY
                    });
                    continue;
                }
                match input.recv_timeout(delay) {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                let Ok(command) = input.recv() else { break };
                command
            };
            match command {
                Command::Record(line) => self.record(&line),
                Command::Retain(count, reply) => {
                    retention = count;
                    let result = self.apply_retention(retention);
                    retry_at = result
                        .is_err()
                        .then(|| Instant::now() + RETENTION_RETRY_DELAY);
                    let _ = reply.send(result);
                }
                Command::Flush(reply) => {
                    let _ = reply.send(());
                }
                #[cfg(feature = "test-seams")]
                Command::Pause(paused, resume) => {
                    let _ = paused.send(());
                    let _ = resume.recv();
                }
                Command::Stop => break,
            }
        }
    }

    fn apply_retention(&mut self, count: i32) -> io::Result<()> {
        if count != 0 && self.archive.is_none() {
            let mut options = OpenOptions::new();
            options.create_new(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut archive = options.open(&self.archive_path)?;
            let copied = self
                .latest
                .rewind()
                .and_then(|()| io::copy(&mut self.latest, &mut archive));
            let positioned = self.latest.seek(SeekFrom::End(0));
            if let Err(error) = copied.and(positioned) {
                drop(archive);
                let _ = fs::remove_file(&self.archive_path);
                return Err(error);
            }
            self.archive = Some(archive);
        }
        if count < 0 {
            return Ok(());
        }
        static NAME: OnceLock<Regex> = OnceLock::new();
        let generated = NAME.get_or_init(|| {
            Regex::new(
                r"(?-u:^session-\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}\.\d{3}Z-[0-9a-f]{32}\.log$)",
            )
            .expect("valid archive-name pattern")
        });
        let mut archives = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path != self.archive_path
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| generated.is_match(name))
                && entry.file_type()?.is_file()
            {
                archives.push(path);
            }
        }
        archives.sort_unstable_by(|a, b| b.cmp(a));
        for path in archives
            .into_iter()
            .skip(count.saturating_sub(1).max(0) as usize)
        {
            remove_archive(&path)?;
        }
        if count == 0 && self.archive.is_some() {
            // Close the active archive only after a successful delete. If pruning fails the
            // writer can keep using the same handle and the setting can be retried.
            remove_archive(&self.archive_path)?;
            self.archive = None;
        }
        Ok(())
    }

    fn record(&mut self, line: &str) {
        if self.latest.write_all(line.as_bytes()).is_err() {
            tracing::warn!("Could not write latest operation log");
        }
        if let Some(archive) = &mut self.archive {
            if archive.write_all(line.as_bytes()).is_err() {
                tracing::warn!("Could not write archived operation log");
            }
        }
    }
}

fn remove_archive(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_latest_replacement_preserves_the_previous_archive() {
        let data = tempfile::tempdir().unwrap();
        let first = OperationLog::open(data.path(), 1).unwrap();
        first.record(format_args!("keep this run"));
        first.flush().await;
        drop(first);
        let logs = data.path().join("logs");
        let archives = || {
            fs::read_dir(&logs)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("session-")
                })
                .collect::<Vec<_>>()
        };
        let previous = archives().pop().unwrap();
        let previous_contents = fs::read(&previous).unwrap();
        let latest = logs.join("latest.log");
        fs::remove_file(&latest).unwrap();
        fs::create_dir(&latest).unwrap();

        assert!(OperationLog::open(data.path(), 1).is_err());
        assert_eq!(archives(), std::slice::from_ref(&previous));
        assert_eq!(fs::read(&previous).unwrap(), previous_contents);

        fs::remove_dir(latest).unwrap();
        let next = OperationLog::open(data.path(), 1).unwrap();
        next.flush().await;
        assert!(!previous.exists());
        assert_eq!(archives().len(), 1);
    }

    #[tokio::test]
    async fn logs_follow_retention_changes_and_keep_unmanaged_files() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let archives = || {
            fs::read_dir(&logs)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("session-")
                })
                .collect::<Vec<_>>()
        };
        let first = OperationLog::open(dir.path(), -1).unwrap();
        first.record(format_args!("first operation"));
        first.flush().await;
        let previous = archives().pop().unwrap();
        assert_eq!(
            fs::read(&previous).unwrap(),
            fs::read(logs.join("latest.log")).unwrap()
        );
        let duplicate = OperationLog::open(dir.path(), -1);
        assert!(duplicate.is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock));
        drop(first);

        let current = OperationLog::open(dir.path(), -1).unwrap();
        assert!(previous.exists());
        current.apply_retention(1).await.unwrap();
        assert!(!previous.exists());
        let unmanaged = logs.join("owner.log");
        fs::write(&unmanaged, "owner data").unwrap();
        current.apply_retention(0).await.unwrap();
        assert!(archives().is_empty());
        current.record(format_args!("latest only"));
        assert_eq!(fs::read_to_string(&unmanaged).unwrap(), "owner data");
        current.apply_retention(-1).await.unwrap();
        current.record(format_args!("archiving resumed"));
        current.flush().await;
        let latest = fs::read(logs.join("latest.log")).unwrap();
        assert_eq!(fs::read(archives().pop().unwrap()).unwrap(), latest);
        assert!(String::from_utf8(latest).unwrap().contains("latest only"));
    }
}
