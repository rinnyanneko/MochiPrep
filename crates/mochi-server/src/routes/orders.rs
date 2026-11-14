//! Orders and positions (`app.ts` "orders"). Node sends these through `runtime.execution(id)`,
//! the paper engine or an OKX session, and so does this: through the `ExecutionEngine` trait.
//! An exchange account that is not connected answers 501 where Node would pick its engine,
//! until connecting is ported.
//!
//! An order the engine refuses is not an HTTP error: it comes back as a REJECTED order in the
//! result, with the reason, as in Node.
//!
//! Where untrusted text first reaches `Dec`: quantity, price, stopLoss, takeProfit and the
//! trailing stop's callback and activationPrice. Each has passed `decimalString`
//! (`^\d+(\.\d+)?$`, at most 40 characters) in `validate` first. `Dec::parse` itself also
//! refuses `0x…`, Infinity and NaN and caps exponents (accepting leading-zero exponents such as
//! `1e05`), so a bypassed schema would still not produce a value JS would not.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{delete, post, put};
use axum::{Json, Router};
use mochi_core::paper::PaperStore;
use mochi_domain::{OrderRequest, Topic};
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::runtime::ProtectionMeta;
use crate::validate::{self, ClosePositionInput, ProtectionInput, Schema};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/orders", post(place))
        .route("/api/orders/{id}", delete(cancel))
        .route("/api/positions/{account_id}/{symbol}/close", post(close))
        .route(
            "/api/positions/{account_id}/{symbol}/protection",
            put(protection),
        )
}

async fn place(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body = validate::read_body(&headers, &body)?;
    let input: OrderRequest = validate::parse_input(Schema::PlaceOrder, body.as_ref())?;
    let (account_id, symbol) = (input.account_id.clone(), input.symbol.clone());
    let target = state
        .runtime
        .call(move |runtime| {
            let account = runtime.require_account(&account_id)?;
            runtime.assert_tradable(&symbol, account.source)?;
            runtime.execution(&account)
        })
        .await??;
    let result = state.runtime.engine_for(target).place_order(input).await?;
    state.runtime.invalidate(TRADED).await?;
    Ok(Json(to_json(result)?))
}

/// What an order, fill or close can change.
const TRADED: &[Topic] = &[
    Topic::Accounts,
    Topic::Orders,
    Topic::Positions,
    Topic::Arena,
];

fn to_json(value: impl serde::Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|e| ApiError::bad_request(e.to_string()))
}

async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    validate::read_body(&headers, &body)?;
    let order_id = id.clone();
    let target = state
        .runtime
        .call(move |runtime| {
            let order = runtime
                .engine()
                .store()
                .get_order(&order_id)
                .map_err(|e| runtime.api_error(e))?
                .ok_or_else(|| ApiError::not_found("Unknown order"))?;
            // Node's `execution()` says "Unknown account" (400) when the order's account is gone.
            let account = runtime
                .require_account(&order.account_id)
                .map_err(|e| ApiError::bad_request(e.message()))?;
            runtime.execution(&account)
        })
        .await??;
    state.runtime.engine_for(target).cancel_order(id).await?;
    state
        .runtime
        .invalidate(&[Topic::Accounts, Topic::Orders])
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn close(
    State(state): State<AppState>,
    Path((account_id, symbol)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body = validate::read_body(&headers, &body)?;
    let input: ClosePositionInput = validate::parse_input(Schema::ClosePosition, body.as_ref())?;
    let symbol = validate::parse_symbol(&symbol)?;
    let engine = state.runtime.execution(&account_id).await?;
    let result = engine
        .close_position(account_id, symbol, input.fraction, input.client_order_id)
        .await?;
    state.runtime.invalidate(TRADED).await?;
    Ok(Json(to_json(result)?))
}

async fn protection(
    State(state): State<AppState>,
    Path((account_id, symbol)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body = validate::read_body(&headers, &body)?;
    let input: ProtectionInput = validate::parse_input(Schema::Protection, body.as_ref())?;
    let symbol = validate::parse_symbol(&symbol)?;
    let engine = state.runtime.execution(&account_id).await?;
    // An absent trailingStop keeps the current one, so a client that only edits SL/TP cannot
    // drop it; an explicit null removes it.
    let trailing_stop = match input.trailing_stop {
        Some(trailing_stop) => trailing_stop,
        None => {
            engine
                .protection_of(account_id.clone(), symbol.clone())
                .await?
                .trailing_stop
        }
    };
    let protection = mochi_domain::Protection {
        stop_loss: input.stop_loss,
        take_profit: input.take_profit,
        trailing_stop,
    };
    let meta = ProtectionMeta {
        client_order_id: format!("manual:{}", uuid::Uuid::new_v4()),
        agent_id: None,
        decision_id: None,
    };
    // A refusal reaches the client as a 400 with its reason.
    let result = engine
        .set_protection(account_id, symbol, protection, meta)
        .await?;
    state
        .runtime
        .invalidate(&[Topic::Orders, Topic::Positions])
        .await?;
    Ok(Json(json!({
        "ok": true,
        "pending": result.pending,
        "before": to_json(&result.before)?,
        "after": to_json(&result.after)?,
    })))
}
