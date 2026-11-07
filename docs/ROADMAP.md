# Roadmap

MochiPrep trades on paper and through OKX. Milestones are intentionally ordered to avoid implying unsupported venue or asset coverage.

1. Prove the OKX adapter on OKX Demo Trading, then with small real orders. See the checklist in [Trading on OKX](OKX_TRADING.md).
2. Harden it: WebSocket order updates instead of polling, editing stops on open positions, and order-book-aware limits.
3. Add other crypto venues, beginning with Bybit and Binance, only after the OKX live path is proven.
4. Package the local application with Tauri.
5. Add separate calculation and execution models for FX/CFD; do not reuse perpetual assumptions.

Every exchange adapter must meet all of these. The OKX adapter implements them, but has only been tested against a simulated exchange:

- live trading is disabled by default and requires an unmistakable opt-in;
- API credentials with withdrawal permission are rejected;
- every order passes the same server-side risk engine immediately before submission;
- client and venue idempotency prevent duplicate submission and safely reconcile ambiguous responses;
- reconnect performs a full venue reconciliation of orders, fills, positions, and balances before trading resumes;
- kill switch, bounded retries, audit logs, sandbox verification, and independent E2E tests are complete.

Roadmap items are plans, not implemented features.
