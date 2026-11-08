# Trailing Stop and Adjustable Protection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Agents (when their per-agent switch is on) can attach trailing stops, and agents and the owner can adjust stop-loss, take-profit and trailing stops on open positions, on paper, OKX Demo and OKX live accounts, with the Rust paper and risk engines kept in parity.

**Architecture:** Protection becomes one value, `Protection { stopLoss, takeProfit, trailingStop }`, that every engine sets in full through a new `ExecutionEngine.setProtection`. Paper simulates a new `TRAILING_STOP_MARKET` order from observed `quote.last`. OKX holds real algo orders, which MochiPrep places before it cancels the old ones and identifies by its own `algoClOrdId`. The agent gets a `PROTECT` action and an optional `trailingStop` field. The risk engine checks both, and the Rust risk and paper engines mirror them through golden fixtures.

**Tech Stack:** TypeScript (Node ≥ 22.13, Zod, vitest, React), Rust (`mochi-domain`, `mochi-core`, `mochi-store`), SQLite, OKX v5 REST.

**Spec:** `docs/superpowers/specs/2026-11-08-trailing-stop-design.md` (Traditional Chinese). Read it first. Its "擁有者對待決事項的回答" section is binding.

## Global Constraints

- Follow every rule in `AGENTS.md`. In particular:
  - Accounting uses `D` (`packages/core/src/decimal.ts`) and Rust uses `mochi_decimal::Dec`, never JS number or `f64` money arithmetic.
  - AI output is untrusted. Only `AgentRunner` turns it into orders or protection changes, and only after Zod and the risk engine.
  - Paper fills come from observed quotes only.
  - Never resend after an unknown OKX outcome: resolve by lookup.
  - Every OKX write passes `preTradeRefusal` (or the protection gate in Task 8).
  - Exits stay possible when disarmed.
- A TypeScript change that alters a golden file regenerates it with `pnpm golden` in the same commit, and the Rust tests pass in that commit.
- Migration `apps/server/src/db/migrations/008_trailing_stop.sql` is the only new migration. Rust embeds it through `MIGRATIONS` in `crates/mochi-store/src/lib.rs`.
- Trailing limits, shared by risk and engine validation: `TRAILING_LIMITS = { minRatio: '0.001', maxRatio: '0.3', maxSpreadFraction: '0.3' }` in `packages/shared/src/domain.ts`, mirrored as constants in `mochi-domain`.
- `agents.allow_trailing_stop` defaults to `0`. Existing agents behave exactly as before.
- Do not claim live trailing stops are verified. `docs/OKX_TRADING.md` lists the six UNCONFIRMED OKX behaviours from the spec.
- Every task runs `pnpm test`, `pnpm typecheck`, `cargo test --workspace`, `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` before committing. All must pass.
- Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- On Windows, a `LNK1104` link error is a transient file lock: retry the cargo command. It is not a failure.

## Shared vocabulary (defined in Task 1, used everywhere)

```ts
// packages/shared/src/domain.ts
export type TrailingKind = 'RATIO' | 'SPREAD';
export interface TrailingStopSpec { kind: TrailingKind; callback: string; activationPrice: string | null }
export interface Protection { stopLoss: string | null; takeProfit: string | null; trailingStop: TrailingStopSpec | null }
export interface TrailingStopView extends TrailingStopSpec {
  /** Highest (long) or lowest (short) last price since activation; null until active. */
  extreme: string | null;
  /** Current trigger price; null until active. */
  triggerPrice: string | null;
}
export interface ExternalProtection { kind: 'STOP_LOSS' | 'TAKE_PROFIT' | 'TRAILING'; triggerPrice: string | null; detail: string }
// OrderType gains 'TRAILING_STOP_MARKET'.
// Order gains: trailingKind: TrailingKind | null; trailingCallback: string | null;
//   trailingActivationPrice: string | null; trailingExtreme: string | null;
//   attachedTrailingStop: TrailingStopSpec | null.
// OrderRequest gains trailingStop?: TrailingStopSpec | null.
// PositionView gains trailingStop: TrailingStopView | null; externalProtection: ExternalProtection[].
// Agent gains allowTrailingStop: boolean.
```

Rust mirrors these in `crates/mochi-domain/src/accounting.rs` with serde camelCase names, as the existing types do.

## Review Focus

1. **Gap through a trailing trigger:** the quote jumps far past the trigger price. The position must fill at that quote's slipped bid or ask, never at the trigger price (Task 2 scenario `trailing-gap`).
2. **Restart while trailing:** the server stops after the extreme moved. On restart the stored `trailing_extreme` must be used and must not reset to the next quote (Task 2 Node recovery test, Task 3 Rust recovery test).
3. **Adding to a position that has a trailing stop:** the trailing stop must keep its extreme and close the whole new quantity (Task 2 scenario `trailing-survives-add`).
4. **Agent switch turned off while a trailing stop exists:** a `PROTECT` without `trailingStop` keeps it, and any `trailingStop` value, `null` included, is rejected (Task 4 risk cases, Task 6 runner test).
5. **Unknown outcome placing a replacement OKX algo:** the old stop stays, nothing is resent, and sync resolves the new one by `algoClOrdId` (Task 8 test).

---

### Task 1: Data model, migration and golden shapes (Node + Rust)

**Files:**
- Modify: `packages/shared/src/domain.ts`, `packages/shared/src/schemas.ts`
- Create: `apps/server/src/db/migrations/008_trailing_stop.sql`
- Modify: `apps/server/src/db/store.ts` (`toOrder`, `insertOrder`, the order update, `toAgent`, `insertAgent`, `saveAgent`)
- Modify: `crates/mochi-domain/src/accounting.rs`, `crates/mochi-store/src/lib.rs` (`MIGRATIONS`), `crates/mochi-store/src/paper.rs` (order row mapping)
- Modify: every place that builds an `Order` or `PositionView` literal (TS: `packages/core/src/paper/engine.ts`, `packages/exchanges/src/okx-engine.ts`; Rust: `crates/mochi-core/src/paper/*.rs`) to fill the new fields with `null` / `[]`
- Regenerate: `crates/golden/schema.json`, `store-rows.json`, `accounting-types.json`, `paper-scenarios.json`

**Interfaces:**
- Produces: the shared vocabulary above. Also `TRAILING_LIMITS`, plus Zod `TrailingStopSchema = z.strictObject({ kind: z.enum(['RATIO','SPREAD']), callback: decimalString, activationPrice: decimalString.nullable() })`. `ProtectionSchema` gains `trailingStop: TrailingStopSchema.nullable()` and `PlaceOrderSchema` gains `trailingStop: TrailingStopSchema.nullish()`. `AgentInputSchema` gains `allowTrailingStop: z.boolean().default(false)`.
- Migration 008: `ALTER TABLE orders ADD COLUMN trailing_kind TEXT; … trailing_callback TEXT; … trailing_activation_price TEXT; … trailing_extreme TEXT; … attached_trailing_stop TEXT;` (JSON or NULL) and `ALTER TABLE agents ADD COLUMN allow_trailing_stop INTEGER NOT NULL DEFAULT 0;`.

- [ ] **Step 1:** Add the types, schemas and migration, and map the columns in `store.ts`. `attached_trailing_stop` is stored as `JSON.stringify(spec)` or `NULL`.
- [ ] **Step 2:** Add a vitest in `apps/server/test/paper-engine.test.ts`, `stores and reads the trailing fields of an order`: insert an order with every trailing field set, read it back, and assert deep equality. Also extend an agent round-trip test (or add one) to assert `allowTrailingStop` survives save and load and defaults to `false`.
- [ ] **Step 3:** Mirror the types in Rust: `OrderType::TrailingStopMarket` (`"TRAILING_STOP_MARKET"`), `TrailingKind`, `TrailingStopSpec`, `TrailingStopView`, `ExternalProtection`, `Protection`, the new `Order` and `PositionView` fields and `TRAILING_LIMITS`. Add `008_trailing_stop.sql` to `MIGRATIONS` and map the new order columns in `SqlitePaperStore`.
- [ ] **Step 4:** Run `pnpm golden`. Check with `git diff --stat crates/golden` that only `schema.json`, `store-rows.json`, `accounting-types.json` and `paper-scenarios.json` changed, and that the `paper-scenarios.json` changes are only the new null fields.
- [ ] **Step 5:** Run every command in Global Constraints. Expected: all pass, including `fresh_schema_matches_node`, `migration_list_matches_directory`, `scenarios_match_typescript` and `apps/server/test/golden.test.ts`.
- [ ] **Step 6:** Commit, with a subject like `Add the trailing stop data model and migration 008`.

### Task 2: Paper engine trailing stops and full-protection `setProtection` (Node)

**Files:**
- Modify: `packages/core/src/paper/engine.ts`, `packages/core/src/trading/fill-model.ts`, `packages/core/src/trading/execution.ts`
- Create: `packages/core/src/trading/trailing.ts`
- Modify: `apps/server/golden/paper-scenarios.ts`, `apps/server/golden/generate.ts`, `apps/server/src/app.ts` (paper protection route body), `apps/server/src/runtime.ts` (callers of `setProtection`), `packages/exchanges/src/okx-engine.ts` (interface stubs, see below), `crates/mochi-store/tests/paper_scenarios.rs` (`setProtection` op reads `op.protection`)
- Create: `crates/golden/paper-trailing-scenarios.json` (generated)
- Modify: `docs/PAPER_ENGINE.md`
- Test: `apps/server/test/paper-engine.test.ts`, `apps/server/test/recovery.test.ts`

**Interfaces:**
- Consumes: Task 1 types.
- Produces:
  - `trailing.ts`:
    - `trailingTrigger(side: PositionSide, spec: TrailingStopSpec, extreme: Dec, tick: Dec): Dec`. For a long, `extreme × (1 − ratio)` or `extreme − spread`, floored to the tick. For a short, `extreme × (1 + ratio)` or `extreme + spread`, ceiled to the tick. Rounding goes against the trader.
    - `advanceExtreme(side, spec, current: string | null, last: Dec): string | null`. Activation is long when `last ≥ activationPrice` and short when `last ≤ activationPrice`. Without an activation price it activates immediately. Once active, the extreme moves to `max` (long) or `min` (short) of the current extreme and `last`.
    - `validateTrailing(side, reference: Dec, spec): string | null`. It returns the rejection text or null and uses `TRAILING_LIMITS`:
      - A `RATIO` callback must be in `[minRatio, maxRatio]`.
      - A `SPREAD` callback must be at least one tick and at most `reference × maxSpreadFraction`.
      - An activation price must be on the profitable side: long `> reference`, short `< reference`.
  - `ExecutionEngine.setProtection(accountId: string, symbol: SymbolId, protection: Protection, meta: { clientOrderId: string; agentId: string | null; decisionId: string | null }): Promise<ProtectionResult>`, with `ProtectionResult = { before: Protection; after: Protection }`. The paper engine implements it by wrapping its sync `setProtection(accountId, symbol, protection: Protection): ProtectionResult`.
  - `ExecutionEngine.protectionOf(accountId: string, symbol: SymbolId): Promise<Protection>`, the current effective protection, which the runner and risk use. Paper wraps a sync `protectionOf`.
  - OKX until Task 8:
    - `protectionOf` returns `{ stopLoss: view.stopLoss, takeProfit: view.takeProfit, trailingStop: null }` from its current view.
    - `setProtection` throws `Error('Protection changes on OKX are not implemented yet')`.
    - Neither stub may affect entries or exits.
  - Paper op in the golden harness: `{ op: 'setProtection', accountId, symbol, protection: Protection }`. The existing scenarios are migrated to that shape, so `paper-scenarios.json` changes only in the op shape. Results stay identical.

- [ ] **Step 1: Write unit tests** in `paper-engine.test.ts`:
  - `trailingTrigger floors a long ratio trigger to the tick`: `extreme 100.37`, `ratio 0.01`, `tick 0.01` gives `99.36`.
  - `trailingTrigger ceils a short spread trigger to the tick`: `extreme 100.00`, `spread 1.234`, `tick 0.01` gives `101.24`.
  - `advanceExtreme waits for the activation price` (long, activation 105: last 104 → null, 105 → '105', 103 → '105', 107 → '107').
  - `validateTrailing rejects a ratio of 0.0009 and 0.31 and an activation below a long reference`.
- [ ] **Step 2:** Run `pnpm --filter @mochiprep/server test paper-engine`. Expected: FAIL (functions not defined).
- [ ] **Step 3:** Implement `trailing.ts`, then the engine:
  - `onQuote` order rank: `STOP_MARKET 0, TRAILING_STOP_MARKET 1, TAKE_PROFIT_MARKET 2, LIMIT 3, MARKET 4`.
  - For each open trailing order, first save the advanced `trailingExtreme` when it changed, with no event. Then trigger when the extreme is not null and long `last ≤ trigger` or short `last ≥ trigger`. It fills like a stop: as taker, at `marketFillPrice`, through `fillResting`.
  - `replaceProtection` takes a `Protection`. A trailing order created with no activation price starts with `trailingExtreme = quote.last` of the quote used to create it.
  - `validateProtection` adds `validateTrailing`.
  - Entry orders carry `attachedTrailingStop` and attach it on fill.
  - `positionViews` fills `trailingStop` (with the current trigger) and `externalProtection: []`.
- [ ] **Step 4:** Add scenarios to a new `TRAILING_SCENARIOS` list. They go in their own fixture `paper-trailing-scenarios.json` (a new key in `generate.ts`), so Rust can adopt them in Task 3 without breaking this commit. Required names, each long unless stated:
  - `trailing-ratio-follows-and-triggers`
  - `trailing-spread-short`
  - `trailing-activation-price`
  - `trailing-gap` (the trigger is 99.36 and the next quote's last is 95; it fills at that quote's bid with slippage)
  - `trailing-and-stop-same-quote` (stop-loss closes, trailing cancelled)
  - `trailing-survives-add`
  - `trailing-replaced-and-removed`
  - `trailing-attached-to-limit-entry`
  - `trailing-rejected-bad-callback`
- [ ] **Step 5:** Add a recovery test in `recovery.test.ts`, `trailing extreme survives a restart`. Open a long with a 1% trailing stop and move last to 110. Reopen the store and engine on the same file. Send last 109: no trigger, because the trigger is 108.90. Send last 108.89: it triggers.
- [ ] **Step 6:** Run `pnpm golden`, then all checks. Expected: pass. `paper-scenarios.json` results are unchanged apart from the op shape. Rust still passes, because the harness's `setProtection` handler is updated in this task to read `op.protection` (stop-loss and take-profit only, with trailing still `None`).
- [ ] **Step 7:** Document the trailing rules in `docs/PAPER_ENGINE.md`: evaluation order, rounding, activation, gaps, same-quote stop priority and restart. Commit.

### Task 3: Rust paper engine trailing stops

**Files:**
- Create: `crates/mochi-core/src/paper/trailing.rs`
- Modify: `crates/mochi-core/src/paper/{engine.rs,market.rs,views.rs,mod.rs}`, `crates/mochi-core/src/fill_model.rs`
- Modify: `crates/mochi-store/tests/paper_scenarios.rs` (also replay `paper-trailing-scenarios.json`), `crates/mochi-store/tests/paper_recovery.rs`

**Interfaces:**
- Consumes: Task 1 Rust types and Task 2 behaviour. The fixtures are the specification.
- Produces:
  - `trailing_trigger`, `advance_extreme` and `validate_trailing`, with the same semantics as Task 2, returning `Result` where TS would throw.
  - `PaperExecutionEngine::set_protection(&mut self, account_id: &str, symbol: &str, protection: &Protection) -> Result<ProtectionResult, EngineError>`.
  - `PaperExecutionEngine::protection_of(&self, account_id: &str, symbol: &str) -> Result<Protection, EngineError>`.

- [ ] **Step 1:** Extend `paper_scenarios.rs` to also replay `paper-trailing-scenarios.json` in a new test, `trailing_scenarios_match_typescript`, with the same harness. Run it. Expected: FAIL.
- [ ] **Step 2:** Implement the Rust port until both scenario tests pass byte for byte, ledger audits included.
- [ ] **Step 3:** Add `trailing_extreme_survives_reopen` to `paper_recovery.rs`, with the same numbers as the Task 2 recovery test.
- [ ] **Step 4:** Run all checks, then commit.

### Task 4: Risk engine for `trailingStop` and `PROTECT` (Node + Rust)

**Files:**
- Modify: `packages/core/src/risk/engine.ts`, `crates/mochi-core/src/risk.rs`
- Modify: `packages/shared/src/domain.ts` (`AiDecisionData`), `crates/mochi-domain/src/accounting.rs` (`AiAction::Protect`, `AiDecisionData.trailing_stop`)
- Modify: `apps/server/golden/generate.ts` (`riskFixture` cases), `packages/agents/src/runner.ts` (`assess` passes `allowTrailingStop: agent.allowTrailingStop` and `protection: null`; Task 6 supplies the real protection)
- Regenerate: `crates/golden/risk.json`, `accounting-types.json`

**Interfaces:**
- Consumes: Task 2 `validateTrailing` and `TRAILING_LIMITS`.
- Produces:
  - `AiDecisionData.action` adds `'PROTECT'`. Also `trailingStop?: { kind: TrailingKind; callback: number; activationPrice: number | null } | null`, where absent means "unchanged / none". Rust uses `Option<Option<AiTrailingStop>>` with a serde helper, so absent and `null` round-trip distinctly.
  - `RiskInput` gains `allowTrailingStop: boolean` and `protection: Protection | null`, the current protection of this symbol's open position.
  - New check ids, in this order and inserted where shown:
    - `trailing-allowed`: right after `expiry`, for any action. It fails when `allowTrailingStop` is false and `trailingStop !== undefined`. Detail: `trailing stops are off for this agent`.
    - `trailing-stop`: right after `take-profit` for LONG/SHORT. Uses `validateTrailing` against `reference`.
    - The `PROTECT` path: after `expiry` and `trailing-allowed`, it runs `has-position`, then `freshness`, then:
      - `stop-loss`: present if `requireStopLoss`, and on the correct side of `quote.last`.
      - `take-profit`: on the correct side.
      - `trailing-stop`: when `trailingStop` is non-null.
      - `stop-before-liq`: against the position's `liquidationPrice`.
      - `protect-risk`: `quantity × |last − stopLoss| ≤ equity × maxRiskPerTradePct / 100`.
    - Then return `approved` with `sized: null`.
  - `HOLD` or `CLOSE` with a non-undefined, non-null `trailingStop` fails a check `trailing-stop` with detail `only LONG, SHORT and PROTECT take a trailing stop`.

- [ ] **Step 1:** Add risk fixture cases to `riskFixture` in `generate.ts` (each is an input plus the recorded output):
  - switch off with `trailingStop` absent / null / set, for LONG and PROTECT
  - ratio 0.0009, 0.001, 0.3 and 0.31
  - spread below one tick, and above 30% of the reference
  - activation on the wrong side, long and short
  - PROTECT with no position, with a stale quote, and removing the stop while `requireStopLoss` is on
  - PROTECT loosening the stop past `maxRiskPerTradePct`
  - PROTECT moving the stop past liquidation
  - HOLD with a trailing stop
- [ ] **Step 2:** Run `pnpm golden`, then `cargo test -p mochi-core --test golden_risk`. Expected: FAIL (the Rust side does not know the cases yet).
- [ ] **Step 3:** Implement the TS checks, then the Rust port, until `golden_risk` and `golden_accounting` pass. Existing risk cases must produce unchanged output. Confirm with `git diff crates/golden/risk.json`: the diff only adds cases and the new `allowTrailingStop`/`protection` input keys.
- [ ] **Step 4:** Run all checks, then commit.

### Task 5: Agent output, parsing and prompt

**Files:**
- Modify: `packages/shared/src/schemas.ts` (`AiDecisionSchema`), `packages/agents/src/parse.ts`, `packages/agents/src/prompt.ts`, `packages/ai-providers/src/mock.ts` (if it must emit PROTECT; otherwise unchanged)
- Test: `packages/agents/test/parse.test.ts`, and a new `packages/agents/test/prompt.test.ts`

**Interfaces:**
- Consumes: Task 4 `AiDecisionData`.
- Produces:
  - `AiDecisionSchema`: `action` adds `'PROTECT'`. `trailingStop: z.strictObject({ kind, callback: positive, activationPrice: positive.nullable() }).nullable().optional()`.
  - `buildSystemPrompt(options: { allowTrailingStop: boolean }): string` replaces the `SYSTEM_PROMPT` constant.
    - It always describes `PROTECT`: it sets the full stop-loss and take-profit, and `null` removes one.
    - Only when `allowTrailingStop` is true does it describe `trailingStop`: RATIO as a fraction (0.015 = 1.5%) or SPREAD in USDT, an optional activationPrice, absent means unchanged, null removes, and it can be used with LONG, SHORT and PROTECT.
    - When false, the text never contains the string `trailingStop`.
  - `buildUserPrompt` includes, for an open position, `stopLoss`, `takeProfit`, `trailingStop` (with its current `triggerPrice`) and, when non-empty, `externalProtection` with the note that these are the owner's orders and MochiPrep will not change them.

- [ ] **Step 1: Tests:**
  - `parse accepts PROTECT with and without trailingStop`
  - `parse distinguishes absent and null trailingStop`
  - `parse rejects an unknown trailing kind`
  - `system prompt never mentions trailingStop when the switch is off` (`expect(buildSystemPrompt({ allowTrailingStop: false })).not.toContain('trailingStop')`)
  - `user prompt shows the current protection and external orders`
- [ ] **Step 2:** Run them. Expected: FAIL. Then implement and run again. Expected: PASS.
- [ ] **Step 3:** Run all checks, then commit.

### Task 6: Runner `PROTECT` path, agent setting, API and agent UI

**Files:**
- Modify: `packages/agents/src/runner.ts`, `packages/shared/src/domain.ts` (`DecisionRecord`), `apps/server/src/runtime.ts`, `apps/server/src/app.ts`
- Modify: `apps/web/src/pages/Agents.tsx` (checkbox), `apps/web/src/components/trading.tsx` (History detail for PROTECT)
- Test: `apps/server/test/agent-runner.test.ts`, `apps/server/test/agent-signals.test.ts`

**Interfaces:**
- Consumes: Task 2 `ExecutionEngine.setProtection` / `protectionOf`, Task 4 `RiskInput`, Task 5 prompt builders.
- Produces:
  - `DecisionRecord` gains `protectionBefore?: Protection` (the snapshot the decision saw) and `protectionAfter?: Protection`.
  - `assess` passes `protection: await engine.protectionOf(account, symbol)` when a position is open, replacing Task 4's `null`. A thrown `setProtection` is recorded as `ORDER_REJECTED` with the redacted message, which is what OKX produces until Task 8.
  - AI numbers become decimal strings through the existing `plain()`: the trailing `callback` and `activationPrice`, and the stop-loss and take-profit.
  - Runner, for `PROTECT`:
    - The target is the decision's protection after merging. `trailingStop` absent means the current one is kept.
    - It is assessed with `allowTrailingStop: agent.allowTrailingStop` and `protection: current`.
    - On approval in AUTO, it calls `setProtection` with `clientOrderId = protect:<decisionId>`. The outcome is `EXECUTED`, with detail `Protection updated` and before/after recorded.
    - In SIGNAL mode it records `protectionBefore` at decision time. `executeSignal` compares the current protection to the snapshot. If they differ, the outcome is `SIGNAL_EXPIRED` with detail `Protection changed since this suggestion`. If they match, it re-assesses and then executes.
    - It is a no-op when the target equals the current protection: outcome `HOLD`, detail `Protection already as requested`.
  - The UI checkbox label is `Allow trailing stops`, next to `requireStopLoss`, with the hint `Off: this agent cannot set, change or remove trailing stops.` `PUT /api/agents/:id` saves it.

- [ ] **Step 1: Tests:**
  - `PROTECT tightens the stop in AUTO and records before and after`
  - `PROTECT without trailingStop keeps an existing trailing stop when the switch is off`
  - `PROTECT with trailingStop null is risk rejected when the switch is off`
  - `a PROTECT signal expires when protection changed before it was accepted`
  - `LONG with a trailing stop attaches it on fill`
- [ ] **Step 2:** Run them. Expected: FAIL. Then implement and run again. Expected: PASS.
- [ ] **Step 3:** Check the Agents page in the browser (`pnpm dev`, or the project's run skill): the checkbox saves and survives a reload, in both themes.
- [ ] **Step 4:** Run all checks plus `pnpm test:e2e`, then commit.

### Task 7: OKX client algo-order methods

**Files:**
- Modify: `packages/exchanges/src/okx-client.ts`
- Test: `packages/exchanges/test/okx-client.test.ts`

**Interfaces:**
- Produces:
  - `toAlgoClOrdId(localOrderId: string): string` (32 alphanumerics, the same hash scheme as `toClOrdId`, but a distinct prefix input `algo:` so the two can never collide).
  - `type AlgoOrdType = 'conditional' | 'move_order_stop'`.
  - `interface AlgoOrderBody`:
    - Common: `instId`, `tdMode: 'isolated'`, `side`, `ordType: AlgoOrdType`, `algoClOrdId`, `sz`, `reduceOnly: true`, and `posSide` only when `toBody` sends it.
    - Conditional: `slTriggerPx`/`slOrdPx: '-1'` or `tpTriggerPx`/`tpOrdPx: '-1'`.
    - Trailing: `callbackRatio` or `callbackSpread`, plus `activePx?`.
  - `interface OkxAlgo { algoId; algoClOrdId; instId; ordType; state; side; sz; slTriggerPx?; tpTriggerPx?; callbackRatio?; callbackSpread?; activePx?; moveTriggerPx? }`.
  - `OkxClient` methods:
    - `placeAlgo(body: AlgoOrderBody): Promise<string>`: `POST /api/v5/trade/order-algo`. Returns the `algoId`. A row `sCode !== '0'` is an `OkxRejection`, and a missing row is `OkxUnknownOutcome`.
    - `cancelAlgos(items: { instId: string; algoId: string }[]): Promise<void>`: `POST /api/v5/trade/cancel-algos`, at most 10 per call.
    - `getPendingAlgos(ordType: AlgoOrdType): Promise<OkxAlgo[]>`: `GET /api/v5/trade/orders-algo-pending?ordType=…&instType=SWAP`.
    - `getAlgo(algoClOrdId: string): Promise<OkxAlgo | null>`: `GET /api/v5/trade/order-algo?algoClOrdId=…`. It returns null only on an empty `data` array. Every rejection rethrows, because the not-found code is UNCONFIRMED and an unknown answer must never become "absent".
  - `PlaceOrderBody.attachAlgoOrds` elements gain an optional `attachAlgoClOrdId`, plus `callbackRatio` / `callbackSpread` / `activePx`.

- [ ] **Step 1: Tests**, using the existing fake-fetch pattern in `okx-client.test.ts`:
  - `placeAlgo signs and posts a move_order_stop body`: assert path, body JSON and that the signature covers the body.
  - `placeAlgo maps an sCode rejection`
  - `placeAlgo with no row is an unknown outcome`
  - `cancelAlgos refuses more than 10 items` (throws before fetching)
  - `getAlgo returns null only for an empty data array`
  - `demo sends x-simulated-trading on algo calls`
- [ ] **Step 2:** Run them. Expected: FAIL. Then implement and run again. Expected: PASS.
- [ ] **Step 3:** Run all checks, then commit.

### Task 8: OKX engine protection: attach, replace, sync, armed gate

**Files:**
- Create: `packages/core/src/trading/protection.ts`
- Modify: `packages/exchanges/src/okx-engine.ts`, `apps/server/src/runtime.ts` (wire `isArmed` into the gate)
- Test: `apps/server/test/okx-live.test.ts`, plus `packages/core` unit tests for `protection.ts` (in `apps/server/test/paper-engine.test.ts` or a new `apps/server/test/protection.test.ts`)

**Interfaces:**
- Consumes: Task 7 client, Task 2 `Protection` / `ExecutionEngine.setProtection`, Task 6 `protectionOf`.
- Produces:
  - `classifyProtectionChange(side: PositionSide, before: Protection, after: Protection): 'REDUCES_RISK' | 'NEEDS_ARMED'` in `protection.ts`.
    - It returns `REDUCES_RISK` only when every changed leg is one of:
      - a stop-loss added, or moved toward the price (long: higher; short: lower);
      - a trailing stop added where there was none;
      - a trailing stop of the same kind and activation whose callback got smaller;
      - an unchanged leg.
    - Anything else, take-profit changes included, returns `NEEDS_ARMED`.
  - `OkxExecutionEngine.setProtection(...)`:
    1. Serialize through the existing `serialized` queue.
    2. Refuse with `REJECTED` and reason `NOT_ARMED: this change increases risk` when the account is not armed and the classification is `NEEDS_ARMED`. Refuse with `EXCHANGE_NOT_SYNCED`, `NO_MARKET_DATA` or `MARKET_DATA_STALE` exactly as `preTradeRefusal` does.
    3. For each leg that changed:
       - Insert a local order row (type `STOP_MARKET` / `TAKE_PROFIT_MARKET` / `TRAILING_STOP_MARKET`, `closePosition` and `reduceOnly` true, `clientOrderId` = the new local id).
       - Call `placeAlgo` with `algoClOrdId = toAlgoClOrdId(row.id)`.
       - Only after it succeeds, cancel the old row's algo and mark the old row `CANCELED`.
    4. On `OkxUnknownOutcome`, leave the new row `OPEN` with `exchangeOrderId` null, stop processing further legs, keep the old row, and return. Sync resolves it with `getAlgo`. Never place again.
    5. On `OkxRejection`, mark the new row `REJECTED` with the code, keep the old one, and stop.
  - Entry orders: `toBody` adds each leg's `attachAlgoClOrdId = toAlgoClOrdId(<local protection row id>)`. These rows are created with the entry, as `OPEN` local protection rows linked by `clientOrderId`. The trailing stop goes in its own `attachAlgoOrds` element.
  - `sync()` adds:
    - Fetch `getPendingAlgos('conditional')` and `getPendingAlgos('move_order_stop')`.
    - Match each algo to a local open protection row by `algoClOrdId`, and fill in `exchangeOrderId`. An algo with no matching row becomes an `ExternalProtection` on that symbol's view.
    - A local row with an `exchangeOrderId` whose algo is no longer pending is closed, `CANCELED` with reason `Gone from OKX`.
    - A local row with no `exchangeOrderId` is resolved with `getAlgo`.
    - Superseded rows: when more than one of our open rows exists per (symbol, leg), the newest is effective and the older ones are cancelled.
    - Our rows whose symbol has no open position are cancelled.
  - `toView` reads `stopLoss`, `takeProfit` and `trailingStop` from the effective local rows. A trailing stop's `triggerPrice` comes from `moveTriggerPx` and `extreme` is null on OKX. It also fills `externalProtection`.
  - `protectionOf` returns the effective protection.
  - `closePosition` and every refusal path keep working when disarmed. The protection gate never blocks an exit.

- [ ] **Step 1: Tests**:
  - `protection.test.ts`: a table test over `classifyProtectionChange` covering every bullet above, each case marked with its expected class.
  - `okx-live.test.ts`, using the existing fake OKX server:
    - `a tightened stop is placed before the old one is cancelled` (assert the request order: place then cancel)
    - `a loosened stop is refused while disarmed`
    - `a tightened stop is allowed while disarmed`
    - `an unknown outcome placing a replacement keeps the old stop and is resolved by sync without a second place` (count the place calls: exactly 1)
    - `an algo placed in the OKX app shows as external and is never cancelled`
    - `sync cancels our protection left on a closed position`
    - `entry with a trailing stop sends it in attachAlgoOrds with attachAlgoClOrdId`
- [ ] **Step 2:** Run them. Expected: FAIL. Then implement and run again. Expected: PASS.
- [ ] **Step 3:** Run all checks, then commit.

### Task 9: Manual protection on OKX, position and History UI, docs

**Files:**
- Modify: `apps/server/src/app.ts` (`PUT /api/positions/:accountId/:symbol/protection` accepts `ProtectionSchema` for all venues and calls `ExecutionEngine.setProtection` with `clientOrderId = manual:<uuid>`). It removes the `Change stops for exchange positions on OKX` refusal.
- Modify: `apps/web/src/components/trading.tsx` (`ProtectionDialog` trailing fields: kind segmented control, callback, optional activation price; position rows show the trailing trigger and an `External` tag list)
- Modify: `docs/OKX_TRADING.md`, `docs/ARCHITECTURE.md`, `docs/PAPER_ENGINE.md` (if not complete), `README.md` feature list if it lists protection
- Test: `apps/server/test/api-integration.test.ts`, the e2e spec that covers positions (extend it)

- [ ] **Step 1: Tests:**
  - `api: protection route sets a trailing stop on a paper position`
  - `api: protection route on an OKX account goes through the armed gate` (a loosening while disarmed returns the refusal text)
  - e2e: open a replay position, set a 1% trailing stop in the dialog, and see the trigger price in the row.
- [ ] **Step 2:** Implement, then run all checks plus `pnpm test:e2e`.
- [ ] **Step 3:** Docs. `docs/OKX_TRADING.md` gets a "Trailing stops and protection changes" section covering:
  - the place-before-cancel order and the armed rule
  - external orders
  - the six UNCONFIRMED items, each with the observed fallback
  - the statement that live trailing stops have not been verified on Demo yet

  `ARCHITECTURE.md` gets one paragraph on `Protection` and `PROTECT`.
- [ ] **Step 4:** Check the dialog and position rows in the browser in both themes, then commit.

### Task 10: Carry-forward, final verification, CodeRabbit

**Files:**
- Modify: `docs/superpowers/plans/2026-11-07-rust-accounting-core.md` ("Carried forward" table)

- [ ] **Step 1:** Add rows to the carried-forward table:
  - **Plan 4:** port the runner `PROTECT` path, the SIGNAL snapshot rule and `buildSystemPrompt({ allowTrailingStop })`.
  - **Plan 5:** port `classifyProtectionChange` (add a golden), the OKX algo client and engine protection (place-before-cancel, `algoClOrdId` matching, external orders, sync cleanup), and the result of the owner's Demo test of the six UNCONFIRMED behaviours.
- [ ] **Step 2:** Run every command in Global Constraints plus `pnpm test:e2e`. Record the counts.
- [ ] **Step 3:** Run the CodeRabbit CLI locally against `main`. It is installed in WSL. The worktree's `.git` file points at a Windows path, so clone the repository into WSL, check out this branch, run `coderabbit review --agent -t committed --base main`, and delete the clone afterwards. Verify each finding against the code, fix the valid ones, and rerun the checks.
- [ ] **Step 4:** Commit. Do not push; the owner decides integration.
