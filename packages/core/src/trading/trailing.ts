import { TRAILING_LIMITS, type PositionSide, type TrailingStopSpec } from '@mochiprep/shared';
import { ceilToStep, d, floorToStep, max, min, str, type Dec } from '../decimal';

/**
 * The trigger price of an active trailing stop, rounded to the tick against the trader: a long's
 * trigger is floored and a short's is ceiled, so rounding never makes the stop tighter.
 */
export function trailingTrigger(side: PositionSide, spec: TrailingStopSpec, extreme: Dec, tick: Dec): Dec {
  const distance = spec.kind === 'RATIO' ? extreme.mul(spec.callback) : d(spec.callback);
  return side === 'LONG' ? floorToStep(extreme.minus(distance), tick) : ceilToStep(extreme.plus(distance), tick);
}

/**
 * Moves the tracked extreme with one observed last price. Null means not active yet: a long
 * activates at `last >= activationPrice`, a short at `last <= activationPrice`, and a spec with
 * no activation price activates on the first price it sees. Once active the extreme only ratchets
 * in the trader's favour. The current string is returned unchanged when the price does not move it,
 * so callers can tell a real change by identity.
 */
export function advanceExtreme(side: PositionSide, spec: TrailingStopSpec, current: string | null, last: Dec): string | null {
  const long = side === 'LONG';
  if (current === null) {
    const activation = spec.activationPrice;
    const active = activation === null || (long ? last.gte(activation) : last.lte(activation));
    return active ? str(last) : null;
  }
  const next = long ? max(d(current), last) : min(d(current), last);
  return next.eq(current) ? current : str(next);
}

/**
 * Why a trailing stop is refused, or null when it is acceptable. The risk engine uses the same
 * rule, so a callback the engine would refuse is never sized or sent. `reference` is the price the
 * position opens at or, for an open position, the last price.
 */
export function validateTrailing(side: PositionSide, reference: Dec, spec: TrailingStopSpec, tick: Dec): string | null {
  const callback = d(spec.callback);
  if (spec.kind === 'RATIO') {
    if (callback.lt(TRAILING_LIMITS.minRatio) || callback.gt(TRAILING_LIMITS.maxRatio)) {
      return `trailing ratio ${spec.callback} outside ${TRAILING_LIMITS.minRatio}-${TRAILING_LIMITS.maxRatio}`;
    }
  } else {
    if (callback.lt(tick)) return `trailing spread ${spec.callback} is below one tick ${tick.toFixed()}`;
    const widest = reference.mul(TRAILING_LIMITS.maxSpreadFraction);
    if (callback.gt(widest)) {
      return `trailing spread ${spec.callback} exceeds ${TRAILING_LIMITS.maxSpreadFraction} of ${reference.toFixed()}`;
    }
  }
  if (spec.activationPrice !== null) {
    const ok = side === 'LONG' ? d(spec.activationPrice).gt(reference) : d(spec.activationPrice).lt(reference);
    if (!ok) return `activation ${spec.activationPrice} is on the wrong side of ${reference.toFixed()} for ${side}`;
  }
  return null;
}
