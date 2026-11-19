use axum::extract::{rejection::JsonRejection, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::{operation_log, ApiError, AppState};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    retention: i32,
}

#[derive(Serialize)]
struct Preference {
    retention: i32,
    available: bool,
}

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/api/settings/operation-logs", get(read).put(update))
}

async fn read(State(state): State<AppState>) -> Result<Json<Preference>, ApiError> {
    let retention = state
        .runtime
        .call(|runtime| {
            operation_log::retention(runtime.connection()).map_err(|error| runtime.api_error(error))
        })
        .await??;
    Ok(Json(Preference {
        retention,
        available: state.operation_logs.is_some(),
    }))
}

async fn update(
    State(state): State<AppState>,
    input: Result<Json<Update>, JsonRejection>,
) -> Result<Json<Preference>, ApiError> {
    let Json(Update { retention }) =
        input.map_err(|_| ApiError::bad_request("Enter an integer for log retention"))?;
    if retention < -1 {
        return Err(ApiError::bad_request(
            "Log retention must be -1, 0, or a positive integer",
        ));
    }
    let log = state.operation_logs.ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Operation logs are unavailable",
        )
    })?;
    // An HTTP disconnect must not interrupt the handoff after persistence.
    tokio::spawn(async move {
        let _settings = log.settings.lock().await;
        // Committing the preference authorizes irreversible pruning, including on restart.
        state
            .runtime
            .call(move |runtime| {
                mochi_store::set_setting(runtime.connection(), operation_log::SETTING, &retention)
                    .map_err(|error| runtime.api_error(error))
            })
            .await??;
        log.apply_retention(retention).await.map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Log retention was saved, but could not be applied on disk. Retry to finish.",
            )
        })
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not update log retention",
        )
    })??;
    Ok(Json(Preference {
        retention,
        available: true,
    }))
}
