//! `GET /api/events`, against the Node route (`apps/server/src/app.ts:112-131`) and the live
//! quote throttle (`runtime.ts:235-242`). Timing tests run on tokio's paused clock.

use std::future::IntoFuture;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use mochi_domain::{Quote, ServerEvent, ToastLevel};
use mochi_server::config::{config_from, Config};
use mochi_server::{app, AppState, RuntimeHandle, RuntimeOptions};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Instant};
use tower::ServiceExt;

const HOST: &str = "localhost:8787";

fn config() -> Config {
    let mut config = config_from(|_| None);
    config.data_dir = PathBuf::from(".");
    config
}

fn options() -> RuntimeOptions {
    // A short tape: these tests never step far, and the full one takes a while to generate.
    let mut options = RuntimeOptions::new(mochi_store::open_in_memory().unwrap());
    options.replay_minutes = Some(mochi_market::tape::WARMUP_MINUTES + 10);
    options
}

fn state_with(options: RuntimeOptions) -> AppState {
    AppState::new(Arc::new(config()), RuntimeHandle::start(options).unwrap())
}

/// Opens a stream the way the web app does, with the token in the query.
async fn connect(state: &AppState) -> Response {
    let request = Request::get(format!("/api/events?token={}", state.token))
        .header("host", HOST)
        .body(Body::empty())
        .unwrap();
    let response = app(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response
}

/// The next body chunk exactly as written, or `None` once the stream has ended.
async fn next_chunk(body: &mut Body) -> Option<Bytes> {
    loop {
        let frame = timeout(Duration::from_secs(60), body.frame())
            .await
            .expect("a message within 60 s")?
            .unwrap();
        if let Ok(data) = frame.into_data() {
            return Some(data);
        }
    }
}

/// One `data: <JSON>\n\n` message, checked for that exact shape.
fn parse(chunk: &[u8]) -> Value {
    let text = std::str::from_utf8(chunk).unwrap();
    let json = text
        .strip_prefix("data: ")
        .and_then(|rest| rest.strip_suffix("\n\n"))
        .unwrap_or_else(|| panic!("not a data-only message: {text:?}"));
    assert!(!json.contains('\n'), "one data line per message: {text:?}");
    serde_json::from_str(json).unwrap()
}

async fn next_event(body: &mut Body) -> Value {
    parse(&next_chunk(body).await.expect("the stream is still open"))
}

/// Reads past the connect snapshot (two feeds and the replay state).
async fn skip_snapshot(body: &mut Body) {
    for _ in 0..3 {
        next_event(body).await;
    }
}

fn toast(n: usize) -> ServerEvent {
    ServerEvent::Toast {
        level: ToastLevel::Info,
        message: format!("toast {n}"),
    }
}

#[tokio::test]
async fn connect_sends_feed_replay_snapshot_in_order() {
    let state = state_with(options());
    let mut body = connect(&state).await.into_body();
    let snapshot = state.runtime.state().await.unwrap();

    let replay_feed = next_event(&mut body).await;
    assert_eq!(
        replay_feed,
        json!({ "type": "feed", "status": snapshot.feeds[0] })
    );
    assert_eq!(replay_feed["status"]["source"], "REPLAY");
    let live_feed = next_event(&mut body).await;
    assert_eq!(
        live_feed,
        json!({ "type": "feed", "status": snapshot.feeds[1] })
    );
    assert_eq!(live_feed["status"]["source"], "LIVE");
    assert_eq!(
        next_event(&mut body).await,
        json!({ "type": "replay", "state": snapshot.replay })
    );

    // Then whatever the runtime emits.
    state.runtime.call(|rt| rt.emit(toast(1))).await.unwrap();
    assert_eq!(
        next_event(&mut body).await,
        json!({ "type": "toast", "level": "info", "message": "toast 1" })
    );
    state.runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn heartbeat_every_15s() {
    let state = state_with(options());
    let mut body = connect(&state).await.into_body();
    let opened = Instant::now();
    skip_snapshot(&mut body).await;

    // Nothing straight away: the first beat comes after 15 s, then every 15 s.
    // The live tick's arena invalidations come and go in between.
    let mut beat = 0;
    while beat < 3 {
        let event = next_event(&mut body).await;
        if event["type"] == "invalidate" {
            continue;
        }
        beat += 1;
        assert_eq!(event, json!({ "type": "heartbeat" }));
        assert_eq!(opened.elapsed(), Duration::from_secs(15 * beat));
    }
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn messages_are_data_only_lines() {
    let state = state_with(options());
    let response = connect(&state).await;
    let headers = response.headers();
    assert_eq!(headers["content-type"], "text/event-stream");
    assert_eq!(headers["cache-control"], "no-cache, no-transform");

    let mut body = response.into_body();
    state.runtime.call(|rt| rt.emit(toast(1))).await.unwrap();
    // The snapshot and an emitted event: each chunk is exactly `data: <JSON>\n\n`, with no
    // `event:`, `id:`, `retry:` or comment lines.
    for _ in 0..4 {
        let chunk = next_chunk(&mut body).await.unwrap();
        let text = std::str::from_utf8(&chunk).unwrap();
        assert!(text.starts_with("data: {\"type\":\""), "{text:?}");
        assert!(text.ends_with("}\n\n"), "{text:?}");
        assert_eq!(text.matches('\n').count(), 2, "{text:?}");
        parse(&chunk);
    }
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn slow_client_is_dropped_without_blocking_others() {
    let state = state_with(options());
    // Never read: its 256-message buffer fills.
    let mut stalled = connect(&state).await.into_body();

    // Read as fast as messages come, passing each one back.
    let mut reader = connect(&state).await.into_body();
    let (seen, mut seen_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(chunk) = next_chunk(&mut reader).await {
            if seen.send(parse(&chunk)).is_err() {
                break;
            }
        }
    });
    for _ in 0..3 {
        seen_rx.recv().await.unwrap();
    }

    // 400 events in batches of 40, each one awaited by the reading client. The runtime is never
    // held up: every call returns.
    for batch in 0..10 {
        let first = batch * 40;
        timeout(
            Duration::from_secs(5),
            state.runtime.call(move |rt| {
                for n in first..first + 40 {
                    rt.emit(toast(n));
                }
            }),
        )
        .await
        .expect("the runtime is not blocked")
        .unwrap();
        for n in first..first + 40 {
            let event = timeout(Duration::from_secs(5), seen_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(event["message"], format!("toast {n}"));
        }
    }

    // The stalled stream holds what fitted (the snapshot and 253 toasts), then ends.
    let mut count = 0;
    while let Some(chunk) = next_chunk(&mut stalled).await {
        parse(&chunk);
        count += 1;
    }
    assert_eq!(count, 256);
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn a_client_leaving_mid_stream_leaves_the_runtime_alone() {
    let state = state_with(options());
    assert_eq!(state.runtime.subscriber_count(), 0);
    let mut leaving = connect(&state).await.into_body();
    let mut staying = connect(&state).await.into_body();
    assert_eq!(state.runtime.subscriber_count(), 2);
    skip_snapshot(&mut leaving).await;
    skip_snapshot(&mut staying).await;
    state.runtime.call(|rt| rt.emit(toast(1))).await.unwrap();
    assert_eq!(next_event(&mut leaving).await["message"], "toast 1");

    // The browser tab closes: the response body is dropped mid-stream.
    drop(leaving);
    timeout(Duration::from_secs(5), async {
        while state.runtime.subscriber_count() != 1 {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the stream's subscription ends with it");

    // Everything else carries on: calls are answered, events reach the other client.
    state.runtime.call(|rt| rt.emit(toast(2))).await.unwrap();
    assert_eq!(next_event(&mut staying).await["message"], "toast 1");
    assert_eq!(next_event(&mut staying).await["message"], "toast 2");
    state.runtime.step_replay(3).await.unwrap();
    assert!(state.runtime.replay_state().await.unwrap().now.is_some());
    state.runtime.shutdown().await;
}

/// The raw bytes read until the server closes the connection.
async fn read_to_close(stream: &mut TcpStream, buffer: &mut Vec<u8>) {
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    }
}

#[tokio::test]
async fn shutdown_closes_open_streams() {
    let state = state_with(options());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let streams = state.streams.clone();
    let server = tokio::spawn(
        axum::serve(listener, app(state.clone()))
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
                streams.close_all();
            })
            .into_future(),
    );

    let mut client = TcpStream::connect(address).await.unwrap();
    let request = format!(
        "GET /api/events?token={} HTTP/1.1\r\nHost: {HOST}\r\n\r\n",
        state.token
    );
    client.write_all(request.as_bytes()).await.unwrap();
    // Wait for the snapshot, so the stream is open when the server is told to stop.
    let mut received = Vec::new();
    let mut chunk = [0u8; 4096];
    while !String::from_utf8_lossy(&received).contains("\"type\":\"replay\"") {
        let n = timeout(Duration::from_secs(5), client.read(&mut chunk))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "closed before the snapshot");
        received.extend_from_slice(&chunk[..n]);
    }

    stop.send(()).unwrap();
    timeout(Duration::from_secs(5), server)
        .await
        .expect("graceful shutdown does not wait on the open stream")
        .unwrap()
        .unwrap();
    timeout(
        Duration::from_secs(5),
        read_to_close(&mut client, &mut received),
    )
    .await
    .expect("the client sees the stream end");
    // The chunked body was ended properly, not cut off.
    assert!(String::from_utf8_lossy(&received).ends_with("0\r\n\r\n"));
    state.runtime.shutdown().await;
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn live_quote(symbol: &str, last: &str) -> Quote {
    serde_json::from_value(json!({
        "source": "LIVE", "symbol": symbol, "bid": last, "ask": last, "last": last,
        "mark": last, "markEstimated": false, "fundingRate": null, "nextFundingTime": null,
        "ts": unix_ms(),
    }))
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn live_quotes_throttled_per_symbol_250ms() {
    let (quotes, live_quotes) = mpsc::channel(16);
    let mut options = options();
    options.live_quotes = Some(live_quotes);
    let state = state_with(options);
    let mut body = connect(&state).await.into_body();
    skip_snapshot(&mut body).await;

    // (milliseconds after the first quote, symbol, last price)
    let sent = [
        (0, "BTCUSDT", "1"),
        (0, "ETHUSDT", "2"), // another symbol has its own window
        (100, "BTCUSDT", "3"),
        (249, "BTCUSDT", "4"),
        (250, "BTCUSDT", "5"), // 250 ms after the last one sent
        (260, "ETHUSDT", "6"),
        (400, "BTCUSDT", "7"),
        (500, "BTCUSDT", "8"),
    ];
    let start = Instant::now();
    for (at, symbol, last) in sent {
        tokio::time::sleep_until(start + Duration::from_millis(at)).await;
        quotes.send(live_quote(symbol, last)).await.unwrap();
        // Let the runtime take it at this instant, before the clock moves on.
        tokio::task::yield_now().await;
        sleep(Duration::ZERO).await;
    }

    let mut received = Vec::new();
    while received.len() < 5 {
        let event = next_event(&mut body).await;
        if event["type"] == "quote" {
            received.push((
                event["quote"]["symbol"].as_str().unwrap().to_string(),
                event["quote"]["last"].as_str().unwrap().to_string(),
            ));
        }
    }
    let pairs: Vec<(&str, &str)> = received
        .iter()
        .map(|(symbol, last)| (symbol.as_str(), last.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("BTCUSDT", "1"),
            ("ETHUSDT", "2"),
            ("BTCUSDT", "5"),
            ("ETHUSDT", "6"),
            // BTC at 400 ms was only 150 ms after the one sent at 250 ms.
            ("BTCUSDT", "8"),
        ]
    );
    // Nothing else was held back: the next quote would be a later one.
    quotes.send(live_quote("SOLUSDT", "9")).await.unwrap();
    loop {
        let event = next_event(&mut body).await;
        if event["type"] == "quote" {
            assert_eq!(event["quote"]["last"], "9");
            break;
        }
    }
    state.runtime.shutdown().await;
}
