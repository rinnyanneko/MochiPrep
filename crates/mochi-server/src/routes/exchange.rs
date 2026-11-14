//! Exchange accounts (`app.ts` "exchange accounts"): status, connect, disconnect, arm and
//! disarm, each with Node's check order. Fastify parses the body first, then the handler checks
//! the account, then the schema.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use super::providers::{body, call};
use crate::error::ApiError;
use crate::runtime::{ExchangeStatus, Runtime};
use crate::validate::{self, ArmInput, OkxConnectInput, Schema};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/exchange/status", get(status))
        .route("/api/exchange/okx/connect", post(connect))
        .route("/api/exchange/okx/{id}/disconnect", post(disconnect))
        .route("/api/exchange/okx/{id}/arm", post(arm))
        .route("/api/exchange/okx/{id}/disarm", post(disarm))
}

/// Node's `requireAccount`: 404 `Unknown account`.
fn require_account(r: &Runtime, id: &str) -> Result<(), ApiError> {
    r.require_account(id).map(|_| ())
}

async fn status(State(state): State<AppState>) -> Result<Json<Vec<ExchangeStatus>>, ApiError> {
    Ok(Json(call(&state, |r| r.exchange_statuses()).await?))
}

async fn connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body = body(&state, &headers, &bytes).await?;
    let input: OkxConnectInput = match validate::parse_input(Schema::OkxConnect, body.as_ref()) {
        Ok(input) => input,
        Err(error) => return call(&state, move |_| Err(error)).await,
    };
    let account = state.runtime.connect_okx(input, None).await?;
    Ok(Json(
        call(&state, move |r| r.account_view(&account.id)).await?,
    ))
}

async fn disconnect(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    body(&state, &headers, &bytes).await?;
    call(&state, move |r| {
        require_account(r, &id)?;
        r.disconnect_exchange(&id)
    })
    .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn arm(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body = body(&state, &headers, &bytes).await?;
    call(&state, move |r| {
        require_account(r, &id)?;
        let input: ArmInput = validate::parse_input(Schema::Arm, body.as_ref())?;
        r.arm(&id, &input.confirmation)
    })
    .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn disarm(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    body(&state, &headers, &bytes).await?;
    call(&state, move |r| {
        require_account(r, &id)?;
        r.disarm(Some(&id));
        Ok(())
    })
    .await?;
    Ok(Json(json!({ "ok": true })))
}
