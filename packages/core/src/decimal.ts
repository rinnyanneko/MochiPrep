import { Decimal } from 'decimal.js';

/** All accounting runs through this clone so precision never depends on a global default. */
export const D = Decimal.clone({ precision: 50, rounding: Decimal.ROUND_HALF_EVEN, toExpNeg: -30, toExpPos: 40 });
export type Dec = InstanceType<typeof D>;
export type DecInput = string | number | Dec;

export const d = (v: DecInput): Dec => new D(v);
export const ZERO = d(0);

/** Canonical storage form: plain notation, no exponent, trailing zeros stripped. */
export const str = (v: Dec): string => v.toFixed();

export const floorToStep = (v: Dec, step: DecInput): Dec => v.div(step).floor().mul(step);
export const ceilToStep = (v: Dec, step: DecInput): Dec => v.div(step).ceil().mul(step);
export const roundToStep = (v: Dec, step: DecInput): Dec => v.div(step).round().mul(step);
export const min = (a: Dec, b: Dec): Dec => (a.lt(b) ? a : b);
export const max = (a: Dec, b: Dec): Dec => (a.gt(b) ? a : b);
