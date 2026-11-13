//! The Rust HTTP backend: the same API as `apps/server` (Fastify), so the web app cannot tell
//! which one it talks to. Node remains the default backend while this port is in progress.

pub mod agent_parse;
pub mod auth;
pub mod config;
pub mod error;
pub mod routes;
pub mod runtime;
pub mod validate;

use std::fmt;
use std::sync::Arc;

use axum::Router;
use rand::rngs::OsRng;
use rand::RngCore;

pub use config::{load_config, Config};
pub use error::ApiError;
pub use runtime::{RuntimeHandle, RuntimeOptions};

/// Shared by every request.
#[derive(Clone)]
pub struct AppState {
    /// The session token: 32 random bytes as hex, new on every start and never stored, so a
    /// page can only use it after fetching it from an allowed origin in this process's lifetime.
    pub token: String,
    pub config: Arc<Config>,
    pub runtime: RuntimeHandle,
    /// The open `/api/events` streams, ended together on shutdown.
    pub streams: routes::events::SseStreams,
    /// The web app's files: the embedded build, or a test's own.
    pub web: routes::static_files::WebFiles,
}

impl AppState {
    pub fn new(config: Arc<Config>, runtime: RuntimeHandle) -> Self {
        Self {
            token: new_token(),
            config,
            runtime,
            streams: routes::events::SseStreams::default(),
            web: routes::static_files::web_dist,
        }
    }
}

// Hand-written so the token can never reach a log line through `{:?}`.
impl fmt::Debug for AppState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppState")
            .field("token", &"<redacted>")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Node's `randomBytes(32).toString('hex')`.
fn new_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The whole app: every route behind the guard.
pub fn app(state: AppState) -> Router {
    auth::protect(routes::router(), state)
}
