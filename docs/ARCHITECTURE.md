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

## Execution modes

In `AUTO`, an approved decision goes straight to the engine. In `SIGNAL`, the cycle stops at outcome `SIGNAL` with an expiry on the feed's clock. `executeSignal` trusts only the idea: it re-runs the risk engine on current data, which also resizes the order. A new suggestion from the same agent replaces the old one, and Stop All clears them all. Replay stops at each suggestion, or it would expire before anyone could read it.

## News

`NewsService` reads enabled RSS feeds at most every five minutes. Feed text is hostile input: `sanitizeHeadline` removes angle brackets and control or format characters, so a title cannot close the `<news>` block in the prompt, and titles are capped in length. The system prompt says headlines are data and never instructions. That lowers prompt injection risk without removing it, so the risk engine stays the real bound on any order. News failures never stop a cycle. Feeds must be public https, and redirects are followed by hand so each hop passes that check, because the backend fetches them from the user's machine.

## Replay clock

Replay is a seeded, bundled minute tape with a warm-up region. Its clock advances only through an explicit step. The runtime publishes one quote per symbol and waits for quote handling and due agent cycles to complete before advancing again. Slow model calls therefore pause tape time instead of skipping simulated minutes. Reset restores the deterministic cursor and account state, so equal inputs produce equal numeric outcomes; random database IDs are not part of determinism.

## Persistence and recovery

SQLite stores accounts, orders, positions, fills, ledger events, agents, decisions, and replay cursor. Financial mutations use one synchronous store transaction, avoiding an `await` boundary inside accounting. On restart, durable paper state and replay position are recovered, agents are forced to paused/not-running, AI provider keys are reloaded from SQLite, and OKX credentials are absent because they never reach disk, so exchange accounts come back disconnected and disarmed.

## Rust backend (in progress)

A Rust port of the backend is being built under `crates/`. So far it has instrument specs and market types (`mochi-domain`), decimal math that matches decimal.js (`mochi-decimal`), the accounting core (`mochi-core`: perp math, fill model, ledger audit, performance, risk engine and the quote-driven paper engine), SQLite opening, the same migrations and the paper store (`mochi-store`), master-password crypto compatible with `vault.ts` in both directions (`mochi-vault`), and the deterministic replay tape and feed (`mochi-market`). Golden fixtures in `crates/golden`, generated from the TypeScript code, keep the two in step; 43 scenarios prove the paper engine matches Node, and a test shows a reopened file database restores its state. There is no Rust HTTP server, runtime, agents or OKX trading yet, and the Rust backend is not usable. The Node backend is still what runs.

## Security model

The server binds local use behind an authorization token plus Host and Origin checks. AI keys are stored unencrypted in the `provider_keys` table, by the owner's choice, in a database only the owner can read. They are omitted from API views. OKX credentials exist only in `Runtime` memory. An optional master password changes this: a random data key seals AI keys (`sealed_provider_keys`) and, when the owner opts in, OKX credentials (`sealed_exchange_credentials`) with AES-256-GCM, bound to their row. The password only wraps the data key through scrypt (`vault_keys`), and neither is ever stored. Until unlocked after a restart, sealed keys are not loaded. `secure_delete` and a WAL checkpoint scrub the plaintext rows that sealing replaces. Extra provider headers stay unencrypted. Known and key-shaped secrets are redacted from provider errors and raw responses before persistence or return. Exchange credentials are never part of a model's context. Public OKX market data needs no key. MochiPrep has no withdrawal path, and it refuses keys that can withdraw.
