//! The request guard, mirroring `apps/server/test/api-integration.test.ts:152-160` and the
//! `onRequest` hook in `apps/server/src/app.ts`.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::Router;
use mochi_server::config::{config_from, Config};
use mochi_server::{app, auth, routes, AppState, RuntimeHandle};
use serde_json::Value;
use tower::ServiceExt;

const HOST: &str = "localhost:8787";

fn config_with(extra_origins: Option<&str>) -> Config {
    let extra = extra_origins.map(str::to_string);
    let mut config = config_from(move |name| match name {
        "MOCHIPREP_ALLOWED_ORIGINS" => extra.clone(),
        _ => None,
    });
    config.data_dir = PathBuf::from(".");
    config
}

fn state_with(config: Config) -> AppState {
    AppState::new(Arc::new(config), RuntimeHandle::default())
}

fn state() -> AppState {
    state_with(config_with(None))
}

/// The real routes plus a POST that reports how many body bytes reached it.
fn app_with_test_route(state: AppState) -> Router {
    let router = routes::router().route(
        "/api/test/body",
        post(|body: axum::body::Bytes| async move {
            axum::Json(serde_json::json!({ "len": body.len() }))
        }),
    );
    auth::protect(router, state)
}

async fn send(router: Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

fn get(path: &str) -> axum::http::request::Builder {
    Request::get(path).header("host", HOST)
}

fn bearer(state: &AppState) -> String {
    format!("Bearer {}", state.token)
}

#[tokio::test]
async fn missing_token_is_401() {
    let (status, body) = send(
        app(state()),
        get("/api/health").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, serde_json::json!({ "error": "Unauthorized" }));

    // An unmatched API path still asks for the token first.
    let (status, _) = send(
        app(state()),
        get("/api/accounts").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_token_is_401() {
    let state = state();
    let wrong = format!("Bearer {}", "0".repeat(64));
    for value in [
        wrong.as_str(),
        "Bearer ",
        "",
        state.token.as_str(),
        "bearer x",
    ] {
        let request = get("/api/health")
            .header("authorization", value)
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(app(state.clone()), request).await;
        // A bare token without "Bearer " is accepted, as Node's regex strip allows it.
        let expected = if value == state.token {
            StatusCode::OK
        } else {
            StatusCode::UNAUTHORIZED
        };
        assert_eq!(status, expected, "authorization {value:?}");
    }
}

#[tokio::test]
async fn health_with_token_is_ok() {
    let state = state();
    let request = get("/api/health")
        .header("authorization", bearer(&state))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app(state), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({ "ok": true }));
}

#[tokio::test]
async fn unknown_host_is_403_even_for_static() {
    let state = state();
    for path in ["/", "/index.html", "/api/session", "/api/health"] {
        let request = Request::get(path)
            .header("host", "evil.invalid")
            .header("authorization", bearer(&state))
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(app(state.clone()), request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(body, serde_json::json!({ "error": "Host not allowed" }));
    }
    // No Host at all is refused too.
    let (status, _) = send(app(state), Request::get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn foreign_origin_with_valid_token_is_403() {
    let state = state();
    for path in ["/", "/api/session", "/api/health"] {
        let request = get(path)
            .header("origin", "http://evil.invalid")
            .header("authorization", bearer(&state))
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(app(state.clone()), request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(body, serde_json::json!({ "error": "Origin not allowed" }));
    }
}

#[tokio::test]
async fn own_origin_is_200() {
    let state = state();
    let request = get("/api/health")
        .header("origin", "http://localhost:8787")
        .header("authorization", bearer(&state))
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(app(state.clone()), request).await;
    assert_eq!(status, StatusCode::OK);

    // The session endpoint hands the token to an allowed origin without a token of its own.
    let request = get("/api/session")
        .header("origin", "http://localhost:5173")
        .header("sec-fetch-site", "same-site")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app(state.clone()), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({ "token": state.token }));
}

#[tokio::test]
async fn session_refuses_cross_site_without_origin() {
    let state = state();
    for site in ["cross-site", "same-site"] {
        let request = get("/api/session")
            .header("sec-fetch-site", site)
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(app(state.clone()), request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{site}");
        assert_eq!(
            body,
            serde_json::json!({ "error": "Cross-site request refused" })
        );
    }
    for site in [Some("same-origin"), Some("none"), None] {
        let mut request = get("/api/session");
        if let Some(site) = site {
            request = request.header("sec-fetch-site", site);
        }
        let (status, body) = send(app(state.clone()), request.body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{site:?}");
        assert_eq!(body["token"], Value::String(state.token.clone()));
    }
}

#[tokio::test]
async fn token_is_64_hex_chars_and_fresh_per_state() {
    let first = state().token;
    let second = state().token;
    assert_eq!(first.len(), 64);
    assert!(first
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(first, second);
}

#[tokio::test]
async fn events_accepts_query_token_but_header_wins() {
    let state = state();
    // /api/events is not routed until Task 5, so a passed guard shows as the JSON 404.
    let passed = StatusCode::NOT_FOUND;

    let request = get(&format!("/api/events?token={}", state.token))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(app(state.clone()), request).await.0, passed);

    // A wrong header beats a right query token.
    let request = get(&format!("/api/events?token={}", state.token))
        .header("authorization", "Bearer wrong")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(app(state.clone()), request).await.0,
        StatusCode::UNAUTHORIZED
    );

    // Any other path ignores the query token.
    let request = get(&format!("/api/health?token={}", state.token))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(app(state.clone()), request).await.0,
        StatusCode::UNAUTHORIZED
    );

    // A wrong or repeated query token is refused.
    let request = get("/api/events?token=wrong").body(Body::empty()).unwrap();
    assert_eq!(
        send(app(state.clone()), request).await.0,
        StatusCode::UNAUTHORIZED
    );
    let request = get(&format!("/api/events?token={0}&token={0}", state.token))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(app(state), request).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unmatched_api_path_with_token_is_json_404() {
    let state = state();
    for request in [
        get("/api/nope")
            .header("authorization", bearer(&state))
            .body(Body::empty())
            .unwrap(),
        // A wrong method on a real route is a 404 in Fastify, not a 405.
        Request::post("/api/health")
            .header("host", HOST)
            .header("authorization", bearer(&state))
            .body(Body::empty())
            .unwrap(),
    ] {
        let (status, body) = send(app(state.clone()), request).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, serde_json::json!({ "error": "Not found" }));
    }
}

#[tokio::test]
async fn body_over_256_kib_is_413_with_error_json() {
    let state = state();
    let request = |body: Vec<u8>| {
        Request::post("/api/test/body")
            .header("host", HOST)
            .header("authorization", bearer(&state))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    let (status, body) = send(
        app_with_test_route(state.clone()),
        request(vec![b' '; 256 * 1024 + 1]),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        body,
        serde_json::json!({ "error": "Request body is too large" })
    );

    // A body of exactly the limit passes.
    let (status, body) = send(
        app_with_test_route(state.clone()),
        request(vec![b' '; 256 * 1024]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({ "len": 256 * 1024 }));

    // A declared oversize content-length is refused before the body is read.
    let declared = Request::post("/api/test/body")
        .header("host", HOST)
        .header("authorization", bearer(&state))
        .header("content-length", (256 * 1024 + 1).to_string())
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app_with_test_route(state.clone()), declared).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        body,
        serde_json::json!({ "error": "Request body is too large" })
    );

    // A streamed body without content-length is cut off the same way.
    let chunks = vec![
        Ok::<_, std::io::Error>(vec![b' '; 200 * 1024]),
        Ok(vec![b' '; 100 * 1024]),
    ];
    let streamed = Body::from_stream(futures_util::stream::iter(chunks));
    let request = Request::post("/api/test/body")
        .header("host", HOST)
        .header("authorization", bearer(&state))
        .body(streamed)
        .unwrap();
    let (status, _) = send(app_with_test_route(state), request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn empty_post_without_content_type_is_accepted() {
    let state = state();
    let request = Request::post("/api/test/body")
        .header("host", HOST)
        .header("authorization", bearer(&state))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app_with_test_route(state), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({ "len": 0 }));
}

#[tokio::test]
async fn extra_allowed_origins_from_env_are_trusted() {
    let config = config_with(Some(" http://app.example:9000 , ,https://desk.test:443"));
    assert_eq!(
        config.allowed_origins,
        [
            "http://localhost:5173",
            "http://127.0.0.1:5173",
            "http://localhost:8787",
            "http://127.0.0.1:8787",
            "http://app.example:9000",
            "https://desk.test:443",
        ]
    );
    let state = state_with(config);
    // The Host allowlist is each origin's URL host, so the default https port drops out.
    for (host, origin) in [
        ("app.example:9000", "http://app.example:9000"),
        ("desk.test", "https://desk.test:443"),
    ] {
        let request = Request::get("/api/health")
            .header("host", host)
            .header("origin", origin)
            .header("authorization", bearer(&state))
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(app(state.clone()), request).await;
        assert_eq!(status, StatusCode::OK, "{host}");
    }
    // Without the extra origins, the same Host is refused.
    let request = Request::get("/api/health")
        .header("host", "app.example:9000")
        .header("authorization", bearer(&state))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(app(state_with(config_with(None))), request).await.0,
        StatusCode::FORBIDDEN
    );
}

#[test]
fn config_defaults_and_port() {
    let config = config_from(|name| (name == "MOCHIPREP_PORT").then(|| "9100".to_string()));
    assert_eq!(config.port, 9100);
    assert!(config
        .allowed_origins
        .contains(&"http://127.0.0.1:9100".to_string()));
    assert!(config.data_dir.ends_with(".mochiprep"));
    assert!(config.data_dir.is_absolute());

    let config = config_from(|name| (name == "MOCHIPREP_DATA_DIR").then(|| "some/dir".to_string()));
    assert_eq!(config.port, 8787);
    assert!(config.data_dir.is_absolute());
    assert!(config.data_dir.ends_with("some/dir"));
}
