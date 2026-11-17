# How paper trading works

Paper trading simulates linear, USDT-margined perpetual futures using prices you can actually observe. It is a simplified model, not a copy of an exchange. Simulated results are not predictions of future performance.

## Prices and fills

- Paper orders only fill at prices the simulator has observed. It never fills at a price it made up, and it never trades along a candle's high or low.
- A market buy fills at the ask, and a market sell fills at the bid. Slippage grows with order size.
- A limit order that crosses the market fills right away as a taker, never worse than its limit price.
- A resting limit order fills in full at its limit price, as a maker, once the market reaches it. Queue position and partial fills are not modelled.
- Cancelling an open order releases the margin it reserved.
- A stop loss or take profit fills at the first price after its trigger. In a fast market, the fill can be far from the trigger price.
- If a stop loss and a take profit both trigger on the same price update, the stop loss wins and the take profit is cancelled.

## Trailing stops

A position can have one trailing stop as well as its fixed stop loss.

- **Tracking.** The trailing stop follows the best price since it was set: the highest price for a long, the lowest for a short. It only moves in your favour.
- **Activation.** A trailing stop can have an activation price. It does nothing until the price reaches that level, and then it starts tracking from there.
- **Trigger.** It is set either as a percentage of the best price (ratio) or as a fixed distance from it (spread). When the price moves back past the trigger, the whole position closes at the next observed price.
- **Same price as the stop loss.** If the stop loss and the trailing stop both trigger on the same price update, the stop loss closes the position.
- **Changing protection.** Changing the stop loss or take profit does not reset the trailing stop's best price. A different trailing stop starts fresh.
- **Restarts.** The best price is saved, so a restart does not reset the trailing stop.

## Positions, margin, PnL and fees

- Notional value is quantity times price. Initial margin is notional divided by leverage.
- Long PnL is (exit − entry) × quantity. Short PnL is the opposite.
- Adding to a position averages the entry price by quantity and adds margin.
- Closing part of a position releases the same share of its margin.
- A trade that flips a position from long to short (or the reverse) closes the old position first, then opens the rest on the other side.
- Fees are charged on every fill, including exits. They reduce your balance and are not counted as trading PnL.

## Liquidation

Liquidation uses a simplified estimate with a single 0.5% maintenance rate for every pair. It is checked against the mark price. When it triggers, the position closes at the liquidation price, the remaining isolated margin is lost, and no fee is charged.

**The simulated liquidation price may not match OKX's**, because OKX uses maintenance tiers that this model does not reproduce.

## Funding

Funding is charged every eight hours. Longs pay shorts when the rate is positive, and shorts pay longs when it is negative. A position opened in the middle of a funding period does not pay for that period.

## What the simulator does not model

- No order book, queue, latency or partial liquidity.
- No cross margin, auto-deleveraging or maintenance tiers.
- Liquidation and fee rounding are simplified.
- No live orders: nothing is sent to an exchange.
