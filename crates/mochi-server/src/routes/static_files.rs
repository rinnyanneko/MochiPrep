//! The web app, embedded in the binary from `apps/web/dist` (Node registers `@fastify/static`
//! over the same folder, `app.ts:449-450`).
//!
//! - `/` is `index.html`; any other path names one file of the build exactly. There is no SPA
//!   fallback: the web app routes by `#`, so it only ever loads `/` and its hashed assets.
//! - Headers are `send`'s defaults that matter to a browser: `Cache-Control: public, max-age=0`
//!   and an ETag, so every load revalidates and an unchanged file costs a 304.
//! - These requests pass the guard's Host and Origin checks like any other, so a rebinding or
//!   foreign page cannot read them; there is no token check, as a browser has none for `/`.

use std::borrow::Cow;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::{EmbeddedFile, RustEmbed};

use crate::error::ApiError;
use crate::AppState;

#[derive(RustEmbed)]
#[folder = "../../apps/web/dist"]
// A checkout that never ran `pnpm build` still compiles, and then serves no UI, as Node does
// when the folder is missing.
#[allow_missing = true]
struct WebDist;

/// Looks a file of the web build up by its path inside the build (`assets/index-1a2b.js`).
/// The server uses [`web_dist`]; tests swap in their own files.
pub type WebFiles = fn(&str) -> Option<EmbeddedFile>;

/// The embedded `apps/web/dist`.
pub fn web_dist(path: &str) -> Option<EmbeddedFile> {
    WebDist::get(path)
}

/// `send`'s default: cache, but revalidate on every use.
const CACHE: &str = "public, max-age=0";

/// Every path no route claims ends here: a file of the build, or the JSON 404.
pub async fn serve(State(state): State<AppState>, request: Request) -> Response {
    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return not_found();
    }
    let Some(name) = file_name(request.uri().path()) else {
        return not_found();
    };
    let Some(file) = (state.web)(name) else {
        return not_found();
    };

    let etag = format!("\"{}\"", hex(&file.metadata.sha256_hash()));
    let revalidated = request
        .headers()
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|tag| {
            let tag = tag.trim();
            tag == "*" || tag.trim_start_matches("W/") == etag
        });
    if revalidated {
        return (
            StatusCode::NOT_MODIFIED,
            [(ETAG, etag), (CACHE_CONTROL, CACHE.to_string())],
        )
            .into_response();
    }

    let content_type = content_type(file.metadata.mimetype());
    let length = file.data.len();
    // A HEAD answer has the same headers and no body.
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        match file.data {
            Cow::Borrowed(bytes) => Body::from(bytes),
            Cow::Owned(bytes) => Body::from(bytes),
        }
    };
    (
        [
            (CONTENT_TYPE, content_type),
            (CACHE_CONTROL, CACHE.to_string()),
            (ETAG, etag),
            (CONTENT_LENGTH, length.to_string()),
        ],
        body,
    )
        .into_response()
}

/// The build path a URL path names, or `None` for one that names no file. The path is taken
/// as it came, undecoded; a name with an empty, `.` or `..` segment, or a backslash, is never a
/// file of the build, so it is refused before any lookup.
fn file_name(path: &str) -> Option<&str> {
    let name = path.strip_prefix('/')?;
    if name.is_empty() {
        return Some("index.html");
    }
    let suspicious = name.contains('\\')
        || name
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..");
    if suspicious {
        return None;
    }
    Some(name)
}

/// `send` names the charset of text it serves, as browsers otherwise guess it.
fn content_type(mime: &str) -> String {
    let text =
        mime.starts_with("text/") || mime == "application/javascript" || mime == "application/json";
    if text {
        format!("{mime}; charset=UTF-8")
    } else {
        mime.to_string()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn not_found() -> Response {
    ApiError::not_found("Not found").into_response()
}
