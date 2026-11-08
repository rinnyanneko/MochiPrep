//! `GET /api/instruments` (`app.ts` "trading pairs"). Adding and removing pairs needs the OKX
//! public instrument list and comes with the OKX public adapter.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use mochi_domain::InstrumentSpecification;

use crate::error::ApiError;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/instruments", get(list))
}

/// Built-ins first, then added pairs, as `Object.values(INSTRUMENTS)`.
async fn list(
    State(state): State<AppState>,
) -> Result<Json<Vec<InstrumentSpecification>>, ApiError> {
    let specs = state
        .runtime
        .call(|runtime| runtime.instruments().to_vec())
        .await?;
    Ok(Json(specs))
}
