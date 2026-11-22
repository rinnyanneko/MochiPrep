//! `POST /api/replay` (`app.ts` "demo & replay"), and `POST /api/replay/load`, which loads
//! the replay's bars the first time the replay is used.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use mochi_domain::ReplayState;

use crate::error::ApiError;
use crate::validate::{self, ReplayControl, Schema};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/replay", post(control))
        .route("/api/replay/load", post(load))
}

/// Starts loading the replay's bars, or does nothing if they are loaded or loading. Answers the
/// state at once; `downloading` tracks a download from OKX.
async fn load(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ReplayState>, ApiError> {
    validate::read_body(&headers, &body)?;
    Ok(Json(
        state
            .runtime
            .call(|r| {
                r.load_replay()?;
                Ok::<_, ApiError>(r.replay_state())
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
    if (input.playing == Some(true) || input.steps.is_some())
        && !state.runtime.replay_state().await?.loaded
    {
        return Err(ApiError::bad_request(crate::runtime::NOT_LOADED));
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
