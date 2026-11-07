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

## Agent cycle

An agent cycle snapshots a fresh quote and closed candles, builds indicators and a prompt, calls the selected provider, strictly validates the response, then passes it through every risk check. Only an approved, sized result reaches the account's `ExecutionEngine`, which is the paper engine or the OKX engine. An exchange account's ceilings override the agent's own limits during sizing, and the OKX engine checks them again. Invalid output, stale data, provider failure, or risk rejection produces a decision record but no order. Analyze-only cycles never submit orders. A per-agent in-flight guard prevents overlapping cycles, and pausing or stopping during model work is checked before execution.

AI output is untrusted data throughout this path. It cannot select another account, bypass sizing, change configured limits, or directly call the engine.

## Replay clock

Replay is a seeded, bundled minute tape with a warm-up region. Its clock advances only through an explicit step. The runtime publishes one quote per symbol and waits for quote handling and due agent cycles to complete before advancing again. Slow model calls therefore pause tape time instead of skipping simulated minutes. Reset restores the deterministic cursor and account state, so equal inputs produce equal numeric outcomes; random database IDs are not part of determinism.

## Persistence and recovery

SQLite stores accounts, orders, positions, fills, ledger events, agents, decisions, and replay cursor. Financial mutations use one synchronous store transaction, avoiding an `await` boundary inside accounting. On restart, durable paper state and replay position are recovered, agents are forced to paused/not-running, AI provider keys are reloaded from SQLite, and OKX credentials are absent because they never reach disk, so exchange accounts come back disconnected and disarmed.

## Security model

The server binds local use behind an authorization token plus Host and Origin checks. AI keys are stored unencrypted in the `provider_keys` table, by the owner's choice, in a database only the owner can read. They are omitted from API views. OKX credentials exist only in `Runtime` memory. Known and key-shaped secrets are redacted from provider errors and raw responses before persistence or return. Exchange credentials are never part of a model's context. Public OKX market data needs no key. MochiPrep has no withdrawal path, and it refuses keys that can withdraw.
