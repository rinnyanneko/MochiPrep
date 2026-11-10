# Architecture

MochiPrep is a local-first, BYOK trading terminal. The server owns secrets and trading state; the browser is an untrusted display and control client.

## Package boundaries

- `packages/shared` defines domain types, Zod schemas, instruments, and indicators. It is the API contract.
- `packages/core` owns decimal arithmetic, perpetual math, fill and risk models, the paper engine, and ledger audit.
- `packages/market-data` exposes `MarketDataProvider`, the OKX public adapter, and the deterministic replay tape.
- `packages/ai-providers` adapts model APIs and redacts secrets. Provider keys are runtime-only.
- `packages/agents` builds prompts, strictly parses model output, evaluates risk, and is the only layer that turns a decision into an order.
- `packages/exchanges` holds the signed OKX client and `OkxExecutionEngine`. See [Trading on OKX](OKX_TRADING.md).
- `apps/server` composes providers, agents, the engine, Fastify routes, and SQLite persistence.
- `apps/web` is the React client and must not receive API keys.

## Instruments

`INSTRUMENTS` in `packages/shared` is a registry: the three built-in pairs plus any the user adds, stored in the `instruments` table and registered at startup. An added pair's specs come from OKX at the moment of adding, and connecting an exchange account rechecks every registered pair. Replay has only the built-in pairs, so `assertTradable` keeps added pairs on live data.

`parseOkxInstrument` decides what is tradable. Its crypto filter uses `instCategory: 1`, a meaning inferred from OKX's data rather than documentation: 3 holds stocks and 4 holds commodities.

## Agent cycle

An agent cycle snapshots a fresh quote and closed candles, builds indicators and a prompt, calls the selected provider, strictly validates the response, then passes it through every risk check. Only an approved, sized result reaches the account's `ExecutionEngine`, which is the paper engine or the OKX engine. An exchange account's ceilings override the agent's own limits during sizing, and the OKX engine checks them again. Invalid output, stale data, provider failure, or risk rejection produces a decision record but no order. Analyze-only cycles never submit orders. A per-agent in-flight guard prevents overlapping cycles, and pausing or stopping during model work is checked before execution.

AI output is untrusted data throughout this path. It cannot select another account, bypass sizing, change configured limits, or directly call the engine.

## Protection

`Protection` is `{ stopLoss, takeProfit, trailingStop }`, the whole protective state of a position. `ExecutionEngine.setProtection` replaces it in one call and returns what was in force before and after, plus `pending` when an exchange has not yet confirmed. The paper engine applies it in one store transaction and holds the trailing extreme on the order row. The OKX engine places every changed leg as an algo order before cancelling any, and applies the armed rule from `classifyProtectionChange`. A `PROTECT` decision from an agent and the position dialog's `PUT /api/positions/:accountId/:symbol/protection` both end here; for an agent, only `AgentRunner` does that, after Zod and the risk engine. An absent `trailingStop` keeps the current one and `null` removes it. Orders the owner placed on OKX are exposed read-only as `externalProtection`.

## Execution modes

In `AUTO`, an approved decision goes straight to the engine. In `SIGNAL`, the cycle stops at outcome `SIGNAL` with an expiry on the feed's clock. `executeSignal` trusts only the idea: it re-runs the risk engine on current data, which also resizes the order. A new suggestion from the same agent replaces the old one, and Stop All clears them all. Replay stops at each suggestion, or it would expire before anyone could read it.

## News

`NewsService` reads enabled RSS feeds at most every five minutes. Feed text is hostile input: `sanitizeHeadline` removes angle brackets and control or format characters, so a title cannot close the `<news>` block in the prompt, and titles are capped in length. The system prompt says headlines are data and never instructions. That lowers prompt injection risk without removing it, so the risk engine stays the real bound on any order. News failures never stop a cycle. Feeds must be public https, and redirects are followed by hand so each hop passes that check, because the backend fetches them from the user's machine.

## Replay clock

Replay is a seeded, bundled minute tape with a warm-up region. Its clock advances only through an explicit step. The runtime publishes one quote per symbol and waits for quote handling and due agent cycles to complete before advancing again. Slow model calls therefore pause tape time instead of skipping simulated minutes. Reset restores the deterministic cursor and account state, so equal inputs produce equal numeric outcomes; random database IDs are not part of determinism.

## Persistence and recovery

SQLite stores accounts, orders, positions, fills, ledger events, agents, decisions, and replay cursor. Financial mutations use one synchronous store transaction, avoiding an `await` boundary inside accounting. On restart, durable paper state and replay position are recovered, agents are forced to paused/not-running, and exchange accounts come back disconnected and disarmed. Without a master password, AI provider keys are reloaded from SQLite and OKX credentials are absent, because they never reach disk. With one, sealed AI keys and any OKX credentials the owner chose to save stay unavailable until unlock; unlocking reconnects saved OKX accounts through the full connect checks, and a real account must still be armed again before it can trade.

## Rust backend (in progress)

A Rust port of the backend is being built under `crates/`. So far it has instrument specs and market types (`mochi-domain`), decimal math that matches decimal.js (`mochi-decimal`), the accounting core (`mochi-core`: perp math, fill model, ledger audit, performance, risk engine and the quote-driven paper engine), SQLite opening, the same migrations and the paper store (`mochi-store`), master-password crypto compatible with `vault.ts` in both directions (`mochi-vault`), and the deterministic replay tape and feed (`mochi-market`). Golden fixtures in `crates/golden`, generated from the TypeScript code, keep the two in step; 47 scenarios prove the paper engine matches Node, and a test shows a reopened file database restores paper state.

`mochi-server` is a Rust HTTP server on axum and tokio. A `Runtime` actor owns the paper engine, replay clock, live OKX public feed, settings and agent state. Routes and asynchronous model/candle/news completions communicate with that actor, so stalled models do not block HTTP or live quotes. Replay awaits due agents at each bar before advancing. The server provides the Node auth, SSE, embedded UI, paper accounts/trading, market data, instruments, news, provider CRUD/keys/tests, agent lifecycle/analysis, suggestions and demo lifecycle. Demo reset removes only demo-owned data and repeats economic results from the first bar. Existing browser tests run with `pnpm test:e2e:rust`; Node stays the default for `pnpm dev`, `pnpm start` and `pnpm test:e2e`. `MOCHIPREP_OFFLINE=1` disables the Rust OKX public feed and news fetching.

Rust vault mutations and OKX trading still answer 501 `Not available in the Rust backend yet`. A saved vault stays LOCKED; remote AI cannot load sealed keys. MOCK needs no key, and provider keys can be stored in plaintext while the vault is OFF. The crypto format exists in `mochi-vault`; runtime unlock/sealing orchestration is Plan 5.

Accepted differences from Node, none on a path the web UI depends on:
- A feed date with no time zone is read as UTC; Node reads it as the machine's local time.
- A lone UTF-16 surrogate in feed text becomes U+FFFD, where Node's JSON writes a `\udXXX` escape. Lengths and cuts match.
- Malformed OKX envelopes and hostile JSON (a lone-surrogate escape, `1e400`, nesting past 128 levels, URLs the `url` crate and Node parse differently) get the same status with Rust's own error text.
- An unknown path or wrong method answers 404 `{"error":"Not found"}`, not Fastify's `{message,error,statusCode}`. On such paths a 415 or 413 can come before the 404, because bodies are checked before routing.
- Static files carry no `Last-Modified`, a different `ETag` and no percent-decoding, unlike `@fastify/send`.
- Playwright runs one worker against one shared backend, so spec files depend on their name order: `demo.spec.ts`'s 40,000.00 total equity holds only before `paper.spec.ts` creates its account.
- Neither backend handles Ctrl-Break on Windows (Node listens for SIGINT and SIGTERM only), so Ctrl-Break ends the process without a clean shutdown. On Ctrl-C the Rust server closes open event streams and the database cleanly.

## Rust carryovers

| Area | Current boundary / next work |
| --- | --- |
| Plan 5 vault | Enable/unlock/change/disable/reset orchestration, sealed credentials and saved OKX reconnect are not implemented in Rust. Existing vaults fail closed. |
| Plan 5 OKX | Private trading sessions, arming, unknown-outcome lookup and exchange protection changes remain unavailable. No simulated exchange execution is presented as real trading. |
| Exchange protection tests | Pending confirmations, partial `ProtectionChangeError.after` and post-alignment unchanged-result scenarios require the Plan 5 exchange engine. Paper protection changes are synchronous, return pending=false and do not align stop prices. |
| Untrusted JSON | Numeric overflow, isolated UTF-16 surrogate escapes and JSON deeper than serde_json's recursion bound are safely rejected with Rust invalid-JSON text; Node may accept the JSON and produce a different validation error. Valid Unicode and finite numeric decisions retain parity. |
| Text cutoffs | Rust preserves UTF-8 at response/sample cutoffs and drops an astral character that would cross the UTF-16 boundary; JavaScript can retain an isolated surrogate. Secrets are redacted before cutting. |

## Security model

The server binds local use behind an authorization token plus Host and Origin checks. AI keys are stored unencrypted in the `provider_keys` table, by the owner's choice, in a database only the owner can read. They are omitted from API views. OKX credentials exist only in `Runtime` memory. An optional master password changes this: a random data key seals AI keys (`sealed_provider_keys`) and, when the owner opts in, OKX credentials (`sealed_exchange_credentials`) with AES-256-GCM, bound to their row. The password only wraps the data key through scrypt (`vault_keys`), and neither is ever stored. Until unlocked after a restart, sealed keys are not loaded. `secure_delete` and a WAL checkpoint scrub the plaintext rows that sealing replaces. Extra provider headers stay unencrypted. Known and key-shaped secrets are redacted from provider errors and raw responses before persistence or return. Exchange credentials are never part of a model's context. Public OKX market data needs no key. MochiPrep has no withdrawal path, and it refuses keys that can withdraw.
