//! Node's `packages/core/src/trading/protection.ts`: whether a protection change can only lower a
//! position's risk. It decides which changes a disarmed real-money account may still make.

use mochi_decimal::{Dec, DecError};
use mochi_domain::{PositionSide, Protection, TrailingStopSpec};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProtectionChangeClass {
    ReducesRisk,
    NeedsArmed,
}

/// Two optional prices, compared as numbers so `95` equals `95.00`.
pub fn same_price(a: Option<&str>, b: Option<&str>) -> Result<bool, DecError> {
    match (a, b) {
        (Some(a), Some(b)) => Ok(Dec::parse(a)?.eq_value(&Dec::parse(b)?)),
        (a, b) => Ok(a.is_none() && b.is_none()),
    }
}

/// Same kind, callback and activation, compared as numbers so `0.01` equals `0.010`.
pub fn same_trailing(
    a: Option<&TrailingStopSpec>,
    b: Option<&TrailingStopSpec>,
) -> Result<bool, DecError> {
    let (Some(a), Some(b)) = (a, b) else {
        return Ok(a.is_none() && b.is_none());
    };
    Ok(a.kind == b.kind
        && Dec::parse(&a.callback)?.eq_value(&Dec::parse(&b.callback)?)
        && same_price(a.activation_price.as_deref(), b.activation_price.as_deref())?)
}

/// Whether a protection change can only lower the risk of a position, in which case it is allowed
/// on a disarmed real account just like an exit. Anything not provably safer needs arming,
/// take-profit changes included: a target closes at a profit, but moving or adding one is still a
/// trading decision. An unreadable price is an error, as decimal.js throws on one.
pub fn classify_protection_change(
    side: PositionSide,
    before: &Protection,
    after: &Protection,
) -> Result<ProtectionChangeClass, DecError> {
    let stop_safe = {
        let (was, next) = (before.stop_loss.as_deref(), after.stop_loss.as_deref());
        if same_price(was, next)? {
            true
        } else {
            match (was, next) {
                (_, None) => false,
                (None, Some(_)) => true,
                (Some(was), Some(next)) => {
                    let (was, next) = (Dec::parse(was)?, Dec::parse(next)?);
                    match side {
                        PositionSide::Long => next.gt(&was),
                        PositionSide::Short => next.lt(&was),
                    }
                }
            }
        }
    };
    let trail_safe = {
        let (was, next) = (before.trailing_stop.as_ref(), after.trailing_stop.as_ref());
        if same_trailing(was, next)? {
            true
        } else {
            match (was, next) {
                (_, None) => false,
                (None, Some(_)) => true,
                (Some(was), Some(next)) => {
                    was.kind == next.kind
                        && same_price(
                            was.activation_price.as_deref(),
                            next.activation_price.as_deref(),
                        )?
                        && Dec::parse(&next.callback)?.lt(&Dec::parse(&was.callback)?)
                }
            }
        }
    };
    let target_safe = same_price(before.take_profit.as_deref(), after.take_profit.as_deref())?;
    Ok(if stop_safe && trail_safe && target_safe {
        ProtectionChangeClass::ReducesRisk
    } else {
        ProtectionChangeClass::NeedsArmed
    })
}
