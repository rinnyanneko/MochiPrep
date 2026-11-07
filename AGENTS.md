# MochiPrep agent guide

Local-first, BYOK AI paper-trading terminal for USDT-margined crypto perpetuals. Paper trading only:
there is no live order path. OKX is the first exchange; every other venue is a roadmap milestone.

## Layout

- `packages/shared`: domain types, Zod schemas, indicators. The API contract lives here.
- `packages/core`: decimal helpers, perp math, fill model, risk engine, paper engine, ledger audit.
- `packages/market-data`: `MarketDataProvider`, OKX public adapter, deterministic replay tape.
- `packages/ai-providers`: mock, OpenAI-compatible, OpenAI, Anthropic, Gemini, secret redaction.
- `packages/agents`: prompt, output parsing, `AgentRunner` cycle.
- `packages/exchanges`: live interfaces only, nothing implemented.
- `apps/server`: Fastify routes, `Runtime`, SQLite store and migrations.
- `apps/web`: React UI.

## Commands

    pnpm install
    pnpm test                 # vitest, all packages
    pnpm --filter @mochiprep/core exec tsc -p tsconfig.json

Needs Node >= 22.13 (`node:sqlite`). Workspace packages export TypeScript source, so there is no build step.

## Rules that must not bend

- Accounting uses `D` from `packages/core/src/decimal.ts`, never JS number arithmetic.
- Every wallet or margin change goes through the ledger, inside one store transaction.
- AI output is untrusted. Only `AgentRunner` turns it into orders, and only after Zod and the risk engine.
- The paper engine must never invent a price: fills come from observed quotes, never OHLC paths.
- API keys stay in `Runtime` memory: never in SQLite, logs, errors or the browser.
- Never present simulated results as predictive, and never claim an unimplemented feature works.
- Comments explain why. Do not add placeholder code for future markets.
