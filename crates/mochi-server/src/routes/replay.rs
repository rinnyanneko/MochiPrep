//! `POST /api/replay` (`app.ts` "demo & replay"). Demo mode needs agents and comes later.

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
    Router::new().route("/api/replay", post(control))
}

/// Speed first, then playing, then the steps, which the answer waits for, as in Node.
async fn control(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ReplayState>, ApiError> {
    let body = validate::read_body(&headers, &body)?;
    let input: ReplayControl = validate::parse_input(Schema::ReplayControl, body.as_ref())?;
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
