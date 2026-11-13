//! The remaining Plan 5 vault/OKX writes, persisted read models and the embedded web build.
//!
//! Their reads against Node are byte-checked in `golden_api.rs`; this file covers what the Node
//! script cannot show: every write answering 501, the read models in states the script never
//! reaches (a locked vault, saved OKX keys, a zero balance), the startup seeding over a restart,
//! and the static files.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use mochi_server::config::{config_from, Config};
use mochi_server::{app, AppState, RuntimeHandle, RuntimeOptions};
use rust_embed::RustEmbed;
use serde_json::{json, Value};
use tower::ServiceExt;

const HOST: &str = "localhost:8787";

/// A stand-in for `apps/web/dist`, which a checkout may not have built.
#[derive(RustEmbed)]
#[folder = "tests/web"]
struct TestWeb;

fn config() -> Config {
    let mut config = config_from(|_| None);
    config.data_dir = PathBuf::from(".");
    config
}

fn options(db: rusqlite::Connection) -> RuntimeOptions {
    // A short tape: these tests never step far, and the full one takes a while to generate.
    let mut options = RuntimeOptions::new(db);
    options.replay_minutes = Some(mochi_market::tape::WARMUP_MINUTES + 30);
    options
}

fn state_on(db: rusqlite::Connection) -> AppState {
    AppState::new(
        Arc::new(config()),
        RuntimeHandle::start(options(db)).unwrap(),
    )
}

fn state() -> AppState {
    state_on(mochi_store::open_in_memory().unwrap())
}

async fn respond(state: &AppState, request: Request<Body>) -> Response {
    app(state.clone()).oneshot(request).await.unwrap()
}

async fn body_bytes(response: Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

/// One API request with the session token; the status and the parsed body.
async fn send(state: &AppState, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", HOST)
        .header("authorization", format!("Bearer {}", state.token));
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let request = builder
        .body(Body::from(body.unwrap_or("").to_string()))
        .unwrap();
    let response = respond(state, request).await;
    let status = response.status().as_u16();
    (
        status,
        serde_json::from_slice(&body_bytes(response).await).unwrap(),
    )
}

async fn get(state: &AppState, path: &str) -> Value {
    let (status, body) = send(state, "GET", path, None).await;
    assert_eq!(status, 200, "GET {path}: {body}");
    body
}

/// Runs SQL on the runtime's database.
async fn sql(state: &AppState, statements: &'static str) {
    state
        .runtime
        .call(move |runtime| runtime.connection().execute_batch(statements).unwrap())
        .await
        .unwrap();
}

/// Every write of the later plans' areas, as `apps/server/src/app.ts` registers it.
const LATER_WRITES: &[(&str, &str)] = &[
    ("POST", "/api/vault"),
    ("POST", "/api/vault/unlock"),
    ("POST", "/api/vault/password"),
    ("POST", "/api/vault/disable"),
    ("POST", "/api/vault/reset"),
    ("POST", "/api/exchange/okx/connect"),
    ("POST", "/api/exchange/okx/acct_1/disconnect"),
    ("POST", "/api/exchange/okx/acct_1/arm"),
    ("POST", "/api/exchange/okx/acct_1/disarm"),
];

#[tokio::test]
async fn every_later_write_answers_501() {
    let state = state();
    let not_available = json!({ "error": "Not available in the Rust backend yet" });
    for (method, path) in LATER_WRITES {
        for body in [
            None,
            Some("{}"),
            Some(r#"{"password":"long enough password"}"#),
        ] {
            let answer = send(&state, method, path, body).await;
            assert_eq!(
                answer,
                (501, not_available.clone()),
                "{method} {path} {body:?}"
            );
        }
        // Fastify parses the body before any handler runs, so bad JSON is still Node's 400.
        let (status, body) = send(&state, method, path, Some("{")).await;
        assert_eq!(status, 400, "{method} {path}: {body}");
    }
    // Nothing was written: the seeded mock provider is still the only one.
    let providers = get(&state, "/api/providers").await;
    assert_eq!(providers.as_array().unwrap().len(), 1);
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn later_reads_on_an_empty_database() {
    let state = state();
    for path in [
        "/api/agents",
        "/api/signals",
        "/api/decisions",
        "/api/arena",
        "/api/exchange/status",
    ] {
        assert_eq!(get(&state, path).await, json!([]), "{path}");
    }
    assert_eq!(get(&state, "/api/vault").await, json!({ "status": "OFF" }));
    let providers = get(&state, "/api/providers").await;
    assert_eq!(providers[0]["id"], "mock");
    assert_eq!(providers[0]["name"], "Mock AI (offline demo)");
    assert_eq!(providers[0]["kind"], "MOCK");
    assert_eq!(providers[0]["defaultModel"], "mock-momentum-v1");
    assert_eq!(providers[0]["hasKey"], true);
    assert_eq!(providers[0]["requiresKey"], false);
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn a_locked_vault_hides_plaintext_keys_and_saved_okx_keys_show() {
    let state = state();
    let (_, account) = send(
        &state,
        "POST",
        "/api/accounts",
        Some(r#"{"name":"OKX","source":"LIVE"}"#),
    )
    .await;
    let id = account["id"].as_str().unwrap().to_string();
    sql(
        &state,
        "INSERT INTO vault_keys (id,kind,params,created_at) VALUES ('password','scrypt','{}',1);
         INSERT INTO providers (id,name,kind,base_url,default_model,input_price,output_price,
           extra_headers,created_at) VALUES ('prov_a','A','OPENAI',NULL,'m',NULL,3,'{\"x-k\":\"v\"}',2);
         INSERT INTO provider_keys (provider_id,api_key,updated_at) VALUES ('prov_a','sk-not-shown',1);",
    )
    .await;
    let id_for_sql = id.clone();
    state
        .runtime
        .call(move |runtime| {
            let conn = runtime.connection();
            conn.execute(
                "UPDATE accounts SET venue='OKX_LIVE' WHERE id=?1",
                [&id_for_sql],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sealed_exchange_credentials (account_id,sealed,updated_at)
                 VALUES (?1,'sealed',1)",
                [&id_for_sql],
            )
            .unwrap();
        })
        .await
        .unwrap();

    assert_eq!(
        get(&state, "/api/vault").await,
        json!({ "status": "LOCKED" })
    );
    let providers = get(&state, "/api/providers").await;
    // Node holds no keys in memory while locked, so only the mock provider has one.
    // Oldest first: it was created at 2 ms, the mock provider when the runtime started.
    let gateway = &providers[0];
    assert_eq!(gateway["id"], "prov_a");
    assert_eq!(gateway["hasKey"], false);
    assert_eq!(gateway["headerNames"], json!(["x-k"]));
    // A REAL column read back as JS writes it: 3, not 3.0.
    let (_, text) = raw_get(&state, "/api/providers").await;
    assert!(
        text.contains(r#""inputPricePerMTok":null,"outputPricePerMTok":3,"#),
        "{text}"
    );
    assert!(
        !text.contains("sk-not-shown") && !text.contains("\"v\""),
        "{text}"
    );

    let statuses = get(&state, "/api/exchange/status").await;
    assert_eq!(
        statuses,
        json!([{
            "accountId": id, "name": "OKX", "venue": "OKX_LIVE", "connected": false,
            "saved": true, "armed": false, "healthy": false, "lastSyncAt": null, "error": null,
            "limits": {
                "maxLeverage": 3, "maxRiskPerTradePct": 0.5, "maxDailyLossPct": 2,
                "maxOpenPositions": 2, "requireStopLoss": true, "maxQuoteAgeSec": 10,
                "maxNotional": 100,
            },
        }])
    );
    state.runtime.shutdown().await;
}

async fn raw_get(state: &AppState, path: &str) -> (u16, String) {
    let request = Request::get(path)
        .header("host", HOST)
        .header("authorization", format!("Bearer {}", state.token))
        .body(Body::empty())
        .unwrap();
    let response = respond(state, request).await;
    let status = response.status().as_u16();
    (
        status,
        String::from_utf8(body_bytes(response).await).unwrap(),
    )
}

#[tokio::test]
async fn arena_return_is_null_for_a_zero_balance() {
    let state = state();
    let (_, account) = send(
        &state,
        "POST",
        "/api/accounts",
        Some(r#"{"name":"Empty","source":"REPLAY","initialBalance":"0"}"#),
    )
    .await;
    let id = account["id"].as_str().unwrap().to_string();
    state
        .runtime
        .call(move |runtime| {
            runtime
                .connection()
                .execute(
                    "INSERT INTO agents (id,name,provider_id,model,symbol,timeframe,strategy,
                     account_id,risk,interval_sec,status,running,created_at)
                     VALUES ('agent_z','Z','mock','m','BTCUSDT','15m','',?1,'{}',900,'IDLE',0,1)",
                    [&id],
                )
                .unwrap()
        })
        .await
        .unwrap();
    let arena = get(&state, "/api/arena").await;
    // decimal.js: 0 / 0 is NaN, which JSON.stringify writes as null.
    assert_eq!(arena[0]["returnPct"], Value::Null);
    assert_eq!(arena[0]["isMock"], true);
    assert_eq!(arena[0]["providerName"], "Mock AI (offline demo)");
    assert_eq!(arena[0]["estimatedAiCost"], Value::Null);
    // Opening the account recorded its first equity point.
    assert_eq!(arena[0]["curve"][0]["equity"], json!(0));
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn a_restart_pauses_busy_agents_and_seeds_the_mock_provider_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mochiprep.db");
    let state = state_on(mochi_store::open_database(&path).unwrap());
    let (_, account) = send(
        &state,
        "POST",
        "/api/accounts",
        Some(r#"{"name":"A","source":"REPLAY"}"#),
    )
    .await;
    let id = account["id"].as_str().unwrap().to_string();
    state
        .runtime
        .call(move |runtime| {
            let insert = "INSERT INTO agents (id,name,provider_id,model,symbol,timeframe,strategy,
                account_id,risk,interval_sec,status,running,next_run_at,status_detail,created_at)
                VALUES (?1,?1,'mock','m','BTCUSDT','15m','',?2,'{}',900,?3,?4,5,'before',?5)";
            let conn = runtime.connection();
            for (n, (agent, status, running)) in [
                ("running", "WAITING", 1),
                ("analyzing", "ANALYZING", 0),
                ("executing", "EXECUTING", 0),
                ("idle", "IDLE", 0),
                ("failed", "ERROR", 0),
            ]
            .into_iter()
            .enumerate()
            {
                conn.execute(
                    insert,
                    rusqlite::params![agent, id, status, running, n as i64],
                )
                .unwrap();
            }
        })
        .await
        .unwrap();
    state.runtime.shutdown().await;

    let state = state_on(mochi_store::open_database(&path).unwrap());
    let agents = get(&state, "/api/agents").await;
    let summary: Vec<(String, String, bool, Value, Value)> = agents
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["id"].as_str().unwrap().to_string(),
                a["status"].as_str().unwrap().to_string(),
                a["running"].as_bool().unwrap(),
                a["nextRunAt"].clone(),
                a["statusDetail"].clone(),
            )
        })
        .collect();
    let paused = || json!("Paused because the backend restarted");
    assert_eq!(
        summary,
        [
            (
                "running".into(),
                "PAUSED".into(),
                false,
                Value::Null,
                paused()
            ),
            (
                "analyzing".into(),
                "PAUSED".into(),
                false,
                Value::Null,
                paused()
            ),
            (
                "executing".into(),
                "PAUSED".into(),
                false,
                Value::Null,
                paused()
            ),
            (
                "idle".into(),
                "IDLE".into(),
                false,
                json!(5),
                json!("before")
            ),
            (
                "failed".into(),
                "ERROR".into(),
                false,
                json!(5),
                json!("before")
            ),
        ]
    );
    let providers = get(&state, "/api/providers").await;
    assert_eq!(providers.as_array().unwrap().len(), 1, "{providers}");
    state.runtime.shutdown().await;
}

// ---- static files -----------------------------------------------------------------------------

fn web_state() -> AppState {
    let mut state = state();
    state.web = TestWeb::get;
    state
}

/// A page load: no token, as a browser sends none for `/` or its assets.
fn page(path: &str) -> axum::http::request::Builder {
    Request::get(path).header("host", HOST)
}

fn fixture_file(name: &str) -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/web")
            .join(name),
    )
    .unwrap()
}

#[tokio::test]
async fn index_and_assets_are_served_with_cache_headers() {
    let state = web_state();
    for (path, file, content_type) in [
        ("/", "index.html", "text/html; charset=UTF-8"),
        ("/index.html", "index.html", "text/html; charset=UTF-8"),
        (
            "/assets/app-1a2b3c.js",
            "assets/app-1a2b3c.js",
            "text/javascript; charset=UTF-8",
        ),
        ("/mochi.svg", "mochi.svg", "image/svg+xml"),
    ] {
        let response = respond(&state, page(path).body(Body::empty()).unwrap()).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let headers = response.headers().clone();
        assert_eq!(headers["cache-control"], "public, max-age=0", "{path}");
        assert_eq!(headers["content-type"], content_type, "{path}");
        let etag = headers["etag"].to_str().unwrap().to_string();
        assert!(
            etag.starts_with('"') && etag.ends_with('"') && etag.len() == 66,
            "{path}: {etag}"
        );
        assert_eq!(body_bytes(response).await, fixture_file(file), "{path}");

        // A revalidation with the same tag is answered without the body.
        let request = page(path)
            .header("if-none-match", &etag)
            .body(Body::empty())
            .unwrap();
        let response = respond(&state, request).await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{path}");
        assert_eq!(response.headers()["etag"], etag.as_str());
        assert!(body_bytes(response).await.is_empty());
    }

    // HEAD: the same headers and no body.
    let request = Request::head("/")
        .header("host", HOST)
        .body(Body::empty())
        .unwrap();
    let response = respond(&state, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/html; charset=UTF-8"
    );
    assert!(body_bytes(response).await.is_empty());
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn only_files_of_the_build_are_served() {
    let state = web_state();
    for path in [
        "/missing.js",
        // No SPA fallback: the web app routes by `#`, so a deep path is just missing.
        "/settings",
        "/assets/",
        "/assets/../index.html",
        "/assets/..%2Findex.html",
        // Paths are looked up in the embedded build undecoded and never joined onto a disk
        // directory: encoded dots and drive letters name no file, and a backslash is refused.
        "/%2e%2e/x",
        "/assets/%2e%2e/index.html",
        r"/assets\..\index.html",
        "/assets%5C..%5Cindex.html",
        "/C:/Windows/win.ini",
        "//index.html",
        "/API/health",
        "//api/health",
    ] {
        let response = respond(&state, page(path).body(Body::empty()).unwrap()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(
            serde_json::from_slice::<Value>(&body_bytes(response).await).unwrap(),
            json!({ "error": "Not found" })
        );
    }
    // Only GET and HEAD read files.
    let request = Request::post("/index.html")
        .header("host", HOST)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        respond(&state, request).await.status(),
        StatusCode::NOT_FOUND
    );
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn without_a_web_build_there_is_no_ui() {
    let mut state = state();
    state.web = |_| None;
    let response = respond(&state, page("/").body(Body::empty()).unwrap()).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // The API is unaffected.
    assert_eq!(get(&state, "/api/vault").await, json!({ "status": "OFF" }));
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn unknown_host_static_403() {
    let state = web_state();
    for path in ["/", "/index.html", "/assets/app-1a2b3c.js", "/missing.js"] {
        for (host, origin, error) in [
            ("evil.invalid", None, "Host not allowed"),
            (HOST, Some("http://evil.invalid"), "Origin not allowed"),
        ] {
            let mut request = Request::get(path).header("host", host);
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            let response = respond(&state, request.body(Body::empty()).unwrap()).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path} {host}");
            assert_eq!(
                serde_json::from_slice::<Value>(&body_bytes(response).await).unwrap(),
                json!({ "error": error })
            );
        }
    }
    state.runtime.shutdown().await;
}
