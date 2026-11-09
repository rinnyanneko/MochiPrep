//! Orders and positions (`app.ts` "orders"). Node sends these through `runtime.execution(id)`,
//! the paper engine or an OKX session; OKX trading is not ported, so an exchange account
//! answers 501 at the point where Node would pick its OKX engine.
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
use mochi_domain::{AccountVenue, OrderRequest, PaperAccount, Topic};
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::runtime::Runtime;
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

/// Node's `runtime.execution(id)`: only paper accounts trade here for now.
fn paper_only(account: &PaperAccount) -> Result<(), ApiError> {
    if account.venue == AccountVenue::Paper {
        Ok(())
    } else {
        Err(ApiError::not_available())
    }
}

fn to_json(runtime: &Runtime, value: impl serde::Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|e| runtime.api_error(e))
}

async fn place(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body = validate::read_body(&headers, &body)?;
    let input: OrderRequest = validate::parse_input(Schema::PlaceOrder, body.as_ref())?;
    let result = state
        .runtime
        .call(move |runtime| {
            let account = runtime.require_account(&input.account_id)?;
            runtime.assert_tradable(&input.symbol, account.source)?;
            paper_only(&account)?;
            let result = runtime
                .engine_mut()
                .place_order(input)
                .map_err(|e| runtime.api_error(e))?;
            runtime.invalidate(&[
                Topic::Accounts,
                Topic::Orders,
                Topic::Positions,
                Topic::Arena,
            ]);
            to_json(runtime, result)
        })
        .await??;
    Ok(Json(result))
}

async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    validate::read_body(&headers, &body)?;
    state
        .runtime
        .call(move |runtime| {
            let order = runtime
                .engine()
                .store()
                .get_order(&id)
                .map_err(|e| runtime.api_error(e))?
                .ok_or_else(|| ApiError::not_found("Unknown order"))?;
            // Node's `execution()` says "Unknown account" (400) when the order's account is gone.
            let account = runtime
                .require_account(&order.account_id)
                .map_err(|e| ApiError::bad_request(e.message()))?;
            paper_only(&account)?;
            runtime
                .engine_mut()
                .cancel_order(&order.id)
                .map_err(|e| runtime.api_error(e))?;
            runtime.invalidate(&[Topic::Accounts, Topic::Orders]);
            Ok::<_, ApiError>(())
        })
        .await??;
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
    let result = state
        .runtime
        .call(move |runtime| {
            let account = runtime.require_account(&account_id)?;
            paper_only(&account)?;
            let result = runtime
                .engine_mut()
                .close_position(&account_id, &symbol, input.fraction, &input.client_order_id)
                .map_err(|e| runtime.api_error(e))?;
            runtime.invalidate(&[
                Topic::Accounts,
                Topic::Orders,
                Topic::Positions,
                Topic::Arena,
            ]);
            to_json(runtime, result)
        })
        .await??;
    Ok(Json(result))
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
    let answer = state
        .runtime
        .call(move |runtime| {
            let account = runtime.require_account(&account_id)?;
            paper_only(&account)?;
            // An absent trailingStop keeps the current one, so a client that only edits SL/TP
            // cannot drop it; an explicit null removes it.
            let trailing_stop = match input.trailing_stop {
                Some(trailing_stop) => trailing_stop,
                None => {
                    runtime
                        .engine()
                        .protection_of(&account_id, &symbol)
                        .map_err(|e| runtime.api_error(e))?
                        .trailing_stop
                }
            };
            let protection = mochi_domain::Protection {
                stop_loss: input.stop_loss,
                take_profit: input.take_profit,
                trailing_stop,
            };
            // A refusal reaches the client as a 400 with its reason.
            let result = runtime
                .engine_mut()
                .set_protection(&account_id, &symbol, &protection)
                .map_err(|e| runtime.api_error(e))?;
            runtime.invalidate(&[Topic::Orders, Topic::Positions]);
            Ok::<_, ApiError>(json!({
                "ok": true,
                "pending": result.pending,
                "before": to_json(runtime, &result.before)?,
                "after": to_json(runtime, &result.after)?,
            }))
        })
        .await??;
    Ok(Json(answer))
}
