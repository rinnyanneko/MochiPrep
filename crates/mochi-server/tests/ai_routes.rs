use axum::{body::Body, extract::State, http::Request, routing::post, Json, Router};
use http_body_util::BodyExt;
use mochi_domain::default_risk_settings;
use mochi_server::{app, config::config_from, AppState, RuntimeHandle, RuntimeOptions};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::{
    sync::Notify,
    time::{timeout, Duration},
};
use tower::ServiceExt;

#[derive(Default)]
struct Gateway {
    text: Mutex<String>,
    blocked: AtomicBool,
    entered: Notify,
    release: Notify,
    requests: Mutex<Vec<Value>>,
}
async fn model(State(g): State<Arc<Gateway>>, Json(body): Json<Value>) -> Json<Value> {
    g.requests.lock().unwrap().push(body);
    g.entered.notify_one();
    if g.blocked.load(Ordering::SeqCst) {
        g.release.notified().await;
    }
    Json(json!({"choices":[{"message":{"content":g.text.lock().unwrap().clone()}}]}))
}
struct Test {
    state: AppState,
    gateway: Arc<Gateway>,
    url: String,
    server: tokio::task::JoinHandle<()>,
}
impl Test {
    async fn new() -> Self {
        let gateway = Arc::new(Gateway::default());
        *gateway.text.lock().unwrap() = "{\"ok\":true}".into();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/chat/completions", post(model))
            .with_state(gateway.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut options = RuntimeOptions::new(mochi_store::open_in_memory().unwrap());
        options.replay_minutes = Some(6500);
        let state = AppState::new(
            Arc::new(config_from(|_| None)),
            RuntimeHandle::start(options).unwrap(),
        );
        Self {
            state,
            gateway,
            url,
            server,
        }
    }
    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        send(&self.state, method, path, body.map(|v| v.to_string())).await
    }
    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.send(method, path, body).await;
        assert_eq!(status, 200, "{method} {path}: {value}");
        value
    }
    fn provider_input(&self) -> Value {
        json!({"name":"Gateway","kind":"OPENAI_COMPATIBLE","baseUrl":self.url,"defaultModel":"m","inputPricePerMTok":null,"outputPricePerMTok":null,"extraHeaders":{"x-org":"header-secret"}})
    }
    async fn provider(&self) -> String {
        let p = self
            .ok("POST", "/api/providers", Some(self.provider_input()))
            .await;
        let id = p["id"].as_str().unwrap().to_string();
        self.ok(
            "PUT",
            &format!("/api/providers/{id}/key"),
            Some(json!({"apiKey":"gateway-secret"})),
        )
        .await;
        id
    }
    async fn agent(&self, provider: &str) -> (String, Value) {
        let account = self
            .ok(
                "POST",
                "/api/accounts",
                Some(json!({"name":"User account","source":"REPLAY"})),
            )
            .await;
        let input = json!({"name":"Agent","providerId":provider,"model":"m","symbol":"BTCUSDT","timeframe":"15m","strategy":"","accountId":account["id"],"risk":default_risk_settings(),"intervalSec":900});
        let agent = self.ok("POST", "/api/agents", Some(input.clone())).await;
        assert_eq!(agent["executionMode"], "SIGNAL");
        (agent["id"].as_str().unwrap().into(), input)
    }
    async fn close(self) {
        self.state.runtime.shutdown().await;
        self.server.abort();
    }
}
async fn send(state: &AppState, method: &str, path: &str, body: Option<String>) -> (u16, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost:8787")
        .header("authorization", format!("Bearer {}", state.token));
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let res = app(state.clone())
        .oneshot(req.body(Body::from(body.unwrap_or_default())).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn provider_crud_views_key_modes_and_validation_order() {
    let t = Test::new().await;
    assert_eq!(
        send(&t.state, "PUT", "/api/providers/missing", Some("{".into()))
            .await
            .0,
        400
    );
    assert_eq!(
        t.send("PUT", "/api/providers/missing", Some(json!({})))
            .await,
        (404, json!({"error":"Unknown provider"}))
    );
    assert_eq!(
        t.send("PUT", "/api/providers/mock", Some(json!({}))).await,
        (400, json!({"error":"The mock provider is built in"}))
    );
    let id = t.provider().await;
    let views = t.ok("GET", "/api/providers", None).await;
    let p = views
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .unwrap();
    assert_eq!(p["headerNames"], json!(["x-org"]));
    assert_eq!(p["hasKey"], true);
    assert!(!views.to_string().contains("header-secret"));
    assert!(!views.to_string().contains("gateway-secret"));
    let mut input = t.provider_input();
    input.as_object_mut().unwrap().remove("extraHeaders");
    assert_eq!(
        t.ok("PUT", &format!("/api/providers/{id}"), Some(input))
            .await["headerNames"],
        json!(["x-org"])
    );
    t.state.runtime.call(|r|r.connection().execute("INSERT INTO vault_keys (id,kind,params,created_at) VALUES ('password-wrap','PASSWORD','{}',1)",[]).unwrap()).await.unwrap();
    assert_eq!(
        t.send(
            "PUT",
            &format!("/api/providers/{id}/key"),
            Some(json!({"apiKey":"new-key"}))
        )
        .await
        .0,
        400
    );
    t.ok("DELETE", &format!("/api/providers/{id}/key"), None)
        .await;
    assert_eq!(
        t.send("POST", &format!("/api/providers/{id}/test"), None)
            .await
            .0,
        400
    );
    assert_eq!(
        t.ok("POST", "/api/providers/mock/test", None).await,
        json!({"ok":true,"sample":"Mock provider needs no connection."})
    );
    t.ok("DELETE", &format!("/api/providers/{id}"), None).await;
    t.close().await;
}
#[tokio::test]
async fn connection_test_uses_transport_and_redacts_old_and_current_secrets_before_cutoff() {
    let t = Test::new().await;
    let id = t.provider().await;
    *t.gateway.text.lock().unwrap() = "x".repeat(112) + "gateway-secret";
    assert_eq!(
        t.ok(
            "POST",
            &format!("/api/providers/{id}/test"),
            Some(json!({"model":"selected"}))
        )
        .await["sample"],
        "x".repeat(112) + "[REDACTE"
    );
    assert_eq!(t.gateway.requests.lock().unwrap()[0]["model"], "selected");
    *t.gateway.text.lock().unwrap() = "gateway-secret new-key header-secret new-header".into();
    t.gateway.blocked.store(true, Ordering::SeqCst);
    let state = t.state.clone();
    let path = format!("/api/providers/{id}/test");
    let pending = tokio::spawn(async move { send(&state, "POST", &path, Some("{}".into())).await });
    // The first request left a notification; consume it before observing the new request.
    while t.gateway.requests.lock().unwrap().len() < 2 {
        tokio::task::yield_now().await;
    }
    t.ok(
        "PUT",
        &format!("/api/providers/{id}/key"),
        Some(json!({"apiKey":"new-key"})),
    )
    .await;
    let mut input = t.provider_input();
    input["extraHeaders"] = json!({"x-org":"new-header"});
    t.ok("PUT", &format!("/api/providers/{id}"), Some(input))
        .await;
    t.gateway.release.notify_one();
    assert_eq!(
        pending.await.unwrap(),
        (
            200,
            json!({"ok":true,"sample":"[REDACTED] [REDACTED] [REDACTED] [REDACTED]"})
        )
    );
    t.close().await;
}
#[tokio::test]
async fn agent_refs_busy_crud_lifecycle_and_signal_statuses() {
    let t = Test::new().await;
    let provider = t.provider().await;
    let (id, input) = t.agent(&provider).await;
    // Node permits sharing a pair/account; configuration has no invented uniqueness rule.
    let duplicate = t.ok("POST", "/api/agents", Some(input.clone())).await;
    t.ok(
        "DELETE",
        &format!("/api/agents/{}", duplicate["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(
        t.send("DELETE", &format!("/api/providers/{provider}"), None)
            .await
            .1,
        json!({"error":"Agents still use this provider"})
    );
    let mut wrong = input.clone();
    wrong["providerId"] = json!("missing");
    assert_eq!(
        t.send("POST", "/api/agents", Some(wrong.clone())).await.1,
        json!({"error":"Unknown AI provider"})
    );
    wrong = input.clone();
    wrong["symbol"] = json!("DOGEUSDT");
    assert!(t.send("POST", "/api/agents", Some(wrong)).await.1["error"]
        .as_str()
        .unwrap()
        .contains("not an added pair"));
    assert_eq!(
        t.send("PUT", "/api/agents/missing", Some(input.clone()))
            .await,
        (404, json!({"error":"Unknown agent"}))
    );
    *t.gateway.text.lock().unwrap()=json!({"symbol":"BTCUSDT","action":"LONG","orderType":"MARKET","entryPrice":null,"stopLoss":60000,"takeProfit":null,"reason":"held","confidence":0.8,"expiresAt":null}).to_string();
    t.gateway.blocked.store(true, Ordering::SeqCst);
    let state = t.state.clone();
    let path = format!("/api/agents/{id}/run");
    let pending = tokio::spawn(async move { send(&state, "POST", &path, None).await });
    timeout(Duration::from_secs(2), t.gateway.entered.notified())
        .await
        .unwrap();
    let mut edited = input.clone();
    edited["executionMode"] = json!("AUTO");
    edited["risk"]["maxLeverage"] = json!(1);
    assert_eq!(
        t.send("PUT", &format!("/api/agents/{id}"), Some(json!({})))
            .await
            .0,
        400
    );
    assert_eq!(
        t.send("PUT", &format!("/api/agents/{id}"), Some(edited.clone()))
            .await
            .0,
        409
    );
    assert_eq!(
        t.send("DELETE", &format!("/api/agents/{id}"), None).await.0,
        409
    );
    let agents = t.ok("GET", "/api/agents", None).await;
    assert_eq!(agents[0]["executionMode"], "SIGNAL");
    assert_eq!(agents[0]["risk"], input["risk"]);
    assert_eq!(
        t.send("POST", &format!("/api/agents/{id}/analyze"), None)
            .await
            .0,
        409
    );
    t.ok("POST", &format!("/api/agents/{id}/start"), None).await;
    assert_eq!(
        t.send("DELETE", &format!("/api/agents/{id}"), None).await,
        (400, json!({"error":"Stop the agent first"}))
    );
    t.ok("POST", &format!("/api/agents/{id}/pause"), None).await;
    t.ok("POST", &format!("/api/agents/{id}/stop"), None).await;
    t.gateway.release.notify_one();
    assert_eq!(pending.await.unwrap().0, 200);
    t.gateway.blocked.store(false, Ordering::SeqCst);
    let signal = t.ok("POST", &format!("/api/agents/{id}/run"), None).await;
    assert_eq!(signal["outcome"], "SIGNAL");
    let sid = signal["id"].as_str().unwrap();
    t.ok("POST", "/api/agents/stop-all", None).await;
    assert_eq!(
        t.send("POST", &format!("/api/signals/{sid}/execute"), None)
            .await
            .0,
        409
    );
    assert_eq!(
        t.send("POST", "/api/signals/missing/dismiss", None).await,
        (404, json!({"error":"Unknown suggestion"}))
    );
    let signal = t.ok("POST", &format!("/api/agents/{id}/run"), None).await;
    let path = format!("/api/signals/{}/dismiss", signal["id"].as_str().unwrap());
    assert_eq!(
        t.ok("POST", &path, None).await["outcome"],
        "SIGNAL_DISMISSED"
    );
    assert_eq!(t.send("POST", &path, None).await.0, 409);
    let signal = t.ok("POST", &format!("/api/agents/{id}/run"), None).await;
    assert_eq!(
        t.ok(
            "POST",
            &format!("/api/signals/{}/execute", signal["id"].as_str().unwrap()),
            None
        )
        .await["outcome"],
        "EXECUTED"
    );
    t.ok("PUT", &format!("/api/agents/{id}"), Some(edited))
        .await;
    t.ok("DELETE", &format!("/api/agents/{id}"), None).await;
    t.close().await;
}
#[tokio::test]
async fn demo_lifecycle_preserves_user_data_and_reset_repeats_economic_sequence() {
    let t = Test::new().await;
    let (_user, input) = t.agent("mock").await;
    assert_eq!(
        t.ok("POST", "/api/demo/start", None).await,
        json!({"demoActive":true})
    );
    t.ok("POST", "/api/demo/start", None).await;
    assert_eq!(
        t.ok("GET", "/api/accounts", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        5
    );
    async fn run(t: &Test) -> Value {
        let agents = t.ok("GET", "/api/agents", None).await;
        for a in agents
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["isDemo"] == true)
        {
            t.ok(
                "POST",
                &format!("/api/agents/{}/start", a["id"].as_str().unwrap()),
                None,
            )
            .await;
        }
        t.state.runtime.step_replay(600).await.unwrap();
        t.state
            .runtime
            .call(|r| {
                use mochi_core::paper::PaperStore;
                let mut accounts = r.engine().store().list_accounts().unwrap();
                accounts.sort_by(|a, b| a.name.cmp(&b.name));
                Value::Array(
                    accounts
                        .into_iter()
                        .filter(|a| a.is_demo)
                        .map(|a| {
                            let mut balance =
                                serde_json::to_value(r.balance_of(&a).unwrap()).unwrap();
                            balance.as_object_mut().unwrap().remove("accountId");
                            let mut fills = r.engine().store().list_fills(&a.id, 10000).unwrap();
                            fills.reverse();
                            let fills: Vec<Value> = fills
                                .into_iter()
                                .map(|f| {
                                    let mut f = serde_json::to_value(f).unwrap();
                                    for k in ["id", "orderId", "accountId"] {
                                        f.as_object_mut().unwrap().remove(k);
                                    }
                                    f
                                })
                                .collect();
                            json!({"name":a.name,"balance":balance,"fills":fills})
                        })
                        .collect(),
                )
            })
            .await
            .unwrap()
    }
    let first = run(&t).await;
    assert!(first
        .as_array()
        .unwrap()
        .iter()
        .any(|a| !a["fills"].as_array().unwrap().is_empty()));
    t.ok("POST", "/api/demo/reset", None).await;
    let second = run(&t).await;
    assert_eq!(second, first);
    assert!(t
        .ok("GET", "/api/accounts", None)
        .await
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"] == input["accountId"]));
    t.ok("POST", "/api/demo/exit", None).await;
    assert_eq!(
        t.ok("GET", "/api/accounts", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        t.ok("GET", "/api/agents", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(t.ok("GET", "/api/state", None).await["demoActive"], false);
    t.close().await;
}

#[tokio::test]
async fn all_new_writes_parse_malformed_bodies_before_missing_ids_and_redact_schema_errors() {
    let t = Test::new().await;
    for (method, path) in [
        ("POST", "/api/providers"),
        ("PUT", "/api/providers/missing"),
        ("DELETE", "/api/providers/missing"),
        ("PUT", "/api/providers/missing/key"),
        ("DELETE", "/api/providers/missing/key"),
        ("POST", "/api/providers/missing/test"),
        ("POST", "/api/agents"),
        ("PUT", "/api/agents/missing"),
        ("DELETE", "/api/agents/missing"),
        ("POST", "/api/agents/missing/start"),
        ("POST", "/api/agents/missing/resume"),
        ("POST", "/api/agents/missing/pause"),
        ("POST", "/api/agents/missing/stop"),
        ("POST", "/api/agents/missing/analyze"),
        ("POST", "/api/agents/missing/run"),
        ("POST", "/api/agents/stop-all"),
        ("POST", "/api/signals/missing/execute"),
        ("POST", "/api/signals/missing/dismiss"),
        ("POST", "/api/demo/start"),
        ("POST", "/api/demo/reset"),
        ("POST", "/api/demo/exit"),
    ] {
        assert_eq!(
            send(&t.state, method, path, Some("{".into())).await.0,
            400,
            "{path}"
        );
    }
    let id = t.provider().await;
    let (_agent, mut input) = t.agent(&id).await;
    input["header-secret"] = json!(true);
    let error = t.send("POST", "/api/agents", Some(input)).await;
    assert_eq!(error.0, 400);
    assert!(error.1["error"].as_str().unwrap().contains("[REDACTED]"));
    assert!(!error.1.to_string().contains("header-secret"));
    let mut provider = t.provider_input();
    provider["gateway-secret"] = json!(true);
    let error = t.send("POST", "/api/providers", Some(provider)).await;
    assert_eq!(error.0, 400);
    assert!(!error.1.to_string().contains("gateway-secret"));
    t.close().await;
}
#[tokio::test]
async fn demo_reset_removes_inflight_demo_ownership_without_late_records_or_orders() {
    let t = Test::new().await;
    let provider = t.provider().await;
    t.ok("POST", "/api/demo/start", None).await;
    let agents = t.ok("GET", "/api/agents", None).await;
    let a = &agents[0];
    let id = a["id"].as_str().unwrap();
    let input = json!({"name":a["name"],"providerId":provider,"model":"m","symbol":a["symbol"],"timeframe":a["timeframe"],"strategy":"","accountId":a["accountId"],"risk":a["risk"],"intervalSec":900,"executionMode":"AUTO"});
    t.ok("PUT", &format!("/api/agents/{id}"), Some(input)).await;
    *t.gateway.text.lock().unwrap()=json!({"symbol":"BTCUSDT","action":"LONG","orderType":"MARKET","entryPrice":null,"stopLoss":60000,"takeProfit":null,"reason":"late","confidence":0.8,"expiresAt":null}).to_string();
    t.gateway.blocked.store(true, Ordering::SeqCst);
    let state = t.state.clone();
    let path = format!("/api/agents/{id}/run");
    let pending = tokio::spawn(async move { send(&state, "POST", &path, None).await });
    timeout(Duration::from_secs(2), t.gateway.entered.notified())
        .await
        .unwrap();
    t.ok("POST", "/api/demo/reset", None).await;
    t.gateway.release.notify_one();
    assert_eq!(pending.await.unwrap().0, 409);
    assert!(t
        .ok("GET", "/api/decisions", None)
        .await
        .as_array()
        .unwrap()
        .is_empty());
    assert!(t
        .ok("GET", "/api/agents", None)
        .await
        .as_array()
        .unwrap()
        .iter()
        .all(|a| a["id"] != id));
    t.close().await;
}
