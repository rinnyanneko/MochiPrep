use crate::prompt::{iso_string, js_json};
use crate::{
    AiProvider, AiRequest, AiResponse, CompletionFuture, MarketContext, ProviderError,
    ProviderErrorCode, ProviderKind,
};
use mochi_domain::jsnum::{js_string_to_number, js_to_fixed};
use serde_json::{json, Value};
const LABEL: &str = "[DEMO: deterministic mock, not a real model]";
pub struct MockProvider;
fn get(c: &MarketContext, key: &str) -> Option<f64> {
    c.indicators.get(key).copied().flatten()
}
fn fmt(n: f64, digits: usize) -> String {
    js_to_fixed(n, digits)
}
fn base(c: &MarketContext) -> Value {
    json!({"symbol":c.symbol,"orderType":"MARKET","entryPrice":null,"expiresAt":iso_string((c.now as f64+c.horizon_sec*1000.).trunc()as i64)})
}
fn passive(c: &MarketContext, action: &str, why: &str) -> Value {
    let mut v = base(c);
    let o = v.as_object_mut().unwrap();
    o.extend(json!({"action":action,"stopLoss":null,"takeProfit":null,"reason":format!("{LABEL} {why}"),"confidence":if action=="CLOSE"{0.55}else{0.3}}).as_object().unwrap().clone());
    v
}
fn hold(c: &MarketContext, why: &str) -> Value {
    passive(c, "HOLD", why)
}
fn close(c: &MarketContext, why: &str) -> Value {
    passive(c, "CLOSE", why)
}
fn round(c: &MarketContext, n: f64) -> f64 {
    let digits = c.quote.last.split('.').nth(1).map_or(2, str::len);
    js_string_to_number(&js_to_fixed(n, digits))
}
fn enter(
    c: &MarketContext,
    side: &str,
    stop_atr: f64,
    target_atr: f64,
    why: &str,
    confidence: f64,
) -> Value {
    let price = js_string_to_number(&c.quote.last);
    let atr = get(c, "atr14").unwrap_or(price * 0.005);
    let sign = if side == "LONG" { 1. } else { -1. };
    let mut v = base(c);
    v.as_object_mut().unwrap().extend(json!({"action":side,"stopLoss":round(c,price-sign*stop_atr*atr),"takeProfit":round(c,price+sign*target_atr*atr),"reason":format!("{LABEL} {why}"),"confidence":confidence}).as_object().unwrap().clone());
    v
}
fn momentum(c: &MarketContext) -> Value {
    let (Some(ema20), Some(ema50), Some(rsi14), Some(hist)) = (
        get(c, "ema20"),
        get(c, "ema50"),
        get(c, "rsi14"),
        get(c, "macdHistogram"),
    ) else {
        return hold(c, "Not enough history for EMA, RSI and MACD yet.");
    };
    let trend = format!(
        "EMA20 {} vs EMA50 {}, RSI {}, MACD histogram {}.",
        fmt(ema20, 2),
        fmt(ema50, 2),
        fmt(rsi14, 1),
        fmt(hist, 2)
    );
    if let Some(p) = &c.position {
        let long = p.position.side == mochi_domain::PositionSide::Long;
        let against = if long { ema20 < ema50 } else { ema20 > ema50 };
        let side = if long { "LONG" } else { "SHORT" };
        return if against {
            close(c, &format!("Trend flipped against the {side}: {trend}"))
        } else {
            hold(c, &format!("Trend still supports the {side}: {trend}"))
        };
    }
    if ema20 > ema50 && hist > 0. && rsi14 < 70. {
        return enter(
            c,
            "LONG",
            2.,
            4.,
            &format!("Uptrend with rising momentum: {trend}"),
            0.62,
        );
    }
    if ema20 < ema50 && hist < 0. && rsi14 > 30. {
        return enter(
            c,
            "SHORT",
            2.,
            4.,
            &format!("Downtrend with falling momentum: {trend}"),
            0.62,
        );
    }
    hold(c, &format!("Trend and momentum disagree: {trend}"))
}
fn reversion(c: &MarketContext) -> Value {
    let Some(rsi14) = get(c, "rsi14") else {
        return hold(c, "Not enough history for RSI yet.");
    };
    let state = format!("RSI {}.", fmt(rsi14, 1));
    if let Some(p) = &c.position {
        let done = if p.position.side == mochi_domain::PositionSide::Long {
            rsi14 >= 50.
        } else {
            rsi14 <= 50.
        };
        return if done {
            close(
                c,
                &format!("RSI back at the midline, reversion done: {state}"),
            )
        } else {
            hold(c, &format!("Waiting for RSI to reach 50: {state}"))
        };
    }
    if rsi14 < 35. {
        return enter(
            c,
            "LONG",
            2.5,
            3.,
            &format!("Oversold, fading the move: {state}"),
            0.55,
        );
    }
    if rsi14 > 65. {
        return enter(
            c,
            "SHORT",
            2.5,
            3.,
            &format!("Overbought, fading the move: {state}"),
            0.55,
        );
    }
    hold(c, &format!("RSI is in the neutral zone: {state}"))
}
fn reckless(c: &MarketContext) -> Value {
    let cycle = (c.now as f64 / (c.horizon_sec * 1000.)).floor();
    let side = if cycle % 2. == 0. { "LONG" } else { "SHORT" };
    if c.position.is_some() {
        return if cycle % 4. == 0. {
            close(c, "Bored of this position.")
        } else {
            hold(c, "Letting it ride.")
        };
    }
    let mut entry = enter(c, side, 1.5, 3., "Feeling lucky, trading every bar.", 0.95);
    match cycle % 3. {
        0. => {
            entry["stopLoss"] = Value::Null;
            entry["reason"] = json!(format!(
                "{LABEL} Stops are for the timid. No stop loss on this one."
            ));
        }
        1. => {
            entry["stopLoss"] = json!(round(
                c,
                js_string_to_number(&c.quote.last) * if side == "LONG" { 1.01 } else { 0.99 }
            ));
            entry["reason"] = json!(format!(
                "{LABEL} Stop deliberately on the wrong side of entry."
            ));
        }
        _ => {}
    }
    entry
}
impl AiProvider for MockProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Mock
    }
    fn complete<'a>(&'a self, request: &'a AiRequest) -> CompletionFuture<'a> {
        Box::pin(async move {
            let c = request.context.as_ref().ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorCode::BadResponse,
                    "Mock analysis requires market context",
                )
            })?;
            if c.quote
                .last
                .split('.')
                .nth(1)
                .is_some_and(|fraction| fraction.len() > 100)
            {
                return Err(ProviderError::new(
                    ProviderErrorCode::BadResponse,
                    "Mock quote precision exceeds 100 fractional digits",
                ));
            }
            let decision = match request.model.as_str() {
                "mock-reversion-v1" => reversion(c),
                "mock-reckless-v1" => reckless(c),
                _ => momentum(c),
            };
            Ok(AiResponse {
                text: js_json(&decision),
                input_tokens: Some(0.),
                output_tokens: Some(0.),
            })
        })
    }
}
