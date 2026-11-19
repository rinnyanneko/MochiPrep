use std::future::IntoFuture;
use std::io::{Read, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use mochi_market::okx_public::{OkxPublicFeed, REST_BASE, WS_URL};
use mochi_server::{
    app, load_config, operation_log, AppState, RuntimeHandle, RuntimeOptions, EXIT_PORT_IN_USE,
    READY_PREFIX,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::sleep;

/// How long open requests get to finish after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const LOG_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// Tokio worker threads. The work is I/O-bound, the ledger and SQLite run in one runtime task, and
/// the vault KDF uses the separate blocking pool, so two workers are enough; each extra worker
/// costs resident memory and the spec gate is 20 MiB.
const WORKER_THREADS: usize = 2;

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run())
}

async fn run() -> ExitCode {
    // Warnings and errors to stderr, like Fastify's `logger: { level: 'warn' }`. Every message
    // the runtime logs has been through its secret redaction.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .init();
    let config = Arc::new(load_config());
    let db = match mochi_store::open_database(&config.db_path()) {
        Ok(db) => db,
        Err(error) => {
            eprintln!("Cannot open {}: {error}", config.db_path().display());
            return ExitCode::FAILURE;
        }
    };
    // Loopback unless the owner set MOCHIPREP_HOST. Anything else exposes the API beyond this
    // machine, where the token and the Host/Origin allowlist are all that stand between it and
    // trading keys.
    let listener = match TcpListener::bind((config.host, config.port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("Cannot listen on {}:{}: {error}", config.host, config.port);
            return if error.kind() == std::io::ErrorKind::AddrInUse {
                ExitCode::from(EXIT_PORT_IN_USE)
            } else {
                ExitCode::FAILURE
            };
        }
    };
    // The port actually bound, which a launcher parses; flushed because piped stdout is buffered.
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            eprintln!("Cannot read the listening address: {error}");
            return ExitCode::FAILURE;
        }
    };
    let retention = match operation_log::retention(&db) {
        Ok(retention) => retention,
        Err(_) => {
            eprintln!("Cannot read log retention; keeping existing archives");
            -1
        }
    };
    let log_directory = match operation_log::LogDirectory::acquire(&config.data_dir) {
        Ok(directory) => Some(directory),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            eprintln!("Another backend is using this data directory");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("Cannot open operation logs: {error}");
            None
        }
    };
    // Public market data only: the feed never holds or sends a credential.
    let options = if config.offline {
        RuntimeOptions::new(db)
    } else {
        RuntimeOptions::with_live_feed(db, OkxPublicFeed::new(REST_BASE, WS_URL))
    };
    let mut operation_logs = None;
    let runtime = match RuntimeHandle::start_with_operation_log(options, || {
        operation_logs = log_directory.and_then(|directory| match directory.open(retention) {
            Ok(log) => Some(Arc::new(log)),
            Err(error) => {
                eprintln!("Cannot open operation logs: {error}");
                None
            }
        });
        operation_logs.clone()
    }) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Cannot start: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(log) = &operation_logs {
        log.backend_started(retention);
    }
    let mut state = AppState::new(config.clone(), runtime.clone());
    state.operation_logs = operation_logs.clone();
    let streams = state.streams.clone();
    let shutdown = state.shutdown.clone();
    println!("{READY_PREFIX}{port} (data: {})", config.data_dir.display());
    let _ = std::io::stdout().flush();
    // stderr, so it shows wherever errors are captured; stdout stays the launcher's ready line.
    if !config.host.is_loopback() {
        eprintln!(
            "MOCHIPREP_HOST={}: listening beyond this machine. The API token is issued to any client that sends an allowed Host header, so other machines can reach the API.",
            config.host
        );
    }
    if config.offline {
        println!("MOCHIPREP_OFFLINE=1: not connecting to OKX market data or news feeds");
        let _ = std::io::stdout().flush();
    }
    let stdin_closed = config.parent_stdin.then(watch_stdin);

    let (signalled, on_signal) = oneshot::channel::<()>();
    let server = axum::serve(listener, app(state))
        .with_graceful_shutdown(async move {
            tokio::select! {
                () = shutdown_signal(stdin_closed) => {}
                () = shutdown.notified() => {}
            }
            // Open event streams never finish on their own, and graceful shutdown waits for
            // every response to finish.
            streams.close_all();
            let _ = signalled.send(());
        })
        .into_future();
    // A client that stops reading can hold its response open past `close_all`. Give requests a
    // few seconds to finish, then stop waiting, so the database is always closed cleanly.
    let grace_over = async {
        if on_signal.await.is_ok() {
            sleep(SHUTDOWN_GRACE).await;
        } else {
            // The server ended without a signal; its own result decides.
            std::future::pending::<()>().await;
        }
    };
    let served = tokio::select! {
        served = server => served,
        () = grace_over => {
            eprintln!("Some requests were still open after {} s; closing anyway", SHUTDOWN_GRACE.as_secs());
            Ok(())
        }
    };
    // Pauses the replay and closes the database, as Node's shutdown does.
    runtime.shutdown().await;
    if let Some(log) = operation_logs {
        log.record(format_args!("backend stopped"));
        if tokio::time::timeout(LOG_SHUTDOWN_GRACE, log.shutdown())
            .await
            .is_err()
        {
            eprintln!("Operation log did not finish before shutdown");
        }
    }
    if let Err(error) = served {
        eprintln!("Server error: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Resolves when the parent closes this process's stdin. A plain thread does the blocking read:
/// a blocking task would keep the runtime from dropping at exit, and the thread dies with the
/// process.
fn watch_stdin() -> oneshot::Receiver<()> {
    let (closed, on_close) = oneshot::channel();
    std::thread::spawn(move || {
        let mut sink = [0u8; 256];
        let mut stdin = std::io::stdin().lock();
        // Anything the parent writes is ignored; only the end of the stream (or an error, which
        // means the pipe is gone) matters.
        while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
        let _ = closed.send(());
    });
    on_close
}

/// Ctrl-C everywhere, and SIGTERM on Unix, as Node handles SIGINT and SIGTERM. In parent mode,
/// stdin ending counts too.
async fn shutdown_signal(stdin_closed: Option<oneshot::Receiver<()>>) {
    let parent_gone = async {
        match stdin_closed {
            Some(closed) => {
                // The thread only ends by sending; a dropped sender would be a bug, so do not
                // treat it as a reason to stop.
                if closed.await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
            None => std::future::pending::<()>().await,
        }
    };

    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            // Without a handler we cannot be stopped gracefully; wait forever rather than exit now.
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
        () = parent_gone => {}
    }
}
