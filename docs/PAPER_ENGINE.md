# Paper execution engine

MochiPrep simulates linear, USDT-margined perpetuals. It is a deterministic paper-trading model, not an exchange emulator. Every accounting value is calculated with `Decimal` and stored as a decimal string.

## Price and fill model

The engine never invents a tradable price and never treats an OHLC path as executable. It only acts on observed quotes.

- A market buy starts at ask; a market sell starts at bid.
- Slippage is `slippageBps + notional / 1,000,000 * impactBpsPerMillion`. Buys round up and sells round down to the instrument tick.
- A marketable limit is a taker fill at the market-fill price, capped so it is never worse than the limit.
- A resting limit reserves initial margin plus its maker fee. It fills in full, at exactly its limit, as maker, on the first quote whose opposite touch reaches the limit. Queue position and partial fills are not modelled.
- Cancelling an open order releases its reservation. Reservations are not ledger balances; they reduce available balance while the order is open.
- Stop-loss and take-profit orders fill as takers at the first quote after their trigger condition becomes observable. A gap therefore fills at that quote's slipped bid or ask, not at the trigger.

For each quote the engine evaluates funding, liquidation, stop-loss, trailing stop, take-profit, and resting limits in that order. If stop-loss and take-profit are both true on the same quote, the stop-loss closes the position and the take-profit is cancelled.

## Trailing stops

A position may have one trailing stop on top of its fixed stop-loss. It is a `TRAILING_STOP_MARKET` order with a kind (`RATIO`, a fraction of the extreme, or `SPREAD`, a price distance), a callback and an optional activation price.

- **Tracking.** The engine tracks the extreme of the observed `last` price: the highest for a long, the lowest for a short. A trail with no activation price starts at the `last` of the quote it was created with. A trail with an activation price is dormant until a long's `last` reaches or exceeds it, or a short's `last` reaches or falls below it; the extreme then starts at that `last`. The extreme only moves in the trader's favour.
- **Trigger.** A long triggers at `extreme × (1 − ratio)` or `extreme − spread`; a short at `extreme × (1 + ratio)` or `extreme + spread`. The trigger is rounded to the tick against the trader: floored for a long, ceiled for a short.
- **Evaluation.** On each quote the extreme is first advanced with that quote's `last` and saved, then the trail triggers when a long's `last` is at or below the trigger, or a short's is at or above it. Like a stop-loss, it then closes the whole position as taker at that quote's slipped bid or ask. A gap through the trigger fills at the gapped quote, never at the trigger price.
- **Same quote as the stop-loss.** The stop-loss is evaluated first. If both are through on one quote, the stop-loss closes the position and the trail is cancelled.
- **Adding.** Adding to the position without protection leaves the trail and its extreme alone, and it closes the whole enlarged quantity.
- **Changing protection.** Setting protection replaces stop-loss, take-profit and trail together in one transaction. Restating the same trail (same kind, callback and activation price) carries its extreme over, so changing only the stop-loss never loosens the trail or puts an activated trail back to sleep. A different trail starts afresh.
- **Restart.** The extreme is stored on the order row (`trailing_extreme`). After a restart the engine resumes from the stored extreme; it does not reset to the next quote.
- **Limits.** A `RATIO` callback must be between 0.001 and 0.3. A `SPREAD` callback must be at least one tick and at most 30% of the reference price. An activation price must be on the profitable side of the reference: above it for a long, below for a short. The reference is the entry price (ask, bid or limit) for an attached trail and `last` for an open position. Reduce-only orders cannot attach a trail.

## Positions, margin, PnL, and fees

Notional is `quantity * price`; isolated initial margin is `notional / leverage`. Long PnL is `(exit - entry) * quantity`; short PnL is its negative. Increasing a same-side position uses a quantity-weighted average entry and adds margin. A partial close releases the same fraction of the position's current isolated margin and recalculates liquidation price for the remainder.

A fill that crosses through zero is processed as two economic legs: it closes the existing position and realizes its PnL, then opens the residual quantity on the other side with new margin. The single fill fee is allocated proportionally between the two legs for position totals. The ledger records the full fee once.

Taker or maker fees are charged on every filled notional, including entry and exit. Fees immediately reduce wallet balance and do not count as realized trading PnL. Reduce-only quantity is capped to the open position; it is rejected when there is no opposite position.

## Liquidation

The simplified isolated liquidation estimate uses entry notional maintenance margin:

`maintenance = entryPrice * quantity * maintenanceRate`

`bufferPerUnit = (isolatedMargin - maintenance) / quantity`

The buffer is subtracted from a long entry and added to a short entry. Liquidation is tested against mark price, never last price. When crossed, the engine records a force-close at the calculated liquidation price, forfeits any remaining isolated margin, charges no trading fee, and cancels every reduce-only order for that position. Loss is capped at isolated margin even if the next mark gaps beyond the threshold.

This simulated liquidation deliberately simplifies maintenance tiers, bankruptcy price, insurance funds, fees, and venue-specific rounding. **Simulated liquidation is not guaranteed to match an exchange's rules.**

## Funding

Funding boundaries are fixed intervals from the Unix epoch, using the instrument interval (currently eight hours). A newly opened position starts after the current boundary and is not charged retroactively. At each crossed boundary, positive funding makes longs pay and shorts receive; a negative rate reverses that direction. The payment uses the observed mark, current quantity, and quote funding rate. Each boundary is recorded at most once through `lastFundingTime`. If a quote jumps across multiple boundaries, each crossed boundary is processed once using that quote's rate and mark. A missing rate advances the boundary with a zero payment.

## Ledger and atomicity

Wallet and used-margin changes occur only through ledger events inside the store transaction that changes the order and position. Events cover deposits, trade fees, realized PnL, funding, liquidation penalty, margin lock, and margin release. `walletAfter` makes the stream independently auditable; `auditLedger` folds it and compares wallet, margin, realized PnL, fees, and funding with account totals.

Order submission is idempotent per account and `clientOrderId`. A duplicate returns the original order and fills without executing again. A rejected transaction rolls back its fills, positions, ledger, and account changes, then persists only a `REJECTED` order describing the failure.

## Deliberate limits

The simulator has no queue model, partial market liquidity, latency model, order-book depth, ADL, cross margin, maintenance tiers, or live-order path. Results are synthetic or paper results, are not predictive, and must not be presented as expected future performance.
