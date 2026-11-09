//! What the server tells the web app: the SSE events and the small status types they carry.
//! They mirror `packages/shared/src/domain.ts` (FeedStatus, ReplayState, ServerEvent, Topic,
//! VaultStatus, NewsSource, NewsItem). Fields are declared in the order Node writes them, because
//! serde writes them in declaration order and the stream must read the same.

use serde::{Deserialize, Serialize, Serializer};

use crate::jsnum::js_number_raw;
use crate::market::{DataSource, Quote};

/// TS `ConnectionState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FeedState {
    Connected,
    Connecting,
    Degraded,
    Disconnected,
    OfflineReplay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedStatus {
    pub source: DataSource,
    pub provider: String,
    pub state: FeedState,
    pub last_message_at: Option<i64>,
    /// Optional in TS: Node leaves the key out when there is nothing to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// JSON.stringify writes 1, not 1.0.
fn serialize_js_number<S: Serializer>(x: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    js_number_raw(*x).serialize(serializer)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayState {
    /// Always true from the server; the web app starts from a placeholder that is not.
    pub loaded: bool,
    pub playing: bool,
    pub speed: u32,
    pub now: Option<i64>,
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub at_end: bool,
    /// 0..1 through the playable part of the tape. Display only, so `f64`.
    #[serde(serialize_with = "serialize_js_number")]
    pub progress: f64,
}

/// What an `invalidate` event asks the web app to refetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Topic {
    Accounts,
    Orders,
    Positions,
    Agents,
    Decisions,
    Providers,
    Arena,
    Exchange,
    Instruments,
    News,
    Vault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToastLevel {
    Info,
    Warn,
    Error,
}

/// One SSE message, sent as `data: <JSON>\n\n`. Tagged by `type`, which Node writes first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerEvent {
    Heartbeat,
    Quote { quote: Quote },
    Feed { status: FeedStatus },
    Replay { state: ReplayState },
    Invalidate { topics: Vec<Topic> },
    Toast { level: ToastLevel, message: String },
}

/// OFF: no master password, AI keys are stored unencrypted. LOCKED: secrets are sealed and not
/// yet loaded. UNLOCKED: the data key is in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum VaultStatus {
    Off,
    Locked,
    Unlocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsSource {
    pub id: String,
    pub name: String,
    pub url: String,
    pub enabled: bool,
    pub built_in: bool,
}

/// A headline from a third party. Untrusted: it may be wrong, stale, or written to manipulate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsItem {
    pub id: String,
    pub source: String,
    pub title: String,
    pub url: String,
    /// ms since epoch.
    pub published_at: i64,
}
