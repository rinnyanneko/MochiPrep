//! The live feed through the HTTP surface: adding and removing pairs (`addInstrument`,
//! `removeInstrument` in runtime.ts), live candles, and live quotes from the real OKX public
//! feed reaching the paper engine. OKX is a local fake throughout; nothing leaves the machine.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, Uri};
use axum::response::IntoResponse;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use mochi_domain::{
    default_instrument, DataSource, FeedState, FeedStatus, InstrumentSpecification, Quote,
    ServerEvent, Topic,
};
use mochi_market::okx_public::OkxPublicFeed;
use mochi_server::config::{config_from, Config};
use mochi_server::runtime::LiveFeed;
use mochi_server::{app, AppState, RuntimeHandle, RuntimeOptions};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

const HOST: &str = "localhost:8787";

fn config() -> Config {
    let mut config = config_from(|_| None);
    config.data_dir = PathBuf::from(".");
    config
}

fn short_tape(mut options: RuntimeOptions) -> RuntimeOptions {
    options.replay_minutes = Some(mochi_market::tape::WARMUP_MINUTES + 10);
    options
}

async fn send(state: &AppState, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", HOST)
        .header("authorization", format!("Bearer {}", state.token));
    let body = match body {
        Some(body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = app(state.clone())
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn error(status: u16, text: &str) -> (u16, Value) {
    (status, json!({ "error": text }))
}

fn now_ms() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    since.as_millis() as i64
}

fn spec(symbol: &str, base: &str) -> InstrumentSpecification {
    let mut spec = default_instrument("SOLUSDT").unwrap();
    spec.symbol = symbol.into();
    spec.base_currency = base.into();
    spec
}

/// OKX's list and live quotes, from memory; it records what it was asked to track.
#[derive(Default)]
struct FakeFeed {
    tracked: Mutex<Vec<String>>,
    listed: Mutex<Vec<(bool, Vec<InstrumentSpecification>)>>,
}

impl FakeFeed {
    fn list() -> Vec<InstrumentSpecification> {
        vec![spec("DOGEUSDT", "DOGE"), spec("PEPEUSDT", "PEPE")]
    }
}

impl LiveFeed for FakeFeed {
    fn track(&self, symbol: &str) {
        self.tracked.lock().unwrap().push(symbol.to_string());
    }
    fn untrack(&self, symbol: &str) {
        self.tracked.lock().unwrap().retain(|s| s != symbol);
    }
    fn get_quote(&self, symbol: &str) -> Option<Quote> {
        Some(Quote {
            source: DataSource::Live,
            symbol: symbol.to_string(),
            bid: "0.3".into(),
            ask: "0.3".into(),
            last: "0.3".into(),
            mark: "0.3".into(),
            mark_estimated: false,
            funding_rate: None,
            next_funding_time: None,
            ts: now_ms(),
        })
    }
    fn status(&self) -> FeedStatus {
        mochi_server::runtime::OfflineFeed.status()
    }
    fn list_instruments(
        &self,
        fresh: bool,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<InstrumentSpecification>, String>> {
        self.listed.lock().unwrap().push((fresh, Self::list()));
        Box::pin(async { Ok(FakeFeed::list()) })
    }
}

#[tokio::test]
async fn instrument_add_and_remove_texts() {
    let feed = Arc::new(FakeFeed::default());
    let mut options = short_tape(RuntimeOptions::new(mochi_store::open_in_memory().unwrap()));
    options.live = feed.clone();
    let state = AppState::new(Arc::new(config()), RuntimeHandle::start(options).unwrap());
    let mut events = state.runtime.subscribe();

    // Available: OKX's list (cached) minus what is registered.
    let (status, available) = send(&state, "GET", "/api/instruments/available", None).await;
    assert_eq!(status, 200);
    let symbols: Vec<&str> = available
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["symbol"].as_str().unwrap())
        .collect();
    assert_eq!(symbols, ["DOGEUSDT", "PEPEUSDT"]);
    assert!(
        !feed.listed.lock().unwrap()[0].0,
        "the list may come from cache"
    );

    let add = |symbol: &str| Some(json!({ "symbol": symbol }));
    assert_eq!(
        send(&state, "POST", "/api/instruments", add("BTCUSDT")).await,
        error(400, "BTCUSDT is already added")
    );
    assert_eq!(
        send(&state, "POST", "/api/instruments", add("FAKEUSDT")).await,
        error(400, "FAKEUSDT is not a live USDT crypto perpetual on OKX")
    );
    assert_eq!(
        send(&state, "POST", "/api/instruments", add("doge")).await,
        error(400, "symbol: expected a pair like DOGEUSDT")
    );
    let (status, added) = send(&state, "POST", "/api/instruments", add("DOGEUSDT")).await;
    assert_eq!(status, 200);
    assert_eq!(
        added,
        serde_json::to_value(spec("DOGEUSDT", "DOGE")).unwrap()
    );
    // Looked up fresh, never from cache: the spec decides real order sizes.
    assert!(feed.listed.lock().unwrap().last().unwrap().0);
    assert_eq!(*feed.tracked.lock().unwrap(), ["DOGEUSDT"]);
    assert_eq!(
        send(&state, "POST", "/api/instruments", add("DOGEUSDT")).await,
        error(400, "DOGEUSDT is already added")
    );
    let (_, list) = send(&state, "GET", "/api/instruments", None).await;
    assert_eq!(list.as_array().unwrap().len(), 4);
    let (_, available) = send(&state, "GET", "/api/instruments/available", None).await;
    assert_eq!(available.as_array().unwrap().len(), 1);
    let saved = state
        .runtime
        .call(|rt| mochi_store::list_instruments(rt.connection()).unwrap())
        .await
        .unwrap();
    assert_eq!(saved, [spec("DOGEUSDT", "DOGE")]);
    loop {
        if let ServerEvent::Invalidate { topics } = events.recv().await.unwrap() {
            assert_eq!(topics, [Topic::Instruments]);
            break;
        }
    }

    // Removal refusals, in Node's order.
    let remove = |symbol: &str| format!("/api/instruments/{symbol}");
    assert_eq!(
        send(&state, "DELETE", &remove("BTCUSDT"), None).await,
        error(400, "BTCUSDT is built in")
    );
    assert_eq!(
        send(&state, "DELETE", &remove("XRPUSDT"), None).await,
        error(400, "XRPUSDT is not an added pair")
    );
    assert_eq!(
        send(&state, "DELETE", &remove("doge"), None).await,
        error(400, "input: expected a pair like DOGEUSDT")
    );

    let (_, account) = send(
        &state,
        "POST",
        "/api/accounts",
        Some(json!({ "name": "Doge desk", "source": "LIVE", "initialBalance": "1000" })),
    )
    .await;
    let account_id = account["id"].as_str().unwrap().to_string();
    let agent = format!(
        "INSERT INTO agents (id,name,provider_id,model,symbol,timeframe,strategy,account_id,risk,
         interval_sec,status,running,created_at)
         VALUES ('agent_1','A','mock','m','DOGEUSDT','15m','','{account_id}','{{}}',900,'IDLE',0,1)"
    );
    let run = |statement: String| {
        state
            .runtime
            .call(move |rt| rt.connection().execute_batch(&statement).unwrap())
    };
    run(agent).await.unwrap();
    assert_eq!(
        send(&state, "DELETE", &remove("DOGEUSDT"), None).await,
        error(400, "Agents still use this pair")
    );
    run("DELETE FROM agents".into()).await.unwrap();

    let order = |client: &str, kind: &str, price: Value| {
        json!({
            "accountId": account_id, "clientOrderId": client, "symbol": "DOGEUSDT", "side": "BUY",
            "type": kind, "quantity": "100", "price": price, "leverage": 2,
        })
    };
    let (status, placed) = send(
        &state,
        "POST",
        "/api/orders",
        Some(order("m1", "MARKET", Value::Null)),
    )
    .await;
    assert_eq!(status, 200, "{placed}");
    assert_eq!(
        send(&state, "DELETE", &remove("DOGEUSDT"), None).await,
        error(400, "Doge desk has an open DOGEUSDT position")
    );
    let (status, _) = send(
        &state,
        "POST",
        &format!("/api/positions/{account_id}/DOGEUSDT/close"),
        Some(json!({ "fraction": 1, "clientOrderId": "c1" })),
    )
    .await;
    assert_eq!(status, 200);
    let (status, placed) = send(
        &state,
        "POST",
        "/api/orders",
        Some(order("l1", "LIMIT", json!("0.1"))),
    )
    .await;
    assert_eq!(status, 200, "{placed}");
    assert_eq!(
        send(&state, "DELETE", &remove("DOGEUSDT"), None).await,
        error(400, "Doge desk has open DOGEUSDT orders")
    );
    let order_id = placed["order"]["id"].as_str().unwrap().to_string();
    let (status, _) = send(&state, "DELETE", &format!("/api/orders/{order_id}"), None).await;
    assert_eq!(status, 200);

    assert_eq!(
        send(&state, "DELETE", &remove("DOGEUSDT"), None).await,
        (200, json!({ "ok": true }))
    );
    assert!(feed.tracked.lock().unwrap().is_empty());
    let (_, list) = send(&state, "GET", "/api/instruments", None).await;
    assert_eq!(list.as_array().unwrap().len(), 3);
    let saved = state
        .runtime
        .call(|rt| mochi_store::list_instruments(rt.connection()).unwrap())
        .await
        .unwrap();
    assert!(saved.is_empty());
    let in_engine = state
        .runtime
        .call(|rt| rt.engine().instruments().get("DOGEUSDT").is_some())
        .await
        .unwrap();
    assert!(!in_engine);
    state.runtime.shutdown().await;
}

#[tokio::test]
async fn the_offline_feed_says_so() {
    let options = short_tape(RuntimeOptions::new(mochi_store::open_in_memory().unwrap()));
    let state = AppState::new(Arc::new(config()), RuntimeHandle::start(options).unwrap());
    let offline = "Live OKX market data is turned off (MOCHIPREP_OFFLINE=1)";
    assert_eq!(
        send(
            &state,
            "GET",
            "/api/market/candles?source=LIVE&symbol=BTCUSDT&timeframe=1m",
            None
        )
        .await,
        error(400, offline)
    );
    assert_eq!(
        send(&state, "GET", "/api/instruments/available", None).await,
        error(400, offline)
    );
    assert_eq!(
        send(
            &state,
            "POST",
            "/api/instruments",
            Some(json!({ "symbol": "DOGEUSDT" }))
        )
        .await,
        error(400, offline)
    );
    state.runtime.shutdown().await;
}

/// A local OKX: REST on one loopback port, the WebSocket on another.
struct LocalOkx {
    rest_base: String,
    ws: TcpListener,
    candle_urls: Arc<Mutex<Vec<String>>>,
}

async fn local_okx() -> LocalOkx {
    let candle_urls: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = candle_urls.clone();
    let router = Router::new().fallback(move |uri: Uri| {
        let seen = seen.clone();
        async move {
            let data = if uri.path().ends_with("/market/candles") {
                seen.lock().unwrap().push(uri.to_string());
                json!([
                    ["1767229200000", "2", "3", "1", "2.5", "10", "20", "50", "0"],
                    [
                        "1767225600000",
                        "1",
                        "2",
                        "0.5",
                        "1.5",
                        "11",
                        "21",
                        "51",
                        "1"
                    ],
                ])
            } else {
                // No REST quotes, so every quote below came over the socket.
                json!([])
            };
            axum::Json(json!({ "code": "0", "msg": "", "data": data })).into_response()
        }
    });
    let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_base = format!("http://{}", rest.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(rest, router).await.unwrap() });
    let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
    LocalOkx {
        rest_base,
        ws,
        candle_urls,
    }
}

#[tokio::test]
async fn okx_quotes_reach_the_paper_engine_and_candles_are_served() {
    let okx = local_okx().await;
    let ws_url = format!("ws://{}/ws/v5/public", okx.ws.local_addr().unwrap());
    let feed = OkxPublicFeed::new(&okx.rest_base, &ws_url);
    let options = short_tape(RuntimeOptions::with_live_feed(
        mochi_store::open_in_memory().unwrap(),
        feed,
    ));
    let state = AppState::new(Arc::new(config()), RuntimeHandle::start(options).unwrap());
    let mut events = state.runtime.subscribe();
    let wait = Duration::from_secs(10);

    let (stream, _) = timeout(wait, okx.ws.accept()).await.unwrap().unwrap();
    let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
    let subscribe = timeout(wait, server.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(subscribe
        .to_text()
        .unwrap()
        .contains("\"op\":\"subscribe\""));

    // The connection change reaches the web app as a feed event.
    let connected = timeout(wait, async {
        loop {
            if let ServerEvent::Feed { status } = events.recv().await.unwrap() {
                if status.state == FeedState::Connected {
                    return status;
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(connected.provider, "OKX public");

    let tick = json!({
        "arg": { "channel": "tickers", "instId": "BTC-USDT-SWAP" },
        "data": [{ "bidPx": "67890", "askPx": "67890.2", "last": "67890.1", "ts": now_ms().to_string() }]
    });
    server.send(Message::text(tick.to_string())).await.unwrap();
    let quote = timeout(wait, async {
        loop {
            if let ServerEvent::Quote { quote } = events.recv().await.unwrap() {
                return quote;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        (quote.symbol.as_str(), quote.last.as_str()),
        ("BTCUSDT", "67890.1")
    );
    assert!(quote.mark_estimated);
    let (_, quotes) = send(&state, "GET", "/api/market/quotes?source=LIVE", None).await;
    assert_eq!(quotes[0]["last"], "67890.1");

    // The engine prices a live paper market order from that quote: the ask plus slippage.
    let (_, account) = send(
        &state,
        "POST",
        "/api/accounts",
        Some(json!({ "name": "Live", "source": "LIVE", "initialBalance": "10000" })),
    )
    .await;
    let account_id = account["id"].as_str().unwrap();
    let (status, placed) = send(
        &state,
        "POST",
        "/api/orders",
        Some(json!({
            "accountId": account_id, "clientOrderId": "live-1", "symbol": "BTCUSDT", "side": "BUY",
            "type": "MARKET", "quantity": "0.01", "leverage": 2,
        })),
    )
    .await;
    assert_eq!(status, 200, "{placed}");
    assert_eq!(placed["order"]["status"], "FILLED", "{placed}");
    let price: f64 = placed["fills"][0]["price"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((67890.2..67900.0).contains(&price), "{placed}");

    // Live candles come from OKX REST, oldest first, at most 300.
    let (status, candles) = send(
        &state,
        "GET",
        "/api/market/candles?source=LIVE&symbol=BTCUSDT&timeframe=4h&limit=1000",
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        candles,
        json!([
            { "time": 1767225600000i64, "open": 1, "high": 2, "low": 0.5, "close": 1.5, "volume": 21, "closed": true },
            { "time": 1767229200000i64, "open": 2, "high": 3, "low": 1, "close": 2.5, "volume": 20, "closed": false },
        ])
    );
    assert_eq!(
        *okx.candle_urls.lock().unwrap(),
        ["/api/v5/market/candles?instId=BTC-USDT-SWAP&bar=4H&limit=300"]
    );
    state.runtime.shutdown().await;
}
