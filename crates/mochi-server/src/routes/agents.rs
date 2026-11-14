//! Agent API parity. The asynchronous runtime runner owns all model-to-order paths.
use super::providers::{body, call};
use crate::{
    error::ApiError,
    runtime::{agents::CycleTrigger, DemoAction},
    validate::{self, AgentInput, Schema},
    AppState,
};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post, put},
    Json, Router,
};
use mochi_store::rows;
use serde_json::{json, Value};
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/agents", get(list).post(create))
        .route("/api/agents/{id}", put(update).delete(remove))
        .route("/api/agents/stop-all", post(stop_all))
        .route(
            "/api/agents/{id}/start",
            post(|s, p, h, b| action(s, p, h, b, "start")),
        )
        .route(
            "/api/agents/{id}/pause",
            post(|s, p, h, b| action(s, p, h, b, "pause")),
        )
        .route(
            "/api/agents/{id}/resume",
            post(|s, p, h, b| action(s, p, h, b, "resume")),
        )
        .route(
            "/api/agents/{id}/stop",
            post(|s, p, h, b| action(s, p, h, b, "stop")),
        )
        .route(
            "/api/agents/{id}/analyze",
            post(|s, p, h, b| action(s, p, h, b, "analyze")),
        )
        .route(
            "/api/agents/{id}/run",
            post(|s, p, h, b| action(s, p, h, b, "run")),
        )
        .route("/api/signals/{id}/execute", post(execute))
        .route("/api/signals/{id}/dismiss", post(dismiss))
        .route(
            "/api/demo/start",
            post(|s, h, b| demo(s, h, b, DemoAction::Start)),
        )
        .route(
            "/api/demo/reset",
            post(|s, h, b| demo(s, h, b, DemoAction::Reset)),
        )
        .route(
            "/api/demo/exit",
            post(|s, h, b| demo(s, h, b, DemoAction::Exit)),
        )
}
async fn list(State(state): State<AppState>) -> Result<Json<Vec<rows::Agent>>, ApiError> {
    Ok(Json(
        call(&state, |r| {
            rows::list_agents(r.connection()).map_err(|e| r.api_error(e))
        })
        .await?,
    ))
}
async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<rows::Agent>, ApiError> {
    let body = body(&state, &headers, &bytes).await?;
    Ok(Json(
        call(&state, move |r| {
            let input: AgentInput = validate::parse_input(Schema::AgentInput, body.as_ref())?;
            r.create_agent(input, false)
        })
        .await?,
    ))
}
async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<rows::Agent>, ApiError> {
    let body = body(&state, &headers, &bytes).await?;
    Ok(Json(
        call(&state, move |r| {
            let input: AgentInput = validate::parse_input(Schema::AgentInput, body.as_ref())?;
            r.update_agent(&id, input)
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
    call(&state, move |r| r.delete_agent(&id)).await?;
    Ok(Json(json!({"ok":true})))
}
async fn action(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
    action: &'static str,
) -> Result<Json<Value>, ApiError> {
    body(&state, &headers, &bytes).await?;
    if action == "run" || action == "analyze" {
        let id = call(&state, move |r| Ok(r.api_agent(&id)?.id)).await?;
        let result = state
            .runtime
            .run_cycle(
                id,
                if action == "run" {
                    CycleTrigger::RunOnce
                } else {
                    CycleTrigger::Analyze
                },
            )
            .await;
        return Ok(Json(
            call(&state, move |_| {
                result?
                    .ok_or_else(|| ApiError::conflict("A cycle is already running for this agent"))
            })
            .await?,
        ));
    }
    Ok(Json(
        call(&state, move |r| {
            r.api_agent(&id)?;
            match action {
                "start" => r.start_agent(&id)?,
                "resume" => r.resume_agent(&id)?,
                "pause" => r.pause_agent(&id, Some("Paused by user".into()))?,
                "stop" => r.stop_agent(&id)?,
                _ => return Err(ApiError::not_found("Not Found")),
            };
            serde_json::to_value(r.api_agent(&id)?).map_err(|e| r.api_error(e))
        })
        .await?,
    ))
}
async fn stop_all(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    body(&state, &headers, &bytes).await?;
    Ok(Json(
        call(&state, |r| {
            let stopped = r.stop_all_agents()?;
            r.set_playing(false);
            // Stop All must leave nothing able to trade real money by accident.
            r.disarm(None);
            Ok(json!({"stopped":stopped}))
        })
        .await?,
    ))
}
async fn execute(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    body(&state, &headers, &bytes).await?;
    let signal = id.clone();
    // Checked before the suggestion is used, so a refusal does not burn it.
    call(&state, move |r| {
        let record = r.api_signal(&signal)?;
        r.assert_can_execute_signal(&record)
    })
    .await?;
    Ok(Json(state.runtime.execute_signal(id).await?))
}
async fn dismiss(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    body(&state, &headers, &bytes).await?;
    Ok(Json(
        call(&state, move |r| {
            r.api_signal(&id)?;
            r.dismiss_signal(&id)
        })
        .await?,
    ))
}
async fn demo(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
    action: DemoAction,
) -> Result<Json<Value>, ApiError> {
    body(&state, &headers, &bytes).await?;
    state.runtime.change_demo(action).await?;
    Ok(Json(json!({"demoActive": action != DemoAction::Exit})))
}
