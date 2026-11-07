# MochiPrep agent guide

Local-first, BYOK AI trading terminal for USDT-margined crypto perpetuals. Agents trade paper accounts,
OKX Demo Trading, or real OKX accounts. OKX is the only exchange; every other venue is a roadmap milestone.

## Layout

- `packages/shared`: domain types, Zod schemas, indicators. The API contract lives here.
- `packages/core`: decimal helpers, perp math, fill model, risk engine, paper engine, ledger audit.
- `packages/market-data`: `MarketDataProvider`, OKX public adapter, deterministic replay tape.
- `packages/ai-providers`: mock, OpenAI-compatible, OpenAI, Anthropic, Gemini, secret redaction.
- `packages/agents`: prompt, output parsing, `AgentRunner` cycle.
- `packages/exchanges`: signed OKX client and `OkxExecutionEngine`. Real money: see docs/OKX_TRADING.md.
- `apps/server`: Fastify routes, `Runtime`, SQLite store and migrations.
- `apps/web`: React UI.

## Commands

    pnpm install
    pnpm test                 # vitest, all packages, network blocked
    pnpm typecheck
    pnpm test:e2e             # Playwright, needs Chrome

Needs Node >= 22.13 (`node:sqlite`). Workspace packages export TypeScript source, so there is no build step.

## Rules that must not bend

- Accounting uses `D` from `packages/core/src/decimal.ts`, never JS number arithmetic.
- Every wallet or margin change goes through the ledger, inside one store transaction.
- AI output is untrusted. Only `AgentRunner` turns it into orders, and only after Zod and the risk engine.
- The paper engine must never invent a price: fills come from observed quotes, never OHLC paths.
- AI keys live in SQLite by the owner's choice, and never in logs, errors or the browser.
- OKX credentials stay in `Runtime` memory only. Never write them to disk or show them to a model.
- Never resend an order after an unknown outcome: resolve it by lookup. Every OKX order passes `preTradeRefusal`.
- Contract specs come from OKX, never from guesses. `SymbolId` is open-ended: use `getInstrument`, and remember replay has only `DEFAULT_SYMBOLS`.
- A key that can withdraw is always refused. Exits stay possible when disarmed.
- Never present simulated results as predictive, and never claim an unimplemented feature works.
- Comments explain why. Do not add placeholder code for future markets.
