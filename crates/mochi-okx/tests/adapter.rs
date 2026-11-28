//! `OkxAdapter` against the in-process fake OKX: neutral rows keep OKX's raw text, and errors keep
//! their exact texts.

use std::sync::Arc;

use mochi_domain::OrderSide;
use mochi_exchange::venue::{
    js_number, nullish, strict, truthy, AlgoKind, RejectKind, VenueAccountConfig, VenueAdapter,
    VenueAlgoOrderType, VenueAttachedAlgo, VenueCancelAlgo, VenueError, VenueField,
    VenueMarginMode, VenueOrderType, VenuePlaceAlgo, VenuePlaceOrder,
};
use mochi_okx::fake::{self, FakeOkx, Refusal};
use mochi_okx::{
    from_inst_id, to_algo_cl_ord_id, to_cl_ord_id, to_inst_id, AlgoOrdType, AlgoOrderBody,
    AttachAlgoOrd, OkxAdapter, OkxClient, OkxError, OrdType, PlaceOrderBody, Row, Side, TdMode,
    Venue,
};
use serde_json::{json, Value};

fn adapter(fake: &Arc<FakeOkx>) -> OkxAdapter {
    OkxAdapter::new(OkxClient::new(
        fake::credentials(),
        Venue::OkxDemo,
        fake.transport(),
    ))
}

fn t(text: &str) -> Option<VenueField> {
    Some(VenueField::Text(text.into()))
}

fn scalar(text: &str) -> Option<VenueField> {
    Some(VenueField::Scalar(text.into()))
}

fn row(value: Value) -> Row {
    value.as_object().cloned().unwrap()
}

fn conditional(client_algo_id: &str) -> VenuePlaceAlgo {
    VenuePlaceAlgo {
        symbol: "BTCUSDT".into(),
        margin_mode: VenueMarginMode::Isolated,
        side: OrderSide::Sell,
        client_algo_id: client_algo_id.into(),
        size: "1".into(),
        reduce_only: true,
        order_type: VenueAlgoOrderType::Conditional,
        sl_trigger_price: Some("70000".into()),
        sl_order_price: Some("-1".into()),
        tp_trigger_price: None,
        tp_order_price: None,
        callback_ratio: None,
        callback_spread: None,
        activation_price: None,
    }
}

#[tokio::test]
async fn okx_adapter_keeps_absent_null_and_empty_apart() {
    let fake = FakeOkx::new();
    fake.add_algo(row(
        json!({ "algoClOrdId": "absent", "ordType": "conditional" }),
    ));
    fake.add_algo(row(
        json!({ "algoClOrdId": "null", "ordType": "conditional", "slTriggerPx": null }),
    ));
    fake.add_algo(row(
        json!({ "algoClOrdId": "empty", "ordType": "conditional", "slTriggerPx": "" }),
    ));
    let adapter = adapter(&fake);
    let algos = adapter.pending_algos(AlgoKind::Conditional).await.unwrap();
    let trigger = |id: &str| {
        algos
            .iter()
            .find(|algo| strict(&algo.client_algo_id) == Some(id))
            .unwrap()
            .sl_trigger_price
            .clone()
    };
    assert_eq!(algos.len(), 3);
    assert_eq!(trigger("absent"), None);
    assert_eq!(trigger("null"), Some(VenueField::Null));
    assert_eq!(trigger("empty"), t(""));
    // `??` passes over absent and null alike, and keeps "".
    assert_eq!(nullish(&trigger("absent")), None);
    assert_eq!(nullish(&trigger("null")), None);
    assert_eq!(nullish(&trigger("empty")), Some(""));
    // A single lookup reads the same way.
    let empty = adapter.get_algo("empty").await.unwrap().unwrap();
    assert_eq!(empty.sl_trigger_price, t(""));
    assert_eq!(empty.symbol, t("BTCUSDT"));
    assert_eq!(empty.exchange_algo_id, t("A3"));
    assert_eq!(empty.state, t("live"));
    assert_eq!(empty.order_type, t("conditional"));
    assert_eq!(empty.size, t("0.1"));
    assert_eq!(adapter.get_algo("missing").await.unwrap(), None);
    // The other kinds ask OKX for their own lists.
    assert!(adapter
        .pending_algos(AlgoKind::Oco)
        .await
        .unwrap()
        .is_empty());
    assert!(adapter
        .pending_algos(AlgoKind::Trailing)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_json_number_field_reads_as_its_text() {
    let fake = FakeOkx::new();
    fake.add_algo(row(
        json!({ "algoClOrdId": "n", "ordType": "conditional", "slTriggerPx": 70000.5 }),
    ));
    let algo = adapter(&fake).get_algo("n").await.unwrap().unwrap();
    assert_eq!(algo.sl_trigger_price, scalar("70000.5"));
    assert_eq!(nullish(&algo.sl_trigger_price), Some("70000.5"));
    assert_eq!(strict(&algo.sl_trigger_price), None);
}

/// The engine reads `tradeId` and `pos` both through `??` and as strict strings, so a number
/// must stay a number.
#[tokio::test]
async fn numeric_trade_id_and_pos_stay_scalar() {
    let fake = FakeOkx::new();
    fake.state().position = Some(row(json!({
        "instId": "BTC-USDT-SWAP", "pos": -2, "tradeId": 7, "mgnMode": "isolated",
    })));
    let positions = adapter(&fake).positions().await.unwrap();
    let position = &positions[0];
    assert_eq!(position.size, scalar("-2"));
    assert_eq!(position.trade_id, scalar("7"));
    assert_eq!(strict(&position.size), None);
    assert_eq!(strict(&position.trade_id), None);
    assert_eq!(nullish(&position.size), Some("-2"));
    assert_eq!(nullish(&position.trade_id), Some("7"));
    assert_eq!(js_number(&position.size), -2.0);
    assert_eq!(strict(&position.symbol), Some("BTCUSDT"));
}

#[tokio::test]
async fn absent_null_and_empty_fields_keep_their_state() {
    let fake = FakeOkx::new();
    fake.state().position = Some(row(json!({
        "instId": "BTC-USDT-SWAP", "pos": "1", "liqPx": null, "markPx": "",
    })));
    let position = adapter(&fake).positions().await.unwrap().remove(0);
    assert_eq!(position.entry_price, None);
    assert_eq!(position.liquidation_price, Some(VenueField::Null));
    assert_eq!(position.mark_price, t(""));
    assert_eq!(truthy(&position.mark_price), None);
}

/// protection.rs compares `uTime` as a whole value, so absent and null must not read as equal.
#[tokio::test]
async fn absent_and_null_utime_compare_unequal() {
    let fake = FakeOkx::new();
    let adapter = adapter(&fake);
    let mut read = Vec::new();
    for position in [
        json!({ "instId": "BTC-USDT-SWAP", "pos": "1" }),
        json!({ "instId": "BTC-USDT-SWAP", "pos": "1", "uTime": null }),
    ] {
        fake.state().position = Some(row(position));
        read.push(adapter.positions().await.unwrap().remove(0));
    }
    assert_eq!(read[0].updated_ms, None);
    assert_eq!(read[1].updated_ms, Some(VenueField::Null));
    assert_ne!(read[0].updated_ms, read[1].updated_ms);
    assert_ne!(read[0], read[1]);
}

#[tokio::test]
async fn rejection_display_is_byte_identical() {
    let fake = FakeOkx::new();
    fake.state().reject_next = Some(Refusal::new("51000", "Parameter error"));
    let error = adapter(&fake)
        .place_order(VenuePlaceOrder {
            symbol: "BTCUSDT".into(),
            margin_mode: VenueMarginMode::Isolated,
            side: OrderSide::Buy,
            order_type: VenueOrderType::Market,
            size: "1".into(),
            client_order_id: to_cl_ord_id("ord_x"),
            price: None,
            reduce_only: None,
            attached_algos: None,
        })
        .await
        .unwrap_err();
    let okx = OkxError::Rejection {
        code: "51000".into(),
        message: "Parameter error".into(),
    };
    assert_eq!(error.to_string(), "OKX 51000: Parameter error");
    assert_eq!(error.to_string(), okx.to_string());
    assert_eq!(
        error,
        VenueError::Rejection {
            code: "51000".into(),
            message: "Parameter error".into(),
            display: "OKX 51000: Parameter error".into(),
            kind: None,
        }
    );
}

#[tokio::test]
async fn trigger_codes_classified() {
    for code in ["51050", "51052", "51277", "51279", "51000"] {
        let fake = FakeOkx::new();
        fake.state().reject_next_algo = Some(Refusal::new(code, "Trigger price refused"));
        let error = adapter(&fake)
            .place_algo(conditional("c1"))
            .await
            .unwrap_err();
        let VenueError::Rejection { kind, display, .. } = error else {
            panic!("{code} is a rejection");
        };
        let expected = (code != "51000").then_some(RejectKind::TriggerWouldFireNow);
        assert_eq!(kind, expected, "{code}");
        assert_eq!(display, format!("OKX {code}: Trigger price refused"));
    }
}

#[tokio::test]
async fn unknown_and_invalid_pass_through() {
    let text = "OKX returned HTTP 500 without JSON".to_string();
    assert_eq!(
        VenueError::from(OkxError::Unknown(text.clone())),
        VenueError::Unknown(text.clone())
    );
    assert_eq!(
        VenueError::from(OkxError::Invalid(text.clone())),
        VenueError::Invalid(text.clone())
    );
    assert_eq!(VenueError::Unknown(text.clone()).to_string(), text);
    assert_eq!(VenueError::Invalid(text.clone()).to_string(), text);

    let fake = FakeOkx::new();
    let adapter = adapter(&fake);
    fake.state().down = true;
    assert_eq!(
        adapter.positions().await.unwrap_err(),
        VenueError::Unknown("No answer from OKX: fetch failed".into())
    );
    fake.state().down = false;
    let mut trailing = conditional("t1");
    trailing.order_type = VenueAlgoOrderType::Trailing;
    assert_eq!(
        adapter.place_algo(trailing).await.unwrap_err(),
        VenueError::Invalid(
            "A trailing stop needs exactly one of callbackRatio or callbackSpread".into()
        )
    );
}

#[tokio::test]
async fn positions_map_fields() {
    let fake = FakeOkx::new();
    fake.state().position = Some(row(json!({
        "instId": "BTC-USDT-SWAP", "pos": "-2", "avgPx": "75000", "liqPx": "", "upl": "1.5",
        "posId": "P1", "mgnMode": "isolated", "posSide": "net", "tradeId": "7", "uTime": "1700",
        "cTime": "1600", "lever": "5", "margin": "300", "markPx": "74990", "realizedPnl": "-0.2",
    })));
    let positions = adapter(&fake).positions().await.unwrap();
    assert_eq!(positions.len(), 1);
    let position = &positions[0];
    assert_eq!(position.symbol, t("BTCUSDT"));
    assert_eq!(position.size, t("-2"));
    assert_eq!(position.entry_price, t("75000"));
    assert_eq!(position.liquidation_price, t(""));
    assert_eq!(position.unrealized_pnl, t("1.5"));
    assert_eq!(position.position_id, t("P1"));
    assert_eq!(position.margin_mode, t("isolated"));
    assert_eq!(position.position_side, t("net"));
    assert_eq!(position.trade_id, t("7"));
    assert_eq!(position.updated_ms, t("1700"));
    assert_eq!(position.created_ms, t("1600"));
    assert_eq!(position.leverage, t("5"));
    assert_eq!(position.margin, t("300"));
    assert_eq!(position.mark_price, t("74990"));
    assert_eq!(position.realized_pnl, t("-0.2"));
}

#[test]
fn client_ids_match_ids_module() {
    let fake = FakeOkx::new();
    let adapter = adapter(&fake);
    assert_eq!(adapter.client_order_id("ord_x"), to_cl_ord_id("ord_x"));
    assert_eq!(adapter.client_algo_id("ord_x"), to_algo_cl_ord_id("ord_x"));
    assert_eq!(adapter.name(), "OKX");
}

#[test]
fn inst_ids_map_back_to_symbols() {
    for symbol in ["BTCUSDT", "1000PEPEUSDT", "USDT", "BTCUSDTUSDT"] {
        assert_eq!(from_inst_id(&to_inst_id(symbol)), symbol);
    }
    // Anything that is not a USDT swap stays as OKX wrote it, so it never matches a pair.
    assert_eq!(from_inst_id("BTC-USD-SWAP"), "BTC-USD-SWAP");
    assert_eq!(from_inst_id(""), "");
}

#[tokio::test]
async fn account_config_reads_the_connect_fields() {
    let fake = FakeOkx::new();
    let adapter = adapter(&fake);
    assert_eq!(
        adapter.account_config().await.unwrap(),
        VenueAccountConfig {
            uid: Some("42".into()),
            master_uid: None,
            can_withdraw: false,
            can_trade: true,
            one_way_mode: true,
            account_mode_ok: true,
        }
    );
    {
        let mut state = fake.state();
        state.perm = "read_only, withdraw".into();
        state.pos_mode = "long_short_mode".into();
        state.acct_lv = "1".into();
    }
    let config = adapter.account_config().await.unwrap();
    assert!(config.can_withdraw);
    assert!(!config.can_trade);
    assert!(!config.one_way_mode);
    assert!(!config.account_mode_ok);
}

#[tokio::test]
async fn contract_balance_and_lookups_read_okx() {
    let fake = FakeOkx::new();
    let adapter = adapter(&fake);
    let contract = adapter.contract("ETHUSDT").await.unwrap();
    assert_eq!(contract.contract_value, "0.1");
    assert_eq!(contract.lot_size, "0.01");

    let balance = adapter.balance().await.unwrap().unwrap();
    assert_eq!(balance.equity, t("1000"));
    assert_eq!(balance.available_equity, t("1000"));
    assert_eq!(balance.unrealized_pnl, t("0"));
    assert_eq!(balance.available_balance, None);

    adapter.set_leverage("BTCUSDT", 5).await.unwrap();
    assert_eq!(
        fake.state().leverage_calls,
        vec![json!({ "instId": "BTC-USDT-SWAP", "lever": "5", "mgnMode": "isolated" })]
    );

    let id = to_cl_ord_id("ord_1");
    assert_eq!(adapter.get_order("BTCUSDT", &id).await.unwrap(), None);
    adapter
        .place_order(VenuePlaceOrder {
            symbol: "BTCUSDT".into(),
            margin_mode: VenueMarginMode::Isolated,
            side: OrderSide::Buy,
            order_type: VenueOrderType::Market,
            size: "1".into(),
            client_order_id: id.clone(),
            price: None,
            reduce_only: None,
            attached_algos: None,
        })
        .await
        .unwrap();
    let order = adapter.get_order("BTCUSDT", &id).await.unwrap().unwrap();
    assert_eq!(order.state, t("filled"));
    assert_eq!(order.filled_size, t("1"));
    assert_eq!(order.average_price, t(fake::PRICE));
    assert!(order.exchange_order_id.is_some());
    // The fake has no cancel endpoint, so OKX's "unhandled" refusal shows the request was sent.
    let refused = adapter.cancel_order("BTCUSDT", &id).await.unwrap_err();
    assert_eq!(
        refused.to_string(),
        "OKX 404: Unhandled /api/v5/trade/cancel-order"
    );
    assert_eq!(
        fake.state().calls.last().map(String::as_str),
        Some("POST /api/v5/trade/cancel-order")
    );
}

#[tokio::test]
async fn fills_bills_and_closed_positions_map_fields() {
    let fake = FakeOkx::new();
    {
        let mut state = fake.state();
        state.recent_fills.push(row(json!({
            "instId": "BTC-USDT-SWAP", "posSide": "net", "ordId": "O1", "clOrdId": "C1",
            "subType": "5", "fillSz": "2", "tradeId": "9", "fillTime": "1800", "side": "buy",
        })));
        state.execution_history.push(row(json!({
            "billId": "11", "ordId": "O1", "instId": "BTC-USDT-SWAP", "posSide": "net",
            "side": "buy", "tradeId": "9", "fillTime": "1800",
        })));
        state.positions_history.push(row(json!({
            "posId": "P1", "instId": "ETH-USDT-SWAP", "cTime": "1000", "uTime": "2000",
            "realizedPnl": "3", "type": "2", "mgnMode": "isolated", "posSide": "net",
            "direction": "long", "pos": "0", "upl": "0",
        })));
    }
    let adapter = adapter(&fake);
    let fills = adapter.recent_fills("BTCUSDT").await.unwrap();
    assert_eq!(fills.len(), 1);
    let fill = &fills[0];
    assert_eq!(fill.symbol, t("BTCUSDT"));
    assert_eq!(fill.position_side, t("net"));
    assert_eq!(fill.exchange_order_id, t("O1"));
    assert_eq!(fill.client_order_id, t("C1"));
    assert_eq!(fill.sub_type, t("5"));
    assert_eq!(fill.fill_size, t("2"));
    assert_eq!(fill.trade_id, t("9"));
    assert_eq!(fill.fill_ms, t("1800"));
    assert!(adapter.recent_fills("ETHUSDT").await.unwrap().is_empty());

    let bills = adapter.execution_history(None).await.unwrap();
    assert_eq!(bills.len(), 1);
    let bill = &bills[0];
    assert_eq!(bill.bill_id, t("11"));
    assert_eq!(bill.exchange_order_id, t("O1"));
    assert_eq!(bill.symbol, t("BTCUSDT"));
    assert_eq!(bill.position_side, t("net"));
    assert_eq!(bill.side, t("buy"));
    assert_eq!(bill.trade_id, t("9"));
    assert_eq!(bill.fill_ms, t("1800"));
    assert!(adapter
        .execution_history(Some(11))
        .await
        .unwrap()
        .is_empty());

    let closed = adapter.positions_history(None).await.unwrap();
    assert_eq!(closed.len(), 1);
    let closed = &closed[0];
    assert_eq!(closed.position_id, t("P1"));
    assert_eq!(closed.symbol, t("ETHUSDT"));
    assert_eq!(closed.created_ms, t("1000"));
    assert_eq!(closed.updated_ms, t("2000"));
    assert_eq!(closed.realized_pnl, t("3"));
    assert_eq!(closed.close_type, t("2"));
    assert_eq!(closed.margin_mode, t("isolated"));
    assert_eq!(closed.position_side, t("net"));
    assert_eq!(closed.direction, t("long"));
    assert!(adapter
        .positions_history(Some(2000))
        .await
        .unwrap()
        .is_empty());
}

/// The adapter sends the very bytes the client sends for the same OKX body.
#[tokio::test]
async fn requests_match_the_okx_bodies() {
    let attached = VenueAttachedAlgo {
        size: Some("1".into()),
        client_algo_id: Some("att1".into()),
        sl_trigger_price: Some("70000".into()),
        sl_order_price: Some("-1".into()),
        tp_trigger_price: None,
        tp_order_price: None,
        callback_ratio: None,
        callback_spread: None,
        activation_price: None,
    };
    let order = VenuePlaceOrder {
        symbol: "BTCUSDT".into(),
        margin_mode: VenueMarginMode::Isolated,
        side: OrderSide::Buy,
        order_type: VenueOrderType::Limit,
        size: "1".into(),
        client_order_id: "cl1".into(),
        price: Some("74000".into()),
        reduce_only: Some(false),
        attached_algos: Some(vec![attached]),
    };
    let body = PlaceOrderBody {
        inst_id: "BTC-USDT-SWAP".into(),
        td_mode: TdMode::Isolated,
        side: Side::Buy,
        ord_type: OrdType::Limit,
        sz: "1".into(),
        cl_ord_id: "cl2".into(),
        px: Some("74000".into()),
        reduce_only: Some(false),
        attach_algo_ords: Some(vec![AttachAlgoOrd {
            sz: Some("1".into()),
            attach_algo_cl_ord_id: Some("att1".into()),
            sl_trigger_px: Some("70000".into()),
            sl_ord_px: Some("-1".into()),
            ..AttachAlgoOrd::default()
        }]),
    };
    let fake = FakeOkx::new();
    fake.state().rest_limit_orders = true;
    let adapter = adapter(&fake);
    adapter.place_order(order).await.unwrap();
    adapter.client().place_order(&body).await.unwrap();
    let placed = fake.state().placed.clone();
    let mut sent = placed[0].clone();
    sent["clOrdId"] = json!("cl2");
    assert_eq!(sent.to_string(), placed[1].to_string());

    let mut algo = conditional("a1");
    algo.tp_trigger_price = Some("80000".into());
    algo.tp_order_price = Some("-1".into());
    let mut trailing = conditional("a2");
    trailing.order_type = VenueAlgoOrderType::Trailing;
    trailing.sl_trigger_price = None;
    trailing.sl_order_price = None;
    trailing.callback_spread = Some("50".into());
    trailing.activation_price = Some("76000".into());
    let bodies = [
        AlgoOrderBody {
            inst_id: "BTC-USDT-SWAP".into(),
            td_mode: TdMode::Isolated,
            side: Side::Sell,
            algo_cl_ord_id: "a1".into(),
            sz: "1".into(),
            reduce_only: true,
            ord_type: AlgoOrdType::Conditional,
            sl_trigger_px: Some("70000".into()),
            sl_ord_px: Some("-1".into()),
            tp_trigger_px: Some("80000".into()),
            tp_ord_px: Some("-1".into()),
            callback_ratio: None,
            callback_spread: None,
            active_px: None,
        },
        AlgoOrderBody {
            inst_id: "BTC-USDT-SWAP".into(),
            td_mode: TdMode::Isolated,
            side: Side::Sell,
            algo_cl_ord_id: "a2".into(),
            sz: "1".into(),
            reduce_only: true,
            ord_type: AlgoOrdType::MoveOrderStop,
            sl_trigger_px: None,
            sl_ord_px: None,
            tp_trigger_px: None,
            tp_ord_px: None,
            callback_ratio: None,
            callback_spread: Some("50".into()),
            active_px: Some("76000".into()),
        },
    ];
    let ids = [
        adapter.place_algo(algo).await.unwrap(),
        adapter.place_algo(trailing).await.unwrap(),
    ];
    let placed = fake.state().algo_placed.clone();
    for (sent, body) in placed.iter().zip(&bodies) {
        assert_eq!(sent.to_string(), serde_json::to_string(body).unwrap());
    }

    adapter
        .cancel_algos(vec![VenueCancelAlgo {
            symbol: "BTCUSDT".into(),
            exchange_algo_id: ids[0].clone(),
        }])
        .await
        .unwrap();
    assert_eq!(fake.state().canceled_algo_ids, vec![ids[0].clone()]);
    // The client's own limit still applies.
    let many = (0..11)
        .map(|i| VenueCancelAlgo {
            symbol: "BTCUSDT".into(),
            exchange_algo_id: format!("A{i}"),
        })
        .collect();
    assert_eq!(
        adapter.cancel_algos(many).await.unwrap_err(),
        VenueError::Invalid("OKX cancels at most 10 algo orders per call".into())
    );
}
