use std::net::Ipv4Addr;
use std::process::ExitCode;
use std::sync::Arc;

use mochi_server::{app, load_config, AppState, RuntimeHandle};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> ExitCode {
    let config = Arc::new(load_config());
    let state = AppState::new(config.clone(), RuntimeHandle::default());

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

    if let Err(error) = axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
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
