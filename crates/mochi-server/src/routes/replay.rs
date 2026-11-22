//! `POST /api/replay` (`app.ts` "demo & replay"), and the replay's recorded bars: which are
//! saved, what loading other dates would take, loading them, and deleting saved ones.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use mochi_domain::ReplayState;

use crate::error::ApiError;
use crate::runtime::{ReplayDataView, ReplayPlan, ReplayRange};
use crate::validate::{self, ReplayControl, Schema};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/replay", post(control))
        .route("/api/replay/data", get(data))
        .route("/api/replay/estimate", post(estimate))
        .route("/api/replay/load", post(load))
        .route("/api/replay/data/delete", post(delete))
}

/// The dates in a body, or `None` for an empty one.
fn range(headers: &HeaderMap, body: &Bytes) -> Result<Option<ReplayRange>, ApiError> {
    match validate::read_body(headers, body)? {
        None => Ok(None),
        Some(value) if value.as_object().is_some_and(|o| o.is_empty()) => Ok(None),
        Some(value) => validate::parse_input(Schema::ReplayRange, Some(&value)).map(Some),
    }
}

async fn data(State(state): State<AppState>) -> Result<Json<ReplayDataView>, ApiError> {
    Ok(Json(state.runtime.call(|r| r.replay_data()).await??))
}

/// What loading `{from, to}` would download, how long that may take, and the disk it needs.
async fn estimate(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ReplayPlan>, ApiError> {
    let range = range(&headers, &body)?.ok_or_else(|| ApiError::bad_request("from: required"))?;
    Ok(Json(
        state.runtime.call(move |r| r.plan_replay(range)).await??,
    ))
}

/// Loads `{from, to}`, or the dates last picked for an empty body. Answers the state at once;
/// `downloading` tracks a download from OKX.
async fn load(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ReplayState>, ApiError> {
    let range = range(&headers, &body)?;
    Ok(Json(
        state
            .runtime
            .call(move |r| {
                r.load_replay(range)?;
                Ok::<_, ApiError>(r.replay_state())
            })
            .await??,
    ))
}

/// Deletes every pair's saved bars from `from` to `to`.
async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ReplayDataView>, ApiError> {
    let span = range(&headers, &body)?.ok_or_else(|| ApiError::bad_request("from: required"))?;
    Ok(Json(
        state
            .runtime
            .call(move |r| {
                r.delete_replay_data(span.from, span.to)?;
                r.replay_data()
            })
            .await??,
    ))
}

/// Speed first, then playing, then the steps, which the answer waits for, as in Node.
async fn control(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ReplayState>, ApiError> {
    let body = validate::read_body(&headers, &body)?;
    let input: ReplayControl = validate::parse_input(Schema::ReplayControl, body.as_ref())?;
    if input.playing == Some(true) || input.steps.is_some() {
        let replay = state.runtime.replay_state().await?;
        if !replay.loaded {
            return Err(ApiError::bad_request(crate::runtime::NOT_LOADED));
        }
        // Moving the loaded dates now would save a cursor the new dates then start from.
        if replay.downloading.is_some() {
            return Err(ApiError::conflict(
                "Wait for the replay download to finish before playing",
            ));
        }
    }
    if let Some(speed) = input.speed {
        state.runtime.set_speed(speed).await?;
    }
    if let Some(playing) = input.playing {
        state.runtime.set_playing(playing).await?;
    }
    if let Some(steps) = input.steps {
        state.runtime.step_replay(steps).await?;
    }
    Ok(Json(state.runtime.replay_state().await?))
}
