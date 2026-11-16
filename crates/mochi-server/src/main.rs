use std::future::IntoFuture;
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use mochi_market::okx_public::{OkxPublicFeed, REST_BASE, WS_URL};
use mochi_server::{
    app, load_config, AppState, RuntimeHandle, RuntimeOptions, EXIT_PORT_IN_USE, READY_PREFIX,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::sleep;

/// How long open requests get to finish after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

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
    // Public market data only: the feed never holds or sends a credential.
    let options = if config.offline {
        RuntimeOptions::new(db)
    } else {
        RuntimeOptions::with_live_feed(db, OkxPublicFeed::new(REST_BASE, WS_URL))
    };
    let runtime = match RuntimeHandle::start(options) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Cannot start: {error}");
            return ExitCode::FAILURE;
        }
    };
    let state = AppState::new(config.clone(), runtime.clone());
    let streams = state.streams.clone();

    // Loopback only, never 0.0.0.0: the token is the only other thing between the network and
    // trading keys.
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, config.port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("Cannot listen on 127.0.0.1:{}: {error}", config.port);
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
    println!("{READY_PREFIX}{port} (data: {})", config.data_dir.display());
    let _ = std::io::stdout().flush();
    if config.offline {
        println!("MOCHIPREP_OFFLINE=1: not connecting to OKX market data or news feeds");
        let _ = std::io::stdout().flush();
    }
    let stdin_closed = config.parent_stdin.then(watch_stdin);

    let (signalled, on_signal) = oneshot::channel::<()>();
    let server = axum::serve(listener, app(state))
        .with_graceful_shutdown(async move {
            shutdown_signal(stdin_closed).await;
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
