import type { InstrumentSpecification, OrderSide, PaperAccountConfig, Quote } from '@mochiprep/shared';
import { ceilToStep, d, floorToStep, type Dec, type DecInput } from '../decimal';

/**
 * Deterministic fill rules. A fill is only ever derived from an observed quote, never from a
 * price an AI asked for and never from guessing the path inside an OHLC bar.
 *
 * Market: buy at ask, sell at bid, worsened by slippageBps + impact per notional, then rounded
 * to tick against the trader.
 */
export function marketFillPrice(
  side: OrderSide,
  quantity: DecInput,
  quote: Quote,
  config: PaperAccountConfig,
  instrument: InstrumentSpecification,
): Dec {
  const touch = d(side === 'BUY' ? quote.ask : quote.bid);
  const notional = touch.mul(quantity);
  const bps = d(config.slippageBps).plus(notional.div(1_000_000).mul(config.impactBpsPerMillion));
  const offset = touch.mul(bps).div(10_000);
  return side === 'BUY'
    ? ceilToStep(touch.plus(offset), instrument.priceTick)
    : floorToStep(touch.minus(offset), instrument.priceTick);
}

/** Crosses the book on arrival: takes liquidity at the touch, never worse than the limit. */
export function isMarketable(side: OrderSide, limit: DecInput, quote: Quote): boolean {
  return side === 'BUY' ? d(quote.ask).lte(limit) : d(quote.bid).gte(limit);
}

/**
 * Resting limit: fills in full, as maker, at exactly its limit, on the first quote whose opposite
 * touch reaches it. Queue position and partial fills are not modelled.
 */
export const isRestingLimitFilled = isMarketable;

export function isStopTriggered(positionIsLong: boolean, trigger: DecInput, last: DecInput): boolean {
  return positionIsLong ? d(last).lte(trigger) : d(last).gte(trigger);
}

export function isTakeProfitTriggered(positionIsLong: boolean, trigger: DecInput, last: DecInput): boolean {
  return positionIsLong ? d(last).gte(trigger) : d(last).lte(trigger);
}
