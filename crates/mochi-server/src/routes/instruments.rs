//! `/api/instruments` (`app.ts` "trading pairs"): the registered pairs, OKX's list of pairs that
//! could be added, and adding or removing one.
//!
//! The OKX list is awaited here, outside the runtime task, so a slow OKX never holds up quotes
//! or other requests. Specs always come from OKX at the moment of adding, never from a guess.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get};
use axum::{Json, Router};
use mochi_domain::InstrumentSpecification;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::validate::{self, AddInstrumentInput, Schema};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/instruments", get(list).post(add))
        .route("/api/instruments/available", get(available))
        .route("/api/instruments/{symbol}", delete(remove))
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

/// A failed OKX request as Node's error handler answers it: a 400 with secrets removed.
async fn feed_error(state: &AppState, message: String) -> ApiError {
    match state
        .runtime
        .call(move |runtime| runtime.api_error(message))
        .await
    {
        Ok(error) | Err(error) => error,
    }
}

/// `availableInstruments`: OKX's list (5 min cache) minus the pairs already registered.
async fn available(
    State(state): State<AppState>,
) -> Result<Json<Vec<InstrumentSpecification>>, ApiError> {
    let live = state.runtime.call(|runtime| runtime.live()).await?;
    let specs = match live.list_instruments(false).await {
        Ok(specs) => specs,
        Err(message) => return Err(feed_error(&state, message).await),
    };
    let specs = state
        .runtime
        .call(move |runtime| {
            specs
                .into_iter()
                .filter(|spec| !runtime.has_instrument(&spec.symbol))
                .collect::<Vec<_>>()
        })
        .await?;
    Ok(Json(specs))
}

/// `addInstrument`: refused if already added, then looked up in a fresh OKX list.
async fn add(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<InstrumentSpecification>, ApiError> {
    let body = validate::read_body(&headers, &body)?;
    let input: AddInstrumentInput = validate::parse_input(Schema::AddInstrument, body.as_ref())?;
    let symbol = input.symbol;
    let live = {
        let symbol = symbol.clone();
        state
            .runtime
            .call(move |runtime| {
                if runtime.has_instrument(&symbol) {
                    return Err(ApiError::bad_request(format!("{symbol} is already added")));
                }
                Ok(runtime.live())
            })
            .await??
    };
    let specs = match live.list_instruments(true).await {
        Ok(specs) => specs,
        Err(message) => return Err(feed_error(&state, message).await),
    };
    let Some(spec) = specs.into_iter().find(|spec| spec.symbol == symbol) else {
        return Err(ApiError::bad_request(format!(
            "{symbol} is not a live USDT crypto perpetual on OKX"
        )));
    };
    let saved = spec.clone();
    state
        .runtime
        .call(move |runtime| runtime.add_instrument(saved))
        .await??;
    Ok(Json(spec))
}

async fn remove(
    State(state): State<AppState>,
    Path(symbol): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    validate::read_body(&headers, &body)?;
    let symbol = validate::parse_symbol(&symbol)?;
    state
        .runtime
        .call(move |runtime| runtime.remove_instrument(&symbol))
        .await??;
    Ok(Json(json!({ "ok": true })))
}
