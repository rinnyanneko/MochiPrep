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
async fn failed_provider_deletion_preserves_saved_and_loaded_keys() {
    let t = Test::new().await;
    let provider = t.provider().await;
    t.state.runtime.call(|r| {
        r.connection().execute_batch("CREATE TRIGGER refuse_provider BEFORE DELETE ON providers BEGIN SELECT RAISE(ABORT, 'provider delete refused'); END").unwrap();
    }).await.unwrap();
    let path = format!("/api/providers/{provider}");
    assert_eq!(t.send("DELETE", &path, None).await.0, 400);
    let id = provider.clone();
    assert!(t
        .state
        .runtime
        .call(
            move |r| mochi_store::rows::get_provider_keys(r.connection())
                .unwrap()
                .contains_key(&id)
        )
        .await
        .unwrap());
    t.ok("POST", &format!("{path}/test"), None).await;
    t.state
        .runtime
        .call(|r| {
            r.connection()
                .execute_batch("DROP TRIGGER refuse_provider")
                .unwrap()
        })
        .await
        .unwrap();
    // A sealed row is the other durable key format; the delete must cascade to both tables.
    let id = provider.clone();
    t.state
        .runtime
        .call(move |r| {
            r.connection()
                .execute(
                    "INSERT INTO sealed_provider_keys (provider_id, sealed, updated_at) VALUES (?1, 'v1.a.b.c', 0)",
                    [&id],
                )
                .unwrap()
        })
        .await
        .unwrap();
    t.ok("DELETE", &path, None).await;
    let remaining: (i64, i64) = t
        .state
        .runtime
        .call(move |r| {
            let count = |table: &str| -> i64 {
                r.connection()
                    .query_row(
                        &format!("SELECT COUNT(*) FROM {table} WHERE provider_id = ?1"),
                        [&provider],
                        |row| row.get(0),
                    )
                    .unwrap()
            };
            (count("provider_keys"), count("sealed_provider_keys"))
        })
        .await
        .unwrap();
    assert_eq!(remaining, (0, 0), "plaintext and sealed keys both deleted");
    t.close().await;
}

#[tokio::test]
async fn running_agent_schedule_moves_to_the_new_source_clock() {
    let t = Test::new().await;
    let provider = t.provider().await;
    let (agent, input) = t.agent(&provider).await;
    let live = t
        .ok(
            "POST",
            "/api/accounts",
            Some(json!({"name":"live data","source":"LIVE"})),
        )
        .await;
    let path = format!("/api/agents/{agent}");
    let mut live_input = input.clone();
    live_input["accountId"] = live["id"].clone();
    t.ok("PUT", &path, Some(live_input)).await;
    let replay_now = t.state.runtime.replay_state().await.unwrap().now.unwrap();
    let running = t.ok("POST", &format!("{path}/start"), None).await;
    assert!(running["nextRunAt"].as_i64().unwrap() > replay_now);
    let updated = t.ok("PUT", &path, Some(input)).await;
    assert_eq!(updated["running"], true);
    assert_eq!(updated["nextRunAt"], replay_now);
    t.close().await;
}

#[tokio::test]
async fn standalone_cycle_blocks_demo_seek_and_idle_barrier() {
    let t = Test::new().await;
    let provider = t.provider().await;
    let (agent, _) = t.agent(&provider).await;
    let first = t.state.runtime.replay_state().await.unwrap().now;
    t.state.runtime.step_replay(2).await.unwrap();
    let before = t.state.runtime.replay_state().await.unwrap().now;
    *t.gateway.text.lock().unwrap() = json!({"symbol":"BTCUSDT","action":"HOLD","orderType":"MARKET","entryPrice":null,"stopLoss":null,"takeProfit":null,"reason":"held","confidence":0.8,"expiresAt":null}).to_string();
    t.gateway.blocked.store(true, Ordering::SeqCst);
    let path = format!("/api/agents/{agent}/run");
    let state = t.state.clone();
    let cycle = tokio::spawn(async move { send(&state, "POST", &path, None).await });
    timeout(Duration::from_secs(2), t.gateway.entered.notified())
        .await
        .unwrap();
    let mut idle = Box::pin(t.state.runtime.wait_replay_idle());
    assert!(futures_util::poll!(idle.as_mut()).is_pending());
    let mut start = Box::pin(t.send("POST", "/api/demo/start", None));
    assert!(futures_util::poll!(start.as_mut()).is_pending());
    let during = t.state.runtime.replay_state().await.unwrap().now;
    let idle_during = futures_util::poll!(idle.as_mut());
    t.gateway.release.notify_one();
    assert_eq!(
        timeout(Duration::from_secs(3), cycle)
            .await
            .unwrap()
            .unwrap()
            .0,
        200
    );
    assert_eq!(
        timeout(Duration::from_secs(3), start.as_mut())
            .await
            .unwrap()
            .0,
        200
    );
    assert_eq!(
        during, before,
        "a standalone replay cycle must finish before the demo seeks"
    );
    assert!(idle_during.is_pending());
    timeout(Duration::from_secs(3), idle.as_mut())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(t.state.runtime.replay_state().await.unwrap().now, first);
    drop((idle, start));
    t.close().await;
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
    timeout(Duration::from_secs(2), async {
        while t.gateway.requests.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second provider connection test did not reach the local transport");
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
async fn escaped_gateway_headers_are_redacted_in_samples_and_stored_model_responses() {
    let t = Test::new().await;
    let provider = t.provider().await;
    let secret = r#"gateway"\diagnostic"#;
    let mut input = t.provider_input();
    input["extraHeaders"] = json!({"x-org": secret});
    t.ok("PUT", &format!("/api/providers/{provider}"), Some(input))
        .await;
    // Place the escaped value across the sample cutoff and in the persisted JSON response.
    let reason = format!("{}{secret}", "x".repeat(102));
    *t.gateway.text.lock().unwrap() = json!({"reason":reason,"symbol":"BTCUSDT","action":"HOLD","orderType":"MARKET","entryPrice":null,"stopLoss":null,"takeProfit":null,"confidence":0.8,"expiresAt":null}).to_string();
    let sample = t
        .ok("POST", &format!("/api/providers/{provider}/test"), None)
        .await;
    let (agent, _) = t.agent(&provider).await;
    let record = t
        .ok("POST", &format!("/api/agents/{agent}/analyze"), None)
        .await;
    let stored = t.ok("GET", "/api/decisions", None).await;
    assert_eq!(stored[0]["id"], record["id"]);
    let raw: Value = serde_json::from_str(stored[0]["rawResponse"].as_str().unwrap()).unwrap();
    // The snippet is checked against the fully sanitized text, so cutoff cannot hide a prefix leak.
    let text = t.gateway.text.lock().unwrap().clone();
    let escaped = serde_json::to_string(secret).unwrap();
    let clean = text.replace(&escaped[1..escaped.len() - 1], "[REDACTED]");
    assert_eq!(
        (
            sample["sample"].as_str().unwrap().to_string(),
            raw["reason"].as_str().unwrap().to_string()
        ),
        (
            clean.chars().take(120).collect::<String>(),
            "x".repeat(102) + "[REDACTED]"
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
async fn demo_reset_waits_then_removes_old_demo_ownership_and_records() {
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
    let mut reset = Box::pin(t.send("POST", "/api/demo/reset", None));
    assert!(futures_util::poll!(reset.as_mut()).is_pending());
    let state = t.ok("GET", "/api/agents", None).await;
    assert!(state
        .as_array()
        .unwrap()
        .iter()
        .any(|agent| agent["id"] == id));
    t.gateway.release.notify_one();
    assert_eq!(
        timeout(Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap()
            .0,
        200
    );
    assert_eq!(
        timeout(Duration::from_secs(3), reset.as_mut())
            .await
            .unwrap()
            .0,
        200
    );
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
    drop(reset);
    t.close().await;
}

async fn queued_step_cannot_cross_demo_idle_handoff(action: &str) {
    let t = Test::new().await;
    let provider = t.provider().await;
    t.ok("POST", "/api/demo/start", None).await;
    let agents = t.ok("GET", "/api/agents", None).await;
    let agent = &agents[0];
    let id = agent["id"].as_str().unwrap();
    t.ok("PUT",&format!("/api/agents/{id}"),Some(json!({"name":agent["name"],"providerId":provider,"model":"m","symbol":"BTCUSDT","timeframe":"15m","strategy":"","accountId":agent["accountId"],"risk":agent["risk"],"intervalSec":10,"executionMode":"AUTO"}))).await;
    t.ok("POST", &format!("/api/agents/{id}/start"), None).await;
    *t.gateway.text.lock().unwrap()=json!({"symbol":"BTCUSDT","action":"HOLD","orderType":"MARKET","entryPrice":null,"stopLoss":null,"takeProfit":null,"reason":"held","confidence":0.8,"expiresAt":null}).to_string();
    t.gateway.blocked.store(true, Ordering::SeqCst);

    // Poll the route at its actor handoffs without allowing it to resume automatically.
    // Another request queues stepping after an idle acknowledgement but before the route
    // can send a separate destructive mutation. A correct actor-owned mutation has already
    // finished here, so that same later step legitimately sees only the new ownership.
    let path = format!("/api/demo/{action}");
    let mut change = Box::pin(t.send("POST", &path, None));
    assert!(futures_util::poll!(change.as_mut()).is_pending());
    t.state.runtime.call(|_| ()).await.unwrap();
    let mut changed = match futures_util::poll!(change.as_mut()) {
        std::task::Poll::Ready(v) => Some(v),
        std::task::Poll::Pending => None,
    };
    let mut step = Box::pin(t.state.runtime.step_replay(10));
    assert!(futures_util::poll!(step.as_mut()).is_pending());
    t.state.runtime.call(|_| ()).await.unwrap();
    let old_batch_held=timeout(Duration::from_secs(2),async {
        tokio::select! {()=t.gateway.entered.notified()=>true,result=step.as_mut()=>{result.unwrap();false}}
    }).await.expect("neither the queued batch nor its local transport made progress");
    if old_batch_held {
        assert!(
            changed.is_none(),
            "old demo model started after ownership was already removed"
        );
        assert!(futures_util::poll!(change.as_mut()).is_pending());
        t.state.runtime.call(|_| ()).await.unwrap();
        let completed_while_batch_held = match futures_util::poll!(change.as_mut()) {
            std::task::Poll::Ready(v) => {
                changed = Some(v);
                true
            }
            std::task::Poll::Pending => false,
        };
        t.gateway.blocked.store(false, Ordering::SeqCst);
        t.gateway.release.notify_one();
        timeout(Duration::from_secs(3), step.as_mut())
            .await
            .unwrap()
            .unwrap();
        if changed.is_none() {
            changed = Some(
                timeout(Duration::from_secs(3), change.as_mut())
                    .await
                    .unwrap(),
            );
        }
        assert!(
            !completed_while_batch_held,
            "demo ownership changed while the queued old replay batch still waited for its model"
        );
    } else if changed.is_none() {
        changed = Some(
            timeout(Duration::from_secs(3), change.as_mut())
                .await
                .unwrap(),
        );
    }
    assert_eq!(
        changed.unwrap(),
        (200, json!({"demoActive":action=="reset"}))
    );
    let agents = t.ok("GET", "/api/agents", None).await;
    assert!(agents.as_array().unwrap().iter().all(|a| a["id"] != id));
    assert!(t
        .ok("GET", "/api/decisions", None)
        .await
        .as_array()
        .unwrap()
        .is_empty());
    drop(change);
    drop(step);
    t.close().await;
}
#[tokio::test]
async fn reset_keeps_replay_idle_and_ownership_change_atomic() {
    queued_step_cannot_cross_demo_idle_handoff("reset").await;
}
#[tokio::test]
async fn exit_keeps_replay_idle_and_ownership_change_atomic() {
    queued_step_cannot_cross_demo_idle_handoff("exit").await;
}

#[tokio::test]
async fn demo_start_waits_for_a_user_replay_bar_before_rewinding() {
    let t = Test::new().await;
    let provider = t.provider().await;
    let (agent, _) = t.agent(&provider).await;
    let initial = t.state.runtime.replay_state().await.unwrap().now;
    t.state.runtime.step_replay(2).await.unwrap();
    t.ok("POST", &format!("/api/agents/{agent}/start"), None)
        .await;
    *t.gateway.text.lock().unwrap() = json!({"symbol":"BTCUSDT","action":"HOLD","orderType":"MARKET","entryPrice":null,"stopLoss":null,"takeProfit":null,"reason":"held","confidence":0.8,"expiresAt":null}).to_string();
    t.gateway.blocked.store(true, Ordering::SeqCst);
    let mut step = Box::pin(t.state.runtime.step_replay(1));
    assert!(futures_util::poll!(step.as_mut()).is_pending());
    timeout(Duration::from_secs(2), t.gateway.entered.notified())
        .await
        .unwrap();
    let held = t.state.runtime.replay_state().await.unwrap().now;
    let mut demo = Box::pin(t.send("POST", "/api/demo/start", None));
    assert!(futures_util::poll!(demo.as_mut()).is_pending());
    let during = t.state.runtime.replay_state().await.unwrap().now;
    t.gateway.blocked.store(false, Ordering::SeqCst);
    t.gateway.release.notify_one();
    timeout(Duration::from_secs(3), step.as_mut())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(3), demo.as_mut())
            .await
            .unwrap()
            .0,
        200
    );
    assert_eq!(
        during, held,
        "demo start moved the tape during the user's held replay bar"
    );
    assert_eq!(t.state.runtime.replay_state().await.unwrap().now, initial);
    drop(step);
    drop(demo);
    t.close().await;
}

#[tokio::test]
async fn reset_drains_held_and_queued_batches_before_seeking() {
    let t = Test::new().await;
    let provider = t.provider().await;
    let (id, _) = t.agent(&provider).await;
    t.ok("POST", "/api/demo/start", None).await;
    let before = t.ok("GET", "/api/state", None).await["replay"]["now"].clone();
    let old_demo = t.ok("GET", "/api/agents", None).await;
    t.ok("POST", &format!("/api/agents/{id}/start"), None).await;
    *t.gateway.text.lock().unwrap()=json!({"symbol":"BTCUSDT","action":"HOLD","orderType":"MARKET","entryPrice":null,"stopLoss":null,"takeProfit":null,"reason":"held","confidence":0.8,"expiresAt":null}).to_string();
    t.gateway.blocked.store(true, Ordering::SeqCst);
    let mut first = Box::pin(t.state.runtime.step_replay(1));
    assert!(futures_util::poll!(first.as_mut()).is_pending());
    timeout(Duration::from_secs(2), t.gateway.entered.notified())
        .await
        .unwrap();
    let mut reset = Box::pin(t.send("POST", "/api/demo/reset", None));
    assert!(futures_util::poll!(reset.as_mut()).is_pending());
    let mut queued = Box::pin(t.state.runtime.step_replay(10));
    assert!(futures_util::poll!(queued.as_mut()).is_pending());
    t.state.runtime.call(|_| ()).await.unwrap();
    assert!(futures_util::poll!(reset.as_mut()).is_pending());
    assert_eq!(
        t.ok("GET", "/api/agents", None)
            .await
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["isDemo"] == true)
            .map(|a| a["id"].clone())
            .collect::<Vec<_>>(),
        old_demo
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["isDemo"] == true)
            .map(|a| a["id"].clone())
            .collect::<Vec<_>>()
    );
    t.gateway.blocked.store(false, Ordering::SeqCst);
    t.gateway.release.notify_one();
    timeout(Duration::from_secs(3), async {
        first.as_mut().await.unwrap();
        queued.as_mut().await.unwrap();
        assert_eq!(reset.as_mut().await.0, 200);
    })
    .await
    .expect("reset or its preceding replay batches did not drain");
    assert_eq!(
        t.ok("GET", "/api/state", None).await["replay"]["now"],
        before
    );
    drop(first);
    drop(queued);
    drop(reset);
    t.close().await;
}
