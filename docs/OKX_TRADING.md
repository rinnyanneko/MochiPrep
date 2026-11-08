# Trading on OKX

> **DYOR: do your own research.** MochiPrep is not financial advice. AI output can be wrong, and you are responsible for your own trades.

> **Status: tested only against a simulated exchange.** The client follows OKX's published API, and its signature format was accepted by the real endpoint, but no order has ever been placed on OKX. Real money carries the risk of real loss, including from a bug in this code, and AI decisions can be wrong. You are responsible for every order.

## Before your first real order

Work through these in order, and stop at the first surprise.

1. In OKX, set the account to **Futures mode** and **one-way** position mode.
2. Create a **Demo Trading** API key with **Trade** permission only, as described in [the API key guide](API_KEYS.md). Bind it to your IP if you can.
3. In **Settings > OKX trading**, connect it with the Demo Trading environment.
4. Place a market order with a stop from **OKX Trading**, then confirm on OKX that the order, its stop, and the isolated margin are what you expect.
5. Close it. Then try a limit order, a cancel, and a run of one agent.
6. Stop the network or the backend while a position is open, and confirm the stop still sits on OKX.
7. Only then create a real key, again **Trade only**, with small ceilings.

These assumptions still need checking on the real exchange:

- The demo REST host is `https://openapi.okx.com`, per OKX's documentation.
- `51603` means an order does not exist. Only that code counts as "not found", and anything else leaves the order unresolved.
- OKX's order docs describe `isolated` in terms of spot margin, while its leverage docs describe isolated swaps. MochiPrep sends `tdMode: isolated`, so confirm this at step 4.

## How orders are protected

| Layer | What it does |
| --- | --- |
| Connect | Refuses a key that can withdraw or cannot trade, hedge mode, Spot mode, or changed contract specs. |
| Arm | Real orders stay off until you type `TRADE REAL MONEY`. Demo accounts need no arming. |
| Account ceilings | Notional, leverage, positions, and daily loss, enforced on every order from you or an agent. The default is 100 USDT per order. |
| Stop loss | Required on every new position, and sent to OKX as an attached order, so it survives MochiPrep stopping. |
| Sync | Reconciles balance, positions and orders every 5 seconds. New exposure is refused after 30 seconds without a good sync, and losing sync pauses agents. |
| Stale data | New exposure is refused on a quote older than 10 seconds. |
| Duplicates | Each order is saved before sending and keyed locally, so a repeated request sends nothing. |
| Unknown outcome | After a timeout the order stays open and is looked up by client ID. It is **never resent**. |
| Stop All | Stops every agent and disarms real orders. Open positions and their stops stay on OKX. |

An agent in Suggest mode can run without arming, because it places nothing. Accepting a suggestion needs an armed account, except for a CLOSE, and the risk engine runs again at that moment.

Agents go through the same risk engine as paper trading, then the ceilings above. On an exchange account, the stricter of the agent's limit and the account's ceiling applies.

An agent reading news on a real account takes third-party text as input. Anyone can publish a headline written to steer an AI. Ceilings limit the damage, not the decision, so prefer Suggest mode here.

Closing is always allowed while connected, even when disarmed, so you can never be locked out of an exit.

## Trailing stops and protection changes

A position's protection is its stop loss, take-profit and an optional trailing stop. On OKX each leg is an algo order: stop loss and take-profit are `conditional` and the trailing stop is `move_order_stop`, all reduce-only. You can change them from the position's pencil button, and an agent can with a `PROTECT` decision (only with the trailing-stop switch on, for the trailing part). Both take the same path.

**Placed before cancelled.** Every changed leg is placed first, and old or removed algos are cancelled only after OKX accepted all the new ones, so the position is never left without a stop. Two stops may briefly coexist. If a placement is refused, nothing is cancelled and the old protection stays.

**Arming.** A change that can only lower risk works while disarmed, like an exit. These need the account armed (OKX Demo accounts always are):

- Moving the stop loss away from the price (down for a long, up for a short), or removing it.
- Adding, moving or removing a take-profit.
- Removing the trailing stop, widening its callback, or changing its kind or activation price.

Adding a stop loss or a trailing stop where there was none, moving the stop loss toward the price, and narrowing a trailing callback of the same kind and activation price do not. A refused change says `NOT_ARMED: this change increases risk`. Other refusals are `PROTECTION_PENDING` (an earlier change is still awaiting OKX), `EXCHANGE_NOT_SYNCED`, `NO_MARKET_DATA` and `MARKET_DATA_STALE`.

**External orders.** Stops and targets you place in the OKX app are shown on the position as External and are read-only. MochiPrep never cancels or edits them, and they are not counted as its own protection.

**Awaiting confirmation.** If OKX does not answer a placement, the order is never resent. The dialog says "Sent to OKX, awaiting confirmation", the old protection stays in force, and the next sync looks the order up by its client ID. Further changes are refused until it is resolved.

**Cleanup.** If a position disappears from OKX, MochiPrep's own leftover algos are cancelled only after the position has been missing for two syncs at least 10 seconds apart, so one bad read cannot remove a stop. A cancel that fails shows on the position as a Cancel pending badge (not External, because it is still MochiPrep's own order) and is retried on every sync.

**Not verified on OKX Demo yet.** Live trailing stops and protection changes have not been verified on OKX Demo, and they are untested with real money. Treat all of this as experimental until you have tried it on a Demo account. These behaviours are UNCONFIRMED, each with what MochiPrep does meanwhile:

1. **Callback range.** The `callbackRatio` range OKX accepts is not confirmed. MochiPrep limits it to 0.001 to 0.3 (0.1% to 30%) and refuses anything else before sending.
2. **Activation direction.** Which side of the price `activePx` may sit on is not confirmed. MochiPrep assumes OKX starts tracking once the last price reaches the activation price (at or above it for a long, at or below it for a short). Separately, MochiPrep itself refuses, before sending, an activation price that is not strictly beyond the reference price in that direction: the entry price when opening, the last price for an open position.
3. **Separate attached orders.** Whether a stop loss, take-profit and trailing stop can be attached to an entry as separate `attachAlgoOrds` elements is not confirmed. MochiPrep sends one element per leg. If OKX refuses, an entry with protection fails and nothing is placed.
4. **Cancelling trailing stops.** Whether `cancel-algos` cancels a `move_order_stop` is not confirmed. A failed cancel shows as a Cancel pending badge and is retried on every sync.
5. **Algos when a position closes.** What OKX does with reduce-only algos once the position closes is not confirmed. MochiPrep cancels its own after the cleanup wait above, and does not touch External ones.
6. **Which price trailing tracks.** Whether OKX trails the last, mark or index price is not confirmed. On OKX, MochiPrep shows the trigger price OKX reports and no extreme of its own.

## Trading pairs

Pairs added in **Settings** trade on OKX like the built-in ones. Orders go out in contracts, and contract sizes vary widely: a BTC contract is 0.01 BTC, a PEPE contract is 10,000,000 PEPE. So one contract may be larger than a small order, and the minimum size differs per pair.

Specs are read from OKX when a pair is added and rechecked on every connect, which refuses to continue if any changed. Prices are rounded to the pair's tick before sending. Check the first order on any new pair on OKX, and note that small coins move faster and have thinner books than BTC.

## Restarts

By default OKX credentials are held only in memory. After a restart the account stays listed but is disconnected and disarmed, and its agents are paused. Positions and stops remain on OKX. Reconnect in **Settings**, then arm again.

With a master password set, you can tick **Save encrypted with my master password** when connecting. The key is then stored sealed with AES-256-GCM, and reconnects when you unlock after a restart. Reconnecting runs every check a manual connect does, so a key that has gained withdrawal permission, or OKX contract specs that changed, still stop it. A real account always comes back **disarmed**: arm it again yourself. Disconnecting an account, reconnecting it without the box ticked, or turning the master password off deletes the saved key.

## Limits

- Balances and positions come from OKX and refresh every 5 seconds. MochiPrep keeps no ledger for exchange accounts.
- Protection can be changed from MochiPrep (see Trailing stops and protection changes), but anything you place on OKX yourself is shown as External and never touched.
- A stop or take-profit that fills on OKX shows up as a position change, not as a fill in MochiPrep's history, so it has no marker on the chart.
- Daily loss is measured from the first equity snapshot of the UTC day, and it includes deposits and withdrawals made that day.
- Order state is polled, not streamed.
