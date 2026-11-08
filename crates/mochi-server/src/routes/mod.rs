//! HTTP routes. Each answers exactly as the same route in `apps/server/src/app.ts` does.

pub mod accounts;
pub mod events;
pub mod instruments;
pub mod later;
pub mod market;
pub mod news;
pub mod orders;
pub mod replay;
pub mod state;
pub mod static_files;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::AppState;

/// Every route, without the guard. [`crate::auth::protect`] wraps this.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/session", get(session))
        .route("/api/health", get(health))
        .route("/api/events", get(events::events))
        .merge(state::router())
        .merge(market::router())
        .merge(news::router())
        .merge(instruments::router())
        .merge(accounts::router())
        .merge(orders::router())
        .merge(replay::router())
        .merge(later::router())
}

/// Hands out the token without asking for one. The guard has already refused a foreign Host or
/// Origin; what is left is a cross-site request that sent no Origin, which a browser does for
/// some navigations and `<script>`/`<img>` loads. Those never get the token.
/// curl gets it too, but curl is already local.
async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let mut sites = headers.get_all("sec-fetch-site").iter();
    let same_origin = match (sites.next(), sites.next()) {
        (None, _) => true,
        // A repeated header reaches Node joined with ", ", which matches neither value.
        (Some(site), None) => site == "same-origin" || site == "none",
        (Some(_), Some(_)) => false,
    };
    // Node checks the Origin header's truthiness, so an empty one counts as absent.
    let has_origin = headers
        .get("origin")
        .is_some_and(|origin| !origin.is_empty());
    if !same_origin && !has_origin {
        return Err(ApiError::forbidden("Cross-site request refused"));
    }
    Ok(Json(json!({ "token": state.token })))
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}
