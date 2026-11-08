//! The request guard: Node's `onRequest` hook in `apps/server/src/app.ts`, plus the body limit.
//!
//! Every request passes three checks, in this order, before any route runs:
//! 1. its Host must be one of the allowed origins' hosts (a DNS-rebinding page sends its own);
//! 2. an Origin, when sent, must be exactly an allowed origin (a foreign page sends its own);
//! 3. under `/api/`, except `/api/session`, it must carry the session token.
//!
//! Static files get checks 1 and 2 as well, so another site cannot read them either.

use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, HOST, ORIGIN};
use axum::http::{HeaderMap, Method};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use subtle::ConstantTimeEq;

use crate::error::ApiError;
use crate::validate;
use crate::AppState;

/// Fastify's `bodyLimit: 256 * 1024`.
pub const BODY_LIMIT: usize = 256 * 1024;

/// What the guard needs, worked out once instead of on every request.
struct Guard {
    token: String,
    allowed_origins: Vec<String>,
    allowed_hosts: HashSet<String>,
}

/// Wraps `router` in the guard and the body limit, and answers unmatched paths with JSON.
pub fn protect(router: Router<AppState>, state: AppState) -> Router {
    let guard = Arc::new(Guard {
        token: state.token.clone(),
        allowed_origins: state.config.allowed_origins.clone(),
        allowed_hosts: state
            .config
            .allowed_origins
            .iter()
            .filter_map(|origin| url_host(origin))
            .collect(),
    });
    router
        // Fastify's default 404 is `{message, error, statusCode}`. We answer `{"error"}` on
        // purpose, so every failure has the one shape the web client reads.
        .fallback(not_found)
        // Fastify has no 405: a known path with another method is a plain 404 there too.
        .method_not_allowed_fallback(not_found)
        .layer(middleware::from_fn_with_state(guard, guard_request))
        .with_state(state)
}

async fn not_found() -> ApiError {
    ApiError::not_found("Not found")
}

async fn guard_request(State(guard): State<Arc<Guard>>, request: Request, next: Next) -> Response {
    if let Err(error) = guard.check(&request) {
        return error.into_response();
    }
    // Only now read the body, as Fastify parses it after `onRequest`: a refused request never
    // gets to upload anything.
    match limit_body(request).await {
        Ok(request) => next.run(request).await,
        Err(error) => error.into_response(),
    }
}

impl Guard {
    fn check(&self, request: &Request) -> Result<(), ApiError> {
        let headers = request.headers();

        // A missing or non-text Host matches nothing in the allowlist.
        let host = headers.get(HOST).and_then(|value| value.to_str().ok());
        if !host.is_some_and(|host| self.allowed_hosts.contains(host)) {
            return Err(ApiError::forbidden("Host not allowed"));
        }

        if !self.origin_allowed(headers) {
            return Err(ApiError::forbidden("Origin not allowed"));
        }

        // The raw path, as Node's `request.url.split('?')[0]`: no decoding, no normalising.
        let path = request.uri().path();
        if !path.starts_with("/api/") || path == "/api/session" {
            return Ok(());
        }
        let supplied = match headers.get(AUTHORIZATION) {
            // A header always wins, even an empty or malformed one, as `header ?? query` does.
            Some(value) => value
                .to_str()
                .ok()
                .map(|value| value.strip_prefix("Bearer ").unwrap_or(value).to_string()),
            None if path == "/api/events" => query_token(request.uri().query()),
            None => None,
        };
        match supplied {
            Some(token) if same_token(&token, &self.token) => Ok(()),
            _ => Err(ApiError::unauthorized()),
        }
    }

    /// No Origin (or an empty one, which Node treats as absent) is fine; anything else must be
    /// an exact allowed origin.
    fn origin_allowed(&self, headers: &HeaderMap) -> bool {
        let mut values = headers.get_all(ORIGIN).iter();
        match (values.next(), values.next()) {
            (None, _) => true,
            (Some(origin), None) if origin.is_empty() => true,
            (Some(origin), None) => origin
                .to_str()
                .is_ok_and(|origin| self.allowed_origins.iter().any(|allowed| allowed == origin)),
            // Node joins repeated Origin headers with ", ", which is never an allowed origin.
            (Some(_), Some(_)) => false,
        }
    }
}

/// `?token=` for `/api/events`, where an `EventSource` cannot send headers. Decoded like
/// Fastify's query parser. A repeated parameter gives Node an array, which never matches, so
/// it is refused here too.
fn query_token(query: Option<&str>) -> Option<String> {
    let mut tokens =
        url::form_urlencoded::parse(query?.as_bytes()).filter(|(name, _)| name == "token");
    let (_, token) = tokens.next()?;
    if tokens.next().is_some() {
        return None;
    }
    Some(token.into_owned())
}

/// Constant time, so response timing does not reveal how much of a guess was right.
/// `ct_eq` on slices of different lengths is false; the token length is public anyway.
fn same_token(supplied: &str, token: &str) -> bool {
    supplied.as_bytes().ct_eq(token.as_bytes()).into()
}

/// `new URL(origin).host`: lowercase host, plus the port unless it is the scheme's default.
fn url_host(origin: &str) -> Option<String> {
    let url = url::Url::parse(origin).ok()?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// Reads the whole body as Fastify would before a route runs, so routes then see a body that is
/// already in memory and small (axum's own, larger, plain-text limit never triggers).
///
/// - GET and HEAD: Fastify never reads their body, so it is dropped unread and never refused.
/// - Otherwise a content-type Fastify has no parser for is 415 before the size is looked at,
///   then a body past [`BODY_LIMIT`] is Fastify's 413 JSON.
///
/// An empty body with no content-type passes untouched.
async fn limit_body(request: Request) -> Result<Request, ApiError> {
    let (parts, body) = request.into_parts();
    if parts.method == Method::GET || parts.method == Method::HEAD {
        return Ok(Request::from_parts(parts, Body::empty()));
    }
    validate::check_media_type(&parts.headers)?;
    // Refuse a declared oversize body before reading any of it.
    let declared = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|length| length > BODY_LIMIT as u64) {
        return Err(ApiError::payload_too_large());
    }
    let bytes = match Limited::new(body, BODY_LIMIT).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) if error.is::<LengthLimitError>() => return Err(ApiError::payload_too_large()),
        Err(_) => return Err(ApiError::bad_request("Request body could not be read")),
    };
    Ok(Request::from_parts(parts, Body::from(bytes)))
}
