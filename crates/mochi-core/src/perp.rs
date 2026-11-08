//! Linear USDT-margined perpetual: PnL and notional in USDT, quantity in base units.
//! Each function keeps the operation order of `linearPerp` in TypeScript, because every decimal
//! operation rounds to 50 digits and a reordered formula can differ in the last digit.

use mochi_decimal::Dec;
use mochi_domain::{InstrumentSpecification, PositionSide};

use crate::CoreError;

fn zero() -> Dec {
    Dec::from_js_number(0.0)
}

pub fn notional(quantity: &Dec, price: &Dec) -> Dec {
    quantity.mul(price)
}

pub fn pnl(side: PositionSide, entry: &Dec, exit: &Dec, quantity: &Dec) -> Dec {
    let diff = exit.sub(entry).mul(quantity);
    match side {
        PositionSide::Long => diff,
        PositionSide::Short => diff.neg(),
    }
}

/// Fails only when `leverage` is 0, which TypeScript would turn into Infinity.
pub fn initial_margin(notional: &Dec, leverage: u32) -> Result<Dec, CoreError> {
    Ok(notional.div(&Dec::from_js_number(f64::from(leverage)))?)
}

/// Price at which isolated margin minus the loss falls to maintenance margin, never below zero.
pub fn liquidation_price(
    side: PositionSide,
    entry: &Dec,
    quantity: &Dec,
    margin: &Dec,
    maintenance_rate: &Dec,
) -> Result<Dec, CoreError> {
    let maintenance = entry.mul(quantity).mul(maintenance_rate);
    let buffer = margin.sub(&maintenance).div(quantity)?;
    let price = match side {
        PositionSide::Long => entry.sub(&buffer),
        PositionSide::Short => entry.add(&buffer),
    };
    Ok(if price.lt(&zero()) { zero() } else { price })
}

/// Positive means this side pays.
pub fn funding(side: PositionSide, mark_price: &Dec, quantity: &Dec, rate: &Dec) -> Dec {
    let payment = mark_price.mul(quantity).mul(rate);
    match side {
        PositionSide::Long => payment,
        PositionSide::Short => payment.neg(),
    }
}

/// Only linear perpetuals have a calculation model. Spot, FX or CFD support would add another
/// model here instead of changing the paper engine.
pub fn math_for(instrument: &InstrumentSpecification) -> Result<(), CoreError> {
    if instrument.market_type != "CRYPTO_PERP" {
        return Err(CoreError::NoCalculationModel(
            instrument.market_type.clone(),
        ));
    }
    Ok(())
}
