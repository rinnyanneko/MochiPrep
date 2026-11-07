# Roadmap

MochiPrep is paper trading only today. Milestones are intentionally ordered to avoid implying unsupported venue or asset coverage.

1. Harden the existing OKX public-data and paper-trading release with deterministic tests, recovery tests, and UI E2E coverage.
2. Add an **OKX live trading adapter** behind an explicit, default-off feature gate.
3. Add other crypto venues, beginning with Bybit and Binance, only after the OKX live path is proven.
4. Package the local application with Tauri.
5. Add separate calculation and execution models for FX/CFD; do not reuse perpetual assumptions.

Before any live adapter can ship, all of these are release blockers:

- live trading is disabled by default and requires an unmistakable opt-in;
- API credentials with withdrawal permission are rejected;
- every order passes the same server-side risk engine immediately before submission;
- client and venue idempotency prevent duplicate submission and safely reconcile ambiguous responses;
- reconnect performs a full venue reconciliation of orders, fills, positions, and balances before trading resumes;
- kill switch, bounded retries, audit logs, sandbox verification, and independent E2E tests are complete.

Roadmap items are plans, not implemented features.
