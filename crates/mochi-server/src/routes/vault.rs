//! The master password routes (`app.ts` "master password"). Each answers `{ status }` read after
//! the action, as Node does; unlock adds the saved OKX accounts that could not reconnect.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use mochi_domain::VaultStatus;
use serde_json::{json, Value};

use super::providers::{body, call};
use crate::error::ApiError;
use crate::runtime::UnlockOutcome;
use crate::validate::{
    self, Schema, VaultChangeInput, VaultCreateInput, VaultPasswordInput, VaultResetInput,
};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/vault", get(status).post(enable))
        .route("/api/vault/unlock", post(unlock))
        .route("/api/vault/password", post(change_password))
        .route("/api/vault/disable", post(disable))
        .route("/api/vault/reset", post(reset))
}

/// The body through the schema, its issues redacted as Node's error handler does.
async fn parse<T: serde::de::DeserializeOwned + Send + 'static>(
    state: &AppState,
    schema: Schema,
    headers: &HeaderMap,
    bytes: &[u8],
) -> Result<T, ApiError> {
    let body = body(state, headers, bytes).await?;
    match validate::parse_input(schema, body.as_ref()) {
        Ok(input) => Ok(input),
        Err(error) => call(state, move |_| Err(error)).await,
    }
}

fn answer(status: VaultStatus) -> Json<Value> {
    Json(json!({ "status": status }))
}

async fn status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(answer(call(&state, |r| r.vault_status()).await?))
}

async fn enable(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    let mut input: VaultCreateInput = parse(&state, Schema::VaultCreate, &headers, &bytes).await?;
    let password = std::mem::take(&mut input.password);
    Ok(answer(state.runtime.enable_vault(password).await?))
}

async fn unlock(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<UnlockOutcome>, ApiError> {
    let mut input: VaultPasswordInput =
        parse(&state, Schema::VaultPassword, &headers, &bytes).await?;
    let password = std::mem::take(&mut input.password);
    Ok(Json(state.runtime.unlock(password).await?))
}

async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    let mut input: VaultChangeInput = parse(&state, Schema::VaultChange, &headers, &bytes).await?;
    let current = std::mem::take(&mut input.current);
    let next = std::mem::take(&mut input.next);
    Ok(answer(state.runtime.change_password(current, next).await?))
}

async fn disable(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    let mut input: VaultPasswordInput =
        parse(&state, Schema::VaultPassword, &headers, &bytes).await?;
    let password = std::mem::take(&mut input.password);
    Ok(answer(state.runtime.disable_vault(password).await?))
}

async fn reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    let input: VaultResetInput = parse(&state, Schema::VaultReset, &headers, &bytes).await?;
    Ok(answer(state.runtime.reset_vault(input.confirmation).await?))
}
