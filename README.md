# MochiPrep

*Your cute little AI trading companion.*

MochiPrep is a local-first, bring-your-own-key AI paper-trading terminal for USDT-margined crypto perpetuals. It combines deterministic replay, public OKX market data, strict risk checks, an auditable ledger, and multiple model providers. There is no live order path, and simulated performance is not predictive.

## Prerequisites

- Node.js 22.13 or newer (`node:sqlite` is required)
- pnpm 9 or newer (the repository pins pnpm 11)

## Run locally

```sh
pnpm install
pnpm dev
```

Open <http://localhost:5173>. The backend binds to `127.0.0.1:8787` and keeps its SQLite database in `.mochiprep/`. Nothing else is required: no Docker, no account, no paid service.

## Test

```sh
pnpm test        # unit and integration, offline, no API key
pnpm typecheck
pnpm test:e2e    # Playwright through the real UI, needs Google Chrome installed
```

`pnpm test` fails any test that tries to reach the network. The browser test runs on its own ports with a throwaway database.

## Demo Mode

Press **Try Demo Mode** on the Dashboard. It needs no API key and no internet: each mock agent gets 10,000 virtual USDT, then press play in the top bar to run the tape. Demo Mode uses a bundled, seeded replay tape and mock agents. Time advances deterministically and waits for agent work, making equal runs reproducible by values and fill sequence. Demo trades, PnL, rejections, and leaderboard results are simulations—not forecasts or investment claims.

## Live OKX prices

Switch the top bar to **Live OKX** to paper trade against OKX public market data. No exchange key is involved: orders are still simulated locally.

## BYOK providers

Add a provider in **Settings**, paste its key, then pick it when creating an agent. You can configure OpenAI-compatible, OpenAI, Anthropic, or Gemini providers. Keys remain only in server process memory: they are never stored in SQLite, logged, returned to the browser, or restored after restart. Model output is strictly validated and must pass server-side risk checks before the paper engine can accept an order.

## Current limits

- Paper trading only; `packages/exchanges` has interfaces but no live implementation.
- Linear USDT perpetual math only, with OKX as the first public market-data venue.
- Simplified fills, funding, and isolated liquidation; no order-book queue, partial liquidity, maintenance tiers, cross margin, or exchange-identical liquidation.
- API keys disappear on restart by design.

See [paper engine rules](docs/PAPER_ENGINE.md), [architecture](docs/ARCHITECTURE.md), and [roadmap](docs/ROADMAP.md).
