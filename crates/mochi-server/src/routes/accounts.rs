//! `/api/accounts` and the per-account history reads (`app.ts` "accounts").
//!
//! Each route checks things in Node's order: Fastify parses the body before the handler runs,
//! then the handler looks the account up (404) before it parses the query.

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use mochi_core::ledger::audit_ledger;
use mochi_core::paper::{CreateAccountArgs, PaperStore, StoreFailure};
use mochi_domain::{AccountVenue, Topic};
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::runtime::Runtime;
use crate::validate::{self, CreateAccountInput, LimitQuery, Schema};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/accounts", get(list).post(create))
        .route("/api/accounts/{id}", delete(remove))
        .route("/api/accounts/{id}/reset", post(reset))
        .route("/api/accounts/{id}/positions", get(positions))
        .route("/api/accounts/{id}/closed-positions", get(closed_positions))
        .route("/api/accounts/{id}/orders", get(orders))
        .route("/api/accounts/{id}/fills", get(fills))
        .route("/api/accounts/{id}/ledger", get(ledger))
        .route("/api/accounts/{id}/equity", get(equity))
        .route("/api/accounts/{id}/audit", get(audit))
}

async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let views = state
        .runtime
        .call(|runtime| {
            let accounts = runtime
                .engine()
                .store()
                .list_accounts()
                .map_err(|e| runtime.api_error(e))?;
            accounts
                .iter()
                .map(|account| runtime.account_view(&account.id))
                .collect::<Result<Vec<_>, _>>()
        })
        .await??;
    Ok(Json(Value::Array(views)))
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body = validate::read_body(&headers, &body)?;
    // Untrusted text reaches `Dec` here: the schema has already held `initialBalance` to
    // `^\d+(\.\d+)?$`, so the engine's parse sees no sign, exponent, hex, Infinity or NaN.
    let input: CreateAccountInput = validate::parse_input(Schema::CreateAccount, body.as_ref())?;
    let view = state
        .runtime
        .call(move |runtime| {
            let account = runtime
                .engine_mut()
                .create_account(CreateAccountArgs {
                    name: input.name,
                    source: input.source,
                    initial_balance: Some(input.initial_balance),
                    is_demo: None,
                    config: None,
                })
                .map_err(|e| runtime.api_error(e))?;
            runtime.invalidate(&[Topic::Accounts]);
            runtime.account_view(&account.id)
        })
        .await??;
    Ok(Json(view))
}

async fn reset(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    // No body is expected, but Fastify still refuses one it cannot parse.
    validate::read_body(&headers, &body)?;
    let view = state
        .runtime
        .call(move |runtime| {
            runtime.require_account(&id)?;
            runtime
                .engine_mut()
                .reset_account(&id)
                .map_err(|e| runtime.api_error(e))?;
            runtime.invalidate(&[
                Topic::Accounts,
                Topic::Orders,
                Topic::Positions,
                Topic::Decisions,
                Topic::Arena,
            ]);
            runtime.account_view(&id)
        })
        .await??;
    Ok(Json(view))
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    validate::read_body(&headers, &body)?;
    state
        .runtime
        .call(move |runtime| {
            runtime.require_account(&id)?;
            if has_agents(runtime, &id)? {
                return Err(ApiError::bad_request(
                    "Delete the agents using this account first",
                ));
            }
            // Its OKX session and saved keys go first, as in Node (for a paper account there
            // are none).
            runtime.disconnect_exchange(&id)?;
            let store = runtime.engine().store();
            store
                .transaction(|| -> Result<(), StoreFailure> {
                    store.delete_account_data(&id, false)
                })
                .map_err(|e| runtime.api_error(e))?;
            runtime.invalidate(&[Topic::Accounts, Topic::Orders, Topic::Positions]);
            Ok(())
        })
        .await??;
    Ok(Json(json!({ "ok": true })))
}

/// Node's `store.listAgents().some((a) => a.accountId === id)`. The agents table is read
/// directly: agents themselves are not ported yet, but an account they trade must stay.
fn has_agents(runtime: &Runtime, account_id: &str) -> Result<bool, ApiError> {
    runtime
        .connection()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM agents WHERE account_id=?)",
            [account_id],
            |row| row.get(0),
        )
        .map_err(|e| runtime.api_error(e))
}

async fn positions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let account_id = id.clone();
    let target = state
        .runtime
        .call(move |runtime| {
            let account = runtime.require_account(&account_id)?;
            match runtime.execution(&account) {
                Ok(target) => Ok(Some(target)),
                // An exchange account with no session has no positions to show (Node's
                // `positionsOf`).
                Err(_) if account.venue != AccountVenue::Paper => Ok(None),
                Err(error) => Err(error),
            }
        })
        .await??;
    let Some(target) = target else {
        return Ok(Json(json!([])));
    };
    let views = state.runtime.engine_for(target).positions(id).await?;
    Ok(Json(
        serde_json::to_value(views).map_err(|e| ApiError::bad_request(e.to_string()))?,
    ))
}

/// The shape of every `LimitQuery` read: the account first (404), then the query (400).
async fn limited_read(
    state: AppState,
    id: String,
    query: Option<String>,
    read: impl FnOnce(&Runtime, &str, usize) -> Result<Value, ApiError> + Send + 'static,
) -> Result<Json<Value>, ApiError> {
    let value = state
        .runtime
        .call(move |runtime| {
            runtime.require_account(&id)?;
            let LimitQuery { limit } =
                validate::parse_query(Schema::LimitQuery, query.as_deref().unwrap_or(""))?;
            read(runtime, &id, limit)
        })
        .await??;
    Ok(Json(value))
}

/// A store read as JSON, its failure as Node's 400.
fn store_json<T: serde::Serialize>(
    runtime: &Runtime,
    result: Result<T, StoreFailure>,
) -> Result<Value, ApiError> {
    let value = result.map_err(|e| runtime.api_error(e))?;
    serde_json::to_value(value).map_err(|e| runtime.api_error(e))
}

async fn closed_positions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    limited_read(state, id, query, |runtime, id, limit| {
        store_json(
            runtime,
            runtime.engine().store().list_closed_positions(id, limit),
        )
    })
    .await
}

async fn orders(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    limited_read(state, id, query, |runtime, id, limit| {
        store_json(runtime, runtime.engine().store().list_orders(id, limit))
    })
    .await
}

async fn fills(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    limited_read(state, id, query, |runtime, id, limit| {
        store_json(runtime, runtime.engine().store().list_fills(id, limit))
    })
    .await
}

/// The newest `limit` events, newest first (`listLedger(id).slice(-limit).reverse()`).
async fn ledger(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    limited_read(state, id, query, |runtime, id, limit| {
        let events = runtime
            .engine()
            .store()
            .list_ledger(id, None)
            .map(|mut events| {
                let newest = events.split_off(events.len().saturating_sub(limit));
                newest.into_iter().rev().collect::<Vec<_>>()
            });
        store_json(runtime, events)
    })
    .await
}

async fn equity(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    limited_read(state, id, query, |runtime, id, limit| {
        store_json(runtime, runtime.engine().store().list_equity(id, limit))
    })
    .await
}

async fn audit(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let body = state
        .runtime
        .call(move |runtime| {
            let account = runtime.require_account(&id)?;
            // The exchange keeps the books for its accounts, so there is no local ledger to audit.
            if account.venue != AccountVenue::Paper {
                return Ok(json!({ "consistent": true, "issues": [], "notApplicable": true }));
            }
            let events = runtime
                .engine()
                .store()
                .list_ledger(&id, None)
                .map_err(|e| runtime.api_error(e))?;
            let issues = audit_ledger(&account, &events).map_err(|e| runtime.api_error(e))?;
            Ok(json!({ "consistent": issues.is_empty(), "issues": issues }))
        })
        .await??;
    Ok(Json(body))
}
