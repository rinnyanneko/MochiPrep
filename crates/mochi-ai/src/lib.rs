//! Provider protocols and deterministic analysis primitives. No execution or accounting.
mod indicators;
mod mock;
mod prompt;
mod redact;
mod remote;
mod types;

pub use indicators::indicator_snapshot;
pub use mock::MockProvider;
pub use prompt::{build_system_prompt, build_user_prompt};
pub use redact::redact_secrets;
pub use remote::{RemoteOptions, RemoteProvider};
pub use types::*;
