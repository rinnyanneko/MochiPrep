//! Agent templates: saved agent form settings. No Node counterpart. A template is only data the
//! web app copies into the agent form, so these routes never create agents or touch orders: an
//! agent made from a template still goes through `POST /api/agents` and its validation, and its
//! orders through the risk engine.
use super::providers::{body, call};
use crate::{
    error::ApiError,
    runtime::Runtime,
    validate::{self, Schema},
    AppState,
};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post, put},
    Json, Router,
};
use mochi_domain::Topic;
use mochi_store::templates::{self, AgentTemplate, AgentTemplateFields, BUILTIN_PREFIX};
use serde_json::{json, Value};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/agent-templates", get(list).post(create))
        .route("/api/agent-templates/{id}", put(update).delete(remove))
        .route(
            "/api/agent-templates/restore-builtins",
            post(restore_builtins),
        )
}

fn input(body: Option<&Value>) -> Result<AgentTemplateFields, ApiError> {
    validate::parse_input(Schema::AgentTemplateInput, body)
}

fn changed(r: &mut Runtime) {
    r.invalidate(&[Topic::Templates]);
}

async fn list(State(state): State<AppState>) -> Result<Json<Vec<AgentTemplate>>, ApiError> {
    Ok(Json(
        call(&state, |r| {
            templates::list_agent_templates(r.connection()).map_err(|e| r.api_error(e))
        })
        .await?,
    ))
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<AgentTemplate>, ApiError> {
    let body = body(&state, &headers, &bytes).await?;
    Ok(Json(
        call(&state, move |r| {
            let fields = input(body.as_ref())?;
            let id = format!("tpl_{}", uuid::Uuid::new_v4());
            let saved = templates::insert_agent_template(r.connection(), &id, &fields)
                .map_err(|e| r.api_error(e))?;
            changed(r);
            Ok(saved)
        })
        .await?,
    ))
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<AgentTemplate>, ApiError> {
    let body = body(&state, &headers, &bytes).await?;
    Ok(Json(
        call(&state, move |r| {
            // Built-ins are read-only so a release can keep improving them; the web app offers
            // Duplicate, which saves an editable copy.
            if id.starts_with(BUILTIN_PREFIX) {
                return Err(ApiError::conflict(
                    "Built-in templates cannot be edited; duplicate it to make your own copy",
                ));
            }
            let fields = input(body.as_ref())?;
            let saved = templates::update_agent_template(r.connection(), &id, &fields)
                .map_err(|e| r.api_error(e))?
                .ok_or_else(|| ApiError::not_found("Unknown template"))?;
            changed(r);
            Ok(saved)
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
        if !templates::delete_agent_template(r.connection(), &id).map_err(|e| r.api_error(e))? {
            return Err(ApiError::not_found("Unknown template"));
        }
        changed(r);
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok":true})))
}

async fn restore_builtins(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    body(&state, &headers, &bytes).await?;
    let restored = call(&state, |r| {
        let restored = templates::restore_builtin_agent_templates(r.connection())
            .map_err(|e| r.api_error(e))?;
        changed(r);
        Ok(restored)
    })
    .await?;
    Ok(Json(json!({"restored":restored})))
}
