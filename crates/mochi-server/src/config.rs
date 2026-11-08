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
}

impl Config {
    /// The SQLite file Node opens at `join(config.dataDir, 'mochiprep.db')`.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("mochiprep.db")
    }
}

/// Reads `MOCHIPREP_PORT`, `MOCHIPREP_DATA_DIR` and `MOCHIPREP_ALLOWED_ORIGINS`.
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
