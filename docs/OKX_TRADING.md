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

## Trading pairs

Pairs added in **Settings** trade on OKX like the built-in ones. Orders go out in contracts, and contract sizes vary widely: a BTC contract is 0.01 BTC, a PEPE contract is 10,000,000 PEPE. So one contract may be larger than a small order, and the minimum size differs per pair.

Specs are read from OKX when a pair is added and rechecked on every connect, which refuses to continue if any changed. Prices are rounded to the pair's tick before sending. Check the first order on any new pair on OKX, and note that small coins move faster and have thinner books than BTC.

## Restarts

OKX credentials are held only in memory. After a restart the account stays listed but is disconnected and disarmed, and its agents are paused. Positions and stops remain on OKX. Reconnect in **Settings**, then arm again.

## Limits

- Balances and positions come from OKX and refresh every 5 seconds. MochiPrep keeps no ledger for exchange accounts.
- Stops can't be edited after entry from MochiPrep. Use OKX.
- A stop or take-profit that fills on OKX shows up as a position change, not as a fill in MochiPrep's history, so it has no marker on the chart.
- Daily loss is measured from the first equity snapshot of the UTC day, and it includes deposits and withdrawals made that day.
- Order state is polled, not streamed.
