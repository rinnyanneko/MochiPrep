# Rust Accounting Core Implementation Plan (Plan 2 of 7)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port `packages/core` (decimal arithmetic, perp math, fill model, ledger, risk engine, paper execution engine) and the accounting half of the SQLite store to Rust, producing the same results, the same error messages and byte-identical database rows as the TypeScript engine.

**Architecture:** A new `mochi-decimal` crate reproduces decimal.js as configured in `packages/core/src/decimal.ts`. A new `mochi-core` crate holds the pure trading math, the risk engine, the `PaperStore` trait and the paper engine. `mochi-store` implements `PaperStore` over the existing tables. Parity is proven in three ways: golden corpora for pure functions; scripted scenarios that the TS generator runs through the real Node engine and SQLite store, recording each operation's result, its events and the raw table rows, which Rust must reproduce exactly; and a ledger audit after every scenario.

**Tech Stack:** Rust stable, num-bigint (plus num-integer and num-traits), rusqlite, serde/serde_json; TypeScript/tsx for the generator (Plan 1 harness).

**Spec:** `docs/superpowers/specs/2026-11-07-rust-backend-desktop-design.md`. This plan builds on `docs/superpowers/plans/2026-11-07-rust-foundation.md` (Plan 1, merged).

## Decision recorded here (deviates from a spec suggestion)

The spec *suggests* `rust_decimal`. It is not used. TS uses decimal.js with **precision 50, ROUND_HALF_EVEN**, and `rust_decimal` holds only 28–29 significant digits, so any inexact result (`notional / leverage`, `bps / 10000`) would print a different string. It also could not read the 50-digit values Node has already stored. Parity and existing data are binding; the crate choice was only a suggestion.

## Global Constraints

- Accounting never uses `f64`. Every money, quantity and price value is a `mochi_decimal::Dec`. Exceptions: `max_drawdown_pct` and `win_rate_pct` take and return `f64`, as TS uses JS numbers there for display only. Leverage and `fraction` are numbers in TS too, and enter decimals through `Dec::from_js_number`.
- `Dec` reproduces `D = Decimal.clone({ precision: 50, rounding: ROUND_HALF_EVEN, toExpNeg: -30, toExpPos: 40 })`. Arithmetic results are rounded to 50 significant digits, half-even. Parsing never rounds. `floor`/`ceil`/`round`/`to_decimal_places` round to an integer or a number of decimal places, not to a precision; `round` and `to_decimal_places` use half-even.
- Every wallet or margin change goes through the ledger, inside one store transaction. A rejected order rolls back everything except the single `REJECTED` order row.
- The paper engine never invents a price. Fills come from observed quotes only, and never from a quote more than 60 000 ms older than `now`.
- AI output is untrusted. `evaluate_risk` is pure: the same input gives the same verdict.
- Rows written by Rust must be byte-identical to the rows Node writes for the same operations: same columns, same decimal strings, booleans as 0/1, JSON columns (`config`, `live_limits`) with the same key order and JS number formatting (`2`, not `2.0`).
- Error messages and rejection reasons are part of the API (they reach the browser in Plan 3): copy the TS strings exactly.
- Readable Rust for a maintainer who "knows some Rust": mainstream crates, no custom macros, no clever generics, comments explain why. Any generic parameter is limited to `S: PaperStore`.
- The Node backend stays the default. Nothing under `apps/server/src` or `packages/*/src` changes behavior. Tests run offline.

## Review Focus

1. **Decimal edge strings.** `-0`, results with exactly 50 significant digits, half-even ties at the 50th digit, very small values (scientific notation in input), and values with more than 28 digits read back from a Node database must print exactly as decimal.js prints them. Covered by the Task 1 corpus.
2. **A rejected order midway through a transaction.** For example, insufficient margin after the position was read: no partial ledger rows and no margin change, only the `REJECTED` order. Covered by Task 6 scenarios.
3. **Two protective triggers on one quote.** The stop loss is evaluated before the take profit, and the sibling is cancelled. A gap past the liquidation price takes exactly the isolated margin. Covered by Task 7 scenarios.
4. **A JSON column written by Rust.** `accounts.config` with integral numbers must match Node byte for byte. Covered by Task 5.
5. **Reopening a file database.** The engine sees the same accounts, positions, orders and ledger as before the restart, and the ledger audit is clean. Covered by Task 8.

---

### Task 1: `mochi-decimal`, decimal.js-exact decimals

**Files:**
- Create: `crates/mochi-decimal/Cargo.toml`, `crates/mochi-decimal/src/lib.rs`, `crates/mochi-decimal/tests/golden_decimal.rs`
- Create: `crates/golden/decimal.json` (generated)
- Modify: `apps/server/golden/generate.ts`
- Commit first, on its own: the pending one-line edit to `docs/superpowers/plans/2026-11-07-rust-foundation.md` (the carried-forward note about the Unix permission tests). It is already in the working tree; commit it with the message `Update carried-forward note for the Unix permission tests`.

**Interfaces:**
- Produces (`mochi_decimal`): `#[derive(Clone)] pub struct Dec` (value = sign × coefficient × 10^exponent, with negative zero representable). Provide:
  - `pub fn parse(s: &str) -> Result<Dec, DecError>`. Accepts what decimal.js accepts from our data: an optional sign, digits, an optional fraction and an optional `e±n` exponent. Never rounds.
  - `pub fn from_js_number(x: f64) -> Dec`. Same value as `new Decimal(x)`: parse Rust's shortest round-trip `format!("{}", x)`.
  - `add`, `sub`, `mul`, `div(&self, &Dec) -> Result<Dec, DecError>` (division by zero is an error), and `neg`. All arithmetic rounds to 50 significant digits, half-even.
  - `floor`, `ceil`, `round` (to an integer, half-even), and `to_decimal_places(dp: u32)` (half-even).
  - `to_fixed(&self) -> String`, matching decimal.js `toFixed()`: plain notation, no exponent, no trailing zeros.
  - `to_fixed_dp(&self, dp: u32) -> String`, matching `toFixed(dp)` with half-even rounding.
  - `is_zero`, `is_negative`, `lt`, `lte`, `gt`, `gte`, `eq_value` (numeric equality, so `-0 == 0`), `min`, `max`.
  - `floor_to_step`, `ceil_to_step` and `round_to_step(&self, step: &Dec) -> Result<Dec, DecError>`, mirroring `decimal.ts`.
  - `impl Display` (as `to_fixed`), `impl FromStr`, and serde as a JSON string.
  - `pub enum DecError { Invalid, DivideByZero }`.

- [ ] **Step 1: Commit the carried doc edit** (see Files).
- [ ] **Step 2: Add the generator entry `"decimal.json"`.** Build a corpus from a seeded PRNG (reuse mulberry32 inline, seed `0x6d6f6368`) plus fixed edge cases, and record `{ op, a, b?, dp?, out }`. `out` is the decimal.js result string, or `"error"` where decimal.js throws or returns non-finite.
  - Ops: `add`, `sub`, `mul`, `div`, `neg`, `floor`, `ceil`, `round`, `toDP` (dp 0–10), `toFixed`, `toFixedDp` (dp 0–8), `cmp` (−1/0/1), `fromNumber` (a JS number), `floorToStep`, `ceilToStep`, `roundToStep`.
  - At least 400 random cases. Operands are random decimal strings with 1–30 integer digits, 0–30 fraction digits and a random sign, drawn so that some products and quotients exceed 50 significant digits.
  - Fixed cases: `"0"` and `"-0"` through every op; `1/3`, `2/3` and `-1/3`; a quotient whose 51st digit is exactly a tie; `"1e-7"`, `"5E+3"` and `"0.00000000000000000000000000000001"`; a 50-digit and a 60-digit input passed through `toFixed`, `neg` and `add "0"`; ties for `round` and `toDP` (`"2.5"`, `"3.5"`, `"-2.5"`, `"1.00000000005"` at dp 10); and `fromNumber` for `0.1`, `1e-7`, `0.0005`, `3`, `100` and `68000`.
  - Run `pnpm golden`.
- [ ] **Step 3: Write the failing test** `golden_decimal.rs::matches_decimal_js`. For every case, apply the op in Rust and `assert_eq!(rust_out, case.out, "{case:?}")`.
- [ ] **Step 4: Run `cargo test -p mochi-decimal`.** Expected: FAIL (not defined).
- [ ] **Step 5: Implement `Dec`** on `num_bigint::BigInt`. Division is the one algorithm the tests do not pin down, so do it like this:

```rust
// Correctly rounded a/b to 50 significant digits, half-even: scale the numerator so the integer
// quotient has at least 51 digits, divide with remainder, round the quotient's last digit
// half-even using the remainder (2r vs |b|), then trim to 50 digits, rounding once more only if
// the extra digit was produced by the scaling (keep the remainder as a sticky bit so this second
// rounding never double-rounds).
```

  Implement `add`, `sub` and `mul` exactly, then call one shared `round_to_precision(50)` helper. Keep `-0` wherever decimal.js does; the corpus says when. Where this plan's description of decimal.js disagrees with the corpus (for example on trailing zeros or the sign of zero), the corpus is right: it is decimal.js's actual output.
- [ ] **Step 6: Run `cargo test -p mochi-decimal`.** Expected: PASS. Then run `cargo clippy --workspace --all-targets -- -D warnings` and `pnpm test`.
- [ ] **Step 7: Commit** with the message `Add decimal.js-exact Dec for Rust accounting`.

### Task 2: Accounting domain types and the instrument registry

**Files:**
- Create: `crates/mochi-domain/src/accounting.rs`, `crates/mochi-domain/src/registry.rs`, `crates/mochi-domain/tests/golden_accounting.rs`, `crates/golden/accounting-types.json` (generated)
- Modify: `crates/mochi-domain/src/lib.rs`, `apps/server/golden/generate.ts`

**Interfaces:**
- Produces (`mochi_domain`): the Rust mirrors of these TS types from `packages/shared/src/domain.ts`, field for field:
  - `PaperAccount`, `PaperAccountConfig`, `AccountVenue`, `Order`, `OrderRequest`, `OrderResult`, `OrderSide`, `OrderType`, `OrderStatus`, `Fill`, `Liquidity`, `Position`, `PositionSide`, `PositionView`, `LedgerEvent`, `LedgerEventType`, `EquityPoint`, `AccountBalance`, `RiskSettings`, `AiDecisionData`, `RiskCheck`, `RiskResult`, `SizedTrade`.
  - Use camelCase serde. Decimal fields stay `String`, exactly as in TS, so these types can cross the API unchanged. Optional TS fields (`?`) are `Option` with `#[serde(skip_serializing_if = "Option::is_none")]`. `X | null` fields are `Option` serialized as `null`.
  - Constants `default_account_config()`, `default_risk_settings()` and `default_live_limits()`, with values copied from TS.
  - `pub fn js_number_json(x: f64) -> serde_json::Value`, which writes integral values without `.0`, as `JSON.stringify` does. Use it in custom `Serialize` impls for every `f64` field of `PaperAccountConfig` and `RiskSettings`.
- Produces (`mochi_domain::registry`): `pub struct Instruments`, with `with_defaults()`, `get(&self, symbol) -> Option<&InstrumentSpecification>`, `register(&mut self, spec)` and `unregister(&mut self, symbol)`. It replaces TS's global `INSTRUMENTS` and `getInstrument`. Callers pass it explicitly.

- [ ] **Step 1: Add the generator entry `"accounting-types.json"`.** Record:
  - `{ defaults: { accountConfig, riskSettings, liveLimits } }`.
  - `samples`: one hand-written fully-populated object and one minimal object for each type in the list, written as TS literals typed with the TS interfaces so tsc checks them.
  - `configJsonText: JSON.stringify(DEFAULT_ACCOUNT_CONFIG)` and `riskJsonText: JSON.stringify(DEFAULT_RISK_SETTINGS)`.
  - Run `pnpm golden`.
- [ ] **Step 2: Write the failing tests.**
  - `defaults_match_typescript`.
  - `samples_round_trip`: each sample deserializes into its Rust type and serializes back to the identical JSON value.
  - `config_json_text_matches`: `serde_json::to_string(&default_account_config())` equals `configJsonText` byte for byte, and the same for risk.
  - `registry_has_defaults_and_registers`.
- [ ] **Step 3: Run `cargo test -p mochi-domain`.** Expected: FAIL.
- [ ] **Step 4: Implement**, following the TS field order in each struct (serde writes declaration order).
- [ ] **Step 5: Run `cargo test --workspace` and `pnpm test`.** Expected: PASS.
- [ ] **Step 6: Commit** with the message `Add accounting domain types and instrument registry`.

### Task 3: Pure trading math in `mochi-core`

**Files:**
- Create: `crates/mochi-core/Cargo.toml`, `crates/mochi-core/src/lib.rs`, `crates/mochi-core/src/perp.rs`, `crates/mochi-core/src/fill_model.rs`, `crates/mochi-core/src/ledger.rs`, `crates/mochi-core/src/performance.rs`, `crates/mochi-core/tests/golden_math.rs`, `crates/golden/core-math.json` (generated)
- Modify: `apps/server/golden/generate.ts`

**Interfaces:**
- Consumes: `Dec` (Task 1) and the domain types (Task 2).
- Produces (`mochi_core`):
  - `perp::{notional, pnl, initial_margin, liquidation_price, funding}`, which mirror `linearPerp` and take and return `Dec`. `initial_margin(notional: &Dec, leverage: u32) -> Dec`. `perp::math_for(spec) -> Result<(), CoreError>` rejects a non-`CRYPTO_PERP` market type with `No calculation model for <type>`.
  - `fill_model::{market_fill_price, is_marketable, is_resting_limit_filled, is_stop_triggered, is_take_profit_triggered}`.
  - `ledger::{fold_ledger(&[LedgerEvent]) -> LedgerTotals, audit_ledger(&PaperAccount, &[LedgerEvent]) -> Vec<String>}`, with the message formats copied from TS.
  - `performance::{max_drawdown_pct(&[f64]) -> f64, win_rate_pct(&[f64]) -> Option<f64>}`.

- [ ] **Step 1: Add the generator entry `"core-math.json"`.**
  - Every assertion input from the first two tests of `packages/core/test/perp.test.ts`, with its TS result.
  - A seeded corpus of at least 100 cases for each perp function and each fill-model function, built from the default instruments and `DEFAULT_ACCOUNT_CONFIG`.
  - Three ledger event streams: consistent, wallet off by `0.00000001`, and `walletAfter` broken at the event with seq 3. Each records its fold and audit output.
  - `max_drawdown_pct` and `win_rate_pct` cases, including an empty list and a list with a peak of 0.
  - Run `pnpm golden`.
- [ ] **Step 2: Write the failing test** `golden_math.rs` (one `#[test]` per function family), comparing with the strings and numbers in the fixture.
- [ ] **Step 3: Run `cargo test -p mochi-core`.** Expected: FAIL.
- [ ] **Step 4: Implement by porting** `packages/core/src/trading/perp.ts`, `trading/fill-model.ts`, `accounting/ledger.ts` and `analytics/performance.ts`. Keep the operation order: decimal rounding happens per operation, so `a.mul(b).div(c)` is not `a.div(c).mul(b)`.
- [ ] **Step 5: Run `cargo test --workspace`.** Expected: PASS.
- [ ] **Step 6: Commit** with the message `Port perp math, fill model and ledger audit to Rust`.

### Task 4: Risk engine

**Files:**
- Create: `crates/mochi-core/src/risk.rs`, `crates/mochi-core/tests/golden_risk.rs`, `crates/golden/risk.json` (generated)
- Modify: `crates/mochi-core/src/lib.rs`, `apps/server/golden/generate.ts`

**Interfaces:**
- Consumes: Tasks 1–3, and `Instruments` in place of `getInstrument`.
- Produces: `pub struct RiskInput { decision, symbol, settings, quote: Option<Quote>, now: i64, balance, open_positions, daily_pnl: String, day_start_equity: String, fees: PaperAccountConfig }` (serde camelCase, so fixtures deserialize straight into it), and `pub fn evaluate_risk(input: &RiskInput, instruments: &Instruments) -> RiskResult`.

- [ ] **Step 1: Add the generator entry `"risk.json"`.** Record a list of `{ input, output }`, where `output = evaluateRisk(input)`. Include:
  - every input used in the risk section of `packages/core/test/perp.test.ts`, including all `it.each` rows;
  - for every check `id` that `evaluateRisk` can emit, one case where it passes and one where it fails;
  - a case with `quote: null`;
  - a HOLD decision and a CLOSE decision;
  - a case with no stop loss where settings allow it;
  - a stop beyond the liquidation price.

  Read `packages/core/src/risk/engine.ts` for the check list; there are ~200 lines. Run `pnpm golden`.
- [ ] **Step 2: Write the failing test** `golden_risk.rs::verdicts_match_typescript`: for each case, `serde_json::to_value(evaluate_risk(&input, &Instruments::with_defaults()))` equals `output`, including every check's `detail` string and the order of `checks`.
- [ ] **Step 3: Run `cargo test -p mochi-core risk`.** Expected: FAIL.
- [ ] **Step 4: Implement by porting** `risk/engine.ts`. `MARGIN_SAFETY = "0.98"` and `ASSUMED_STOP_FRACTION = "0.02"`. Copy every label and detail string exactly.
- [ ] **Step 5: Run `cargo test --workspace`.** Expected: PASS.
- [ ] **Step 6: Commit** with the message `Port the risk engine to Rust`.

### Task 5: `PaperStore` trait and its SQLite implementation

**Files:**
- Create: `crates/mochi-core/src/paper/store.rs`, `crates/mochi-store/src/paper.rs`, `crates/mochi-store/tests/paper_store.rs`, `crates/golden/store-rows.json` (generated)
- Modify: `crates/mochi-core/src/lib.rs`, `crates/mochi-store/src/lib.rs`, `crates/mochi-store/Cargo.toml` (depends on `mochi-core`, `mochi-domain`), `crates/mochi-store/tests/migrations.rs`, `apps/server/golden/generate.ts`

**Interfaces:**
- Produces (`mochi_core::paper::PaperStore`): a trait whose methods mirror `packages/core/src/paper/store.ts` one for one, in snake_case. All are `&self`. Methods that can fail return `Result<_, StoreFailure>`, where `pub struct StoreFailure(pub String)`. Transactions take a closure:
  - `fn transaction<T, E: From<StoreFailure>>(&self, f: impl FnOnce() -> Result<T, E>) -> Result<T, E>`. On `Err` it rolls back. When nested, it uses a savepoint, as the TS `SqliteStore` does.
- Produces (`mochi_store::SqlitePaperStore`): `pub fn new(conn: rusqlite::Connection) -> Self` and `pub fn connection(&self) -> &Connection`. It implements `PaperStore`, using the same SQL, columns and row converters as `apps/server/src/db/store.ts`. Read that file and port only the accounting tables: accounts, orders, fills, positions, ledger_events and equity_history.

- [ ] **Step 1: Add the generator entry `"store-rows.json"`.**
  - Open `openDatabase(':memory:')` with the Node `SqliteStore` and insert the full and minimal samples from `accounting-types.json` (one account, two orders, two fills, one open and one closed position, three ledger events, two equity points).
  - Record `rows: { [table]: SELECT * ORDER BY rowid }` for the six tables, using `SELECT *` so every column and its stored type and text are captured.
  - Record `reads`: the results of `getAccount`, `listOpenOrders`, `getOpenPosition`, `listLedger` and `listEquity`.
  - Run `pnpm golden`.
- [ ] **Step 2: Write the failing tests** in `paper_store.rs`:
  - `writes_the_same_rows_as_node`: same inserts on `open_in_memory()`, and raw rows equal `rows` exactly. Compare `rusqlite::types::Value` mapped to JSON, so integer versus text type matters.
  - `reads_back_what_node_reads`.
  - `transaction_rolls_back_on_error`.
  - `nested_transaction_rolls_back_only_inner`.
- [ ] **Step 3: Carried from Plan 1:** in `migrations.rs::failed_migration_leaves_nothing`, also assert `conn.is_autocommit()`.
- [ ] **Step 4: Run `cargo test -p mochi-store`.** Expected: the new tests FAIL.
- [ ] **Step 5: Implement.** JSON columns use `serde_json::to_string` on types from Task 2, which already use JS number formatting. Booleans are stored as `0`/`1`.
- [ ] **Step 6: Run `cargo test --workspace` and `pnpm test`.** Expected: PASS.
- [ ] **Step 7: Commit** with the message `Add PaperStore trait and its SQLite implementation`.

### Task 6: Scenario harness and the paper engine, order path

**Files:**
- Create: `apps/server/golden/paper-scenarios.ts`, `crates/mochi-core/src/paper/engine.rs`, `crates/mochi-core/src/paper/mod.rs`, `crates/mochi-store/tests/paper_scenarios.rs`, `crates/golden/paper-scenarios.json` (generated)
- Modify: `apps/server/golden/generate.ts`, `crates/mochi-core/src/lib.rs`

**Interfaces:**
- Scenario format (TS exports it, the fixture carries it, and Rust reads it):

```ts
type Op =
  | { op: 'quote'; quote: Quote; feed: boolean }           // set the latest quote; feed=true also calls engine.onQuote
  | { op: 'now'; source: DataSource; ts: number }
  | { op: 'createAccount'; args: { name; source; initialBalance?; isDemo?; config? } }
  | { op: 'resetAccount'; accountId: string }
  | { op: 'placeOrder'; request: OrderRequest }
  | { op: 'cancelOrder'; orderId: string }
  | { op: 'closePosition'; accountId; symbol; fraction: number; clientOrderId }
  | { op: 'setProtection'; accountId; symbol; stopLoss: string | null; takeProfit: string | null }
  | { op: 'recordEquity'; accountId: string }
  | { op: 'read'; what: 'balance' | 'positions' | 'openOrders' | 'dailyRisk'; accountId: string };
interface Scenario { name: string; snapshot: 'every' | 'end'; ops: Op[] }
// Recorded per op: { result?: unknown; error?: string; events: EngineEvent[]; rows?: Record<table, row[]> }
// After every scenario: audit = auditLedger(account, listLedger(account)) for each account.
```

  IDs are deterministic: `newId = (prefix) => \`${prefix}_${++counter}\``, with the counter reset per scenario. Quotes and `now` come only from ops. Run against `openDatabase(':memory:')` plus the Node `SqliteStore` plus `PaperExecutionEngine`.
- Produces (`mochi_core::paper::PaperExecutionEngine<S: PaperStore>`):
  - `pub fn new(deps: PaperEngineDeps<S>) -> Self`, where `PaperEngineDeps { store: S, instruments: Instruments, get_quote: Box<dyn Fn(DataSource, &str) -> Option<Quote>>, now: Box<dyn Fn(DataSource) -> i64>, new_id: Box<dyn FnMut(&str) -> String>, on_event: Box<dyn FnMut(EngineEvent)> }`.
  - `create_account`, `reset_account`, `place_order(&mut self, OrderRequest) -> Result<OrderResult, EngineError>`, `cancel_order`, `close_position(account_id, symbol, fraction: f64, client_order_id)`, `set_protection`.
  - `pub enum EngineEvent` mirrors the TS union, with serde `{ kind, ... }` so events compare as JSON.
  - `pub enum EngineError` has variants `Invalid(String)` (an Error thrown in TS) and `Store(StoreFailure)`. Its `Display` gives the exact TS message. `OrderRejection` is internal, and as in TS it becomes a `REJECTED` order with `rejectReason = "<CODE>: <detail>"`.
  - The TS async wrappers and the `ExecutionEngine` interface are not ported here. Plan 5 defines the shared trait, once the async OKX engine exists.

- [ ] **Step 1: Write `paper-scenarios.ts`** with one scenario for each `apps/server/test/paper-engine.test.ts` test that does not need `onQuote`, using that test's values. These are:
  - charges entry and exit taker fees
  - partial close
  - idempotent by clientOrderId
  - insufficient margin rolls back
  - reserves a resting limit and releases it on cancel (the fill part goes to Task 7)
  - marketable limit
  - caps reduce-only and rejects it when flat
  - flip
  - refuses a quote older than a minute

  Add these error-path scenarios:
  - cancel an unknown order
  - cancel a filled order
  - `setProtection` with no position, with no quote, and with a stop on the wrong side
  - leverage 0 and 101
  - quantity below minimum
  - a limit price that rounds to zero

  Add `"paper-scenarios.json"` to the generator (it runs every scenario and records per the format). Run `pnpm golden`.
- [ ] **Step 2: Write the failing runner** `paper_scenarios.rs::scenarios_match_typescript`. For each scenario, build the engine on `SqlitePaperStore::new(open_in_memory())`, replay the ops, and assert per op: `result` as JSON, `error` string, `events` as JSON, and `rows` when recorded. Then assert `audit` is empty for every account. On mismatch, print the scenario name, op index and first differing field.
- [ ] **Step 3: Run `cargo test -p mochi-store --test paper_scenarios`.** Expected: FAIL.
- [ ] **Step 4: Implement by porting** `packages/core/src/paper/engine.ts`: accounts, `placeOrderSync`, `executeRequest`, `validateProtection`, `fillOrder`, `refreshLiquidation`, `post`, `replaceProtection`, `cancelProtection`, `closeOrder`, `availableOf` and `flush`. Read `docs/PAPER_ENGINE.md` for the rules. `MAX_FILL_QUOTE_AGE_MS = 60_000`. `money(v)` is `to_decimal_places(8)`. `Math.round((now - quote.ts) / 1000)` in the stale message uses `mochi_domain::jsnum::js_round`. Events are queued inside a transaction and flushed after commit, and dropped on rollback, exactly as TS does.
- [ ] **Step 5: Run `cargo test --workspace` and `pnpm test`.** Expected: PASS.
- [ ] **Step 6: Commit** with the message `Port the paper engine order path with scenario parity`.

### Task 7: Paper engine, market events and read models

**Files:**
- Modify: `crates/mochi-core/src/paper/engine.rs`, `apps/server/golden/paper-scenarios.ts`, `crates/golden/paper-scenarios.json` (regenerated)

**Interfaces:**
- Produces: `on_quote(&mut self, quote: &Quote)`, `record_equity(&mut self, account_id)`, `balance_of(account_id) -> Result<AccountBalance, EngineError>`, `position_views(account_id)`, `open_orders(account_id)`, `daily_risk(account_id) -> Result<(String, String), EngineError>` (daily PnL, day-start equity).

- [ ] **Step 1: Add scenarios** for every remaining test in `paper-engine.test.ts`:
  - the resting limit fills as maker at its limit
  - maker rate on a resting reduce-only exit
  - protection triggers at the next quote and cancels its sibling
  - stop loss evaluated before take profit on the same quote
  - both funding tests
  - all four liquidation tests
  - short stop at the ask
  - short take profit
  - averaging, then a stop closes the whole enlarged position
  - `keeps the ledger auditable through a fixed pseudo-random trade sequence`, with `snapshot: 'end'`

  Add `read` ops after the key steps (balance, positions, dailyRisk) and `recordEquity` ops at two different `now` values on the same day, plus one across a UTC day boundary. Run `pnpm golden`.
- [ ] **Step 2: Run the runner.** Expected: the new scenarios FAIL.
- [ ] **Step 3: Implement by porting** `onQuote`, `applyFunding`, `liquidate`, `evaluateOrder`, `fillResting`, `balanceOf`, `positionViews`, `dailyRiskSync` and `recordEquity`. Port these in the TS order, which matters:
  - Quote-driven checks per position follow the order in `onQuote`.
  - Stop loss comes before take profit.
  - The liquidation penalty and the gap cap follow `liquidate`.
- [ ] **Step 4: Run `cargo test --workspace` and `pnpm test`.** Expected: PASS.
- [ ] **Step 5: Commit** with the message `Port quote-driven paper engine events and read models`.

### Task 8: Reopen recovery and wrap-up

**Files:**
- Create: `crates/mochi-store/tests/paper_recovery.rs`
- Modify: `AGENTS.md` (Rust section: list `mochi-decimal` and `mochi-core`, and add the rule "Rust accounting uses `mochi_decimal::Dec`, never `f64` or `rust_decimal`"), `docs/ARCHITECTURE.md` (Rust paragraph: state what now exists, and that the Rust backend still cannot run)

- [ ] **Step 1: Write the test** `paper_recovery.rs::reopened_database_restores_engine_state`, mirroring the paper part of `apps/server/test/recovery.test.ts`:
  1. On a file database in a tempdir, open the engine and run a few orders, including an open position, a resting limit and a stop.
  2. Drop the engine and the connection, then reopen with `open_database`.
  3. Assert that the account, positions, open orders and ledger equal what they were before the drop, and that the ledger audit is empty.
  4. Feed one quote that triggers the stop, and assert that it fills.
- [ ] **Step 2: Run it.** Expected: PASS. It ports existing behavior, so if it fails, fix the code, not the test.
- [ ] **Step 3: Update the docs** listed in Files.
- [ ] **Step 4: Full verification:** `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `pnpm test`, `pnpm typecheck`. Expected: all clean.
- [ ] **Step 5: Commit** with the message `Add paper recovery test and document the Rust accounting core`.

---

## Carried forward from Plan 2 reviews

Real findings that nothing in Plan 2 builds on. The plan named for each must pick it up. Rows marked done say where; the rows after the Plan 2 ones were added by Plan 3 (Rust server and feeds).

| Plan | Item |
|---|---|
| 3 (done: Plan 3 Task 1, `d40f2bb`, `d17713f`) | Split `engine.rs` into impl-block modules (`types.rs` for events, errors, deps and helpers; `fills.rs` for `fill_order`, posting and `refresh_liquidation`; `protection.rs` for replace/cancel protection and `close_order`) as a pure move in Plan 3's first commit, verified by the unchanged scenario suite |
| 3 (done: Plan 3 Task 4, `361b3b5`; `paper_carried.rs::on_quote_has_no_quote_age_check`) | TS `onQuote` applies no quote-age check. The 60 000 ms rule for quote-driven fills rests on the Runtime/feed and must stay identical |
| 3 (done: Plan 3 Task 4, `361b3b5`) | Settle `on_quote` event semantics on error (flush per committed step, drop on rollback), consistent with `set_protection` |
| 3 (done: Plan 3 Task 4, `361b3b5`; `paper_carried.rs::every_engine_write_is_inside_a_transaction`) | All writes go through `PaperStore::transaction` or a shared depth. `SqlitePaperStore::connection()` must not be used to `BEGIN` |
| 3 (done: Plan 3 Task 4, `361b3b5`; `StoreFailureKind`) | Keep the rusqlite error kind in `StoreFailure` (BUSY/retry detection) |
| 3 (done: Plan 3 Task 6, `4f5ca49`; documented in `mochi-server/src/routes/orders.rs`) | `Dec::parse` deliberately rejects `0x…`/Infinity/NaN and caps exponents. Document this where untrusted strings first reach `Dec`, and accept leading-zero exponents |
| 3 (done: Plan 3 Task 3, `716f787`, and Task 6, `4f5ca49`; `golden_validation.rs`, `api-script.json`) | `OrderRequest` explicit-null inputs and byte-level API JSON goldens |
| 4 | Risk `now - quote.ts` should use `checked_sub`. Reject non-finite `RiskInput` numbers up front, mapped to `input`. The zero-ask unit test should reach the /0 path. Comment `expiresAt ""` |
| 4 | Decide the recorded outcome label when the risk engine returns `input` (TS throws, so the runner records an error) |
| 4 | `AiDecisionData` goes through the JS number serializers when stored |
| 5 | Rust `scrub` now returns `CheckpointBlocked` when a reader stops the WAL truncation (CodeRabbit). Node's `store.scrub()` still ignores the busy flag. Decide how `enableVault`/sealing reacts (retry, report) and make Node and Rust agree |
| 5 | Exotic JS numbers in `live_limits`/config, covered when live limits become editable (still open: Plan 3 only reads live limits; their writes answer 501) |
| 3/4 (done: Plan 3 Task 6, `4f5ca49`) | Rust `ProtectionResult` lacks the new `pending` field (paper is always false; TS has it). Add it when the Rust runtime/runner exists |
| 4 | Port the runner `PROTECT` path, the SIGNAL snapshot rule and `buildSystemPrompt({ allowTrailingStop })`. Also port `ProtectionChangeError` handling (partial apply becomes ORDER_REJECTED with `protectionAfter`) and the "awaiting confirmation" ORDER_PLACED outcome |
| 5 | Port `classifyProtectionChange` (add a golden), the OKX algo client and engine protection (place-before-cancel, `algoClOrdId` matching, external orders, sync cleanup) |
| 5 | OKX protection reconciliation rules: place all legs before cancelling any, `PROTECTION_PENDING` refusal, no-position cleanup only after at least 2 missed syncs and at least 10 s, stray "cancel pending" visibility and retry, the re-adopt rule, one `attachAlgoOrds` element per leg with `attachAlgoClOrdId`. Plus the result of the owner's Demo test of the seven UNCONFIRMED behaviours in `docs/OKX_TRADING.md` |
| 5 | OKX side/position matching: a protection row or stray belongs to the held position only if its side closes it (SELL a long, BUY a short); others are cancelled (`Position closed`), never put in force or re-adopted, and a row closed as `Position closed` is never re-adopted. An opening order on a symbol with open protection rows, or live leftovers from the last sync, and no position is refused with `PROTECTION_PENDING: protection from the previous position is still being cleaned up`; reduce-only orders are unaffected |
| server | `PUT` protection route semantics: an absent `trailingStop` keeps the current one, open to OKX accounts through the protection gate, and the response is `{ ok, pending, before, after }` |
| server | `allowTrailingStop` defaults to `false` in `createAgent` and `AgentInputSchema` |
| server | `assertCanExecuteSignal` exempts `PROTECT` as well as `CLOSE` from the arm requirement; the OKX protection gate still refuses `NEEDS_ARMED` changes while disarmed |
| 4 | Runner HOLD-on-no-change: after `setProtection`, when nothing is pending and `before` equals `after` (decimal comparison), record `HOLD` `Protection already as requested` instead of `EXECUTED` |
| any | Cleanup: `sub_exact` duplication, `zero()` re-parse, `side_text`/`status_text`/`liquidity_text` duplication, `queued` mutated from `market.rs`, `DAY_MS`, `first_difference` on arrays, `last_funding_time` comment, `query_one`, split `risk::run_checks` (Sonar cognitive complexity 32 > 30; extract the sizing section, keeping check order and detail strings, which the risk golden pins) |
| 4 | Pending-suggestion expiry: Node's runner tick expires SIGNAL suggestions. Until the Rust runner exists, Rust `GET /api/signals` can list a suggestion Node would already have expired |
| 4 | Load provider keys and extra headers into the runtime's redaction secrets list, as Node does. Plan 3 leaves it empty because no Plan 3 path handles them |
| 4 | Roadmap change: demo routes (`/api/demo/start`, `reset`, `exit`) answer 501 in Plan 3 because demo start creates agents. Port them with the agents, and make `e2e/demo.spec.ts` against the Rust server (`pnpm test:e2e:rust`) Plan 4's proof |
| 4 | `PUT`/`DELETE /api/agents/stop-all` 404 body may differ from Node; 501 writes answer 501 even for unknown ids. Settle both when the agent routes are ported |
| launcher | Neither backend handles Ctrl-Break on Windows (Node listens for SIGINT/SIGTERM; Rust for Ctrl-C/SIGTERM), so Ctrl-Break kills the process without closing the database. Whatever stops the server (the launcher, a service) must send Ctrl-C or SIGTERM, or both backends must learn another stop signal |
| any | Public-feed handshake timeout (Plan 3 CodeRabbit finding): neither backend bounds the initial WebSocket dial/handshake. A hung handshake can leave the feed CONNECTING indefinitely, although REST polling continues. Add a bounded handshake and a stalled-handshake/redial test to both backends together; deferred from Plan 3 to preserve the agreed Node behaviour |
| any | SECURITY, needs an owner decision: `validateFeedUrl` (Node) and its Rust port are text-only, so `localhost.` and names that resolve to private IPs (e.g. `127.0.0.1.nip.io`) pass the private-host check, an SSRF gap in both backends by parity. Whoever owns news hardening should decide whether to resolve and check addresses (on every redirect hop), and change both backends together |
| any | Plan 3 cleanup: `validate.rs` (~1,100 lines), `news.rs` (~1,590; the `Date.parse` port could be `js_date.rs`) and `okx_public.rs` (~860; transport could split) are large; `href_attr` is O(n^2) on many unterminated `<link` (mirrors V8); `begin_batch_for_test` is `pub #[doc(hidden)]`; `AppState.web` is public; the e2e Rust binary path ignores `CARGO_TARGET_DIR`, and `e2e/run-rust.mjs` (`shell: true`) does not quote arguments with spaces |
