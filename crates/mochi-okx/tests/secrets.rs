//! Credentials never show in an error, or in the `Debug` of anything that holds them.

mod common;

use std::sync::{Arc, Mutex};

use common::Canned;
use mochi_okx::{
    AlgoOrdType, AlgoOrderBody, BoxFuture, CancelAlgo, Credentials, HttpReply, HttpRequest,
    OkxClient, OkxError, OrdType, PlaceOrderBody, Side, TdMode, Transport, Venue,
};

const API_KEY: &str = "secret-api-key-7d1f";
const SECRET_KEY: &str = "secret-hmac-key-93ab";
const PASSPHRASE: &str = "secret-passphrase-c04e";

fn credentials() -> Credentials {
    Credentials::new(API_KEY, SECRET_KEY, PASSPHRASE)
}

fn client(reply: Option<(u16, &str)>) -> OkxClient {
    let transport = Canned::new(reply.map(|(status, body)| (status, body.to_string())));
    OkxClient::new(credentials(), Venue::OkxDemo, transport)
}

fn order() -> PlaceOrderBody {
    PlaceOrderBody {
        inst_id: "BTC-USDT-SWAP".into(),
        td_mode: TdMode::Isolated,
        side: Side::Buy,
        ord_type: OrdType::Market,
        sz: "1".into(),
        cl_ord_id: "c".into(),
        px: None,
        reduce_only: None,
        attach_algo_ords: None,
    }
}

fn trail(ratio: Option<&str>) -> AlgoOrderBody {
    AlgoOrderBody {
        inst_id: "BTC-USDT-SWAP".into(),
        td_mode: TdMode::Isolated,
        side: Side::Sell,
        algo_cl_ord_id: "a".into(),
        sz: "1".into(),
        reduce_only: true,
        ord_type: AlgoOrdType::MoveOrderStop,
        sl_trigger_px: None,
        sl_ord_px: None,
        tp_trigger_px: None,
        tp_ord_px: None,
        callback_ratio: ratio.map(str::to_string),
        callback_spread: None,
        active_px: None,
    }
}

fn assert_clean(text: &str) {
    for secret in [API_KEY, SECRET_KEY, PASSPHRASE] {
        assert!(!text.contains(secret), "{text:?} shows a credential");
    }
}

/// One error from every path that makes one.
async fn every_error() -> Vec<OkxError> {
    let ok_empty = r#"{"code":"0","msg":"","data":[]}"#;
    let mut errors = vec![
        client(Some((
            200,
            r#"{"code":"1","msg":"x","data":[{"sCode":"51008","sMsg":"m"}]}"#,
        )))
        .get_positions()
        .await
        .unwrap_err(),
        client(Some((200, r#"{"code":"50113","msg":"Invalid Sign"}"#)))
            .get_positions()
            .await
            .unwrap_err(),
        client(Some((200, r#"{"msg":""}"#)))
            .get_positions()
            .await
            .unwrap_err(),
        client(None).get_positions().await.unwrap_err(),
        client(Some((200, "not json")))
            .get_positions()
            .await
            .unwrap_err(),
        client(Some((502, ok_empty)))
            .get_positions()
            .await
            .unwrap_err(),
        client(Some((200, "null")))
            .get_positions()
            .await
            .unwrap_err(),
        client(Some((200, r#"{"code":"0","data":{}}"#)))
            .get_positions()
            .await
            .unwrap_err(),
        client(Some((200, r#"{"code":"0","data":[1]}"#)))
            .get_positions()
            .await
            .unwrap_err(),
        client(Some((200, ok_empty)))
            .get_config()
            .await
            .unwrap_err(),
        client(Some((200, ok_empty)))
            .place_order(&order())
            .await
            .unwrap_err(),
        client(Some((200, r#"{"code":"0","data":[{"sCode":"1"}]}"#)))
            .place_order(&order())
            .await
            .unwrap_err(),
        client(Some((200, ok_empty)))
            .place_algo(&trail(Some("0.01")))
            .await
            .unwrap_err(),
        client(Some((200, r#"{"code":"0","data":[{}]}"#)))
            .place_algo(&trail(Some("0.01")))
            .await
            .unwrap_err(),
        client(Some((200, ok_empty)))
            .place_algo(&trail(None))
            .await
            .unwrap_err(),
        client(Some((200, r#"{"code":"0","data":[{"sCode":"51400"}]}"#)))
            .cancel_algos(&[CancelAlgo {
                inst_id: "BTC-USDT-SWAP".into(),
                algo_id: "1".into(),
            }])
            .await
            .unwrap_err(),
    ];
    let eleven: Vec<CancelAlgo> = (0..11)
        .map(|i| CancelAlgo {
            inst_id: "BTC-USDT-SWAP".into(),
            algo_id: i.to_string(),
        })
        .collect();
    errors.push(client(None).cancel_algos(&eleven).await.unwrap_err());
    errors
}

#[tokio::test]
async fn no_error_shows_a_credential() {
    let errors = every_error().await;
    assert!(errors
        .iter()
        .any(|e| matches!(e, OkxError::Rejection { .. })));
    assert!(errors.iter().any(|e| matches!(e, OkxError::Unknown(_))));
    assert!(errors.iter().any(|e| matches!(e, OkxError::Invalid(_))));
    for error in &errors {
        assert_clean(&error.to_string());
        assert_clean(&format!("{error:?}"));
    }
}

/// Keeps the last request it was sent, to show its `Debug`.
struct Keep(Mutex<Option<String>>);

impl Transport for Keep {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpReply, String>> {
        assert_eq!(request.header("ok-access-key"), Some(API_KEY));
        *self.0.lock().unwrap() = Some(format!("{request:?}"));
        Box::pin(async { Err("fetch failed".to_string()) })
    }
}

#[tokio::test]
async fn debug_never_shows_a_credential() {
    let keep = Arc::new(Keep(Mutex::new(None)));
    let client = OkxClient::new(credentials(), Venue::OkxLive, keep.clone());
    client.get_config().await.unwrap_err();
    let request = keep.0.lock().unwrap().clone().unwrap();
    assert!(request.contains("OK-ACCESS-KEY"));
    assert_clean(&request);
    assert_clean(&format!("{client:?}"));
    assert_clean(&format!("{:?}", credentials()));
}
