//! `OkxAdapter`: the exchange-neutral `VenueAdapter` over `OkxClient`. Each call is the client
//! call of the same name, and each OKX row becomes a neutral row read with the helpers the engine
//! uses on OKX's JSON, so every text stays as the engine saw it.

use mochi_domain::jsnum::js_trim;
use mochi_domain::OrderSide;
use mochi_exchange::venue::{
    AlgoKind, BoxFuture, RejectKind, VenueAccountConfig, VenueAdapter, VenueAlgo,
    VenueAlgoOrderType, VenueAttachedAlgo, VenueBalance, VenueBill, VenueCancelAlgo,
    VenueClosedPosition, VenueContract, VenueError, VenueField, VenueFill, VenueMarginMode,
    VenueOrder, VenueOrderType, VenuePlaceAlgo, VenuePlaceOrder, VenuePosition,
};
use serde_json::Value;

use crate::client::{
    nullish_text, AlgoOrdType, AlgoOrderBody, AttachAlgoOrd, CancelAlgo, OkxClient, OkxError,
    OrdType, PendingAlgoType, PlaceOrderBody, Row, Side, TdMode, EXECUTION_HISTORY_PAGE,
    ORDER_NOT_FOUND, POSITIONS_HISTORY_PAGE, RECENT_FILLS_LIMIT,
};
use crate::ids::{from_inst_id, to_algo_cl_ord_id, to_cl_ord_id, to_inst_id};

/// OKX's explicit trigger-price refusals: no algo was accepted, and the engine lets the next fresh
/// quote decide whether the target has already been reached.
const TRIGGER_WOULD_FIRE_NOW: [&str; 4] = ["51050", "51052", "51277", "51279"];

pub struct OkxAdapter {
    client: OkxClient,
}

impl OkxAdapter {
    pub fn new(client: OkxClient) -> Self {
        OkxAdapter { client }
    }

    /// The server's connect step still calls the client directly.
    pub fn client(&self) -> &OkxClient {
        &self.client
    }
}

impl From<OkxError> for VenueError {
    fn from(error: OkxError) -> Self {
        match error {
            OkxError::Rejection { code, message } => {
                let display = OkxError::Rejection {
                    code: code.clone(),
                    message: message.clone(),
                }
                .to_string();
                let kind = if TRIGGER_WOULD_FIRE_NOW.contains(&code.as_str()) {
                    Some(RejectKind::TriggerWouldFireNow)
                } else if code == ORDER_NOT_FOUND {
                    Some(RejectKind::NotFound)
                } else {
                    None
                };
                VenueError::Rejection {
                    code,
                    message,
                    display,
                    kind,
                }
            }
            OkxError::Unknown(text) => VenueError::Unknown(text),
            OkxError::Invalid(text) => VenueError::Invalid(text),
        }
    }
}

/// A field with its JSON kind kept. The text of a number or bool is `nullish_text`'s, so
/// `venue::nullish` reads exactly what the engine read from the row.
fn text(row: &Row, key: &str) -> Option<VenueField> {
    let value = row.get(key)?;
    Some(match value {
        Value::String(text) => VenueField::Text(text.clone()),
        Value::Number(_) | Value::Bool(_) => {
            VenueField::Scalar(nullish_text(Some(value)).unwrap_or_default())
        }
        // OKX sends no object or array in the fields read here; like null, `??` and `as_str`
        // both pass over one.
        Value::Null | Value::Array(_) | Value::Object(_) => VenueField::Null,
    })
}

/// `instId` as a pair, its JSON kind kept so a strict read still refuses a non-string.
fn symbol(row: &Row) -> Option<VenueField> {
    text(row, "instId").map(|field| match field {
        VenueField::Text(inst_id) => VenueField::Text(from_inst_id(&inst_id)),
        VenueField::Scalar(inst_id) => VenueField::Scalar(from_inst_id(&inst_id)),
        VenueField::Null => VenueField::Null,
    })
}

/// One OKX row as a neutral row.
trait FromRow {
    fn from_row(row: &Row) -> Self;
}

impl FromRow for VenueBalance {
    fn from_row(row: &Row) -> Self {
        VenueBalance {
            equity: text(row, "eq"),
            unrealized_pnl: text(row, "upl"),
            available_equity: text(row, "availEq"),
            available_balance: text(row, "availBal"),
        }
    }
}

impl FromRow for VenuePosition {
    fn from_row(row: &Row) -> Self {
        VenuePosition {
            symbol: symbol(row),
            size: text(row, "pos"),
            entry_price: text(row, "avgPx"),
            liquidation_price: text(row, "liqPx"),
            mark_price: text(row, "markPx"),
            margin: text(row, "margin"),
            leverage: text(row, "lever"),
            unrealized_pnl: text(row, "upl"),
            realized_pnl: text(row, "realizedPnl"),
            position_id: text(row, "posId"),
            margin_mode: text(row, "mgnMode"),
            position_side: text(row, "posSide"),
            trade_id: text(row, "tradeId"),
            created_ms: text(row, "cTime"),
            updated_ms: text(row, "uTime"),
        }
    }
}

impl FromRow for VenueOrder {
    fn from_row(row: &Row) -> Self {
        VenueOrder {
            exchange_order_id: text(row, "ordId"),
            state: text(row, "state"),
            filled_size: text(row, "accFillSz"),
            average_price: text(row, "avgPx"),
            fee: text(row, "fee"),
            realized_pnl: text(row, "pnl"),
            trade_id: text(row, "tradeId"),
            fill_ms: text(row, "fillTime"),
        }
    }
}

impl FromRow for VenueAlgo {
    fn from_row(row: &Row) -> Self {
        VenueAlgo {
            symbol: symbol(row),
            exchange_algo_id: text(row, "algoId"),
            client_algo_id: text(row, "algoClOrdId"),
            state: text(row, "state"),
            order_type: text(row, "ordType"),
            size: text(row, "sz"),
            sl_trigger_price: text(row, "slTriggerPx"),
            sl_order_price: text(row, "slOrdPx"),
            tp_trigger_price: text(row, "tpTriggerPx"),
            tp_order_price: text(row, "tpOrdPx"),
            callback_ratio: text(row, "callbackRatio"),
            callback_spread: text(row, "callbackSpread"),
            activation_price: text(row, "activePx"),
            move_trigger_price: text(row, "moveTriggerPx"),
        }
    }
}

impl FromRow for VenueFill {
    fn from_row(row: &Row) -> Self {
        VenueFill {
            symbol: symbol(row),
            exchange_order_id: text(row, "ordId"),
            client_order_id: text(row, "clOrdId"),
            position_side: text(row, "posSide"),
            sub_type: text(row, "subType"),
            fill_size: text(row, "fillSz"),
            trade_id: text(row, "tradeId"),
            fill_ms: text(row, "fillTime"),
        }
    }
}

impl FromRow for VenueBill {
    fn from_row(row: &Row) -> Self {
        VenueBill {
            bill_id: text(row, "billId"),
            symbol: symbol(row),
            exchange_order_id: text(row, "ordId"),
            position_side: text(row, "posSide"),
            side: text(row, "side"),
            trade_id: text(row, "tradeId"),
            fill_ms: text(row, "fillTime"),
        }
    }
}

impl FromRow for VenueClosedPosition {
    fn from_row(row: &Row) -> Self {
        VenueClosedPosition {
            symbol: symbol(row),
            position_id: text(row, "posId"),
            realized_pnl: text(row, "realizedPnl"),
            close_type: text(row, "type"),
            margin_mode: text(row, "mgnMode"),
            position_side: text(row, "posSide"),
            direction: text(row, "direction"),
            created_ms: text(row, "cTime"),
            updated_ms: text(row, "uTime"),
        }
    }
}

fn all<T: FromRow>(rows: Vec<Row>) -> Vec<T> {
    rows.iter().map(T::from_row).collect()
}

/// `get_config`'s row as the connect checks read it (`config.x ?? ''` and `===` against a
/// string), so a non-string field never matches.
fn account_config_of(config: &Row) -> VenueAccountConfig {
    let config_text = |key: &str| config.get(key).and_then(Value::as_str);
    let permissions: Vec<&str> = config_text("perm")
        .unwrap_or("")
        .split(',')
        .map(js_trim)
        .collect();
    let id = |key: &str| match config.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => Some(id.clone()),
        Some(other) => Some(other.to_string()),
    };
    VenueAccountConfig {
        uid: id("uid"),
        master_uid: id("mainUid"),
        can_withdraw: permissions.contains(&"withdraw"),
        can_trade: permissions.contains(&"trade"),
        one_way_mode: config_text("posMode") == Some("net_mode"),
        account_mode_ok: config_text("acctLv") != Some("1"),
    }
}

fn side(side: OrderSide) -> Side {
    match side {
        OrderSide::Buy => Side::Buy,
        OrderSide::Sell => Side::Sell,
    }
}

fn td_mode(mode: VenueMarginMode) -> TdMode {
    match mode {
        VenueMarginMode::Isolated => TdMode::Isolated,
    }
}

fn order_body(order: VenuePlaceOrder) -> PlaceOrderBody {
    PlaceOrderBody {
        inst_id: to_inst_id(&order.symbol),
        td_mode: td_mode(order.margin_mode),
        side: side(order.side),
        ord_type: match order.order_type {
            VenueOrderType::Market => OrdType::Market,
            VenueOrderType::Limit => OrdType::Limit,
        },
        sz: order.size,
        cl_ord_id: order.client_order_id,
        px: order.price,
        reduce_only: order.reduce_only,
        attach_algo_ords: order
            .attached_algos
            .map(|legs| legs.into_iter().map(attached_body).collect()),
    }
}

fn attached_body(leg: VenueAttachedAlgo) -> AttachAlgoOrd {
    AttachAlgoOrd {
        sz: leg.size,
        attach_algo_cl_ord_id: leg.client_algo_id,
        sl_trigger_px: leg.sl_trigger_price,
        sl_ord_px: leg.sl_order_price,
        tp_trigger_px: leg.tp_trigger_price,
        tp_ord_px: leg.tp_order_price,
        callback_ratio: leg.callback_ratio,
        callback_spread: leg.callback_spread,
        active_px: leg.activation_price,
    }
}

fn algo_body(algo: VenuePlaceAlgo) -> AlgoOrderBody {
    AlgoOrderBody {
        inst_id: to_inst_id(&algo.symbol),
        td_mode: td_mode(algo.margin_mode),
        side: side(algo.side),
        algo_cl_ord_id: algo.client_algo_id,
        sz: algo.size,
        reduce_only: algo.reduce_only,
        ord_type: match algo.order_type {
            VenueAlgoOrderType::Conditional => AlgoOrdType::Conditional,
            VenueAlgoOrderType::Trailing => AlgoOrdType::MoveOrderStop,
        },
        sl_trigger_px: algo.sl_trigger_price,
        sl_ord_px: algo.sl_order_price,
        tp_trigger_px: algo.tp_trigger_price,
        tp_ord_px: algo.tp_order_price,
        callback_ratio: algo.callback_ratio,
        callback_spread: algo.callback_spread,
        active_px: algo.activation_price,
    }
}

impl VenueAdapter for OkxAdapter {
    fn name(&self) -> &str {
        "OKX"
    }

    fn account_config(&self) -> BoxFuture<'_, Result<VenueAccountConfig, VenueError>> {
        Box::pin(async move { Ok(account_config_of(&self.client.get_config().await?)) })
    }

    fn contract(&self, symbol: &str) -> BoxFuture<'_, Result<VenueContract, VenueError>> {
        let symbol = symbol.to_string();
        Box::pin(async move {
            let contract = self.client.get_contract_value(&symbol).await?;
            Ok(VenueContract {
                contract_value: contract.ct_val,
                lot_size: contract.lot_sz,
            })
        })
    }

    fn balance(&self) -> BoxFuture<'_, Result<Option<VenueBalance>, VenueError>> {
        Box::pin(async move {
            Ok(self
                .client
                .get_usdt_balance()
                .await?
                .as_ref()
                .map(VenueBalance::from_row))
        })
    }

    fn positions(&self) -> BoxFuture<'_, Result<Vec<VenuePosition>, VenueError>> {
        Box::pin(async move { Ok(all(self.client.get_positions().await?)) })
    }

    fn positions_history(
        &self,
        after: Option<i64>,
    ) -> BoxFuture<'_, Result<Vec<VenueClosedPosition>, VenueError>> {
        Box::pin(async move { Ok(all(self.client.get_positions_history(after).await?)) })
    }

    fn execution_history(
        &self,
        after: Option<u64>,
    ) -> BoxFuture<'_, Result<Vec<VenueBill>, VenueError>> {
        Box::pin(async move { Ok(all(self.client.get_execution_history(after).await?)) })
    }

    fn recent_fills(&self, symbol: &str) -> BoxFuture<'_, Result<Vec<VenueFill>, VenueError>> {
        let inst_id = to_inst_id(symbol);
        Box::pin(async move { Ok(all(self.client.recent_fills(&inst_id).await?)) })
    }

    fn set_leverage(&self, symbol: &str, leverage: u32) -> BoxFuture<'_, Result<(), VenueError>> {
        let inst_id = to_inst_id(symbol);
        Box::pin(async move { Ok(self.client.set_leverage(&inst_id, leverage).await?) })
    }

    fn place_order(&self, order: VenuePlaceOrder) -> BoxFuture<'_, Result<String, VenueError>> {
        let body = order_body(order);
        Box::pin(async move { Ok(self.client.place_order(&body).await?) })
    }

    fn get_order(
        &self,
        symbol: &str,
        client_order_id: &str,
    ) -> BoxFuture<'_, Result<Option<VenueOrder>, VenueError>> {
        let inst_id = to_inst_id(symbol);
        let client_order_id = client_order_id.to_string();
        Box::pin(async move {
            Ok(self
                .client
                .get_order(&inst_id, &client_order_id)
                .await?
                .as_ref()
                .map(VenueOrder::from_row))
        })
    }

    fn cancel_order(
        &self,
        symbol: &str,
        client_order_id: &str,
    ) -> BoxFuture<'_, Result<(), VenueError>> {
        let inst_id = to_inst_id(symbol);
        let client_order_id = client_order_id.to_string();
        Box::pin(async move { Ok(self.client.cancel_order(&inst_id, &client_order_id).await?) })
    }

    fn place_algo(&self, algo: VenuePlaceAlgo) -> BoxFuture<'_, Result<String, VenueError>> {
        let body = algo_body(algo);
        Box::pin(async move { Ok(self.client.place_algo(&body).await?) })
    }

    fn pending_algos(&self, kind: AlgoKind) -> BoxFuture<'_, Result<Vec<VenueAlgo>, VenueError>> {
        let ord_type = match kind {
            AlgoKind::Conditional => PendingAlgoType::Conditional,
            AlgoKind::Oco => PendingAlgoType::Oco,
            AlgoKind::Trailing => PendingAlgoType::MoveOrderStop,
        };
        Box::pin(async move { Ok(all(self.client.get_pending_algos(ord_type).await?)) })
    }

    fn get_algo(
        &self,
        client_algo_id: &str,
    ) -> BoxFuture<'_, Result<Option<VenueAlgo>, VenueError>> {
        let client_algo_id = client_algo_id.to_string();
        Box::pin(async move {
            Ok(self
                .client
                .get_algo(&client_algo_id)
                .await?
                .as_ref()
                .map(VenueAlgo::from_row))
        })
    }

    fn cancel_algos(&self, items: Vec<VenueCancelAlgo>) -> BoxFuture<'_, Result<(), VenueError>> {
        let items: Vec<CancelAlgo> = items
            .into_iter()
            .map(|item| CancelAlgo {
                inst_id: to_inst_id(&item.symbol),
                algo_id: item.exchange_algo_id,
            })
            .collect();
        Box::pin(async move { Ok(self.client.cancel_algos(&items).await?) })
    }

    fn client_order_id(&self, order_id: &str) -> String {
        to_cl_ord_id(order_id)
    }

    fn client_algo_id(&self, order_id: &str) -> String {
        to_algo_cl_ord_id(order_id)
    }

    /// The inverse of the rows' `from_inst_id`, exactly: a USDT swap's pair has no dash and
    /// `to_inst_id` restores it; any other swap id kept its dashes and comes back as it is.
    /// History records store this id, so the round trip must not change its text.
    fn instrument_id(&self, symbol: &str) -> String {
        if symbol.is_empty() || symbol.contains('-') {
            symbol.to_string()
        } else {
            to_inst_id(symbol)
        }
    }

    fn recent_fills_page(&self) -> usize {
        RECENT_FILLS_LIMIT
    }

    fn execution_history_page(&self) -> usize {
        EXECUTION_HISTORY_PAGE
    }

    fn positions_history_page(&self) -> usize {
        POSITIONS_HISTORY_PAGE
    }
}
