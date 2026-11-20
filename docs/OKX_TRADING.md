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

Closing isolated net positions is available while disarmed. The close request identifies the selected row; MochiPrep checks that identity and size against OKX before sizing and again before submission. OKX accepts an instrument and margin mode on its order endpoint, so this check cannot lock the position against trades happening after the last read. Close cross-margin positions in OKX.

To reduce an isolated position by a specified quantity, select **Reduce only** on the trading ticket, choose the position, then choose Market or Limit. The ticket sends the selected `positionId` with `reduceOnly: true` to `POST /api/orders`. MochiPrep verifies that the order reduces that position and the requested quantity fits. OKX handles the matching and lifetime of a resting reduce-only limit order.

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
3. **Combined trailing protection.** OKX rejected a fixed SL plus attached trailing stop with `54091`. MochiPrep now attaches the fixed SL by itself, then submits the optional TP and trailing stop separately after the entry fills. If the remainder of a partially filled order is canceled, it is unconfirmed whether OKX keeps the attached stop for the filled size. MochiPrep sends the optional exits only after it confirms that attached stop on OKX, or a replacement fixed stop covering the current position. Until you have checked the stop on OKX, do not rely on it for a partially filled position. TP plus trailing without SL also defers trailing. A trailing-only entry remains attached, subject to the account's stop-loss requirement. The trailing stop starts tracking only when that separate order reaches OKX; it is not atomic with entry and cannot recover earlier price extremes. A rejection leaves the fixed stop untouched and is shown on the trailing order row. Unsent intent survives restart; once submission starts, it is resolved only by lookup and is never resent. A crash immediately before sending may therefore leave the trailing stop unplaced.

   Before sending, MochiPrep verifies the entry fills that still belong to the current isolated position. It allows a short window for the position, execution history, or fixed stop to appear. Each deferred exit is sized to the entry fills verified in the current position. Executions from before a close are excluded. OKX fills do not state their margin mode, so an ordinary fill on the same pair from an order MochiPrep did not place (a cross-margin trade, or one made in the OKX app) leaves the entry unverified. MochiPrep then waits and cancels the deferred exit with its reason, rather than sizing it to part of the entry. If MochiPrep cannot verify any remaining entry fills or confirm the fixed stop after a partial cancel, it cancels the deferred exit and shows the reason. If a fresh OKX quote dated strictly after the final entry fill shows that the activation price has been reached, OKX starts trailing from submission. If an OKX quote dated after the final entry fill shows that take profit has been reached, MochiPrep sends a reduce-only market order for the verified quantity. Simultaneous targets keep separate allocations when execution history confirms that the earlier reduction came from another verified target and the position remained open. The execution price can differ from the target. This lifecycle has fake-exchange regression coverage and still needs Demo verification. See the [OKX order contract](https://www.okx.com/docs-v5/en/#order-book-trading-trade-post-place-order) and [attached trailing changelog](https://www.okx.com/docs-v5/log_en/#2026-04-13).
   A position adjustment can advance its update time without a trade. An otherwise attributable deferred exit stays pending when the adjustment is unexplained. If the same position, trade and quantity were already verified in the session, the adjustment alone does not block the exit.
4. **Cancelling trailing stops.** If OKX refuses to cancel a trailing stop after its position closed, new positions on that pair stay refused until you cancel it in the OKX app.
5. **Reduce-only stops after a close.** It is not confirmed what OKX does with reduce-only stops once their position closes. MochiPrep cancels its own after the cleanup wait, and never touches External ones.
6. **Which price is tracked.** It is not confirmed whether OKX trails the last, mark or index price. MochiPrep shows the trigger price that OKX reports.
7. **Fixed size.** Protection orders are sent with the position size at the time they were placed. If you add to the position in the OKX app, MochiPrep's stop covers only the original size until the protection is changed again.
8. **Stop size with stacked entries.** When two entries fill into one position, it is not confirmed how OKX reports each attached stop's size. MochiPrep keeps every entry's stop until a stop sized to the whole position replaces them.
9. **Extra full-position stop.** It is not confirmed whether OKX accepts one more stop sized to the whole position beside the per-entry stops. If it refuses, the per-entry stops stay in force.
10. **Oversized stop at trigger.** It is not confirmed whether OKX trims or rejects a triggered reduce-only stop that is larger than the remaining position. If it rejects, part of the position may stay open after its stop triggers.
11. **Net-mode fill classification.** The OKX fills endpoint documents subtypes `3` and `4` for opening long and short. MochiPrep requires these explicit open subtypes before using a newer fill to confirm that an earlier entry still belongs to the current position. Demo testing is needed to confirm which subtypes OKX returns for adds to an isolated net swap position. [OKX fills endpoint](https://www.okx.com/docs-v5/en/#order-book-trading-trade-get-transaction-details-last-3-days)

## Agent results in the Trading Arena

An exchange account's balance moves with deposits and transfers, so the arena shows no return for an OKX agent. It shows the agent's own P&L in USDT instead:

- Closed positions come from OKX's position history (`account/positions-history`), read once a minute and kept in MochiPrep, because OKX keeps only three months. Each counts OKX's `realizedPnl`, which includes fees and funding. Stops, take profits and closes made in the OKX app count too. A long history is read over several minutes.
- The open position on the agent's pair adds what OKX has realized on it so far (fees, funding, partial closes) and its unrealized P&L.
- A confirmed opening-side execution from one of the agent's orders must fall inside an isolated net position's exchange timestamps. An order that is only partially filled still provides execution evidence. MochiPrep uses OKX's execution time even if reconciliation happened later. Ambiguous overlapping lifetimes cannot establish ownership. MochiPrep records recent executions immediately and backfills OKX's three-month transaction history in bounded pages. Executions that OKX can no longer supply stay outside the reported total. The result covers the whole net position, including manual activity that changes it.
- Agents configured for the same account and pair do not get separate P&L. Confirmed participation by another agent also marks the P&L as shared after that agent is moved or deleted.

Not yet confirmed against OKX:

1. **Partial closes.** It is not confirmed whether a partly closed position's history record carries the total realized so far or only the latest part. MochiPrep keeps one record per position (its posId and open time) and takes the newest, which would undercount if OKX reports each part separately.
2. **Paging.** MochiPrep assumes `after` returns records last updated before the given time. The first read stops after 1,000 records.
3. **Open time.** MochiPrep reads a position's open time from `cTime`, and assumes a position that went flat and reopened gets a new `cTime`. An open position without one is left out.
4. **Realized P&L on an open position.** MochiPrep assumes the open position's `realizedPnl` already includes any partial close, and then skips that position's history record so it is not counted twice.

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
