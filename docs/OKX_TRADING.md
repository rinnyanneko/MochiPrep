# Trading on OKX

> **DYOR: do your own research.** MochiPrep is not financial advice. AI output can be wrong, and you are responsible for your own trades.

> **Status: tested only against a simulated exchange.** The OKX client follows OKX's published API, and its signature format was accepted by the real endpoint, but no order has ever been placed on OKX Demo Trading or a real account. Real money can be lost, including through a bug in this software, and AI decisions can be wrong. You are responsible for every order.

## Before your first real order

Go through these steps in order, and stop at the first surprise.

1. In OKX, set the account to **Futures mode** and **one-way** position mode.
2. Create a **Demo Trading** API key with **Trade** permission only, as described in [How to get API keys](API_KEYS.md). Bind it to your IP address if you can.
3. In **Settings > OKX trading**, connect with the **Demo Trading** environment.
4. Place a market order with a stop loss from **OKX Trading**. On OKX, confirm that the order, its stop loss and the isolated margin are what you expect.
5. Close the position. Then try a limit order, a cancel, and one agent run.
6. Stop the network or MochiPrep while a position is open, and confirm the stop loss is still on OKX.
7. Only then create a real key, again with **Trade** only and no Withdraw permission, and start with small limits.

## How orders are protected

| Layer | What it does |
| --- | --- |
| Connect | Refuses a key that can withdraw or cannot trade, and refuses accounts in hedge mode or Spot mode. Contract specifications must match what was read when the pair was added. |
| Arm | Real orders stay off until you type `TRADE REAL MONEY`. Demo accounts do not need arming. |
| Account limits | Order size, leverage, number of positions and daily loss, checked on every order from you or an agent. The default maximum is 100 USDT per order. |
| Stop loss | Required on every new position. It is sent to OKX as an attached order, so it keeps working if MochiPrep stops. |
| Sync | Balance, positions and orders refresh every 5 seconds. New positions are refused if sync has been lost for 30 seconds, and agents pause. |
| Stale prices | New positions are refused when the price is older than 10 seconds. |
| Duplicates | Each order is saved before it is sent, so a repeated request does not send a second order. |
| Unknown outcome | If OKX does not answer, MochiPrep looks the order up by its client ID. It is **never sent again**. |
| Stop All | Stops every agent and disarms real orders. Open positions and their stop losses stay on OKX. |

Closing is always allowed while connected, even when disarmed, so you can always get out of a position.

An agent in Suggest mode can run without arming, because it places no orders. Accepting a suggestion needs an armed account, except for a close, and the risk checks run again at that moment.

An agent that reads news on a real account takes third-party text as input. Anyone can publish a headline written to steer an AI. The account limits limit the damage, but not the decision, so prefer Suggest mode here.

Keys with Trade permission can also transfer funds and change account settings on OKX. Store them as carefully as a password. We suggest a separate OKX sub-account that holds only money you can afford to lose.

## Stop losses, take profits and trailing stops

A position's protection is its stop loss, an optional take profit and an optional trailing stop. You can change them from the position's pencil button. An agent can also change them with a `PROTECT` decision.

- **Placed before cancelled.** New protection is placed on OKX first, and the old protection is cancelled only after OKX has accepted the new one. The position is never left without a stop. If OKX refuses a change, the protection that OKX already accepted stays in force, and the dialog shows what is in force now.
- **Arming.** Changes that can only lower risk work while disarmed, like an exit. Changes that add risk need the account to be armed. Adding a stop loss or trailing stop where there was none, and moving a stop loss toward the price, are examples of changes that add risk. A refused change says `NOT_ARMED: this change increases risk`.
- **Risk budget for agents.** A new or looser stop loss from an agent must keep the loss within the agent's risk per trade if it fires. Keeping or tightening a stop is always allowed. Manual changes in the dialog do not use this budget.
- **Awaiting confirmation.** If OKX does not answer a change, the order is never sent again. The dialog says "Sent to OKX, awaiting confirmation", the old protection stays in force, and further changes wait until it is resolved.
- **External orders.** Stops and targets you place in the OKX app appear on the position as External. MochiPrep never cancels or edits them.
- **Cleanup.** When a position closes on OKX, MochiPrep cancels its own leftover stops only after the position has been missing for two checks at least 10 seconds apart. A failed cancel shows as a Cancel pending badge and is retried on every sync.

## Trailing stops: what is not verified yet

Live trailing stops and protection changes have not been verified on OKX Demo Trading, and they are untested with real money. Treat them as experimental until you have tried them on a Demo account. These points are still unconfirmed against OKX. Until they are, MochiPrep does the following:

1. **Callback range.** MochiPrep accepts a ratio callback from 0.1% to 30% only, and refuses anything else before sending.
2. **Activation direction.** MochiPrep assumes OKX starts tracking once the last price reaches the activation price, and refuses an activation price that is not beyond the reference price on the profitable side.
3. **Separate attached orders.** A stop loss, take profit and trailing stop are sent as separate attached orders on an entry. If OKX refuses this, an entry with protection fails, and nothing is placed.
4. **Cancelling trailing stops.** If OKX refuses to cancel a trailing stop after its position closed, new positions on that pair stay refused until you cancel it in the OKX app.
5. **Reduce-only stops after a close.** It is not confirmed what OKX does with reduce-only stops once their position closes. MochiPrep cancels its own after the cleanup wait, and never touches External ones.
6. **Which price is tracked.** It is not confirmed whether OKX trails the last, mark or index price. MochiPrep shows the trigger price that OKX reports.
7. **Fixed size.** Protection orders are sent with the position size at the time they were placed. If you add to the position in the OKX app, MochiPrep's stop covers only the original size until the protection is changed again.
8. **Stop size with stacked entries.** When two entries fill into one position, it is not confirmed how OKX reports each attached stop's size. MochiPrep keeps every entry's stop until a stop sized to the whole position replaces them.
9. **Extra full-position stop.** It is not confirmed whether OKX accepts one more stop sized to the whole position beside the per-entry stops. If it refuses, the per-entry stops stay in force.
10. **Oversized stop at trigger.** It is not confirmed whether OKX trims or rejects a triggered reduce-only stop that is larger than the remaining position. If it rejects, part of the position may stay open after its stop triggers.

## Trading pairs

Pairs you add in **Settings** trade on OKX like the built-in ones. Orders are sent in contracts, and contract sizes vary widely: a BTC contract is 0.01 BTC, and a PEPE contract is 10,000,000 PEPE. One contract can be larger than a small order, and each pair has its own minimum size.

Specifications are read from OKX when you add a pair, and checked again every time you connect. If they have changed, connecting stops. Prices are rounded to the pair's tick size before they are sent. Check the first order on any new pair on OKX, and remember that small coins move faster and have thinner order books than BTC.

## Restarts

By default, OKX keys are held only in memory. After a restart, the account stays listed but is disconnected and disarmed, and its agents are paused. Your positions and stop losses remain on OKX. Reconnect in **Settings**, then arm again.

With a master password set, you can tick **Save encrypted with my master password** when you connect. The key is then stored encrypted, and it reconnects when you unlock after a restart. Reconnecting runs every check a manual connect does. A real account always comes back **disarmed**, so you must arm it again yourself. Disconnecting an account, or turning off the master password, deletes the saved key.

## Limits

- Balances and positions come from OKX and refresh every 5 seconds. MochiPrep keeps no ledger for exchange accounts.
- You can change protection from MochiPrep. Anything you place on OKX yourself is shown as External and is never touched.
- A stop or take profit that fills on OKX appears as a position change, not as a fill in MochiPrep's history, so it has no chart marker.
- Daily loss is measured from the first equity snapshot of each UTC day, and it includes deposits and withdrawals made that day.
- Order status is checked by polling, not streamed.
