//! Deterministic fill rules. A fill is only ever derived from an observed quote, never from a
//! price an AI asked for and never from guessing the path inside an OHLC bar.

use mochi_decimal::Dec;
use mochi_domain::{InstrumentSpecification, OrderSide, PaperAccountConfig, Quote};

use crate::CoreError;

/// Market: buy at ask, sell at bid, worsened by `slippage_bps` plus impact per notional, then
/// rounded to the tick against the trader.
pub fn market_fill_price(
    side: OrderSide,
    quantity: &Dec,
    quote: &Quote,
    config: &PaperAccountConfig,
    instrument: &InstrumentSpecification,
) -> Result<Dec, CoreError> {
    let touch: Dec = match side {
        OrderSide::Buy => quote.ask.parse()?,
        OrderSide::Sell => quote.bid.parse()?,
    };
    let notional = touch.mul(quantity);
    let slippage: Dec = config.slippage_bps.parse()?;
    let impact: Dec = config.impact_bps_per_million.parse()?;
    let million = Dec::from_js_number(1_000_000.0);
    let ten_thousand = Dec::from_js_number(10_000.0);
    let bps = slippage.add(&notional.div(&million)?.mul(&impact));
    let offset = touch.mul(&bps).div(&ten_thousand)?;
    let tick: Dec = instrument.price_tick.parse()?;
    Ok(match side {
        OrderSide::Buy => touch.add(&offset).ceil_to_step(&tick)?,
        OrderSide::Sell => touch.sub(&offset).floor_to_step(&tick)?,
    })
}

/// Crosses the book on arrival: takes liquidity at the touch, never worse than the limit.
pub fn is_marketable(side: OrderSide, limit: &Dec, quote: &Quote) -> Result<bool, CoreError> {
    Ok(match side {
        OrderSide::Buy => quote.ask.parse::<Dec>()?.lte(limit),
        OrderSide::Sell => quote.bid.parse::<Dec>()?.gte(limit),
    })
}

/// Resting limit: fills in full, as maker, at exactly its limit, on the first quote whose
/// opposite touch reaches it. Queue position and partial fills are not modelled.
pub fn is_resting_limit_filled(
    side: OrderSide,
    limit: &Dec,
    quote: &Quote,
) -> Result<bool, CoreError> {
    is_marketable(side, limit, quote)
}

/// Stop-loss and trailing stop: a long triggers when last falls to the trigger, a short when it
/// rises to it.
pub fn is_stop_triggered(position_is_long: bool, trigger: &Dec, last: &Dec) -> bool {
    if position_is_long {
        last.lte(trigger)
    } else {
        last.gte(trigger)
    }
}

pub fn is_take_profit_triggered(position_is_long: bool, trigger: &Dec, last: &Dec) -> bool {
    if position_is_long {
        last.gte(trigger)
    } else {
        last.lte(trigger)
    }
}
