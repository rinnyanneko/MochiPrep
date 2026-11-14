//! Shared by the integration tests: a transport that records what it was sent and answers from a
//! fixed reply, and a way to call any client endpoint by its Node name.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use mochi_okx::{
    AlgoOrderBody, BoxFuture, CancelAlgo, HttpReply, HttpRequest, OkxClient, OkxError,
    PendingAlgoType, PlaceOrderBody, Transport,
};
use serde_json::{json, Value};

/// What one request looked like on the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct Sent {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

/// Answers every request with `reply`; `None` fails as an unreachable host does.
pub struct Canned {
    pub reply: Option<(u16, String)>,
    pub sent: Mutex<Vec<Sent>>,
}

impl Canned {
    pub fn new(reply: Option<(u16, String)>) -> Arc<Self> {
        Arc::new(Canned {
            reply,
            sent: Mutex::new(Vec::new()),
        })
    }
}

impl Transport for Canned {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpReply, String>> {
        self.sent.lock().unwrap().push(Sent {
            url: request.url.clone(),
            method: request.method.to_string(),
            headers: request
                .headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            body: request.body.clone(),
        });
        let reply = match &self.reply {
            Some((status, body)) => Ok(HttpReply {
                status: *status,
                body: body.clone().into_bytes(),
            }),
            None => Err("fetch failed".to_string()),
        };
        Box::pin(async move { reply })
    }
}

fn arg<T: serde::de::DeserializeOwned>(args: &Value, i: usize) -> T {
    serde_json::from_value(args[i].clone()).unwrap()
}

/// Calls the endpoint Node calls `name`, with Node's JSON arguments; the result as Node's JSON.
pub async fn invoke(client: &OkxClient, name: &str, args: &Value) -> Result<Value, OkxError> {
    let s = |i: usize| args[i].as_str().unwrap().to_string();
    Ok(match name {
        "getConfig" => json!(client.get_config().await?),
        "getContractValue" => json!(client.get_contract_value(&s(0)).await?),
        "getUsdtBalance" => json!(client.get_usdt_balance().await?),
        "getPositions" => json!(client.get_positions().await?),
        "setLeverage" => {
            let lever = u32::try_from(args[1].as_u64().unwrap()).unwrap();
            client.set_leverage(&s(0), lever).await?;
            Value::Null
        }
        "placeOrder" => json!(client.place_order(&arg::<PlaceOrderBody>(args, 0)).await?),
        "placeAlgo" => json!(client.place_algo(&arg::<AlgoOrderBody>(args, 0)).await?),
        "cancelAlgos" => {
            client
                .cancel_algos(&arg::<Vec<CancelAlgo>>(args, 0))
                .await?;
            Value::Null
        }
        "getPendingAlgos" => {
            json!(
                client
                    .get_pending_algos(arg::<PendingAlgoType>(args, 0))
                    .await?
            )
        }
        "getAlgo" => json!(client.get_algo(&s(0)).await?),
        "cancelOrder" => {
            client.cancel_order(&s(0), &s(1)).await?;
            Value::Null
        }
        "getOrder" => json!(client.get_order(&s(0), &s(1)).await?),
        other => panic!("no endpoint {other}"),
    })
}

/// An outcome as the golden records it.
pub fn outcome(result: Result<Value, OkxError>) -> Value {
    match result {
        Ok(value) => json!({ "result": value }),
        Err(error) => {
            let message = error.to_string();
            match error {
                OkxError::Rejection { code, .. } => {
                    json!({ "error": { "kind": "rejection", "message": message, "code": code } })
                }
                OkxError::Unknown(_) => {
                    json!({ "error": { "kind": "unknown", "message": message } })
                }
                OkxError::Invalid(_) => {
                    json!({ "error": { "kind": "error", "message": message } })
                }
            }
        }
    }
}
