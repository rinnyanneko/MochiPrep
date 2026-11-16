//! The launcher's supervisor and client against the real backend binary: what the launcher
//! relies on is checked here, against the server it will actually run.

use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};

use mochi_launcher_core::client::{BackendClient, ClientError};
use mochi_launcher_core::model::BackendSnapshot;
use mochi_launcher_core::supervisor::{self, BackendCommand, StartError};

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn command(port: u16, data: &Path) -> BackendCommand {
    BackendCommand {
        program: PathBuf::from(env!("CARGO_BIN_EXE_mochi-server")),
        port,
        data_dir: data.to_path_buf(),
        offline: true,
    }
}

#[test]
fn constants_match_the_server() {
    assert_eq!(supervisor::READY_PREFIX, mochi_server::READY_PREFIX);
    assert_eq!(supervisor::EXIT_PORT_IN_USE, mochi_server::EXIT_PORT_IN_USE);
}

#[tokio::test]
async fn start_reports_ready_and_health() {
    let data = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut backend = supervisor::start(&command(port, data.path()))
        .await
        .expect("starts");
    assert_eq!(backend.port(), port);
    assert!(backend.is_alive());
    // `start` already waited for health; a second client must get there on its own too.
    let health = BackendClient::new(port).health().await;
    backend.stop().await;
    health.expect("health answers 200 with the session token");
}

#[tokio::test]
async fn snapshot_of_a_fresh_backend_is_idle() {
    let data = tempfile::tempdir().unwrap();
    let port = free_port();
    let backend = supervisor::start(&command(port, data.path()))
        .await
        .expect("starts");
    let client = BackendClient::new(port);
    let snapshot = client.snapshot().await;
    let disarmed = client.disarm_all_live().await;
    backend.stop().await;
    assert_eq!(
        snapshot.expect("snapshot"),
        BackendSnapshot {
            agents_running: 0,
            armed_live_accounts: Vec::new(),
        }
    );
    assert_eq!(disarmed.expect("disarm"), 0);
}

/// Disarm is the launcher's only real-money request. No live account exists here, so an unknown
/// id is disarmed: the server must take the request itself (a 400 or 415 would mean the body or
/// its content type is wrong) and then answer that the account does not exist.
#[tokio::test]
async fn disarm_request_shape_is_accepted() {
    let data = tempfile::tempdir().unwrap();
    let port = free_port();
    let backend = supervisor::start(&command(port, data.path()))
        .await
        .expect("starts");
    let answer = BackendClient::new(port).disarm("acct_unknown").await;
    backend.stop().await;
    match answer {
        Ok(()) => {}
        Err(ClientError::Status { status: 404, .. }) => {}
        Err(other) => panic!("disarm request refused: {other}"),
    }
}

#[tokio::test]
async fn stop_frees_the_port() {
    let data = tempfile::tempdir().unwrap();
    let port = free_port();
    let backend = supervisor::start(&command(port, data.path()))
        .await
        .expect("starts");
    backend.stop().await;
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect("port is free after stop");
}

#[tokio::test]
async fn busy_port_is_port_in_use() {
    let data = tempfile::tempdir().unwrap();
    let holder = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = holder.local_addr().unwrap().port();
    let result = supervisor::start(&command(port, data.path())).await;
    let error = result.expect_err("refused");
    assert_eq!(error, StartError::PortInUse(port));
    assert_eq!(error.to_string(), format!("port {port} is in use"));
}

#[tokio::test]
async fn missing_program_is_spawn_error() {
    let data = tempfile::tempdir().unwrap();
    let mut cmd = command(free_port(), data.path());
    cmd.program = data.path().join("no-such-backend.exe");
    let error = supervisor::start(&cmd).await.expect_err("refused");
    assert!(matches!(error, StartError::Spawn(_)), "got {error:?}");
    assert!(error.to_string().starts_with("cannot start backend: "));
}

#[tokio::test]
async fn token_is_refetched_after_restart() {
    let data = tempfile::tempdir().unwrap();
    let port = free_port();
    let cmd = command(port, data.path());
    let client = BackendClient::new(port);

    let first = supervisor::start(&cmd).await.expect("first start");
    let before = client.snapshot().await;
    first.stop().await;
    before.expect("snapshot before restart");

    // A new process hands out a new token, so the cached one now earns a 401.
    let second = supervisor::start(&cmd).await.expect("second start");
    let after = client.snapshot().await;
    second.stop().await;
    after.expect("snapshot after restart");
}
