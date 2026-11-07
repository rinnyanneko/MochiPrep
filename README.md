# MochiPrep

MochiPrep is a local-first, bring-your-own-key AI paper-trading terminal for USDT-margined crypto perpetuals. It combines deterministic replay, public OKX market data, strict risk checks, an auditable ledger, and multiple model providers. There is no live order path, and simulated performance is not predictive.

> Screenshot placeholder: the final application screenshot will be added after the web UI is complete.

## Prerequisites

- Node.js 22.13 or newer (`node:sqlite` is required)
- pnpm 9 or newer (the repository pins pnpm 11)

## Run locally

```sh
pnpm install
pnpm dev
```

The server and React client run together. For validation, use `pnpm test`; tests are offline and require no API key. Individual package typechecks are available with commands such as `pnpm --filter @mochiprep/core exec tsc -p tsconfig.json`.

## Demo Mode

Demo Mode uses a bundled, seeded replay tape and mock agents. Time advances deterministically and waits for agent work, making equal runs reproducible by values and fill sequence. Demo trades, PnL, rejections, and leaderboard results are simulations—not forecasts or investment claims.

## BYOK providers

You can configure OpenAI-compatible, OpenAI, Anthropic, or Gemini providers. Keys remain only in server process memory: they are never stored in SQLite, logged, returned to the browser, or restored after restart. Model output is strictly validated and must pass server-side risk checks before the paper engine can accept an order.

## Current limits

- Paper trading only; `packages/exchanges` has interfaces but no live implementation.
- Linear USDT perpetual math only, with OKX as the first public market-data venue.
- Simplified fills, funding, and isolated liquidation; no order-book queue, partial liquidity, maintenance tiers, cross margin, or exchange-identical liquidation.
- API keys disappear on restart by design.

See [paper engine rules](docs/PAPER_ENGINE.md), [architecture](docs/ARCHITECTURE.md), and [roadmap](docs/ROADMAP.md).
