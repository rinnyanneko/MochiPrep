//! Server configuration from the environment, as `apps/server/src/config.ts` reads it.

use std::path::{Path, PathBuf};

/// The server only ever binds the loopback address (Node's `host: '127.0.0.1'`).
pub const HOST: &str = "127.0.0.1";

const DEFAULT_PORT: u16 = 8787;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub data_dir: PathBuf,
    /// Exact `Origin` values a browser may send. Their URL hosts also form the Host allowlist.
    pub allowed_origins: Vec<String>,
    /// `MOCHIPREP_OFFLINE=1`: no connection to OKX and no news feeds, so e2e and local runs can
    /// stay off the network. Live quotes, live candles, adding pairs and headlines are then
    /// unavailable (`/api/news` names every enabled feed as unreadable). Node has no such
    /// switch; it always connects.
    pub offline: bool,
    /// `MOCHIPREP_PARENT_STDIN=1`: a launcher started this process and holds its stdin, so the
    /// server shuts down gracefully when stdin ends. That also covers a launcher that crashed or
    /// was killed, which cannot send a signal. Off by default so a shell with no input (CI,
    /// `< /dev/null`) never stops a server.
    pub parent_stdin: bool,
}

impl Config {
    /// The SQLite file Node opens at `join(config.dataDir, 'mochiprep.db')`.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("mochiprep.db")
    }
}

/// Reads `MOCHIPREP_PORT`, `MOCHIPREP_DATA_DIR`, `MOCHIPREP_ALLOWED_ORIGINS` and
/// `MOCHIPREP_OFFLINE`.
///
/// Panics on a port that is not a number: Node would fail to listen on it too, and a server
/// silently falling back to another port would leave the web app pointing at nothing.
pub fn load_config() -> Config {
    config_from(|name| std::env::var(name).ok())
}

/// [`load_config`] over any variable lookup, so tests need not touch the process environment.
pub fn config_from(var: impl Fn(&str) -> Option<String>) -> Config {
    let port = match var("MOCHIPREP_PORT") {
        Some(value) => value
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("MOCHIPREP_PORT must be a port number, got {value:?}")),
        None => DEFAULT_PORT,
    };

    // Node treats an empty MOCHIPREP_DATA_DIR as unset.
    let data_dir = match var("MOCHIPREP_DATA_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => std::path::absolute(&dir).unwrap_or_else(|error| {
            panic!("MOCHIPREP_DATA_DIR {dir:?} cannot be resolved: {error}")
        }),
        None => repo_root().join(".mochiprep"),
    };

    let mut allowed_origins = vec![
        "http://localhost:5173".to_string(),
        "http://127.0.0.1:5173".to_string(),
        format!("http://localhost:{port}"),
        format!("http://127.0.0.1:{port}"),
    ];
    let extra = var("MOCHIPREP_ALLOWED_ORIGINS").unwrap_or_default();
    allowed_origins.extend(
        extra
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
    );

    Config {
        port,
        data_dir,
        allowed_origins,
        offline: var("MOCHIPREP_OFFLINE").is_some_and(|value| value.trim() == "1"),
        parent_stdin: var("MOCHIPREP_PARENT_STDIN").is_some_and(|value| value.trim() == "1"),
    }
}

/// The repository this binary was built from: two levels above this crate's manifest.
///
/// Node uses the repo root three levels above `apps/server/src`, so the default data dir is the
/// same `<repo>/.mochiprep` whichever backend runs, and whatever the working directory of
/// `cargo run` is. A binary moved off the build machine should set `MOCHIPREP_DATA_DIR`.
fn repo_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("crates/mochi-server sits two levels below the repo root")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent_stdin(value: Option<&str>) -> bool {
        config_from(|name| match name {
            "MOCHIPREP_PARENT_STDIN" => value.map(String::from),
            _ => None,
        })
        .parent_stdin
    }

    #[test]
    fn parent_stdin_is_on_only_for_one() {
        assert!(parent_stdin(Some("1")));
        assert!(parent_stdin(Some(" 1 ")));
        assert!(!parent_stdin(None));
        assert!(!parent_stdin(Some("0")));
        assert!(!parent_stdin(Some("true")));
    }
}
