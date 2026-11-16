//! The launcher's HTTP client for the backend's existing API: what is running and armed, and
//! disarming real accounts. One connection per request: the backend is on loopback, and a
//! pooled connection would outlive a backend restart.

use std::fmt;
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::{timeout_at, Instant};

use crate::model::BackendSnapshot;

/// Per request, connecting included: a hung backend must not hang the tray or a Quit.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// The launcher's view of one backend port. The session token is fetched on first use and
/// fetched again once when the backend answers 401, which is what a restarted backend does.
pub struct BackendClient {
    port: u16,
    token: Mutex<Option<String>>,
}

/// Never carries the token or a response body: `/api/session`'s body is the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    Connect(String),
    Timeout,
    Status { path: String, status: u16 },
    Unreadable(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Connect(error) => write!(f, "cannot reach the backend: {error}"),
            ClientError::Timeout => write!(f, "backend did not answer within 5 s"),
            ClientError::Status { path, status } => {
                write!(f, "backend answered {status} to {path}")
            }
            ClientError::Unreadable(what) => write!(f, "unreadable backend answer: {what}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl fmt::Debug for BackendClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let has_token = self.token.lock().is_ok_and(|token| token.is_some());
        f.debug_struct("BackendClient")
            .field("port", &self.port)
            .field("token", &if has_token { "<redacted>" } else { "<none>" })
            .finish()
    }
}

struct Answer {
    status: StatusCode,
    body: Bytes,
}

#[derive(Deserialize)]
struct Session {
    token: String,
}

#[derive(Deserialize)]
struct AgentRow {
    running: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExchangeRow {
    account_id: String,
    venue: String,
    armed: bool,
}

impl BackendClient {
    pub fn new(port: u16) -> Self {
        BackendClient {
            port,
            token: Mutex::new(None),
        }
    }

    /// `/api/health` with the token: 200 means the backend serves the API, not just the port.
    pub async fn health(&self) -> Result<(), ClientError> {
        self.authed(Method::GET, "/api/health", false).await?;
        Ok(())
    }

    pub async fn snapshot(&self) -> Result<BackendSnapshot, ClientError> {
        let agents = self.authed(Method::GET, "/api/agents", false).await?;
        let exchange = self
            .authed(Method::GET, "/api/exchange/status", false)
            .await?;
        Ok(BackendSnapshot {
            agents_running: count_running(&agents)?,
            armed_live_accounts: armed_live(&exchange)?,
        })
    }

    /// Disarms every armed `OKX_LIVE` account and returns how many. One failure does not stop
    /// the others: each account left armed is real money, so all are tried before the first
    /// error is reported.
    pub async fn disarm_all_live(&self) -> Result<usize, ClientError> {
        let armed = self.snapshot().await?.armed_live_accounts;
        let mut disarmed = 0;
        let mut first_error = None;
        for id in &armed {
            let result = match account_path(id) {
                Some(path) => self.authed(Method::POST, &path, true).await.map(|_| ()),
                None => Err(ClientError::Unreadable("unexpected account id".to_string())),
            };
            match result {
                Ok(()) => disarmed += 1,
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(disarmed),
        }
    }

    /// A request with the bearer token. A 401 means the token belongs to an earlier backend
    /// process; the request is retried once with a fresh one.
    async fn authed(
        &self,
        method: Method,
        path: &str,
        json_body: bool,
    ) -> Result<Bytes, ClientError> {
        let token = self.token().await?;
        let mut answer = self
            .send(method.clone(), path, Some(&token), json_body)
            .await?;
        if answer.status == StatusCode::UNAUTHORIZED {
            self.forget_token();
            let token = self.token().await?;
            answer = self.send(method, path, Some(&token), json_body).await?;
        }
        if !answer.status.is_success() {
            return Err(ClientError::Status {
                path: path.to_string(),
                status: answer.status.as_u16(),
            });
        }
        Ok(answer.body)
    }

    async fn token(&self) -> Result<String, ClientError> {
        if let Some(token) = self.cached_token() {
            return Ok(token);
        }
        // No Origin and no Sec-Fetch-Site: the backend gives the token only to such a request
        // or a same-origin one.
        let answer = self.send(Method::GET, "/api/session", None, false).await?;
        if !answer.status.is_success() {
            return Err(ClientError::Status {
                path: "/api/session".to_string(),
                status: answer.status.as_u16(),
            });
        }
        // A fixed message: a serde error can quote the body, and this body is the token.
        let session: Session = serde_json::from_slice(&answer.body)
            .map_err(|_| ClientError::Unreadable("session".to_string()))?;
        if let Ok(mut cached) = self.token.lock() {
            *cached = Some(session.token.clone());
        }
        Ok(session.token)
    }

    fn cached_token(&self) -> Option<String> {
        self.token.lock().ok().and_then(|token| token.clone())
    }

    fn forget_token(&self) {
        if let Ok(mut token) = self.token.lock() {
            *token = None;
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        json_body: bool,
    ) -> Result<Answer, ClientError> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        // A direct socket to 127.0.0.1: no proxy can ever see the token.
        let stream = timeout_at(
            deadline,
            TcpStream::connect((Ipv4Addr::LOCALHOST, self.port)),
        )
        .await
        .map_err(|_| ClientError::Timeout)?
        .map_err(|error| ClientError::Connect(error.to_string()))?;
        let (mut sender, connection) = timeout_at(
            deadline,
            hyper::client::conn::http1::handshake(TokioIo::new(stream)),
        )
        .await
        .map_err(|_| ClientError::Timeout)?
        .map_err(|error| ClientError::Connect(error.to_string()))?;
        let driver = tokio::spawn(connection);

        // The backend refuses any Host but its own origin's.
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, format!("127.0.0.1:{}", self.port));
        if let Some(token) = token {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = if json_body {
            request = request.header(CONTENT_TYPE, "application/json");
            Full::new(Bytes::from_static(b"{}"))
        } else {
            Full::new(Bytes::new())
        };
        let request = request
            .body(body)
            .map_err(|error| ClientError::Connect(error.to_string()))?;

        let exchange = async {
            let response = sender
                .send_request(request)
                .await
                .map_err(|error| ClientError::Connect(error.to_string()))?;
            let status = response.status();
            let body = response
                .into_body()
                .collect()
                .await
                .map_err(|error| ClientError::Connect(error.to_string()))?
                .to_bytes();
            Ok(Answer { status, body })
        };
        let answer = timeout_at(deadline, exchange).await;
        // The connection task would otherwise outlive a timed-out request on a hung backend.
        driver.abort();
        answer.map_err(|_| ClientError::Timeout)?
    }
}

/// Account ids are `acct_<uuid>`; anything else is refused rather than spliced into a path.
fn account_path(id: &str) -> Option<String> {
    let plain = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    plain.then(|| format!("/api/exchange/okx/{id}/disarm"))
}

fn count_running(json: &[u8]) -> Result<usize, ClientError> {
    let agents: Vec<AgentRow> = serde_json::from_slice(json)
        .map_err(|error| ClientError::Unreadable(format!("agents: {error}")))?;
    Ok(agents.iter().filter(|agent| agent.running).count())
}

/// Only real money counts: an armed OKX Demo account is not a reason to ask before quitting.
fn armed_live(json: &[u8]) -> Result<Vec<String>, ClientError> {
    let rows: Vec<ExchangeRow> = serde_json::from_slice(json)
        .map_err(|error| ClientError::Unreadable(format!("exchange status: {error}")))?;
    Ok(rows
        .into_iter()
        .filter(|row| row.venue == "OKX_LIVE" && row.armed)
        .map(|row| row.account_id)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_row(id: &str, venue: &str, armed: bool) -> String {
        format!(
            r#"{{"accountId":"{id}","name":"n","venue":"{venue}","connected":true,"saved":false,
            "armed":{armed},"healthy":true,"lastSyncAt":null,"error":null,"limits":{{}}}}"#
        )
    }

    #[test]
    fn counts_running_agents() {
        let json = r#"[
            {"id":"agent_1","name":"a","running":true,"status":"RUNNING"},
            {"id":"agent_2","name":"b","running":false,"status":"PAUSED"},
            {"id":"agent_3","name":"c","running":true,"status":"RUNNING"}
        ]"#;
        assert_eq!(count_running(json.as_bytes()).unwrap(), 2);
        assert_eq!(count_running(b"[]").unwrap(), 0);
    }

    #[test]
    fn collects_only_armed_live_accounts() {
        let json = format!(
            "[{},{},{},{}]",
            status_row("acct_live_armed", "OKX_LIVE", true),
            status_row("acct_live_disarmed", "OKX_LIVE", false),
            status_row("acct_demo_armed", "OKX_DEMO", true),
            status_row("acct_paper", "PAPER", false),
        );
        assert_eq!(
            armed_live(json.as_bytes()).unwrap(),
            vec!["acct_live_armed".to_string()]
        );
    }

    #[test]
    fn malformed_answers_are_errors() {
        assert!(matches!(
            count_running(b"{\"error\":\"x\"}"),
            Err(ClientError::Unreadable(_))
        ));
        assert!(matches!(
            armed_live(b"[{\"accountId\":\"a\"}]"),
            Err(ClientError::Unreadable(_))
        ));
    }

    #[test]
    fn account_paths_take_only_plain_ids() {
        assert_eq!(
            account_path("acct_0b6f-9c").as_deref(),
            Some("/api/exchange/okx/acct_0b6f-9c/disarm")
        );
        assert_eq!(account_path("../vault/reset"), None);
        assert_eq!(account_path("a?b"), None);
        assert_eq!(account_path(""), None);
    }

    #[test]
    fn debug_redacts_the_token() {
        let client = BackendClient::new(8787);
        *client.token.lock().unwrap() = Some("secret-token-value".to_string());
        let shown = format!("{client:?}");
        assert!(!shown.contains("secret-token-value"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
    }
}
