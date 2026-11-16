//! Runs `mochi-server` as the launcher's child: start it, wait until it serves the API, stop it.

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, timeout_at, Instant};

use crate::client::BackendClient;

/// Copies of `mochi_server::READY_PREFIX` and `EXIT_PORT_IN_USE`, so the launcher does not link
/// the whole server; `mochi-server/tests/launcher_contract.rs` checks they still match.
pub const READY_PREFIX: &str = "MochiPrep backend on http://127.0.0.1:";
pub const EXIT_PORT_IN_USE: u8 = 3;

const START_TIMEOUT: Duration = Duration::from_secs(20);
/// The backend gives open requests 5 s after stdin closes; this leaves room for that.
const STOP_GRACE: Duration = Duration::from_secs(10);
const STDERR_TAIL_BYTES: usize = 2048;
const HEALTH_RETRY: Duration = Duration::from_millis(100);
/// How long a dead child's stderr gets to reach the tail before an error is built from it.
const STDERR_SETTLE: Duration = Duration::from_secs(1);

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendCommand {
    pub program: PathBuf,
    pub port: u16,
    pub data_dir: PathBuf,
    pub offline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    PortInUse(u16),
    Exited {
        code: Option<i32>,
        stderr_tail: String,
    },
    Timeout,
    Spawn(String),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::PortInUse(port) => write!(f, "port {port} is in use"),
            StartError::Exited { code, stderr_tail } => match code {
                Some(code) => write!(f, "backend exited (code {code}): {stderr_tail}"),
                // Killed by a signal: there is no code to show.
                None => write!(f, "backend exited (code none): {stderr_tail}"),
            },
            StartError::Timeout => write!(f, "backend did not start within 20 s"),
            StartError::Spawn(error) => write!(f, "cannot start backend: {error}"),
        }
    }
}

impl std::error::Error for StartError {}

/// A running backend child. Dropping it kills the process; `stop` lets it shut down cleanly.
#[derive(Debug)]
pub struct Backend {
    child: Child,
    stdin: Option<ChildStdin>,
    port: u16,
}

type StderrTail = Arc<Mutex<VecDeque<u8>>>;

/// Starts the backend and returns once `/api/health` answers 200 with the session token, within
/// 20 s of spawning. The child is killed on every error path.
pub async fn start(cmd: &BackendCommand) -> Result<Backend, StartError> {
    let deadline = Instant::now() + START_TIMEOUT;
    let mut command = Command::new(&cmd.program);
    command
        .env("MOCHIPREP_PORT", cmd.port.to_string())
        .env("MOCHIPREP_DATA_DIR", &cmd.data_dir)
        // Closing stdin is how `stop` asks it to exit, and how it notices the launcher died.
        .env("MOCHIPREP_PARENT_STDIN", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // The command is the whole truth: an inherited MOCHIPREP_OFFLINE must not override it.
    if cmd.offline {
        command.env("MOCHIPREP_OFFLINE", "1");
    } else {
        command.env_remove("MOCHIPREP_OFFLINE");
    }
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);

    let mut child = command
        .spawn()
        .map_err(|error| StartError::Spawn(error.to_string()))?;
    let stdin = child.stdin.take();
    let (ready_tx, ready_rx) = oneshot::channel();
    // Both pipes are read until the child exits: a full pipe would block it, and a closed
    // stdout would make its next `println!` panic.
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(drain_stdout(stdout, ready_tx));
    }
    let tail: StderrTail = Arc::default();
    let stderr_task = child
        .stderr
        .take()
        .map(|stderr| tokio::spawn(drain_stderr(stderr, tail.clone())));

    match timeout_at(deadline, ready_rx).await {
        Ok(Ok(())) => {}
        // Stdout ended without a ready line: the child is exiting.
        Ok(Err(_)) => return Err(exited(child, cmd.port, stderr_task, &tail, deadline).await),
        Err(_) => return Err(kill(child).await),
    }

    let client = BackendClient::new(cmd.port);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return Err(exited(child, cmd.port, stderr_task, &tail, deadline).await);
        }
        if let Ok(Ok(())) = timeout_at(deadline, client.health()).await {
            break;
        }
        if Instant::now() + HEALTH_RETRY >= deadline {
            return Err(kill(child).await);
        }
        sleep(HEALTH_RETRY).await;
    }
    Ok(Backend {
        child,
        stdin,
        port: cmd.port,
    })
}

impl Backend {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Only a confirmed exit counts as dead: a status that cannot be read leaves a process
    /// that may still hold armed accounts.
    pub fn is_alive(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Closes stdin, which the backend takes as the signal to shut down, and kills it if it
    /// has not exited within 10 s.
    pub async fn stop(mut self) {
        drop(self.stdin.take());
        if timeout(STOP_GRACE, self.child.wait()).await.is_err() {
            let _ = self.child.kill().await;
        }
    }
}

async fn drain_stdout(stdout: impl AsyncRead + Unpin, ready: oneshot::Sender<()>) {
    let mut reader = BufReader::new(stdout);
    let mut ready = Some(ready);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if line.starts_with(READY_PREFIX.as_bytes()) {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(());
                    }
                }
            }
        }
    }
}

async fn drain_stderr(mut stderr: impl AsyncRead + Unpin, tail: StderrTail) {
    let mut buffer = [0u8; 1024];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                if let Ok(mut tail) = tail.lock() {
                    tail.extend(&buffer[..read]);
                    let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                    tail.drain(..excess);
                }
            }
        }
    }
}

/// The error for a child that exited before it was ready, with its exit code mapped.
async fn exited(
    mut child: Child,
    port: u16,
    stderr_task: Option<JoinHandle<()>>,
    tail: &StderrTail,
    deadline: Instant,
) -> StartError {
    let status = match timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) | Err(_) => return kill(child).await,
    };
    if status.code() == Some(i32::from(EXIT_PORT_IN_USE)) {
        return StartError::PortInUse(port);
    }
    if let Some(task) = stderr_task {
        let _ = timeout(STDERR_SETTLE, task).await;
    }
    let stderr_tail = tail
        .lock()
        .map(|tail| {
            let bytes: Vec<u8> = tail.iter().copied().collect();
            String::from_utf8_lossy(&bytes).trim().to_string()
        })
        .unwrap_or_default();
    StartError::Exited {
        code: status.code(),
        stderr_tail,
    }
}

async fn kill(mut child: Child) -> StartError {
    let _ = child.kill().await;
    StartError::Timeout
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_read_as_the_tray_shows_them() {
        assert_eq!(
            StartError::PortInUse(8787).to_string(),
            "port 8787 is in use"
        );
        let exited = StartError::Exited {
            code: Some(1),
            stderr_tail: "Cannot open db".to_string(),
        };
        assert_eq!(
            exited.to_string(),
            "backend exited (code 1): Cannot open db"
        );
        assert_eq!(
            StartError::Timeout.to_string(),
            "backend did not start within 20 s"
        );
        assert_eq!(
            StartError::Spawn("not found".to_string()).to_string(),
            "cannot start backend: not found"
        );
    }
}
