use axum::{body::Body, extract::State, http::Request, routing::post, Json, Router};
use http_body_util::BodyExt;
use mochi_core::paper::PaperStore;
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
    /// The answer from the second request on, when set.
    then: Mutex<Option<String>>,
    /// From this request on (1 is the first), the gateway refuses the key with a 401.
    refuse_from: Mutex<Option<usize>>,
}
async fn model(
    State(g): State<Arc<Gateway>>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    let n = {
        let mut requests = g.requests.lock().unwrap();
        requests.push(body);
        requests.len()
    };
    g.entered.notify_one();
    if g.refuse_from.lock().unwrap().is_some_and(|from| n >= from) {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"invalid key"}})),
        );
    }
    if g.blocked.load(Ordering::SeqCst) {
        g.release.notified().await;
    }
    let text = g.text.lock().unwrap().clone();
    if let Some(next) = g.then.lock().unwrap().take() {
        *g.text.lock().unwrap() = next;
    }
    (
        axum::http::StatusCode::OK,
        Json(json!({"choices":[{"message":{"content":text}}]})),
    )
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
        let input = json!({"name":"Agent","providerId":provider,"model":"m","symbols":["BTCUSDT"],"timeframe":"15m","strategy":"","accountId":account["id"],"risk":default_risk_settings(),"intervalSec":900});
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
    // No mock provider is seeded any more: a fresh database has no providers.
    assert_eq!(t.ok("GET", "/api/providers", None).await, json!([]));
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
    wrong["symbols"] = json!(["ADAUSDT"]);
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

fn decision(symbol: &str, action: &str, stop: Option<f64>) -> Value {
    json!({"symbol":symbol,"action":action,"orderType":"MARKET","entryPrice":null,
           "stopLoss":stop,"takeProfit":null,"reason":"test","confidence":0.8,"expiresAt":null})
}

/// An Auto agent on a fresh replay account that trades `symbols`.
async fn multi_agent(t: &Test, symbols: &[&str]) -> String {
    multi_agent_in(t, symbols, "AUTO").await
}

async fn multi_agent_in(t: &Test, symbols: &[&str], mode: &str) -> String {
    let provider = t.provider().await;
    let account = t
        .ok(
            "POST",
            "/api/accounts",
            Some(json!({"name":"Pairs","source":"REPLAY"})),
        )
        .await;
    let input = json!({"name":"Pairs","providerId":provider,"model":"m","symbols":symbols,
        "timeframe":"15m","strategy":"","accountId":account["id"],
        "risk":default_risk_settings(),"intervalSec":900,"executionMode":mode});
    let agent = t.ok("POST", "/api/agents", Some(input)).await;
    assert_eq!(agent["symbols"], json!(symbols));
    assert_eq!(agent["symbol"], symbols[0]);
    agent["id"].as_str().unwrap().into()
}

#[tokio::test]
async fn one_model_call_decides_every_pair_and_each_pair_trades_on_its_own() {
    let t = Test::new().await;
    let id = multi_agent(&t, &["BTCUSDT", "ETHUSDT"]).await;
    *t.gateway.text.lock().unwrap() = json!({"decisions":[
        decision("ETHUSDT", "LONG", Some(3000.0)),
        decision("BTCUSDT", "HOLD", None),
    ]})
    .to_string();
    let first = t.ok("POST", &format!("/api/agents/{id}/run"), None).await;
    assert_eq!(first["symbol"], "BTCUSDT");

    // One request for both pairs, in the multi-pair format.
    let requests = t.gateway.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let prompt = requests[0].to_string();
    assert!(prompt.contains("decisions"), "{prompt}");
    assert!(prompt.contains("BTCUSDT") && prompt.contains("ETHUSDT"));

    let decisions = t
        .ok("GET", &format!("/api/decisions?agentId={id}"), None)
        .await;
    let by_symbol = |symbol: &str| {
        decisions
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["symbol"] == symbol)
            .unwrap()
            .clone()
    };
    assert_eq!(decisions.as_array().unwrap().len(), 2);
    assert_eq!(by_symbol("BTCUSDT")["outcome"], "HOLD");
    let eth = by_symbol("ETHUSDT");
    assert_eq!(eth["outcome"], "EXECUTED", "{eth}");
    assert_eq!(eth["decision"]["symbol"], "ETHUSDT");
    t.close().await;
}

#[tokio::test]
async fn only_the_refused_pairs_are_asked_about_again_with_the_reasons() {
    let t = Test::new().await;
    let id = multi_agent(&t, &["BTCUSDT", "ETHUSDT"]).await;
    // BTC holds; ETH goes long without the stop loss the limits require.
    *t.gateway.text.lock().unwrap() = json!({"decisions":[
        decision("BTCUSDT", "HOLD", None),
        decision("ETHUSDT", "LONG", None),
    ]})
    .to_string();
    *t.gateway.then.lock().unwrap() =
        Some(json!({"decisions":[decision("ETHUSDT", "LONG", Some(3000.0))]}).to_string());
    t.ok("POST", &format!("/api/agents/{id}/run"), None).await;

    let requests = t.gateway.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let retry = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    assert!(
        retry.contains(
            "<refused>
ETHUSDT: Stop loss present"
        ),
        "{retry}"
    );
    // Only ETH's market data is sent again; BTC's decision stands.
    let btc_data = "\\\"symbol\\\":\\\"BTCUSDT\\\",\\\"timeframe\\\"";
    assert!(requests[0].to_string().contains(btc_data));
    assert!(!requests[1].to_string().contains(btc_data));

    let decisions = t
        .ok("GET", &format!("/api/decisions?agentId={id}"), None)
        .await;
    let by_symbol = |symbol: &str| {
        decisions
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["symbol"] == symbol)
            .unwrap()
            .clone()
    };
    let btc = by_symbol("BTCUSDT");
    assert_eq!(btc["outcome"], "HOLD");
    assert!(btc.get("attempts").is_none(), "{btc}");
    let eth = by_symbol("ETHUSDT");
    assert_eq!(eth["outcome"], "EXECUTED", "{eth}");
    assert_eq!(eth["attempts"][0]["outcome"], "RISK_REJECTED");
    assert_eq!(eth["attempts"][0]["decision"]["action"], "LONG");
    t.close().await;
}

#[tokio::test]
async fn a_pair_the_model_leaves_out_fails_alone() {
    let t = Test::new().await;
    let id = multi_agent(&t, &["BTCUSDT", "ETHUSDT"]).await;
    *t.gateway.text.lock().unwrap() = json!({"decisions":[
        decision("BTCUSDT", "HOLD", None),
        decision("SOLUSDT", "LONG", Some(100.0)),
    ]})
    .to_string();
    t.ok("POST", &format!("/api/agents/{id}/run"), None).await;
    let decisions = t
        .ok("GET", &format!("/api/decisions?agentId={id}"), None)
        .await;
    let outcomes: Vec<(String, String)> = decisions
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["symbol"].as_str().unwrap().into(),
                d["outcome"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert!(
        outcomes.contains(&("BTCUSDT".into(), "HOLD".into())),
        "{outcomes:?}"
    );
    assert!(outcomes.contains(&("ETHUSDT".into(), "INVALID_OUTPUT".into())));
    // A decision for a pair the agent does not trade is never used.
    assert!(!outcomes.iter().any(|(s, _)| s == "SOLUSDT"));
    let agent = t.ok("GET", "/api/agents", None).await[0].clone();
    assert_ne!(
        agent["status"], "ERROR",
        "one pair failing is not a failed cycle"
    );
    assert_eq!(agent["consecutiveErrors"], 0);

    // An answer that is not the decisions object fails every pair.
    *t.gateway.text.lock().unwrap() = decision("BTCUSDT", "HOLD", None).to_string();
    t.ok("POST", &format!("/api/agents/{id}/run"), None).await;
    let agent = t.ok("GET", "/api/agents", None).await[0].clone();
    assert_eq!(agent["status"], "ERROR");
    assert_eq!(agent["consecutiveErrors"], 1);
    t.close().await;
}

#[tokio::test]
async fn an_agent_takes_one_to_ten_distinct_pairs() {
    let t = Test::new().await;
    let provider = t.provider().await;
    let account = t
        .ok(
            "POST",
            "/api/accounts",
            Some(json!({"name":"A","source":"REPLAY"})),
        )
        .await;
    let input = |symbols: Value| {
        json!({"name":"A","providerId":provider,"model":"m","symbols":symbols,"timeframe":"15m",
            "strategy":"","accountId":account["id"],"intervalSec":900})
    };
    for (symbols, error) in [
        (json!([]), "symbols: choose at least one pair"),
        (
            json!(["BTCUSDT", "BTCUSDT"]),
            "symbols.1: each pair can be chosen once",
        ),
        (json!(["btc"]), "symbols.0: expected a pair like DOGEUSDT"),
    ] {
        assert_eq!(
            t.send("POST", "/api/agents", Some(input(symbols))).await,
            (400, json!({ "error": error }))
        );
    }
    let eleven: Vec<String> = (0..11).map(|i| format!("P{i}USDT")).collect();
    assert_eq!(
        t.send("POST", "/api/agents", Some(input(json!(eleven))))
            .await,
        (
            400,
            json!({"error":"symbols: an agent trades at most 10 pairs"})
        )
    );
    t.close().await;
}

/// Every decision of the agent as (symbol, outcome).
async fn outcomes(t: &Test, id: &str) -> Vec<(String, String)> {
    t.ok("GET", &format!("/api/decisions?agentId={id}"), None)
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["symbol"].as_str().unwrap().into(),
                d["outcome"].as_str().unwrap().into(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_refused_key_on_the_second_call_pauses_a_batch_another_pair_settled() {
    let t = Test::new().await;
    let id = multi_agent(&t, &["BTCUSDT", "ETHUSDT"]).await;
    // BTC holds; ETH is refused for its missing stop loss and asked again, and the provider
    // then refuses the key.
    *t.gateway.text.lock().unwrap() = json!({"decisions":[
        decision("BTCUSDT", "HOLD", None),
        decision("ETHUSDT", "LONG", None),
    ]})
    .to_string();
    *t.gateway.refuse_from.lock().unwrap() = Some(2);
    t.ok("POST", &format!("/api/agents/{id}/run"), None).await;
    assert_eq!(t.gateway.requests.lock().unwrap().len(), 2);
    let agent = t.ok("GET", "/api/agents", None).await[0].clone();
    assert_eq!(agent["status"], "PAUSED", "{agent}");
    assert!(agent["statusDetail"]
        .as_str()
        .unwrap()
        .starts_with("Auto-paused"));
    t.close().await;
}

#[tokio::test]
async fn a_stale_entry_that_cannot_be_cancelled_holds_back_only_its_pair() {
    let t = Test::new().await;
    let id = multi_agent(&t, &["BTCUSDT", "ETHUSDT"]).await;
    let limit = |symbol: &str, entry: f64| {
        json!({"symbol":symbol,"action":"LONG","orderType":"LIMIT","entryPrice":entry,
               "stopLoss":entry * 0.9,"takeProfit":null,"reason":"test","confidence":0.8,
               "expiresAt":null})
    };
    *t.gateway.text.lock().unwrap() =
        json!({"decisions":[limit("BTCUSDT", 1000.0), limit("ETHUSDT", 100.0)]}).to_string();
    t.ok("POST", &format!("/api/agents/{id}/run"), None).await;
    let placed = outcomes(&t, &id).await;
    assert_eq!(
        placed.iter().filter(|(_, o)| o == "ORDER_PLACED").count(),
        2,
        "{placed:?}"
    );

    // BTC's resting entry can no longer be cancelled; ETH's can.
    t.state
        .runtime
        .call(|r| {
            r.connection()
                .execute_batch(
                    "CREATE TRIGGER keep_btc BEFORE UPDATE ON orders WHEN OLD.symbol='BTCUSDT'
                     BEGIN SELECT RAISE(ABORT, 'order locked'); END;",
                )
                .unwrap()
        })
        .await
        .unwrap();
    *t.gateway.text.lock().unwrap() = json!({"decisions":[
        decision("BTCUSDT", "HOLD", None),
        decision("ETHUSDT", "HOLD", None),
    ]})
    .to_string();
    t.ok("POST", &format!("/api/agents/{id}/run"), None).await;
    let after = outcomes(&t, &id).await;
    assert!(
        after.contains(&("BTCUSDT".into(), "DATA_ERROR".into())),
        "{after:?}"
    );
    assert!(
        after.contains(&("ETHUSDT".into(), "HOLD".into())),
        "{after:?}"
    );
    let agent = t.ok("GET", "/api/agents", None).await[0].clone();
    let account = agent["accountId"].as_str().unwrap().to_string();
    let open: Vec<String> = t
        .state
        .runtime
        .call(move |r| {
            r.engine()
                .store()
                .list_open_orders(&account)
                .unwrap()
                .into_iter()
                .map(|o| o.symbol)
                .collect()
        })
        .await
        .unwrap();
    assert_eq!(open, ["BTCUSDT"]);
    t.close().await;
}

#[tokio::test]
async fn a_pair_whose_record_cannot_be_saved_keeps_its_earlier_suggestion() {
    let t = Test::new().await;
    let id = multi_agent_in(&t, &["BTCUSDT", "ETHUSDT"], "SIGNAL").await;
    *t.gateway.text.lock().unwrap() = json!({"decisions":[
        decision("BTCUSDT", "LONG", Some(60000.0)),
        decision("ETHUSDT", "LONG", Some(3000.0)),
    ]})
    .to_string();
    t.ok("POST", &format!("/api/agents/{id}/run"), None).await;
    let first = outcomes(&t, &id).await;
    assert_eq!(
        first.iter().filter(|(_, o)| o == "SIGNAL").count(),
        2,
        "{first:?}"
    );

    refuse_eth_records(&t).await;
    let (status, _) = t.send("POST", &format!("/api/agents/{id}/run"), None).await;
    assert_ne!(status, 200);
    // BTC's suggestion was replaced. ETH's new one could not be saved, so its earlier one
    // is still pending rather than retired with nothing in its place.
    let after = outcomes(&t, &id).await;
    let count = |symbol: &str, outcome: &str| {
        after
            .iter()
            .filter(|(s, o)| s == symbol && o == outcome)
            .count()
    };
    assert_eq!(count("BTCUSDT", "SIGNAL"), 1, "{after:?}");
    assert_eq!(count("BTCUSDT", "SIGNAL_EXPIRED"), 1, "{after:?}");
    assert_eq!(count("ETHUSDT", "SIGNAL"), 1, "{after:?}");
    assert_eq!(count("ETHUSDT", "SIGNAL_EXPIRED"), 0, "{after:?}");
    t.close().await;
}

/// The next ETH decision record fails to insert, as a full disk would make it.
async fn refuse_eth_records(t: &Test) {
    t.state
        .runtime
        .call(|r| {
            r.connection()
                .execute_batch(
                    "CREATE TRIGGER no_eth BEFORE INSERT ON decisions
                     WHEN json_extract(NEW.body, '$.symbol')='ETHUSDT'
                     BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
                )
                .unwrap()
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn an_executed_pair_keeps_its_record_when_another_pair_cannot_save() {
    let t = Test::new().await;
    let id = multi_agent(&t, &["BTCUSDT", "ETHUSDT"]).await;
    refuse_eth_records(&t).await;
    *t.gateway.text.lock().unwrap() = json!({"decisions":[
        decision("BTCUSDT", "LONG", Some(60000.0)),
        decision("ETHUSDT", "HOLD", None),
    ]})
    .to_string();
    let (status, _) = t.send("POST", &format!("/api/agents/{id}/run"), None).await;
    assert_ne!(status, 200);
    // The BTC order went through, and its record says so.
    assert_eq!(
        outcomes(&t, &id).await,
        [("BTCUSDT".to_string(), "EXECUTED".to_string())]
    );
    t.close().await;
}

#[tokio::test]
async fn a_suggestion_waiting_on_another_pair_s_retry_is_not_kept_after_stop_all() {
    let t = Test::new().await;
    let id = multi_agent_in(&t, &["BTCUSDT", "ETHUSDT"], "SIGNAL").await;
    // BTC is a suggestion at once; ETH is refused for its missing stop and asked again.
    *t.gateway.text.lock().unwrap() = json!({"decisions":[
        decision("BTCUSDT", "LONG", Some(60000.0)),
        decision("ETHUSDT", "LONG", None),
    ]})
    .to_string();
    *t.gateway.then.lock().unwrap() =
        Some(json!({"decisions":[decision("ETHUSDT", "HOLD", None)]}).to_string());
    t.gateway.blocked.store(true, Ordering::SeqCst);
    let state = t.state.clone();
    let path = format!("/api/agents/{id}/run");
    let run = tokio::spawn(async move { send(&state, "POST", &path, None).await });
    // The first call answers; the second, the retry, waits until Stop All has run. Bounded, so
    // a missing retry fails the test instead of hanging it.
    let bound = Duration::from_secs(5);
    timeout(bound, t.gateway.entered.notified())
        .await
        .expect("the first call");
    t.gateway.release.notify_one();
    timeout(bound, t.gateway.entered.notified())
        .await
        .expect("the retry");
    t.ok("POST", "/api/agents/stop-all", None).await;
    t.gateway.blocked.store(false, Ordering::SeqCst);
    t.gateway.release.notify_one();
    timeout(bound, run).await.expect("the run ends").unwrap();

    let after = outcomes(&t, &id).await;
    assert!(!after.iter().any(|(_, o)| o == "SIGNAL"), "{after:?}");
    assert!(
        after.contains(&("BTCUSDT".into(), "ANALYSIS_ONLY".into())),
        "{after:?}"
    );
    t.close().await;
}
