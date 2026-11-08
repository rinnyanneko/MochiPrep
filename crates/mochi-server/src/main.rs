use std::future::IntoFuture;
use std::net::Ipv4Addr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use mochi_server::{app, load_config, AppState, RuntimeHandle, RuntimeOptions};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::sleep;

/// How long open requests get to finish after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> ExitCode {
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
    let runtime = match RuntimeHandle::start(RuntimeOptions::new(db)) {
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
            return ExitCode::FAILURE;
        }
    };
    println!(
        "MochiPrep backend on http://127.0.0.1:{} (data: {})",
        config.port,
        config.data_dir.display()
    );

    let (signalled, on_signal) = oneshot::channel::<()>();
    let server = axum::serve(listener, app(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
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

/// Ctrl-C everywhere, and SIGTERM on Unix, as Node handles SIGINT and SIGTERM.
async fn shutdown_signal() {
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
    }
}
