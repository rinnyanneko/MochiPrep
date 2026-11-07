import type { InstrumentSpecification, MarketType, PositionSide } from '@mochiprep/shared';
import { d, type Dec, type DecInput } from '../decimal';

/**
 * Calculation rules that differ per asset class. Spot, FX or CFD support means another
 * implementation of this interface, not changes to the paper engine.
 */
export interface PositionMath {
  marketType: MarketType;
  notional(quantity: DecInput, price: DecInput): Dec;
  pnl(side: PositionSide, entry: DecInput, exit: DecInput, quantity: DecInput): Dec;
  initialMargin(notional: Dec, leverage: number): Dec;
  /** Price at which isolated margin minus the loss falls to maintenance margin. */
  liquidationPrice(args: {
    side: PositionSide;
    entry: DecInput;
    quantity: DecInput;
    margin: DecInput;
    maintenanceRate: DecInput;
  }): Dec;
  /** Positive = this side pays. */
  funding(side: PositionSide, markPrice: DecInput, quantity: DecInput, rate: DecInput): Dec;
}

/** Linear USDT-margined perpetual. PnL and notional are in USDT, quantity in base units. */
export const linearPerp: PositionMath = {
  marketType: 'CRYPTO_PERP',
  notional: (quantity, price) => d(quantity).mul(price),
  pnl: (side, entry, exit, quantity) => {
    const diff = d(exit).minus(entry).mul(quantity);
    return side === 'LONG' ? diff : diff.neg();
  },
  initialMargin: (notional, leverage) => notional.div(leverage),
  liquidationPrice: ({ side, entry, quantity, margin, maintenanceRate }) => {
    const maintenance = d(entry).mul(quantity).mul(maintenanceRate);
    const buffer = d(margin).minus(maintenance).div(quantity);
    const price = side === 'LONG' ? d(entry).minus(buffer) : d(entry).plus(buffer);
    return price.lt(0) ? d(0) : price;
  },
  funding: (side, markPrice, quantity, rate) => {
    const payment = d(markPrice).mul(quantity).mul(rate);
    return side === 'LONG' ? payment : payment.neg();
  },
};

export function mathFor(instrument: InstrumentSpecification): PositionMath {
  if (instrument.marketType !== 'CRYPTO_PERP') {
    throw new Error(`No calculation model for ${instrument.marketType}`);
  }
  return linearPerp;
}
