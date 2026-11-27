# Exchange core extraction (phase 1a) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split `OkxEngine` into an exchange-neutral engine (new crate `mochi-exchange`) that talks to a `VenueAdapter` trait, with OKX as the first in-process adapter, and no change to OKX behaviour, texts, database contents or golden fixtures.

**Architecture:** First add neutral venue types, the `VenueAdapter` trait and an `OkxAdapter` that wraps `OkxClient`. Then migrate the engine's call sites from `OkxClient`/`Row` to the adapter one file at a time, keeping every test green. Then move `engine/` into `mochi-exchange` and make the server's runtime (`ExecutionTarget`, `ExchangeSession`, connect checks, `AccountVenue`) exchange-neutral. Phase 1b (plugin processes) builds on the trait defined here.

**Tech Stack:** Rust 1.99 workspace, tokio, serde_json (preserve_order), `mochi_decimal::Dec`, the in-process fake OKX (`mochi-okx` feature `fake`).

**Spec:** `docs/superpowers/specs/2026-11-27-modular-exchanges-design.md` (sections 1, 4, 5, 8 phase 1a).

## Global Constraints

- Branch from `dev` (already includes PR #26). One PR to `dev` for this phase. Never merge it.
- OKX behaviour unchanged: every error and refusal text stays byte-identical, including the word "OKX" (for example `OKX {code}: {message}`, `EXCHANGE_NOT_SYNCED: no recent sync with OKX`).
- `crates/golden` is not regenerated. `cargo test --workspace` passes after every task.
- Money and quantities stay as exchange text in neutral types (`String`, parsed with `Dec` where the engine already parses), never `f64`.
- Neutral field rule: each field holds the exchange's raw text. `None` means absent or JSON `null`, and `Some("")` stays `Some("")`. This keeps the engine's `nullish_text` / `truthy_text` (Node `??` and truthiness) semantics exact. Where the current code turns a JSON number into text (`js_number_string`), the adapter does the same.
- No new migration in this phase. The spec's migration 019 (`master_uid`, `group_label`, `exchange_plugins`) moves to the phases that first read those columns (1b and 4).
- Store tables named `okx_*` and the sealed OKX credential format are not touched.
- AGENTS.md rules that must not bend apply unchanged. Only their file paths are updated (Task 8).
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Before pushing, check that `gh api user -q .login` prints `rinnyanneko`. Run CodeRabbit locally against `dev` before pushing.

## Review Focus

1. Absent, `null` and `""` fields from OKX (for example `slTriggerPx`) must keep their distinction through the adapter, or protection reconciliation silently changes. This is pinned in Task 1 (`okx_adapter_keeps_absent_null_and_empty_apart`).
2. A rejection text must read exactly `OKX 51000: <msg>` after the move. This is pinned in Task 1 (`rejection_display_is_byte_identical`).
3. The four OKX codes `51050 51052 51277 51279` must still take the "trigger would fire now" path in `protection.rs`. This is pinned in Task 1 (`trigger_codes_classified`) and Task 4 (existing protection tests).
4. An existing database with `venue = 'OKX_LIVE'` / `'OKX_DEMO'` rows must route to the OKX engine after the upgrade. This is pinned in Task 7 (`legacy_venue_strings_round_trip`) and by the existing store tests.
5. A venue string from a newer version (for example `BYBIT_LIVE`) must not break account listing. It must read as an exchange account that cannot connect, with a clear error. This is pinned in Task 7 (`unknown_exchange_lists_but_refuses_connect`).

---

### Task 1: Neutral venue types, `VenueAdapter`, `OkxAdapter`

**Files:**
- Create: `crates/mochi-exchange/Cargo.toml` (deps: mochi-core, mochi-decimal, mochi-domain, serde, serde_json, thiserror, tokio), `crates/mochi-exchange/src/lib.rs`, `crates/mochi-exchange/src/venue.rs`
- Create: `crates/mochi-okx/src/adapter.rs`
- Modify: `crates/mochi-okx/Cargo.toml` (depend on mochi-exchange), `crates/mochi-okx/src/lib.rs` (`pub mod adapter; pub use adapter::OkxAdapter;`)
- Move: `BoxFuture` from `crates/mochi-okx/src/client.rs:249` to `mochi-exchange::venue`, and re-export it from `mochi-okx` so existing imports still compile.
- Test: `crates/mochi-okx/tests/adapter.rs`

**Interfaces:**
- Produces (in `mochi_exchange::venue`):
  - `pub enum VenueError { Rejection { code: String, message: String, display: String, kind: Option<RejectKind> }, Unknown(String), Invalid(String) }`. `Display` prints `display` for `Rejection` and the inner text otherwise.
  - `pub enum RejectKind { TriggerWouldFireNow }`
  - `pub enum AlgoKind { Conditional, Oco, Trailing }`. These are OKX's `conditional`, `oco` and `move_order_stop`.
  - `pub struct VenueAccountConfig { uid: Option<String>, master_uid: Option<String>, can_withdraw: bool, can_trade: bool, one_way_mode: bool, account_mode_ok: bool }`
  - `pub struct VenueContract { contract_value: String, lot_size: String }`
  - Row structs carrying exactly the fields the engine reads today (the field list comes from `grep -ohE '"[a-zA-Z]+"' crates/mochi-okx/src/engine/*.rs`): `VenueBalance`, `VenuePosition`, `VenueOrder`, `VenueAlgo`, `VenueFill`, `VenueBill`, `VenueClosedPosition`. Field names are snake_case neutral names (`symbol` not `instId`, `exchange_order_id` not `ordId`, `client_order_id` not `clOrdId`, `updated_ms` not `uTime`). Every field is `Option<String>`, except `symbol: String` where the engine requires it.
  - Request structs `VenuePlaceOrder`, `VenuePlaceAlgo`, `VenueCancelAlgo`, which mirror `PlaceOrderBody`, `AlgoOrderBody` and `CancelAlgo` with neutral names and `symbol` instead of `inst_id`.
  - The trait:

```rust
pub trait VenueAdapter: Send + Sync {
    /// Shown in texts the engine writes, e.g. "OKX".
    fn name(&self) -> &str;
    fn account_config(&self) -> BoxFuture<'_, Result<VenueAccountConfig, VenueError>>;
    fn contract(&self, symbol: &str) -> BoxFuture<'_, Result<VenueContract, VenueError>>;
    fn balance(&self) -> BoxFuture<'_, Result<Option<VenueBalance>, VenueError>>;
    fn positions(&self) -> BoxFuture<'_, Result<Vec<VenuePosition>, VenueError>>;
    fn positions_history(&self, after: Option<i64>) -> BoxFuture<'_, Result<Vec<VenueClosedPosition>, VenueError>>;
    fn execution_history(&self, after: Option<u64>) -> BoxFuture<'_, Result<Vec<VenueBill>, VenueError>>;
    fn recent_fills(&self, symbol: &str) -> BoxFuture<'_, Result<Vec<VenueFill>, VenueError>>;
    fn set_leverage(&self, symbol: &str, leverage: u32) -> BoxFuture<'_, Result<(), VenueError>>;
    fn place_order(&self, order: VenuePlaceOrder) -> BoxFuture<'_, Result<String, VenueError>>;
    fn get_order(&self, symbol: &str, client_order_id: &str) -> BoxFuture<'_, Result<Option<VenueOrder>, VenueError>>;
    fn cancel_order(&self, symbol: &str, client_order_id: &str) -> BoxFuture<'_, Result<(), VenueError>>;
    fn place_algo(&self, algo: VenuePlaceAlgo) -> BoxFuture<'_, Result<String, VenueError>>;
    fn pending_algos(&self, kind: AlgoKind) -> BoxFuture<'_, Result<Vec<VenueAlgo>, VenueError>>;
    fn get_algo(&self, client_algo_id: &str) -> BoxFuture<'_, Result<Option<VenueAlgo>, VenueError>>;
    fn cancel_algos(&self, items: Vec<VenueCancelAlgo>) -> BoxFuture<'_, Result<(), VenueError>>;
    /// The exchange's client-id form of our order id (OKX: `to_cl_ord_id`).
    fn client_order_id(&self, order_id: &str) -> String;
    fn client_algo_id(&self, order_id: &str) -> String;
}
```

  - The parameter types of `execution_history` and `positions_history` must match the current `OkxClient` signatures. If they differ from the above, the client's types win.
  - `pub struct OkxAdapter { client: OkxClient }` with `OkxAdapter::new(client: OkxClient) -> Self` and `fn client(&self) -> &OkxClient`. The server's connect step needs `client()` until Task 6.

- [ ] **Step 1: Write failing tests in `crates/mochi-okx/tests/adapter.rs`** against the fake OKX (`crates/mochi-okx/tests/common`):
  - `okx_adapter_keeps_absent_null_and_empty_apart`: fake algo rows with `slTriggerPx` absent, `null` and `""` map to `None`, `None` and `Some("")`.
  - `rejection_display_is_byte_identical`: a fake rejection `51000 / "Parameter error"` gives `VenueError::Rejection` whose `to_string()` is `"OKX 51000: Parameter error"` and equals `OkxError::Rejection{..}.to_string()`.
  - `trigger_codes_classified`: codes `51050 51052 51277 51279` give `kind == Some(RejectKind::TriggerWouldFireNow)`, and `51000` gives `None`.
  - `unknown_and_invalid_pass_through`: `OkxError::Unknown(t)` maps to `VenueError::Unknown(t)`, and `Invalid` maps the same way.
  - `positions_map_fields`: one fake position maps `instId BTC-USDT-SWAP` to `symbol "BTCUSDT"` (via `ids`), and `pos`, `avgPx`, `liqPx`, `upl`, `posId`, `mgnMode` map to the matching fields.
  - `client_ids_match_ids_module`: `client_order_id("ord_x") == to_cl_ord_id("ord_x")`, and `client_algo_id` is the same check against `to_algo_cl_ord_id`.
- [ ] **Step 2: Run** `cargo test -p mochi-okx --features fake --test adapter`. Expected: it fails to compile (no `OkxAdapter`).
- [ ] **Step 3: Implement** the types and trait in `mochi-exchange/src/venue.rs`, and `OkxAdapter` in `mochi-okx/src/adapter.rs`. Each method calls the matching `OkxClient` method and converts each `Row` with one `fn from_row(row: &Row) -> Venue*` per struct. The conversion reads fields with the same helpers the engine uses today (`nullish_text`, `truthy_text`, `js_number`), so that text is identical.
- [ ] **Step 4: Run** `cargo test -p mochi-okx --features fake` and then `cargo test --workspace`. Expected: all pass.
- [ ] **Step 5: Commit** with the message "Add exchange-neutral venue types and the OKX adapter".

### Task 2: Engine reads sync and views through the adapter

**Files:**
- Modify: `crates/mochi-okx/src/engine/mod.rs:320-330` (`OkxEngineDeps` gains `venue: Arc<dyn VenueAdapter>`, and keeps `client` for now)
- Modify: `crates/mochi-okx/src/engine/sync.rs`, `crates/mochi-okx/src/engine/views.rs`
- Modify: every `OkxEngineDeps { .. }` construction (`crates/mochi-server/src/runtime/execution.rs:203-222`, `crates/mochi-okx/tests/common/mod.rs`), which passes `Arc::new(OkxAdapter::new(client.clone()))`
- Test: the existing `crates/mochi-okx/tests/engine.rs` and `protection.rs`, and `crates/mochi-server/tests/okx_*.rs`

**Interfaces:**
- Consumes: `VenueAdapter`, `OkxAdapter` (Task 1).
- Produces: `PositionSnapshot::new(Vec<VenuePosition>)` (was `Vec<Row>`); the views take `&VenuePosition` / `&VenueAlgo`.

- [ ] **Step 1: Switch the reads in `sync.rs`.** Change `get_pending_algos` ×3, `get_order`, `get_usdt_balance` and `get_positions` (`sync.rs:162-178, 481-482`) to `self.deps.venue.*`. Change their `Row` field reads to struct fields. Change `PositionSnapshot` and `LiveRows` to hold neutral rows.
- [ ] **Step 2: Switch `views.rs`** to the neutral rows.
- [ ] **Step 3: Run** `cargo test --workspace`. Expected: all pass, with no test edits other than the deps construction.
- [ ] **Step 4: Commit** with the message "Read engine sync and views through VenueAdapter".

### Task 3: History through the adapter

**Files:** Modify `crates/mochi-okx/src/engine/history.rs` (lines 135 and 268 and their row reads).

- [ ] **Step 1:** Change `get_execution_history` and `get_positions_history` to `venue.execution_history` and `venue.positions_history`, and the row reads to `VenueBill` and `VenueClosedPosition` fields.
- [ ] **Step 2: Run** `cargo test --workspace`. Expected: all pass, including `crates/mochi-server/tests/okx_runtime.rs` position-history tests.
- [ ] **Step 3: Commit** with the message "Read position history through VenueAdapter".

### Task 4: Orders, protection and rejections through the adapter, then drop `client`

**Files:**
- Modify: `crates/mochi-okx/src/engine/mod.rs` (`place_order` at :1536, the positions reads at :1202 and :1456, and the rejection match at :1478)
- Modify: `crates/mochi-okx/src/engine/protection.rs` (every `self.deps.client.*` call; the code match at :2463-2471 becomes `kind == Some(RejectKind::TriggerWouldFireNow)`)
- Modify: `crates/mochi-okx/src/engine/refusal.rs` (`to_inst_id` uses become `venue.client_order_id`/symbol or are removed if only used for keys)
- Modify: `EngineError::Okx(OkxError)` becomes `EngineError::Venue(VenueError)`, keeping `#[error(transparent)]`
- Remove `client` from `OkxEngineDeps` and from its constructions.

- [ ] **Step 1:** Build `VenuePlaceOrder` / `VenuePlaceAlgo` / `VenueCancelAlgo` where `PlaceOrderBody` / `AlgoOrderBody` / `CancelAlgo` were built. `OkxAdapter` converts them back to the OKX bodies, keeping the same field values and order, so the golden request bytes in `golden_okx_private.rs` are unchanged.
- [ ] **Step 2:** Replace every `to_cl_ord_id` and `to_algo_cl_ord_id` in `engine/` with `self.deps.venue.client_order_id` / `client_algo_id`.
- [ ] **Step 3: Check** that `grep -rn "crate::client\|crate::ids\|OkxClient\|OkxError" crates/mochi-okx/src/engine` prints nothing.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: all pass, including `crates/mochi-okx/tests/protection.rs` and `golden_okx_private.rs`.
- [ ] **Step 5: Commit** with the message "Send orders and protection through VenueAdapter".

### Task 5: Move the engine into `mochi-exchange`

**Files:**
- Move: `crates/mochi-okx/src/engine/` to `crates/mochi-exchange/src/engine/` (with `git mv`)
- Rename: `OkxEngine` to `ExchangeEngine`, `OkxEngineDeps` to `EngineDeps`. Remove the `venue: Venue` dep, because `name()` and the account venue replace it. `UnsentOrder`, `CheckedProtection` and the rest keep their names.
- Modify: engine texts that say "OKX" now use `self.deps.venue.name()`, for example `format!("no recent sync with {}", name)`.
- Modify: `crates/mochi-okx/Cargo.toml` (`mochi-exchange` becomes a regular dependency; the engine tests stay in `crates/mochi-okx/tests` because they need the fake), `crates/mochi-server/Cargo.toml` (adds `mochi-exchange`), and every import of `mochi_okx::engine::*` in `crates/mochi-server/src`.

- [ ] **Step 1: Write a failing test** `engine_texts_use_venue_name` in `crates/mochi-exchange/tests/name.rs`, with a stub adapter whose `name()` is `"TESTX"`. With no sync done, an opening order's refusal reads `EXCHANGE_NOT_SYNCED: no recent sync with TESTX`. The stub returns `VenueError::Invalid("stub")` for every call.
- [ ] **Step 2: Run** `cargo test -p mochi-exchange`. Expected: it fails to compile.
- [ ] **Step 3:** Do the move and rename, and replace "OKX" in the engine's texts with the name.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: all pass, and every OKX text is unchanged because `OkxAdapter::name()` is `"OKX"`.
- [ ] **Step 5: Commit** with the message "Move the trading engine into mochi-exchange".

### Task 6: Runtime sessions and connect checks are exchange-neutral

**Files:**
- Modify: `crates/mochi-server/src/runtime/execution.rs` (`ExecutionTarget::Okx(Arc<OkxEngine>)` becomes `ExecutionTarget::Exchange(Arc<ExchangeEngine>)`; `ExchangeSession.engine: Arc<ExchangeEngine>`; `secrets: Vec<Zeroizing<String>>`; `okx_engine(..)` becomes `exchange_engine(runtime, account_id, venue: Arc<dyn VenueAdapter>, ..)`; `OkxExecution` becomes `ExchangeExecution`)
- Modify: `crates/mochi-server/src/runtime/exchange.rs:86-130` (`check_key` becomes `fn key_refusal(config: &VenueAccountConfig, name: &str) -> Option<String>` plus an async contract-spec loop over `venue.contract(symbol)`)
- Modify: `crates/mochi-server/src/runtime/exchange_sync.rs`, `position_watch.rs` and `agent_execution.rs`, wherever they match `ExecutionTarget::Okx` or name `OkxEngine`
- Modify: `crates/mochi-okx/src/adapter.rs` (`account_config` fills `VenueAccountConfig` from `get_config`: `perm` contains `withdraw` / `trade`, `posMode == "net_mode"`, `acctLv != "1"`, `uid`, `mainUid`)
- Test: `crates/mochi-server/src/runtime/exchange.rs` `#[cfg(test)] mod key_refusal_tests`

**Interfaces:**
- Consumes: `ExchangeEngine`, `EngineDeps` (Task 5); `VenueAccountConfig` (Task 1).
- Produces: `ExecutionTarget::Exchange(Arc<ExchangeEngine>)`, `key_refusal(&VenueAccountConfig, &str) -> Option<String>`.

- [ ] **Step 1: Write failing tests** in `key_refusal_tests`. Each asserts the exact current text when the name is `"OKX"`:
  - withdraw gives `"This API key has withdrawal permission. Create a key without it."`
  - no trade gives `"This API key has no trade permission."`
  - not one-way gives `"Set OKX position mode to one-way (net) first. MochiPrep does not support hedge mode."`
  - account mode not ok gives `"This OKX account is in Spot mode, which cannot trade perpetuals. Switch it to Futures mode."`
  - all ok gives `None`
  - withdraw and no trade together gives the withdraw text (withdraw is checked first, as today)
- [ ] **Step 2: Run** `cargo test -p mochi-server key_refusal`. Expected: it fails to compile.
- [ ] **Step 3: Implement.** Write `key_refusal` and the rename. `secrets` keeps the same three values in the same order for OKX, so redaction and the sealed `SavedCredentials` format are unchanged. The contract-spec loop keeps today's text, `"OKX contract specs for {} changed. Refusing to size orders from stale values."`, with the name substituted.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: all pass, including `okx_trading.rs`, `okx_runtime.rs`, `vault.rs` and `golden_api.rs`.
- [ ] **Step 5: Commit** with the message "Make exchange sessions and connect checks exchange-neutral".

### Task 7: `AccountVenue` names any exchange

**Files:**
- Modify: `crates/mochi-domain/src/accounting.rs:160-167`
- Modify: the call sites that `grep -rn "AccountVenue::Okx" crates` lists (about 44 uses in 15 files, including `routes/accounts.rs`, `validate/exchange.rs`, `runtime/mod.rs` and `mochi-core/src/paper/engine.rs`)
- Test: `crates/mochi-domain/tests/account_venue.rs`, `crates/mochi-server/tests/okx_runtime.rs` (a new test)

**Interfaces:**
- Produces:
  - `pub enum AccountVenue { Paper, Exchange { exchange: ExchangeId, environment: Environment } }`
  - `pub enum Environment { Demo, Live }`
  - `pub struct ExchangeId(Arc<str>)` with `ExchangeId::OKX` (as a `fn okx() -> Self`) and `fn as_str(&self) -> &str`
  - `AccountVenue::okx_demo()` and `okx_live()` constructors, and `fn is_live(&self) -> bool`
  - Serde stays `"PAPER"` or `"<EXCHANGE upper>_<DEMO|LIVE>"`. It deserializes any `^[A-Z0-9]+_(DEMO|LIVE)$`, and the exchange id is lowercased.
  - The type is `Clone`, not `Copy`. Fix the call sites with `.clone()` or `&`.

- [ ] **Step 1: Write failing tests:**
  - `legacy_venue_strings_round_trip`: `"PAPER"`, `"OKX_DEMO"` and `"OKX_LIVE"` deserialize to `Paper`, `okx_demo()` and `okx_live()`, and serialize back to the same strings.
  - `new_exchange_strings_parse`: `"BYBIT_LIVE"` gives `Exchange{bybit, Live}`.
  - `garbage_rejected`: `"OKX"`, `"okx_demo"` and `"OKX_PAPER"` fail to deserialize.
  - `unknown_exchange_lists_but_refuses_connect` (in `okx_runtime.rs`): insert an account row with venue `BYBIT_LIVE` straight through the store. `GET /api/accounts` still lists it. A connect or trade on it fails with `"No exchange support installed for BYBIT"`, and the runtime does not panic.
- [ ] **Step 2: Run** `cargo test -p mochi-domain --test account_venue`. Expected: it fails.
- [ ] **Step 3: Implement** the type, a hand-written `Serialize`/`Deserialize`, and the call sites. `okx_venue()` in `execution.rs` matches `Exchange { exchange, environment }` where `exchange == okx`, and every other exchange gives the "No exchange support installed for {EXCHANGE}" error.
- [ ] **Step 4: Run** `cargo test --workspace`, `pnpm typecheck` and `pnpm test`. Expected: all pass. The web contract in `packages/shared` keeps `"OKX_DEMO" | "OKX_LIVE" | "PAPER"`, because no new venue reaches the UI until phase 2.
- [ ] **Step 5: Commit** with the message "Let AccountVenue name any exchange".

### Task 8: Docs, verification, review, PR

**Files:** Modify `AGENTS.md` (the layout entry for `mochi-exchange`, the `pre_trade_refusal` path becoming `crates/mochi-exchange/src/engine/refusal.rs`, and the `mochi-okx` entry becoming "the OKX client, `OkxAdapter` and the fake"), and `docs/OKX_TRADING.md` (any file paths it names).

- [ ] **Step 1:** Update those paths. Change no rule wording; the rule changes belong to phase 1b.
- [ ] **Step 2: Run** `cargo test --workspace`, `pnpm build`, `pnpm typecheck`, `pnpm test`, `pnpm test:e2e` and `pnpm measure:memory`. Expected: all pass, and memory is within the gate.
- [ ] **Step 3:** Run CodeRabbit locally against `dev`, fix the valid findings, and re-run Step 2's cargo and pnpm tests.
- [ ] **Step 4:** Check that `gh api user -q .login` prints `rinnyanneko`. Push `claude/modular-exchanges-design`, and open a ready-for-review PR to `dev` titled "Exchange core extraction (phase 1a)". Its body links the spec and this plan, states "OKX behaviour unchanged; only tested against the fake OKX", and ends with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
