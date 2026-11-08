//! Trailing stop rules: the trigger an extreme implies, how the extreme moves with each last
//! price, and which specs are refused. A port of `packages/core/src/trading/trailing.ts`; where
//! decimal.js would throw on a malformed number these return the error instead.

use mochi_decimal::Dec;
use mochi_domain::{PositionSide, TrailingKind, TrailingStopSpec, TRAILING_LIMITS};

use crate::CoreError;

fn side_text(side: PositionSide) -> &'static str {
    match side {
        PositionSide::Long => "LONG",
        PositionSide::Short => "SHORT",
    }
}

/// The trigger price of an active trailing stop, rounded to the tick against the trader: a long's
/// trigger is floored and a short's is ceiled, so rounding never makes the stop tighter.
pub fn trailing_trigger(
    side: PositionSide,
    spec: &TrailingStopSpec,
    extreme: &Dec,
    tick: &Dec,
) -> Result<Dec, CoreError> {
    let callback = Dec::parse(&spec.callback)?;
    let distance = match spec.kind {
        TrailingKind::Ratio => extreme.mul(&callback),
        TrailingKind::Spread => callback,
    };
    Ok(match side {
        PositionSide::Long => extreme.sub(&distance).floor_to_step(tick)?,
        PositionSide::Short => extreme.add(&distance).ceil_to_step(tick)?,
    })
}

/// Moves the tracked extreme with one observed last price. None means not active yet: a long
/// activates at `last >= activation_price`, a short at `last <= activation_price`, and a spec with
/// no activation price activates on the first price it sees. Once active the extreme only ratchets
/// in the trader's favour. The current text is returned unchanged when the price does not move it,
/// so callers can tell a real change by comparing the strings.
pub fn advance_extreme(
    side: PositionSide,
    spec: &TrailingStopSpec,
    current: Option<&str>,
    last: &Dec,
) -> Result<Option<String>, CoreError> {
    let long = side == PositionSide::Long;
    let Some(current) = current else {
        let active = match &spec.activation_price {
            None => true,
            Some(activation) => {
                let activation = Dec::parse(activation)?;
                if long {
                    last.gte(&activation)
                } else {
                    last.lte(&activation)
                }
            }
        };
        return Ok(active.then(|| last.to_fixed()));
    };
    let held = Dec::parse(current)?;
    let next = if long { held.max(last) } else { held.min(last) };
    Ok(Some(if next.eq_value(&held) {
        current.to_string()
    } else {
        next.to_fixed()
    }))
}

/// Why a trailing stop is refused, or None when it is acceptable. The risk engine uses the same
/// rule, so a callback the engine would refuse is never sized or sent. `reference` is the price the
/// position opens at or, for an open position, the last price.
pub fn validate_trailing(
    side: PositionSide,
    reference: &Dec,
    spec: &TrailingStopSpec,
    tick: &Dec,
) -> Result<Option<String>, CoreError> {
    let callback = Dec::parse(&spec.callback)?;
    match spec.kind {
        TrailingKind::Ratio => {
            let min = Dec::parse(TRAILING_LIMITS.min_ratio)?;
            let max = Dec::parse(TRAILING_LIMITS.max_ratio)?;
            if callback.lt(&min) || callback.gt(&max) {
                return Ok(Some(format!(
                    "trailing ratio {} outside {}-{}",
                    spec.callback, TRAILING_LIMITS.min_ratio, TRAILING_LIMITS.max_ratio
                )));
            }
        }
        TrailingKind::Spread => {
            if callback.lt(tick) {
                return Ok(Some(format!(
                    "trailing spread {} is below one tick {}",
                    spec.callback,
                    tick.to_fixed()
                )));
            }
            let widest = reference.mul(&Dec::parse(TRAILING_LIMITS.max_spread_fraction)?);
            if callback.gt(&widest) {
                return Ok(Some(format!(
                    "trailing spread {} exceeds {} of {}",
                    spec.callback,
                    TRAILING_LIMITS.max_spread_fraction,
                    reference.to_fixed()
                )));
            }
        }
    }
    if let Some(activation) = &spec.activation_price {
        let level = Dec::parse(activation)?;
        let ok = match side {
            PositionSide::Long => level.gt(reference),
            PositionSide::Short => level.lt(reference),
        };
        if !ok {
            return Ok(Some(format!(
                "activation {activation} is on the wrong side of {} for {}",
                reference.to_fixed(),
                side_text(side)
            )));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Dec {
        Dec::parse(s).unwrap()
    }

    fn spec(kind: TrailingKind, callback: &str, activation: Option<&str>) -> TrailingStopSpec {
        TrailingStopSpec {
            kind,
            callback: callback.to_string(),
            activation_price: activation.map(str::to_string),
        }
    }

    #[test]
    fn trigger_rounds_against_the_trader() {
        let ratio = spec(TrailingKind::Ratio, "0.01", None);
        let tick = d("0.01");
        // 104.5 x 0.99 = 103.455: a long floors, a short's 105.545 ceils.
        let long = trailing_trigger(PositionSide::Long, &ratio, &d("104.5"), &tick).unwrap();
        assert_eq!(long.to_fixed(), "103.45");
        let short = trailing_trigger(PositionSide::Short, &ratio, &d("104.5"), &tick).unwrap();
        assert_eq!(short.to_fixed(), "105.55");
        let spread = spec(TrailingKind::Spread, "1.5", None);
        let short = trailing_trigger(PositionSide::Short, &spread, &d("97.25"), &tick).unwrap();
        assert_eq!(short.to_fixed(), "98.75");
    }

    #[test]
    fn extreme_activates_then_only_ratchets() {
        let gated = spec(TrailingKind::Ratio, "0.02", Some("105"));
        let long = PositionSide::Long;
        assert_eq!(
            advance_extreme(long, &gated, None, &d("104")).unwrap(),
            None
        );
        let active = advance_extreme(long, &gated, None, &d("105")).unwrap();
        assert_eq!(active.as_deref(), Some("105"));
        let up = advance_extreme(long, &gated, Some("105"), &d("108")).unwrap();
        assert_eq!(up.as_deref(), Some("108"));
        // A lower last leaves the text exactly as stored.
        let held = advance_extreme(long, &gated, Some("108.0"), &d("107")).unwrap();
        assert_eq!(held.as_deref(), Some("108.0"));
        let short = spec(TrailingKind::Spread, "1", Some("95"));
        let low = advance_extreme(PositionSide::Short, &short, None, &d("95")).unwrap();
        assert_eq!(low.as_deref(), Some("95"));
        let lower = advance_extreme(PositionSide::Short, &short, Some("95"), &d("94")).unwrap();
        assert_eq!(lower.as_deref(), Some("94"));
    }

    #[test]
    fn validation_messages_match_typescript() {
        let tick = d("0.01");
        let refuse = |side, spec: &TrailingStopSpec| {
            validate_trailing(side, &d("100"), spec, &tick).unwrap()
        };
        let long = PositionSide::Long;
        assert_eq!(
            refuse(long, &spec(TrailingKind::Ratio, "0.0009", None)).as_deref(),
            Some("trailing ratio 0.0009 outside 0.001-0.3")
        );
        assert_eq!(
            refuse(long, &spec(TrailingKind::Spread, "0.001", None)).as_deref(),
            Some("trailing spread 0.001 is below one tick 0.01")
        );
        assert_eq!(
            refuse(long, &spec(TrailingKind::Spread, "40", None)).as_deref(),
            Some("trailing spread 40 exceeds 0.3 of 100")
        );
        assert_eq!(
            refuse(
                PositionSide::Short,
                &spec(TrailingKind::Ratio, "0.01", Some("101"))
            )
            .as_deref(),
            Some("activation 101 is on the wrong side of 100 for SHORT")
        );
        assert_eq!(
            refuse(long, &spec(TrailingKind::Ratio, "0.01", Some("101"))),
            None
        );
    }
}
