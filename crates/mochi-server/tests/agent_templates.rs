//! `/api/agent-templates`: strict validation, read-only built-ins, hide and restore.
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use mochi_server::{app, config::config_from, AppState, RuntimeHandle, RuntimeOptions};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn state() -> AppState {
    let options = RuntimeOptions::new(mochi_store::open_in_memory().unwrap());
    AppState::new(
        Arc::new(config_from(|_| None)),
        RuntimeHandle::start(options).unwrap(),
    )
}

async fn send(state: &AppState, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost:8787")
        .header("authorization", format!("Bearer {}", state.token));
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let body = body.map(|v| v.to_string()).unwrap_or_default();
    let res = app(state.clone())
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn template(name: &str) -> Value {
    json!({
        "name": name,
        "strategy": "Buy dips",
        "timeframe": "5m",
        "intervalSec": 300,
    })
}

#[tokio::test]
async fn lists_builtins_first() {
    let s = state();
    let (status, list) = send(&s, "GET", "/api/agent-templates", None).await;
    assert_eq!(status, 200);
    let list = list.as_array().unwrap();
    assert!(list.len() >= 3);
    for t in list {
        assert_eq!(t["builtIn"], true);
        assert_eq!(t["executionMode"], "SIGNAL");
        assert_eq!(t["risk"]["requireStopLoss"], true);
        assert_eq!(t["createdAt"], Value::Null);
    }
}

#[tokio::test]
async fn create_fills_defaults_and_edits() {
    let s = state();
    let (status, saved) = send(
        &s,
        "POST",
        "/api/agent-templates",
        Some(template("  Mine  ")),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["name"], "Mine");
    assert_eq!(saved["description"], "");
    assert_eq!(saved["executionMode"], "SIGNAL");
    assert_eq!(saved["useNews"], false);
    assert_eq!(saved["allowTrailingStop"], false);
    assert_eq!(saved["builtIn"], false);
    // The paper defaults, written as JS numbers.
    assert_eq!(saved["risk"]["maxLeverage"], json!(3));
    let id = saved["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("tpl_"));

    let mut edit = template("Edited");
    edit["executionMode"] = json!("AUTO");
    edit["risk"] = json!({"maxLeverage":2,"maxRiskPerTradePct":0.25,"maxDailyLossPct":1,"maxOpenPositions":1,"requireStopLoss":true,"maxQuoteAgeSec":20,"maxNotional":1000});
    let path = format!("/api/agent-templates/{id}");
    let (status, updated) = send(&s, "PUT", &path, Some(edit)).await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(updated["name"], "Edited");
    assert_eq!(updated["executionMode"], "AUTO");
    assert_eq!(updated["risk"]["maxNotional"], json!(1000));

    let (_, list) = send(&s, "GET", "/api/agent-templates", None).await;
    assert_eq!(list.as_array().unwrap().last().unwrap()["id"], json!(id));

    assert_eq!(send(&s, "DELETE", &path, None).await.0, 200);
    assert_eq!(send(&s, "DELETE", &path, None).await.0, 404);
    assert_eq!(send(&s, "PUT", &path, Some(template("x"))).await.0, 404);
}

#[tokio::test]
async fn rejects_invalid_input() {
    let s = state();
    let cases = [
        json!({"name":"","strategy":"","timeframe":"5m","intervalSec":300}),
        json!({"name":"x","strategy":"","timeframe":"2m","intervalSec":300}),
        json!({"name":"x","strategy":"","timeframe":"5m","intervalSec":5}),
        json!({"name":"x","strategy":"","timeframe":"5m","intervalSec":300,"executionMode":"YOLO"}),
        json!({"name":"x","strategy":"","timeframe":"5m","intervalSec":300,"providerId":"mock"}),
        json!({"name":"x","strategy":"","timeframe":"5m","intervalSec":300,"risk":{"maxLeverage":500,"maxRiskPerTradePct":1,"maxDailyLossPct":1,"maxOpenPositions":1,"requireStopLoss":true,"maxQuoteAgeSec":30,"maxNotional":1}}),
        json!({"name":"x","description":"d".repeat(301),"strategy":"","timeframe":"5m","intervalSec":300}),
    ];
    for case in cases {
        let (status, body) = send(&s, "POST", "/api/agent-templates", Some(case.clone())).await;
        assert_eq!(status, 400, "{case} -> {body}");
    }
    let (_, list) = send(&s, "GET", "/api/agent-templates", None).await;
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t["builtIn"] == true));
}

#[tokio::test]
async fn builtins_are_read_only_and_restorable() {
    let s = state();
    let (_, list) = send(&s, "GET", "/api/agent-templates", None).await;
    let first = list[0]["id"].as_str().unwrap().to_string();
    let count = list.as_array().unwrap().len();
    let path = format!("/api/agent-templates/{first}");

    let (status, body) = send(&s, "PUT", &path, Some(template("Mine"))).await;
    assert_eq!(status, 409, "{body}");

    assert_eq!(send(&s, "DELETE", &path, None).await.0, 200);
    let (_, list) = send(&s, "GET", "/api/agent-templates", None).await;
    assert_eq!(list.as_array().unwrap().len(), count - 1);
    assert_eq!(send(&s, "DELETE", &path, None).await.0, 404);

    let (status, restored) = send(&s, "POST", "/api/agent-templates/restore-builtins", None).await;
    assert_eq!(status, 200);
    assert_eq!(restored["restored"], 1);
    let (_, list) = send(&s, "GET", "/api/agent-templates", None).await;
    assert_eq!(list.as_array().unwrap().len(), count);
    assert_eq!(list[0]["id"], json!(first));
}
