//! The signed client against `crates/golden/okx-private.json`, which
//! `apps/server/golden/okx-private.ts` recorded from Node's `OkxClient` with a fixed clock: the
//! same signatures and ids, the same request bytes, and the same result or error text for every
//! recorded answer.

mod common;

use std::sync::Arc;

use common::{invoke, outcome, Canned, Sent};
use mochi_okx::{
    iso_string, sign, to_algo_cl_ord_id, to_cl_ord_id, to_inst_id, Credentials, OkxClient, Venue,
};
use serde_json::Value;

fn golden() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/okx-private.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn venue(text: &str) -> Venue {
    serde_json::from_value(Value::String(text.into())).unwrap()
}

fn client(g: &Value, venue: Venue, transport: Arc<Canned>) -> OkxClient {
    let c = &g["credentials"];
    let credentials = Credentials::new(
        c["apiKey"].as_str().unwrap(),
        c["secretKey"].as_str().unwrap(),
        c["passphrase"].as_str().unwrap(),
    );
    let now = g["now"].as_i64().unwrap();
    OkxClient::new(credentials, venue, transport)
        .with_base_url(g["base"].as_str().unwrap())
        .with_clock(move || now)
}

fn reply(value: &Value) -> Option<(u16, String)> {
    let status = u16::try_from(value[0].as_u64().unwrap()).unwrap();
    value[1].as_str().map(|body| (status, body.to_string()))
}

#[test]
fn signs_and_hashes_ids_as_node() {
    let g = golden();
    for case in g["sign"].as_array().unwrap() {
        let s = |k: &str| case[k].as_str().unwrap();
        assert_eq!(
            sign(
                s("secretKey"),
                s("timestamp"),
                s("method"),
                s("path"),
                s("body")
            ),
            s("sign"),
            "{case}"
        );
    }
    for case in g["instIds"].as_array().unwrap() {
        assert_eq!(
            to_inst_id(case["symbol"].as_str().unwrap()),
            case["instId"].as_str().unwrap()
        );
    }
    for case in g["ids"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        assert_eq!(to_cl_ord_id(id), case["clOrdId"].as_str().unwrap());
        assert_eq!(to_algo_cl_ord_id(id), case["algoClOrdId"].as_str().unwrap());
    }
    for case in g["isoTimes"].as_array().unwrap() {
        assert_eq!(
            iso_string(case["ms"].as_i64().unwrap()),
            case["iso"].as_str().unwrap()
        );
    }
}

/// Expected values from openssl, as in packages/exchanges/test/okx-client.test.ts.
#[test]
fn signs_timestamp_method_path_and_body() {
    let timestamp = "2020-12-08T09:08:57.715Z";
    assert_eq!(
        sign(
            "test-secret",
            timestamp,
            "POST",
            "/api/v5/trade/order",
            r#"{"instId":"BTC-USDT-SWAP"}"#
        ),
        "pTcfOssGhm9sSvJrX7EenxdYrhHiqBkSG5mJc6vZoQg="
    );
    assert_eq!(
        sign(
            "test-secret",
            timestamp,
            "GET",
            "/api/v5/account/balance?ccy=USDT",
            ""
        ),
        "Yt2R9ATAX6Jz6+ud53rCYenVK09+CEpafaWGadqhybE="
    );
}

#[tokio::test]
async fn sends_the_same_requests_as_node() {
    let g = golden();
    let cases = g["requests"].as_array().unwrap();
    assert!(cases.len() >= 48);
    for case in cases {
        let transport = Canned::new(reply(&case["reply"]));
        let client = client(
            &g,
            venue(case["venue"].as_str().unwrap()),
            transport.clone(),
        );
        let call = &case["call"];
        let result = invoke(&client, call["name"].as_str().unwrap(), &call["args"]).await;
        assert_eq!(outcome(result), case["outcome"], "{call}");
        let expected: Vec<Sent> = case["sent"]
            .as_array()
            .unwrap()
            .iter()
            .map(|sent| Sent {
                url: sent["url"].as_str().unwrap().into(),
                method: sent["method"].as_str().unwrap().into(),
                headers: sent["headers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|h| (h[0].as_str().unwrap().into(), h[1].as_str().unwrap().into()))
                    .collect(),
                body: sent["body"].as_str().map(str::to_string),
            })
            .collect();
        assert_eq!(*transport.sent.lock().unwrap(), expected, "{call}");
    }
}

#[tokio::test]
async fn reads_every_answer_as_node() {
    let g = golden();
    let cases = g["outcomes"].as_array().unwrap();
    assert!(cases.len() >= 12 * 20);
    for case in cases {
        let name = case["reply"].as_str().unwrap();
        let transport = Canned::new(reply(&g["replies"][name]));
        let client = client(&g, Venue::OkxLive, transport);
        let call = &case["call"];
        let result = invoke(&client, call["name"].as_str().unwrap(), &call["args"]).await;
        assert_eq!(outcome(result), case["outcome"], "{call} with {name}");
    }
}
