//! The OKX public adapter against `crates/golden/okx-public.json`, which
//! `apps/server/golden/okx-public.ts` recorded from Node's `OkxMarketData` over fixed OKX
//! payloads. The same payloads go through the Rust feed, which must produce the same JSON.

mod fake_okx;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use axum::Router;
use fake_okx::{FakeOkx, REST, WS};
use mochi_domain::{Quote, Timeframe};
use mochi_market::instrument_parse::parse_okx_instrument;
use mochi_market::okx_public::{parse_okx_candles, OkxPublicFeed};
use serde::Serialize;
use serde_json::Value;

fn golden() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/okx-public.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// JSON as the web app receives it.
fn json<T: Serialize>(value: &T) -> Value {
    serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
}

type Replies = Arc<Mutex<HashMap<String, (u16, Value)>>>;

/// REST that answers from the fixture's URL -> [status, body] table; anything else fails as a
/// network error would.
fn table_router(replies: Replies) -> Router {
    Router::new().fallback(move |uri: Uri| {
        let replies = replies.clone();
        async move {
            let url = format!("{REST}{}", uri.path_and_query().unwrap());
            match replies.lock().unwrap().get(&url) {
                Some((status, body)) => {
                    (StatusCode::from_u16(*status).unwrap(), body.to_string()).into_response()
                }
                None => StatusCode::IM_A_TEAPOT.into_response(),
            }
        }
    })
}

fn load(replies: &Replies, table: &Value) {
    let mut map = replies.lock().unwrap();
    map.clear();
    for (url, reply) in table.as_object().unwrap() {
        map.insert(
            url.clone(),
            (reply[0].as_u64().unwrap() as u16, reply[1].clone()),
        );
    }
}

/// A feed over the table, which the fake answers as Node's fake fetch did, except that an
/// unknown URL is a teapot here and a thrown "fetch failed" there; [`missing_is_fetch_failed`]
/// maps one onto the other.
fn feed(replies: &Replies) -> (OkxPublicFeed, Arc<FakeOkx>) {
    let (fake, _sockets) = FakeOkx::new(table_router(replies.clone()));
    (OkxPublicFeed::with_transport(REST, WS, fake.clone()), fake)
}

fn missing_is_fetch_failed(error: String) -> String {
    if error == "OKX HTTP 418" {
        "fetch failed".to_string()
    } else {
        error
    }
}

fn timeframe(name: &str) -> Timeframe {
    serde_json::from_value(Value::String(name.to_string())).unwrap()
}

#[test]
fn instruments_match() {
    for case in golden()["instruments"].as_array().unwrap() {
        let spec = parse_okx_instrument(&case["row"]).unwrap();
        assert_eq!(json(&spec), case["spec"], "{}", case["row"]);
    }
}

#[tokio::test]
async fn instrument_list_matches() {
    let golden = golden();
    let list = &golden["list"];
    let url = format!("{REST}/api/v5/public/instruments?instType=SWAP");
    let replies: Replies = Arc::default();
    replies
        .lock()
        .unwrap()
        .insert(url.clone(), (200, list["reply"].clone()));
    let (feed, _) = feed(&replies);
    assert_eq!(
        json(&feed.list_instruments(false).await.unwrap()),
        list["specs"]
    );

    replies
        .lock()
        .unwrap()
        .insert(url, (200, list["badReply"].clone()));
    let error = feed.list_instruments(true).await.unwrap_err();
    assert_eq!(Value::String(error), list["badError"]);
}

#[tokio::test]
async fn candles_match() {
    let golden = golden();
    let candles = &golden["candles"];
    let rows = candles["replies"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .map(|reply| reply[1]["data"].as_array().unwrap().clone())
        .unwrap();
    assert_eq!(rows.len(), 3, "the first reply carries every sample row");
    assert_eq!(json(&parse_okx_candles(&rows).unwrap()), candles["parsed"]);

    let replies: Replies = Arc::default();
    load(&replies, &candles["replies"]);
    let (feed, fake) = feed(&replies);
    for case in candles["results"].as_array().unwrap() {
        let before = fake.gets.lock().unwrap().len();
        let outcome = feed
            .get_candles(
                case["symbol"].as_str().unwrap(),
                timeframe(case["timeframe"].as_str().unwrap()),
                case["limit"].as_u64().unwrap() as usize,
            )
            .await;
        let asked = fake
            .gets
            .lock()
            .unwrap()
            .get(before)
            .map(|(_, url)| url.clone());
        assert_eq!(asked.as_deref(), case["url"].as_str(), "{case}");
        match outcome {
            Ok(got) => assert_eq!(json(&got), case["candles"], "{case}"),
            Err(error) => assert_eq!(
                Value::String(missing_is_fetch_failed(error)),
                case["error"],
                "{case}"
            ),
        }
    }
}

fn track_all(feed: &OkxPublicFeed, tracked: &Value) {
    for symbol in tracked.as_array().unwrap() {
        feed.track(symbol.as_str().unwrap());
    }
}

fn quotes_of(feed: &OkxPublicFeed) -> Vec<Value> {
    ["BTCUSDT", "ETHUSDT", "SOLUSDT", "DOGEUSDT", "XRPUSDT"]
        .iter()
        .map(|symbol| json(&feed.get_quote(symbol)))
        .collect()
}

#[test]
fn socket_messages_merge_into_the_same_quotes() {
    let golden = golden();
    let messages = &golden["messages"];
    let replies: Replies = Arc::default();
    let (feed, _) = feed(&replies);
    track_all(&feed, &messages["tracked"]);
    for step in messages["steps"].as_array().unwrap() {
        let quote = feed.handle_message(step["raw"].as_str().unwrap());
        assert_eq!(json(&quote), step["quote"], "{}", step["raw"]);
    }
    assert_eq!(Value::Array(quotes_of(&feed)), messages["final"]);
}

#[tokio::test]
async fn rest_polls_merge_into_the_same_quotes() {
    let golden = golden();
    let polls = &golden["polls"];
    let replies: Replies = Arc::default();
    let (feed, _) = feed(&replies);
    track_all(&feed, &polls["tracked"]);
    let mut quotes = feed.quotes();
    for round in polls["rounds"].as_array().unwrap() {
        load(&replies, &round["replies"]);
        feed.poll_rest().await;
        let mut emitted: Vec<Quote> = Vec::new();
        while let Ok(quote) = quotes.try_recv() {
            emitted.push(quote);
        }
        emitted.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        assert_eq!(json(&emitted), round["emitted"]);
        assert_eq!(Value::Array(quotes_of(&feed)), round["quotes"]);
    }
}
