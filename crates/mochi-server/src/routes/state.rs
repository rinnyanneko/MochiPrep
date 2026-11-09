//! `GET /api/state` (`app.ts` "session / health / state").

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::error::ApiError;
use crate::runtime::StateBody;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/state", get(state))
}

async fn state(State(state): State<AppState>) -> Result<Json<StateBody>, ApiError> {
    Ok(Json(state.runtime.state().await?))
}
