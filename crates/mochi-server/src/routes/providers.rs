//! Provider configuration, key ownership and nonblocking connection tests.
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post, put},
    Json, Router,
};
use mochi_ai::{redact_secrets, AiRequest, ProviderConfig, ProviderKind};
use mochi_domain::{Topic, VaultStatus};
use mochi_store::rows;

use crate::{
    error::ApiError,
    runtime::Runtime,
    validate::{self, ProviderInput, ProviderKeyInput, ProviderTestInput, Schema},
    AppState,
};
use serde_json::{json, Value};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/providers", get(list).post(create))
        .route("/api/providers/{id}", put(update).delete(remove))
        .route("/api/providers/{id}/key", put(set_key).delete(delete_key))
        .route("/api/providers/{id}/test", post(test))
}
/// Schema diagnostics can contain untrusted field names or values; Node's top-level handler
/// redacts them with all loaded credentials, preserving the original HTTP status.
pub(super) async fn call<T: Send + 'static>(
    state: &AppState,
    work: impl FnOnce(&mut Runtime) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    state
        .runtime
        .call(move |r| work(r).map_err(|e| ApiError::new(e.status(), r.redact(e.message()))))
        .await?
}
pub(super) async fn body(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Option<Value>, ApiError> {
    match validate::read_body(headers, body) {
        Ok(v) => Ok(v),
        Err(e) => call(state, move |_| Err(e)).await,
    }
}
fn views(r: &Runtime) -> Result<Vec<rows::ProviderView>, ApiError> {
    rows::list_provider_views(r.connection(), r.vault_status()? != VaultStatus::Off)
        .map_err(|e| r.api_error(e))
}
fn view(r: &Runtime, id: &str) -> Result<rows::ProviderView, ApiError> {
    views(r)?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::not_found("Unknown provider"))
}
fn require(r: &Runtime, id: &str) -> Result<ProviderConfig, ApiError> {
    rows::get_provider(r.connection(), id)
        .map_err(|e| r.api_error(e))?
        .ok_or_else(|| ApiError::not_found("Unknown provider"))
}
fn editable(r: &Runtime, id: &str) -> Result<ProviderConfig, ApiError> {
    if id == rows::MOCK_PROVIDER_ID {
        return Err(ApiError::bad_request("The mock provider is built in"));
    }
    require(r, id)
}
async fn list(State(state): State<AppState>) -> Result<Json<Vec<rows::ProviderView>>, ApiError> {
    Ok(Json(call(&state, |r| views(r)).await?))
}
fn config(input: ProviderInput, id: String, created_at: i64) -> ProviderConfig {
    ProviderConfig {
        id,
        name: input.name,
        kind: input.kind,
        base_url: input.base_url,
        default_model: input.default_model,
        input_price_per_m_tok: input.input_price_per_m_tok,
        output_price_per_m_tok: input.output_price_per_m_tok,
        extra_headers: input.extra_headers,
        created_at,
    }
}
async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<rows::ProviderView>, ApiError> {
    let body = body(&state, &headers, &bytes).await?;
    Ok(Json(
        call(&state, move |r| {
            let input = validate::parse_input(Schema::ProviderInput, body.as_ref())?;
            let p = config(input, format!("prov_{}", uuid::Uuid::new_v4()), now());
            rows::insert_provider(r.connection(), &p).map_err(|e| r.api_error(e))?;
            r.invalidate(&[Topic::Providers]);
            view(r, &p.id)
        })
        .await?,
    ))
}
async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<rows::ProviderView>, ApiError> {
    let body = body(&state, &headers, &bytes).await?;
    Ok(Json(
        call(&state, move |r| {
            let current = editable(r, &id)?;
            let input = validate::parse_input(Schema::ProviderInput, body.as_ref())?;
            let mut p = config(input, id.clone(), current.created_at);
            p.extra_headers = p
                .extra_headers
                .or(current.extra_headers)
                .or_else(|| Some(Default::default()));
            rows::save_provider(r.connection(), &p).map_err(|e| r.api_error(e))?;
            r.invalidate(&[Topic::Providers]);
            view(r, &id)
        })
        .await?,
    ))
}
async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    body(&state, &headers, &bytes).await?;
    call(&state, move |r| {
        editable(r, &id)?;
        if rows::list_agents(r.connection())
            .map_err(|e| r.api_error(e))?
            .iter()
            .any(|a| a.provider_id == id)
        {
            return Err(ApiError::bad_request("Agents still use this provider"));
        }
        r.delete_key(&id)?;
        rows::delete_provider(r.connection(), &id).map_err(|e| r.api_error(e))?;
        r.invalidate(&[Topic::Providers]);
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok":true})))
}
async fn set_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body = body(&state, &headers, &bytes).await?;
    call(&state, move |r| {
        editable(r, &id)?;
        let input: ProviderKeyInput = validate::parse_input(Schema::ProviderKey, body.as_ref())?;
        r.set_key(&id, &input.api_key)
    })
    .await?;
    Ok(Json(json!({"ok":true})))
}
async fn delete_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    body(&state, &headers, &bytes).await?;
    call(&state, move |r| {
        editable(r, &id)?;
        r.delete_key(&id)
    })
    .await?;
    Ok(Json(json!({"ok":true})))
}
async fn test(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body = body(&state, &headers, &bytes).await?;
    let (provider, model, is_mock, secrets) = call(&state, move |r| {
        let c = require(r, &id)?;
        let empty = json!({});
        let input: ProviderTestInput = validate::parse_input(
            Schema::ProviderTest,
            Some(body.as_ref().filter(|v| !v.is_null()).unwrap_or(&empty)),
        )?;
        let p = r.provider_for(&c).map_err(|e| r.api_error(e))?;
        Ok((
            p,
            input.model.unwrap_or(c.default_model),
            c.kind == ProviderKind::Mock,
            r.secrets(),
        ))
    })
    .await?;
    if is_mock {
        return Ok(Json(
            json!({"ok":true,"sample":"Mock provider needs no connection."}),
        ));
    }
    let request = AiRequest {
        model,
        system: "Reply with this JSON object and nothing else: {\"ok\":true}".into(),
        user: "Connection test. Answer in JSON.".into(),
        context: None,
    };
    let response = provider.complete(&request).await;
    let text = call(&state, move |r| {
        let all = secrets.into_iter().chain(r.secrets()).collect::<Vec<_>>();
        let clean = |s: &str| redact_secrets(s, all.iter().map(String::as_str));
        response
            .map(|v| clean(&v.text))
            .map_err(|e| ApiError::bad_request(clean(&e.message)))
    })
    .await?;
    Ok(Json(json!({"ok":true,"sample":cut_sample(&text)})))
}
fn cut_sample(text: &str) -> String {
    let mut n = 0;
    text.chars()
        .take_while(|c| {
            n += c.len_utf16();
            n <= 120
        })
        .collect()
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis()
        .try_into()
        .expect("timestamp")
}
