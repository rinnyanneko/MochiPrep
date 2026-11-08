import type { PositionSide, Protection, TrailingStopSpec } from '@mochiprep/shared';
import { d } from '../decimal';

export type ProtectionChangeClass = 'REDUCES_RISK' | 'NEEDS_ARMED';

const samePrice = (a: string | null, b: string | null) => (a === null || b === null ? a === b : d(a).eq(b));

/** Same kind, callback and activation, compared as numbers so `0.01` equals `0.010`. */
export const sameTrailing = (a: TrailingStopSpec | null, b: TrailingStopSpec | null) =>
  a === null || b === null
    ? a === b
    : a.kind === b.kind && d(a.callback).eq(b.callback) && samePrice(a.activationPrice, b.activationPrice);

/**
 * Whether a protection change can only lower the risk of a position, in which case it is allowed on
 * a disarmed real account just like an exit. Anything not provably safer needs arming, take-profit
 * changes included: a target closes at a profit, but moving or adding one is still a trading decision.
 */
export function classifyProtectionChange(side: PositionSide, before: Protection, after: Protection): ProtectionChangeClass {
  const stopSafe = (() => {
    if (samePrice(before.stopLoss, after.stopLoss)) return true;
    if (after.stopLoss === null) return false;
    if (before.stopLoss === null) return true;
    return side === 'LONG' ? d(after.stopLoss).gt(before.stopLoss) : d(after.stopLoss).lt(before.stopLoss);
  })();
  const trailSafe = (() => {
    const [was, next] = [before.trailingStop, after.trailingStop];
    if (sameTrailing(was, next)) return true;
    if (next === null) return false;
    if (was === null) return true;
    return was.kind === next.kind && samePrice(was.activationPrice, next.activationPrice) && d(next.callback).lt(was.callback);
  })();
  const targetSafe = samePrice(before.takeProfit, after.takeProfit);
  return stopSafe && trailSafe && targetSafe ? 'REDUCES_RISK' : 'NEEDS_ARMED';
}
