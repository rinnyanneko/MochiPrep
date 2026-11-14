//! The fake OKX behind the real client: signature and key checks, the demo header, and the
//! handlers and knobs the engine tests rely on.

use std::sync::Arc;

use mochi_okx::fake::{self, FakeOkx, Refusal};
use mochi_okx::{
    to_algo_cl_ord_id, to_cl_ord_id, AlgoOrdType, AlgoOrderBody, AttachAlgoOrd, CancelAlgo,
    Credentials, OkxClient, OkxError, OrdType, PendingAlgoType, PlaceOrderBody, Row, Side, TdMode,
    Venue,
};
use serde_json::{json, Value};

fn client(okx: &Arc<FakeOkx>, venue: Venue) -> OkxClient {
    OkxClient::new(fake::credentials(), venue, okx.transport()).with_base_url("https://okx.invalid")
}

/// What the runtime asks on connect: the account configuration and each pair's contract spec.
async fn connect(client: &OkxClient) {
    client.get_config().await.unwrap();
    for symbol in ["BTCUSDT", "ETHUSDT", "SOLUSDT"] {
        client.get_contract_value(symbol).await.unwrap();
    }
    client.get_usdt_balance().await.unwrap();
    client.get_positions().await.unwrap();
}

fn market(side: Side, sz: &str, id: &str) -> PlaceOrderBody {
    PlaceOrderBody {
        inst_id: "BTC-USDT-SWAP".into(),
        td_mode: TdMode::Isolated,
        side,
        ord_type: OrdType::Market,
        sz: sz.into(),
        cl_ord_id: to_cl_ord_id(id),
        px: None,
        reduce_only: None,
        attach_algo_ords: None,
    }
}

fn stop(id: &str, trigger: &str) -> AlgoOrderBody {
    AlgoOrderBody {
        inst_id: "BTC-USDT-SWAP".into(),
        td_mode: TdMode::Isolated,
        side: Side::Sell,
        algo_cl_ord_id: to_algo_cl_ord_id(id),
        sz: "10".into(),
        reduce_only: true,
        ord_type: AlgoOrdType::Conditional,
        sl_trigger_px: Some(trigger.into()),
        sl_ord_px: Some("-1".into()),
        tp_trigger_px: None,
        tp_ord_px: None,
        callback_ratio: None,
        callback_spread: None,
        active_px: None,
    }
}

fn row(value: Value) -> Row {
    value.as_object().unwrap().clone()
}

#[tokio::test]
async fn signs_every_request_and_marks_only_demo_requests_as_simulated() {
    let okx = FakeOkx::new();
    connect(&client(&okx, Venue::OkxLive)).await;
    {
        let state = okx.state();
        assert!(!state.simulated_headers.is_empty());
        assert!(state.simulated_headers.iter().all(Option::is_none));
    }
    okx.state().simulated_headers.clear();
    connect(&client(&okx, Venue::OkxDemo)).await;
    let state = okx.state();
    assert!(!state.simulated_headers.is_empty());
    assert!(state
        .simulated_headers
        .iter()
        .all(|h| h.as_deref() == Some("1")));
}

#[tokio::test]
async fn refuses_a_wrong_key_or_signature_before_handling_anything() {
    let okx = FakeOkx::new();
    let wrong_key = Credentials::new("other", fake::SECRET_KEY, fake::PASSPHRASE);
    let client = OkxClient::new(wrong_key, Venue::OkxDemo, okx.transport());
    let error = client.get_config().await.unwrap_err();
    assert_eq!(error.to_string(), "OKX 50119: bad key");

    let wrong_secret = Credentials::new(fake::API_KEY, "other", fake::PASSPHRASE);
    let client = OkxClient::new(wrong_secret, Venue::OkxDemo, okx.transport());
    let error = client.get_positions().await.unwrap_err();
    assert_eq!(error.to_string(), "OKX 50113: bad signature");
    assert!(okx.state().calls.is_empty());
}

#[tokio::test]
async fn answers_config_specs_and_balance_from_the_knobs() {
    let okx = FakeOkx::new();
    let client = client(&okx, Venue::OkxDemo);
    {
        let mut state = okx.state();
        state.perm = "read_only,trade,withdraw".into();
        state.equity = "50".into();
        state.pepe_ct_val_override = Some("1000".into());
    }
    let config = client.get_config().await.unwrap();
    assert_eq!(config["perm"], "read_only,trade,withdraw");
    assert_eq!(config["posMode"], "net_mode");
    assert_eq!(config["acctLv"], "2");
    let pepe = client.get_contract_value("PEPEUSDT").await.unwrap();
    assert_eq!(
        (pepe.ct_val.as_str(), pepe.lot_sz.as_str()),
        ("1000", "0.1")
    );
    let btc = client.get_contract_value("BTCUSDT").await.unwrap();
    assert_eq!((btc.ct_val.as_str(), btc.lot_sz.as_str()), ("0.01", "0.01"));
    let unknown = client.get_contract_value("XRPUSDT").await.unwrap();
    assert_eq!(unknown.ct_val, "");
    okx.state().ct_val_override = Some("0.001".into());
    assert_eq!(
        client.get_contract_value("BTCUSDT").await.unwrap().ct_val,
        "0.001"
    );
    let usdt = client.get_usdt_balance().await.unwrap().unwrap();
    assert_eq!(usdt["eq"], "50");
    assert_eq!(usdt["availEq"], "50");
    assert_eq!(
        okx.state().calls[0],
        "GET /api/v5/account/config",
        "calls record METHOD pathname"
    );
}

#[tokio::test]
async fn fills_a_market_order_with_its_attached_algos() {
    let okx = FakeOkx::new();
    let client = client(&okx, Venue::OkxDemo);
    client.set_leverage("BTC-USDT-SWAP", 3).await.unwrap();
    let mut body = market(Side::Buy, "10", "entry");
    body.attach_algo_ords = Some(vec![
        AttachAlgoOrd {
            attach_algo_cl_ord_id: Some(to_algo_cl_ord_id("ord_1:sl")),
            sl_trigger_px: Some("74000".into()),
            sl_ord_px: Some("-1".into()),
            ..AttachAlgoOrd::default()
        },
        AttachAlgoOrd {
            attach_algo_cl_ord_id: Some(to_algo_cl_ord_id("ord_1:trail")),
            callback_ratio: Some("0.02".into()),
            ..AttachAlgoOrd::default()
        },
    ]);
    assert_eq!(client.place_order(&body).await.unwrap(), "1");
    let order = client
        .get_order("BTC-USDT-SWAP", &to_cl_ord_id("entry"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        Value::Object(order),
        json!({ "ordId": "1", "clOrdId": to_cl_ord_id("entry"), "state": "filled", "accFillSz": "10",
                "avgPx": "75000", "fee": "-0.05", "pnl": "0" })
    );
    let positions = client.get_positions().await.unwrap();
    assert_eq!(positions[0]["pos"], "10");
    assert_eq!(positions[0]["lever"], "3");
    let stops = client
        .get_pending_algos(PendingAlgoType::Conditional)
        .await
        .unwrap();
    assert_eq!(
        Value::Object(stops[0].clone()),
        json!({ "algoId": "A1", "algoClOrdId": to_algo_cl_ord_id("ord_1:sl"), "instId": "BTC-USDT-SWAP",
                "state": "live", "side": "sell", "sz": "10", "slTriggerPx": "74000", "ordType": "conditional" })
    );
    let trails = client
        .get_pending_algos(PendingAlgoType::MoveOrderStop)
        .await
        .unwrap();
    assert_eq!(trails[0]["moveTriggerPx"], "74250");
    assert_eq!(trails[0]["callbackRatio"], "0.02");

    // Closing leaves the attached algos live: the engine's cleanup is what is tested.
    client
        .place_order(&market(Side::Sell, "10", "exit"))
        .await
        .unwrap();
    assert!(client.get_positions().await.unwrap().is_empty());
    assert_eq!(
        client
            .get_pending_algos(PendingAlgoType::Conditional)
            .await
            .unwrap()
            .len(),
        1
    );
    let state = okx.state();
    assert_eq!(
        state.leverage_calls,
        vec![json!({ "instId": "BTC-USDT-SWAP", "lever": "3", "mgnMode": "isolated" })]
    );
    assert_eq!(state.placed.len(), 2);
}

#[tokio::test]
async fn loses_or_drops_orders_and_refuses_on_request() {
    let okx = FakeOkx::new();
    let client = client(&okx, Venue::OkxDemo);
    okx.state().lose_next_response = true;
    let error = client
        .place_order(&market(Side::Buy, "1", "a"))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        OkxError::Unknown("No answer from OKX: socket hang up".into())
    );
    assert!(okx.state().position.is_some(), "the order was applied");

    okx.state().drop_next_request = true;
    client
        .place_order(&market(Side::Buy, "1", "b"))
        .await
        .unwrap_err();
    assert_eq!(okx.state().placed.len(), 1, "the order never arrived");
    assert_eq!(
        client.get_order("BTC-USDT-SWAP", &to_cl_ord_id("b")).await,
        Ok(None)
    );

    okx.state().reject_next = Some(Refusal::new("51008", "Insufficient margin"));
    let error = client
        .place_order(&market(Side::Buy, "1", "c"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "OKX 51008: Insufficient margin");
    okx.state().reject_row_next = Some(Refusal::new("51000", "Parameter error"));
    let error = client
        .place_order(&market(Side::Buy, "1", "d"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "OKX 51000: Parameter error");

    okx.state().lookup_error_code = Some("50001".into());
    let error = client
        .get_order("BTC-USDT-SWAP", &to_cl_ord_id("a"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "OKX 50001: Systems are busy");

    okx.state().down = true;
    let error = client.get_config().await.unwrap_err();
    assert_eq!(error.to_string(), "No answer from OKX: fetch failed");
}

#[tokio::test]
async fn places_cancels_and_refuses_algos() {
    let okx = FakeOkx::new();
    let client = client(&okx, Venue::OkxLive);
    let algo_id = client.place_algo(&stop("ord_2", "74000")).await.unwrap();
    let found = client
        .get_algo(&to_algo_cl_ord_id("ord_2"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found["algoId"], algo_id.as_str());
    assert_eq!(found["ordType"], "conditional");
    assert!(!found.contains_key("tdMode") && !found.contains_key("slOrdPx"));

    okx.state().cancel_fails = true;
    let cancel = [CancelAlgo {
        inst_id: "BTC-USDT-SWAP".into(),
        algo_id: algo_id.clone(),
    }];
    let error = client.cancel_algos(&cancel).await.unwrap_err();
    assert_eq!(error.to_string(), "OKX 51400: Cancellation failed");
    okx.state().cancel_fails = false;
    client.cancel_algos(&cancel).await.unwrap();
    assert_eq!(okx.state().canceled_algo_ids, vec![algo_id]);
    assert!(client
        .get_pending_algos(PendingAlgoType::Conditional)
        .await
        .unwrap()
        .is_empty());

    okx.state().reject_algo_if = Some(Box::new(|body| body["slTriggerPx"] == "1"));
    let error = client.place_algo(&stop("ord_3", "1")).await.unwrap_err();
    assert_eq!(error.to_string(), "OKX 51000: Parameter error");
    okx.state().reject_next_algo = Some(Refusal::new("51280", "SL trigger price error"));
    let error = client.place_algo(&stop("ord_4", "2")).await.unwrap_err();
    assert_eq!(error.to_string(), "OKX 51280: SL trigger price error");

    okx.state().lose_next_algo_response = true;
    let error = client.place_algo(&stop("ord_5", "3")).await.unwrap_err();
    assert!(matches!(error, OkxError::Unknown(_)));
    assert!(client
        .get_algo(&to_algo_cl_ord_id("ord_5"))
        .await
        .unwrap()
        .is_some());

    okx.state().algo_lookup_down = true;
    let error = client
        .get_pending_algos(PendingAlgoType::Oco)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "OKX 50001: Systems are busy");
    assert_eq!(okx.state().algo_placed.len(), 4);
}

#[tokio::test]
async fn shows_an_algo_placed_in_the_okx_app() {
    let okx = FakeOkx::new();
    let client = client(&okx, Venue::OkxLive);
    let added = okx.add_algo(row(
        json!({ "ordType": "conditional", "slTriggerPx": "70000" }),
    ));
    assert_eq!(added["algoId"], "A1");
    assert_eq!(added["algoClOrdId"], "");
    let pending = client
        .get_pending_algos(PendingAlgoType::Conditional)
        .await
        .unwrap();
    assert_eq!(pending, vec![added]);
    assert!(okx.state().algos.contains_key("A1"));
}

#[tokio::test]
async fn answers_unhandled_paths_with_404() {
    let okx = FakeOkx::new();
    let client = client(&okx, Venue::OkxLive);
    // Node's fake has no cancel-order handler either; the engine never reads its row.
    let error = client
        .cancel_order("BTC-USDT-SWAP", &to_cl_ord_id("x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "OKX 404: Unhandled /api/v5/trade/cancel-order"
    );
}
