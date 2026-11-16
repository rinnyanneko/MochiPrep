//! The binary as a launcher's child: ready line with the bound port, stop by closing stdin, and
//! a distinct exit code when the port is taken.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use mochi_server::{EXIT_PORT_IN_USE, READY_PREFIX};

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn(port: u16, data: &std::path::Path, parent_mode: bool) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mochi-server"));
    command
        .env("MOCHIPREP_OFFLINE", "1")
        .env("MOCHIPREP_PORT", port.to_string())
        .env("MOCHIPREP_DATA_DIR", data)
        .env_remove("MOCHIPREP_PARENT_STDIN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if parent_mode {
        command.env("MOCHIPREP_PARENT_STDIN", "1");
    }
    command.spawn().unwrap()
}

/// Returns the reader too: dropping it would close the pipe, and the server's next println
/// (the offline notice) would then panic.
fn first_line(stdout: ChildStdout) -> (String, BufReader<ChildStdout>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    (line, reader)
}

fn wait_for_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn prints_the_ready_line_with_the_bound_port() {
    let data = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = spawn(port, data.path(), false);
    let (line, _stdout) = first_line(child.stdout.take().unwrap());
    child.kill().unwrap();
    child.wait().unwrap();
    let rest = line
        .strip_prefix(READY_PREFIX)
        .unwrap_or_else(|| panic!("unexpected first line {line:?}"));
    assert!(
        rest.starts_with(&format!("{port} (data: ")),
        "line was {line:?}"
    );
}

#[test]
fn exits_cleanly_when_stdin_closes_in_parent_mode() {
    let data = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = spawn(port, data.path(), true);
    let (line, _stdout) = first_line(child.stdout.take().unwrap());
    assert!(line.starts_with(READY_PREFIX), "line was {line:?}");
    drop(child.stdin.take());
    let status = wait_for_exit(&mut child, Duration::from_secs(10));
    if status.is_none() {
        child.kill().unwrap();
    }
    assert!(status.expect("exits within 10 s").success());
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect("port is free again");
}

#[test]
fn ignores_stdin_without_parent_mode() {
    let data = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = spawn(port, data.path(), false);
    let (line, _stdout) = first_line(child.stdout.take().unwrap());
    assert!(line.starts_with(READY_PREFIX), "line was {line:?}");
    drop(child.stdin.take());
    std::thread::sleep(Duration::from_secs(1));
    let still_running = child.try_wait().unwrap().is_none();
    let mut body = String::new();
    if still_running {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        // A hung server must fail the test, not hang it.
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let read = stream.read_to_string(&mut body);
        if read.is_err() {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("no health response within 5 s: {read:?}");
        }
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(still_running, "backend stopped when stdin closed");
    // Health needs the session token, so 401 is the answer of a live server that accepted the
    // Host header; a host rejection or no answer would be something else.
    assert!(body.starts_with("HTTP/1.1 401"), "response was {body:?}");
}

#[test]
fn port_in_use_exits_with_code_3() {
    let data = tempfile::tempdir().unwrap();
    let holder = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = holder.local_addr().unwrap().port();
    let mut child = spawn(port, data.path(), false);
    let status = wait_for_exit(&mut child, Duration::from_secs(10));
    if status.is_none() {
        child.kill().unwrap();
    }
    assert_eq!(
        status.expect("exits on its own").code(),
        Some(i32::from(EXIT_PORT_IN_USE))
    );
}
