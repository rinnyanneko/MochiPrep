//! Server configuration from the environment, as `apps/server/src/config.ts` reads it.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

/// The address bound unless `MOCHIPREP_HOST` says otherwise (Node's `host: '127.0.0.1'`).
pub const DEFAULT_HOST: &str = "127.0.0.1";

const DEFAULT_PORT: u16 = 8787;

#[derive(Debug, Clone)]
pub struct Config {
    /// Loopback unless `MOCHIPREP_HOST` is set. Any other address makes the API reachable from
    /// other machines, so it is the owner's choice.
    pub host: IpAddr,
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

/// Reads `MOCHIPREP_HOST`, `MOCHIPREP_PORT`, `MOCHIPREP_DATA_DIR`, `MOCHIPREP_ALLOWED_ORIGINS` and
/// `MOCHIPREP_OFFLINE`.
///
/// Panics on a host or port that does not parse: Node would fail to listen on it too, and a server
/// silently falling back to another address or port would leave the web app pointing at nothing.
pub fn load_config() -> Config {
    config_from(|name| std::env::var(name).ok())
}

/// [`load_config`] over any variable lookup, so tests need not touch the process environment.
pub fn config_from(var: impl Fn(&str) -> Option<String>) -> Config {
    let host: IpAddr = match var("MOCHIPREP_HOST").filter(|host| !host.trim().is_empty()) {
        Some(value) => value
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("MOCHIPREP_HOST must be an IP address, got {value:?}")),
        None => DEFAULT_HOST.parse().expect("DEFAULT_HOST is an IP address"),
    };

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
    // A loopback bind address other than 127.0.0.1 (say ::1) must work in a browser on its own.
    // A LAN address is never added here: letting other machines in stays an explicit
    // MOCHIPREP_ALLOWED_ORIGINS entry, and a wildcard (0.0.0.0, ::) is no address to open.
    if host.is_loopback() {
        let own = format!("http://{}", SocketAddr::new(host, port));
        if !allowed_origins.contains(&own) {
            allowed_origins.push(own);
        }
    }
    let extra = var("MOCHIPREP_ALLOWED_ORIGINS").unwrap_or_default();
    allowed_origins.extend(
        extra
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
    );

    Config {
        host,
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

    fn host(value: Option<&str>) -> IpAddr {
        config_from(|name| match name {
            "MOCHIPREP_HOST" => value.map(String::from),
            _ => None,
        })
        .host
    }

    #[test]
    fn host_is_loopback_unless_set() {
        assert_eq!(host(None), IpAddr::from([127, 0, 0, 1]));
        assert_eq!(host(Some("")), IpAddr::from([127, 0, 0, 1]));
        assert_eq!(host(Some(" 0.0.0.0 ")), IpAddr::from([0, 0, 0, 0]));
        assert_eq!(host(Some("::1")), "::1".parse::<IpAddr>().unwrap());
    }

    fn origins(host: &str) -> Vec<String> {
        config_from(|name| match name {
            "MOCHIPREP_HOST" => Some(host.to_string()),
            _ => None,
        })
        .allowed_origins
    }

    #[test]
    fn only_a_loopback_bind_address_becomes_an_allowed_origin() {
        assert!(origins("::1").contains(&"http://[::1]:8787".to_string()));
        // A LAN address needs MOCHIPREP_ALLOWED_ORIGINS.
        assert_eq!(origins("192.168.1.20").len(), 4);
        // The default is already listed once, and a wildcard is no origin at all.
        let default = origins("127.0.0.1");
        assert_eq!(
            default
                .iter()
                .filter(|o| *o == "http://127.0.0.1:8787")
                .count(),
            1
        );
        assert_eq!(origins("0.0.0.0").len(), 4);
        assert_eq!(origins("::").len(), 4);
    }

    #[test]
    #[should_panic(expected = "MOCHIPREP_HOST must be an IP address")]
    fn host_must_be_an_ip_address() {
        host(Some("localhost"));
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
