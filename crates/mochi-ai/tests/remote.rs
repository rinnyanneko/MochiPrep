use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    routing::post,
    Router,
};
use mochi_ai::{
    AiProvider, AiRequest, ProviderConfig, ProviderErrorCode, ProviderKind, RemoteOptions,
    RemoteProvider,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone)]
struct Reply {
    status: u16,
    body: String,
    delay: Duration,
}
impl Reply {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            body: body.to_string(),
            delay: Duration::ZERO,
        }
    }
    fn error(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }
}
type Captured = Arc<Mutex<Vec<(String, HeaderMap, Value)>>>;
#[derive(Clone)]
struct ServerState {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    captured: Captured,
}
async fn handler(
    State(s): State<ServerState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    s.captured.lock().unwrap().push((
        uri.to_string(),
        headers,
        serde_json::from_slice(&body).unwrap(),
    ));
    let r = s
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .expect("unexpected retry");
    tokio::time::sleep(r.delay).await;
    (StatusCode::from_u16(r.status).unwrap(), r.body)
}
async fn serve(replies: Vec<Reply>) -> (String, Captured, tokio::task::JoinHandle<()>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let state = ServerState {
        replies: Arc::new(Mutex::new(replies.into())),
        captured: captured.clone(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(post(handler)).with_state(state);
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, captured, task)
}
fn config(kind: ProviderKind, base: &str) -> ProviderConfig {
    serde_json::from_value(json!({"id":"p","name":"test","kind":kind,"baseUrl":format!("{base}///"),"defaultModel":"unused","inputPricePerMTok":null,"outputPricePerMTok":null,"extraHeaders":{"Authorization":"evil","Content-Type":"text/plain","x-api-key":"evil","x-goog-api-key":"evil","anthropic-version":"evil","x-gateway":"header-secret"},"createdAt":0})).unwrap()
}
fn request() -> AiRequest {
    AiRequest {
        model: "model/with space".into(),
        system: "system".into(),
        user: "user".into(),
        context: None,
    }
}
fn options() -> RemoteOptions {
    RemoteOptions {
        timeout: Duration::from_secs(2),
        retry_delays: vec![Duration::ZERO, Duration::ZERO],
    }
}
fn success() -> Reply {
    Reply::json(
        json!({"choices":[{"message":{"content":"{}"}}],"usage":{"prompt_tokens":4,"completion_tokens":2}}),
    )
}
#[tokio::test]
async fn openai_family_paths_reserved_headers_json_modes_and_tokens() {
    for kind in [
        ProviderKind::Openai,
        ProviderKind::OpenaiCompatible,
        ProviderKind::Deepseek,
        ProviderKind::Opencode,
    ] {
        let (base, captured, task) = serve(vec![success()]).await;
        let p =
            RemoteProvider::with_options(config(kind, &base), "provider-secret".into(), options())
                .unwrap();
        let r = p.complete(&request()).await.unwrap();
        assert_eq!(p.kind(), kind);
        assert_eq!(
            (r.text, r.input_tokens, r.output_tokens),
            ("{}".into(), Some(4.), Some(2.))
        );
        let rows = captured.lock().unwrap();
        let (path, headers, body) = &rows[0];
        assert_eq!(path, "/chat/completions");
        assert_eq!(headers["authorization"], "Bearer provider-secret");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["x-gateway"], "header-secret");
        let mut expected = json!({"model":"model/with space","messages":[{"role":"system","content":"system"},{"role":"user","content":"user"}]});
        if matches!(kind, ProviderKind::Openai | ProviderKind::Deepseek) {
            expected["response_format"] = json!({"type":"json_object"});
        }
        assert_eq!(*body, expected);
        task.abort();
    }
}
#[tokio::test]
async fn anthropic_path_body_headers_and_text_blocks() {
    let(base,captured,task)=serve(vec![Reply::json(json!({"content":[{"type":"text","text":"a"},{"type":"tool_use","text":"ignore"},{"type":"text","text":"b"}],"usage":{"input_tokens":5,"output_tokens":3}}))]).await;
    let p = RemoteProvider::with_options(
        config(ProviderKind::Anthropic, &base),
        "provider-secret".into(),
        options(),
    )
    .unwrap();
    let r = p.complete(&request()).await.unwrap();
    assert_eq!(
        (r.text, r.input_tokens, r.output_tokens),
        ("ab".into(), Some(5.), Some(3.))
    );
    let rows = captured.lock().unwrap();
    let (path, headers, body) = &rows[0];
    assert_eq!(path, "/v1/messages");
    assert_eq!(headers["x-api-key"], "provider-secret");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert_eq!(
        *body,
        json!({"model":"model/with space","max_tokens":2048,"system":"system","messages":[{"role":"user","content":"user"}]})
    );
    task.abort();
}
#[tokio::test]
async fn gemini_encoded_path_body_headers_and_parts() {
    let(base,captured,task)=serve(vec![Reply::json(json!({"candidates":[{"content":{"parts":[{"text":"a"},{"inlineData":{}},{"text":"b"}]}}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":4}}))]).await;
    let p = RemoteProvider::with_options(
        config(ProviderKind::Gemini, &base),
        "provider-secret".into(),
        options(),
    )
    .unwrap();
    let r = p.complete(&request()).await.unwrap();
    assert_eq!(
        (r.text, r.input_tokens, r.output_tokens),
        ("ab".into(), Some(7.), Some(4.))
    );
    let rows = captured.lock().unwrap();
    let (path, headers, body) = &rows[0];
    assert_eq!(path, "/v1beta/models/model%2Fwith%20space:generateContent");
    assert_eq!(headers["x-goog-api-key"], "provider-secret");
    assert_eq!(
        *body,
        json!({"systemInstruction":{"parts":[{"text":"system"}]},"contents":[{"role":"user","parts":[{"text":"user"}]}],"generationConfig":{"responseMimeType":"application/json"}})
    );
    task.abort();
}
#[tokio::test]
async fn blank_missing_and_malformed_responses_do_not_retry() {
    for (kind, body, message) in [
        (
            ProviderKind::Openai,
            "{\"choices\":[{\"message\":{\"content\":\" \\uFEFF\\t\"}}]}",
            "No message content in the response",
        ),
        (
            ProviderKind::Openai,
            "{}",
            "No message content in the response",
        ),
        (
            ProviderKind::Anthropic,
            "{}",
            "No text block in the response",
        ),
        (
            ProviderKind::Gemini,
            "{}",
            "No candidate text in the response",
        ),
        (
            ProviderKind::Openai,
            "not-json",
            "Provider returned a body that is not JSON",
        ),
    ] {
        let (base, captured, task) = serve(vec![Reply::error(200, body)]).await;
        let p = RemoteProvider::with_options(config(kind, &base), "key".into(), options()).unwrap();
        let e = p.complete(&request()).await.unwrap_err();
        assert_eq!(e.code, ProviderErrorCode::BadResponse);
        assert_eq!(e.message, message);
        assert_eq!(captured.lock().unwrap().len(), 1);
        task.abort();
    }
}
#[tokio::test]
async fn status_classification_retry_attempts_and_redaction() {
    for (status, code, count, critical) in [
        (401, ProviderErrorCode::Auth, 1, true),
        (403, ProviderErrorCode::Auth, 1, true),
        (429, ProviderErrorCode::RateLimit, 3, false),
        (500, ProviderErrorCode::Server, 3, false),
        (503, ProviderErrorCode::Server, 3, false),
        (400, ProviderErrorCode::Http, 1, false),
    ] {
        let (base, captured, task) =
            serve(vec![
                Reply::error(status, "provider-secret header-secret");
                count
            ])
            .await;
        let p = RemoteProvider::with_options(
            config(ProviderKind::Openai, &base),
            "provider-secret".into(),
            options(),
        )
        .unwrap();
        let e = p.complete(&request()).await.unwrap_err();
        assert_eq!(e.code, code);
        assert_eq!(e.critical(), critical);
        assert!(!e.message.contains("provider-secret"));
        assert!(!e.message.contains("header-secret"));
        assert_eq!(captured.lock().unwrap().len(), count);
        assert_eq!(
            e.message,
            format!(
                "Provider returned HTTP {status}: [REDACTED] [REDACTED]{}",
                if count > 1 { " (after 3 attempts)" } else { "" }
            )
        );
        task.abort();
    }
}
#[tokio::test]
async fn transient_failure_then_success_retries() {
    let (base, captured, task) = serve(vec![
        Reply::error(429, "wait"),
        Reply::error(502, "retry"),
        success(),
    ])
    .await;
    let p =
        RemoteProvider::with_options(config(ProviderKind::Openai, &base), "key".into(), options())
            .unwrap();
    assert_eq!(p.complete(&request()).await.unwrap().text, "{}");
    assert_eq!(captured.lock().unwrap().len(), 3);
    task.abort();
}
#[tokio::test]
async fn timeout_is_not_retried() {
    let mut reply = success();
    reply.delay = Duration::from_millis(200);
    let (base, captured, task) = serve(vec![reply]).await;
    let p = RemoteProvider::with_options(
        config(ProviderKind::Openai, &base),
        "key".into(),
        RemoteOptions {
            timeout: Duration::from_millis(10),
            ..options()
        },
    )
    .unwrap();
    let e = p.complete(&request()).await.unwrap_err();
    assert_eq!(e.code, ProviderErrorCode::Timeout);
    assert_eq!(e.message, "The model did not answer in time");
    assert_eq!(captured.lock().unwrap().len(), 1);
    task.abort();
}
#[tokio::test]
async fn network_failures_retry_and_preserve_attempt_suffix() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        }
    });
    let p =
        RemoteProvider::with_options(config(ProviderKind::Openai, &base), "key".into(), options())
            .unwrap();
    let e = p.complete(&request()).await.unwrap_err();
    assert_eq!(e.code, ProviderErrorCode::Network);
    assert!(e.message.ends_with(" (after 3 attempts)"));
    task.abort();
}
#[tokio::test]
async fn missing_token_usage_is_nullable() {
    let (base, _, task) = serve(vec![Reply::json(
        json!({"choices":[{"message":{"content":"{}"}}]}),
    )])
    .await;
    let p =
        RemoteProvider::with_options(config(ProviderKind::Openai, &base), "key".into(), options())
            .unwrap();
    let r = p.complete(&request()).await.unwrap();
    assert_eq!((r.input_tokens, r.output_tokens), (None, None));
    task.abort();
}
#[test]
fn default_transport_settings_match_node() {
    let o = RemoteOptions::default();
    assert_eq!(o.timeout, Duration::from_secs(60));
    assert_eq!(
        o.retry_delays,
        vec![Duration::from_secs(1), Duration::from_secs(3)]
    );
}
#[tokio::test]
async fn secrets_crossing_error_limit_are_redacted_before_truncating() {
    for secret in ["provider-secret", "header-secret"] {
        let (base, _, task) = serve(vec![Reply::error(
            400,
            &format!("{}{secret}", "x".repeat(395)),
        )])
        .await;
        let p = RemoteProvider::with_options(
            config(ProviderKind::Openai, &base),
            "provider-secret".into(),
            options(),
        )
        .unwrap();
        let e = p.complete(&request()).await.unwrap_err();
        assert!(!e.message.contains("provi"));
        assert!(!e.message.contains("heade"));
        task.abort();
    }
}

#[tokio::test]
async fn auth_after_transient_error_stays_critical_and_reports_attempts() {
    let (base, captured, task) =
        serve(vec![Reply::error(500, "busy"), Reply::error(401, "denied")]).await;
    let p =
        RemoteProvider::with_options(config(ProviderKind::Openai, &base), "key".into(), options())
            .unwrap();
    let e = p.complete(&request()).await.unwrap_err();
    assert_eq!(e.code, ProviderErrorCode::Auth);
    assert!(e.critical());
    assert!(e.message.ends_with(" (after 2 attempts)"));
    assert_eq!(captured.lock().unwrap().len(), 2);
    task.abort();
}
