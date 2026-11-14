# Rust Vault Runtime and OKX Trading Implementation Plan (Plan 5 of 7)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Rust backend gains the master-password vault and real OKX trading (Demo and live), with the same API, refusals, safety rules and on-disk formats as the Node backend. After this plan no Rust route answers 501.

**Architecture:**
- **Vault:** the runtime actor owns the vault state machine. scrypt runs in `spawn_blocking`, outside the actor.
- **OKX:** a new crate, `crates/mochi-okx`, holds the signed private client and the per-account execution engine.
- **Actor pattern (Plan 4's):**
  - OKX network calls run off the actor.
  - Results come back to the actor, which writes the shared SQLite connection.
  - Each account's operations are serialised through its own queue, like Node's `serialized`.
- **Engine trait:** a small `ExecutionEngine` trait, implemented by the paper engine and the OKX engine, replaces the routes' `paper_only` checks and the runner's direct paper calls.

**Tech Stack:** Rust stable, tokio, axum, reqwest (rustls, no proxy), hmac + sha2, aes-gcm/scrypt (mochi-vault), zeroize; TypeScript/vitest for goldens.

**Spec:** `docs/superpowers/specs/2026-11-07-rust-backend-desktop-design.md`. Facts with file:line refs into the Node reference: `docs/superpowers/plans/2026-11-14-rust-vault-okx-facts.md`. The executor hands each task its sections. Reference code: `apps/server/src/{vault,runtime,app}.ts`, `packages/exchanges/src/{okx-client,okx-engine}.ts`, `packages/core/src/trading/protection.ts`, `apps/server/test/{okx-live,vault,protection}.test.ts`, `docs/OKX_TRADING.md`.

## Global Constraints

- Follow every rule in `AGENTS.md`, above all these:
  - OKX credentials stay in runtime memory, unless the owner saves them sealed. Never write them to disk in plaintext, and never show them to a model, a log, an error or the browser.
  - The master password and data key are never written anywhere.
  - Unlocking reconnects saved OKX keys through the full connect checks and never arms.
  - Never resend after an unknown outcome: resolve it by lookup.
  - Every OKX order passes `preTradeRefusal`.
  - A key that can withdraw is always refused.
  - Exits stay possible when disarmed.
- Node stays the default backend. Node behaviour changes only where a task says so, in both backends at once.
- **Byte-identical with Node:** API paths, statuses, `{error}` texts, refusal codes and texts, response shapes, SQLite rows, the sealed format (`v1.<iv>.<tag>.<body>`, contexts `mochiprep:vault-key`, `provider-key:<id>`, `okx-credentials:<accountId>`) and the saved-credential JSON key order (`apiKey, secretKey, passphrase, environment`). Every text in the facts sheet is copied verbatim.
- A wrong master password answers HTTP **403** `Wrong master password`. Every other vault or exchange refusal answers 400 with the redacted message.
- **Constants:** `ARM_PHRASE = "TRADE REAL MONEY"`, `VAULT_RESET_PHRASE = "DELETE SAVED KEYS"`, `EXCHANGE_SYNC_MS = 5000`, `TIMEOUT_MS = 8000`, `ORDER_NOT_FOUND = "51603"`, `DEFAULT_KDF = {N: 131072, r: 8, p: 1}`.
  - Hosts: `OKX_LIVE` → `https://www.okx.com`, `OKX_DEMO` → `https://openapi.okx.com`, plus the header `x-simulated-trading: 1`.
- **Secret types:** they derive no `Debug` that prints them: the data key, credentials, the client and saved-credential structs. Copies of passwords and keys are zeroized on drop.
- **Tests run offline:** a signed in-process fake OKX and an injected clock; the vault tests use the fast KDF `{N: 1024, r: 8, p: 1}`. The real OKX is never contacted.
- Money is `mochi_decimal::Dec`, never `f64`. Contract sizing and prices follow Node exactly, including plain-decimal formatting on the tick.
- Every task runs `pnpm test`, `pnpm typecheck`, `cargo test --workspace`, `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`. A TypeScript change that alters a golden regenerates it with `pnpm golden` in the same commit.
- **Phase workflow (AGENTS.md):** CodeRabbit, verification, commit, push, ready PR. Do not merge.
- Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. On Windows, `LNK1104` is a transient file lock: retry.

## Review Focus

1. **Lost or unknown order outcome** (network drop after OKX applied the order, or the lookup itself failing): the order stays OPEN, it is never sent again, and sync resolves it by `clOrdId` lookup. Covered in Task 5.
2. **Restart, then unlock:** credentials are gone unless saved sealed. Unlock reconnects saved keys through every connect check (a key that has since gained withdraw permission is refused and listed in `failed`), never arms, and the WAL and DB bytes never hold a credential or AI key after enable plus scrub. Covered in Tasks 3 and 7.
3. **Lost sync while agents run:** agents on that account are paused once (on the healthy-to-unhealthy transition), opening orders are refused, and exits still work. Covered in Task 7.
4. **Vault actions racing each other or a connect:** `Another master password action is in progress`. `remember: true` while locked, or with the password removed mid-connect, gives the exact refusal texts. Covered in Tasks 3 and 7.
5. **Protection on OKX** (every rule carried from the trailing-stop work):
   - no moment without a stop;
   - the owner's own algos are never touched;
   - cleanup only after 2 misses at least 10 s apart;
   - side matching on re-entry;
   - an endless non-2xx body cannot hold a socket.

   Covered in Task 6.

---

### Task 1: Vault crate hardening (carried from Plan 1)

**Files:** Modify `crates/mochi-vault/{Cargo.toml,src/lib.rs,tests/compat.rs}`.

**Interfaces:**
- `pub struct DataKey([u8; 32])`:
  - `Debug` prints `DataKey(…)`;
  - `Zeroize` and `ZeroizeOnDrop`;
  - `from_bytes`, `as_bytes`.
  - `seal`, `open`, `wrap_with_password` and `unwrap_with_password` take or return `DataKey`.
- **Password inputs:** `&str`, normalised into a `Zeroizing<String>` inside `derive`. Derived KEKs are `Zeroizing<[u8; 32]>`.
- **`unwrap_with_password` mirrors Node:** every failure after a successful key derivation (malformed `wrappedKey`, decrypt failure, base64 or length of the inner key) is `VaultError::WrongPassword`. A bad salt or bad KDF parameters stay `Malformed` / `Kdf`. Node gets those as raw errors and answers 400, and so does Rust.
- **scrypt cost cap:** `n ≤ 2^20`, `r ≤ 16`, `p ≤ 16` and `128·n·r ≤ 1 GiB`, else `VaultError::Kdf`. Every value Node writes (`2^17/8/1`, and the test KDF `2^10/8/1`) is accepted.

- [ ] **Step 1:** Write tests (RED):
  - `data_key_debug_does_not_print_bytes`
  - `malformed_wrapped_key_after_derivation_is_wrong_password`
  - `oversized_scrypt_cost_is_refused_before_allocating` (n = 2^30)
  - `node_default_and_test_kdfs_are_accepted`
  - the tamper tests assert their exact variant, with a comment that `open` is deliberately stricter than Node about IV and tag lengths.
- [ ] **Step 2:** Implement; `opens_what_node_sealed` and `wrap_round_trip_and_json_shape` stay green. Run `MOCHI_UPDATE_GOLDEN=1 cargo test -p mochi-vault` only if the fixture writer's output changes, and confirm `golden.test.ts` "Node opens what Rust sealed" passes. Run all checks and commit.

### Task 2: Vault store helpers (Rust)

**Files:** Create `crates/mochi-store/src/vault.rs` (re-exported); tests in `crates/mochi-store/tests/vault_rows.rs`.

**Interfaces:**
- Free functions on `&Connection`, mirroring the Node store's SQL (facts §1.6):
  - `vault_key(id) -> Option<PasswordWrapJson>`, `save_vault_key(id, kind, params)` (upsert keeping `created_at`), `delete_vault()` (the three tables);
  - `sealed_provider_keys() -> Vec<(id, sealed)>`, `save_sealed_provider_key(id, sealed)` (also deletes the plaintext row);
  - `list_sealed_exchange_credentials()`, `save_sealed_exchange_credentials(id, sealed)`, `delete_sealed_exchange_credentials(id)`.
- **Transactions:** grouped writes use the existing transaction mechanism, never a raw `BEGIN` (carried rule).

- [ ] **Step 1:** Tests (RED):
  - **Golden `crates/golden/vault-rows.json`:** a TS step in `generate.ts` drives Node's `SqliteStore` on an in-memory database through a fixed script: save a vault key, upsert it, save provider and sealed provider keys, save and delete sealed exchange credentials, then `deleteVault`. It records the raw rows of the four tables after each step, with time fixed by the script. Rust runs the same script through the new functions and must produce identical rows.
  - Rust reads every row Node wrote.
  - the upsert keeps `created_at`;
  - saving a sealed provider key removes the plaintext row in the same transaction;
  - `delete_vault` empties all three tables;
  - foreign-key cascades remove sealed rows with their provider or account.
- [ ] **Step 2:** Implement, run all checks, commit.

### Task 3: Vault runtime, routes and the scrub decision

**Files:** Create `crates/mochi-server/src/runtime/vault.rs` and `routes/vault.rs`. Modify `runtime/{mod.rs,agents.rs}` (keys, `secrets()`, `vault_status`, `set_key`) and `routes/later.rs` (remove the vault 501s). Extend `validate.rs` with `VaultCreate`, `VaultPassword`, `VaultChange` and `VaultReset`, plus their Zod golden cases in `validation.json`. Modify `apps/server/src/{runtime.ts,db/store.ts}` only for the scrub decision below. Tests in `crates/mochi-server/tests/vault.rs`.

**Interfaces:**
- `RuntimeHandle` methods, exactly per facts §1.3, with their check orders and texts:
  - `enable_vault(password)`
  - `unlock(password) -> {status, failed}`
  - `change_password(current, next)`
  - `disable_vault(password)`
  - `reset_vault(confirmation)` (synchronous, checks only the busy flag)
- **scrypt off the actor:** the actor snapshots what is needed, runs scrypt in `spawn_blocking`, then applies the result in the actor.
- **The `exclusive` guard:** a busy flag on the runtime, set and cleared around the whole action, including the off-actor phase.
- **State:** `vault_status()` gains `Unlocked`; the runtime holds `data_key: Option<DataKey>`.
- **Keys:**
  - `set_key` seals when unlocked, refuses when locked, and stores plaintext when off.
  - Unlock loads every sealed AI key, all-or-nothing as Node does.
  - Provider views and `provider_for` follow.
  - `secrets()` includes loaded keys after unlock, plus the exchange-session secrets (filled in by Task 7).
- **Unlock reconnect:** unlock ends by calling `reconnect_saved_okx(&data_key)`. In this task it is a no-op returning `failed = []`; Task 7 implements it.
- **Scrub decision** (carried row; it changes Node too):
  - Both backends read `wal_checkpoint(TRUNCATE)`'s busy column after `enableVault`'s scrub.
  - When blocked, both emit one warning toast `Deleted key data may stay in the database file until MochiPrep closes.` and still succeed. Node changes `store.scrub()` to return the busy flag.
  - Rust calls `mochi_store::scrub` and maps `CheckpointBlocked` the same way.
  - No other vault action scrubs, matching Node.

- [ ] **Step 1:** Port the Node tests as Rust tests (RED). From `okx-live.test.ts`, the master-password group with OKX parts deferred to Task 7:
  - `seals saved AI keys, scrubs the plaintext, and loads them only after unlock`, using a DB-plus-WAL byte scan helper like `fileHolds`;
  - `keys saved while unlocked are sealed too`;
  - `changes the password without re-entering keys`;
  - `turning it off restores plaintext AI keys…`;
  - `a lost password can only be reset by deleting what it sealed`.

  From `api-integration.test.ts`: `manages the master password over HTTP without echoing it` (403 on a wrong current password, 400 on short or bad phrase).

  Also:
  - the exclusive race test;
  - the scrub-blocked toast test in both languages;
  - the validation golden cases.
- [ ] **Step 2:** Implement; update `later_and_static.rs` / `golden_api.rs` expectations that pinned vault 501s. Run all checks and commit.

### Task 4: `mochi-okx` signed client and fake OKX

**Files:**
- Create `crates/mochi-okx/{Cargo.toml,src/lib.rs,src/client.rs,src/ids.rs}`.
- Create the test support `crates/mochi-okx/src/fake.rs` behind a `fake` feature, also used by mochi-server's dev-dependencies.
- Golden: `apps/server/golden/okx-private.ts` writes `crates/golden/okx-private.json`, registered in `generate.ts`.
- Tests in `crates/mochi-okx/tests/`.

**Interfaces:**
- `Credentials { api_key, secret_key, passphrase }` (zeroized, no `Debug` of the values).
- `Venue {OkxDemo, OkxLive}`.
- `OkxClient::new(credentials, venue, Transport)`:
  - **Transport:** an injectable async trait with method, path, headers and body; the default uses reqwest with rustls and no proxy, timing out after 8 s.
  - **Clock:** injectable as `Fn() -> i64`, so signatures are reproducible.
- **Errors:** `OkxError::{Rejection{code, message}, Unknown(String), Invalid(String)}`. `Display` is exactly Node's (`OKX <code>: <message>`, `No answer from OKX: …`, and so on).
- **Every endpoint in facts §2.1**, with Node's result handling:
  - `get_config`, `get_contract_value`, `get_usdt_balance`, `get_positions`, `set_leverage`;
  - `place_order`, `place_algo` (with the exactly-one-callback guard), `cancel_algos` (at most 10), `get_pending_algos`, `get_algo` (null only on empty data);
  - `cancel_order`, `get_order` (51603 → `None`).
- **Helpers:** `to_inst_id`, `to_cl_ord_id`, `to_algo_cl_ord_id`, `sign`.
- **Fake:** `FakeOkx`, a faithful port of the Node test's `FakeOkx` (facts §5.1):
  - signature and key checks;
  - every handler, at the same 75000 price;
  - every fault knob and recorder, under the same names in snake_case;
  - a `FakeOkx::transport()` that plugs into `OkxClient`.

- [ ] **Step 1:** The golden records, from TS:
  - `sign` and the id hashes over a corpus;
  - the exact request (method, path with query, body bytes, headers except time-dependent ones), with a fixed timestamp injected through a tiny exported test hook on `OkxClient` that does not change Node behaviour;
  - the parsed result or error text for canned responses per endpoint: `code != 0` with and without a row, HTTP 5xx, non-JSON, a network error, and a missing row.

  Rust tests replay it (RED). Port `signs every request, and marks only demo requests as simulated`.
- [ ] **Step 2:** Implement; add `hmac` to the workspace. Run all checks and commit.

### Task 5: ExecutionEngine trait and the OKX engine core

**Files:**
- Create `crates/mochi-okx/src/engine/{mod.rs,refusal.rs,sync.rs,views.rs}`.
- Create `crates/mochi-server/src/runtime/execution.rs`, holding the trait and the paper adapter.
- Modify `runtime/{agent_execution.rs,agents.rs}` and `routes/{orders.rs,accounts.rs}`, replacing `paper_only` and the direct paper calls.
- Tests in `crates/mochi-okx/tests/engine.rs` and `crates/mochi-server/tests/okx_trading.rs`.

**Interfaces:**
- **`ExecutionEngine` trait:** `place_order`, `cancel_order`, `close_position`, `positions`, `open_orders`, `balance`, `daily_risk`, `set_protection` and `protection_of`.
  - Each call is driven by the runtime as Plan 4 drives provider calls: prepare in the actor, run network work in a spawned task with no actor borrow, and apply the result back in the actor through `RuntimeHandle::call`.
  - The paper adapter runs synchronously in the actor.
- **Ordering:** a per-account FIFO serialises OKX operations (Node's `serialized`), and sync takes the same queue.
- **`OkxEngine`, a port of `okx-engine.ts` minus protection** (Task 6):
  - `submit` with `clientOrderId` idempotency, writing the local row before sending;
  - `preTradeRefusal`, with its 15 ordered checks and exact texts, and reduce-only exits passing before the arm, health and limit checks;
  - leverage set before opening orders;
  - contract conversion via `venueContractValue` / `venueLotSize`, with prices in plain decimals on the tick;
  - unknown-outcome handling: the row stays OPEN and is resolved by `get_order` lookup during sync; a failing lookup does not give up; a lookup returning `None` closes it only as Node does;
  - per-order refusals inside a successful response;
  - `sync`, `sync_order`, `to_view`, `to_balance` and `cached_balance`;
  - health, last sync and error state;
  - `close_position` clipped to the size.
- **Live limits:** re-read from the account row on every call.

- [ ] **Step 1:** Port as Rust tests (RED), using `FakeOkx` and an injected clock, the Node groups listed in facts §5.2:
  - "real-money safety" (every test, including `still lets a disarmed account exit, clipped to the position size`);
  - "added pairs";
  - "duplicate protection and ambiguous outcomes" (every test).
- [ ] **Step 2:** Implement. The paper paths behave identically to before: every existing golden and the paper e2e stay green. Run all checks and commit.

### Task 6: OKX protection (carried rules)

**Files:**
- Create `crates/mochi-core/src/trading/protection.rs` (`classify_protection_change`) and `crates/mochi-okx/src/engine/protection.rs`.
- Golden: `crates/golden/protection-classify.json` from `packages/core/src/trading/protection.ts` over the 28 `protection.test.ts` cases plus edge cases.
- Tests in `crates/mochi-okx/tests/protection.rs`.

**Interfaces:**
- **`classify_protection_change(side, before, after) -> ReducesRisk | NeedsArmed`**, golden-checked.
- **OKX protection engine, a port of Node:**
  - `apply_protection`: every changed leg is placed before any is cancelled; on an unknown outcome, resolve by sync and never place twice; `pending` and `ProtectionChangeError` semantics as in Node;
  - `protection_refusal`: 8 ordered checks with exact texts, including `NOT_ARMED: this change increases risk` and both `PROTECTION_PENDING` texts;
  - `record_attached`, with deterministic ids `<entryOrderId>:sl|tp|trail` and `attachAlgoClOrdId`;
  - `reconcile_protection`: newest wins; the re-adopt rule; side matching; missing-position cleanup only after at least 2 misses at least 10 s apart; cancel-pending strays visible and retried; a row closed as `Position closed` is never re-adopted;
  - `external_of`, whose detail is built from numeric fields only and carries the `CANCEL_PENDING_PREFIX` marker;
  - the re-entry refusal while cleanup is pending.

- [ ] **Step 1:** Port every Node test in "protection on OKX" (facts §5.2), plus `entry with a trailing stop sends it in attachAlgoOrds with attachAlgoClOrdId` (RED).
- [ ] **Step 2:** Implement. The engine's protection reaches routes and the runner through the `ExecutionEngine` trait from Task 5. The agent protection-failure cases carried from Plan 4 get Rust tests here. Run all checks and commit.

### Task 7: Runtime orchestration, routes and unlock reconnect

**Files:** Create `crates/mochi-server/src/runtime/exchange.rs` and `routes/exchange.rs`. Modify `runtime/{mod.rs,vault.rs,agents.rs,scheduling.rs}`, `routes/{later.rs,agents.rs,accounts.rs}` and `validate.rs`, adding `OkxConnect` (with the `RiskSettings` default), `Arm`, and their golden cases. Tests in `crates/mochi-server/tests/okx_runtime.rs`.

**Interfaces:**
- `RuntimeHandle` methods:
  - `connect_okx(input, expected_account_id)`, with the 12 ordered steps and exact texts (facts §3). The account row is written before the first sync, as Node does.
  - `disconnect_exchange`.
  - `arm(id, confirmation)` and `disarm(Option<id>)`.
  - `exchange_statuses()`, in the exact key order.
- **Sync loop:** `sync_exchanges` runs inside the 1 s live tick every 5 s. It pauses agents on the transition to unhealthy with the exact toast, and invalidates the 5 topics.
- **Routing:**
  - `execution(account_id)`, with the exact errors.
  - `assert_can_run` gains the armed check for AUTO.
  - `assert_can_execute_signal` exempts `CLOSE` and `PROTECT` (carried row).
- **Account and agent routes:**
  - `DELETE /api/accounts/:id` disconnects first.
  - `POST /api/agents/stop-all` also disarms (carried).
  - The `PUT` protection route keeps an absent `trailingStop` and goes through the gate (carried).
  - `allowTrailingStop` defaults to false (carried; verify that it already holds).
- **Unlock reconnect (real):** sequential, skipping accounts that are missing or already connected, with `failed` holding account names and the exact toast. It never arms.
- **Secrets:** `secrets()` includes each session's `[apiKey, secretKey, passphrase]`.
- **Restart:** sessions are memory-only, so a restart leaves accounts `connected:false, armed:false`.
- **Remaining 501s:** the routes in `later.rs`, `accounts.rs` and `orders.rs` that still return 501 are removed, and the tests that pinned them are updated.
- **Live limits:** `connect_okx` writes `live_limits` as JSON in Node's byte form, including JS number formatting of fractional and exotic values such as `0.5`, `1e-7` and `0.30000000000000004`. This closes the carried "exotic JS numbers in `live_limits`" row; golden cases go in `api-script.json` or `store-rows.json`.

- [ ] **Step 1:** Port as Rust tests (RED):
  - "connecting" (all);
  - "AI agents" (all, including the suggestions-on-a-real-account group);
  - "restart";
  - the OKX parts of "master password": saves OKX keys only on request…; still refuses a saved key that gained withdrawal permission; forgets saved OKX keys on disconnect…; keeps saved OKX keys when reconnected by hand while locked; turning it off … deletes saved OKX keys;
  - `api: protection route on an OKX account goes through the armed gate`;
  - `never exposes credentials through status`;
  - `never shows the exchange credentials to the model`.
- [ ] **Step 2:** Implement. Run all checks, plus `pnpm test:e2e` and `pnpm test:e2e:rust -- e2e/display.spec.ts e2e/paper.spec.ts e2e/demo.spec.ts`. Commit.

### Task 8: Docs, carry-forward, phase verification

**Files:** Modify `AGENTS.md` (Rust section: vault and OKX trading served; no 501s left), `docs/ARCHITECTURE.md`, `docs/OKX_TRADING.md` (one line saying both backends implement it; the UNCONFIRMED list is unchanged and still needs the owner's Demo test) and the carried-forward tables in `docs/superpowers/plans/2026-11-07-rust-accounting-core.md` and `2026-11-07-rust-foundation.md`: mark the Plan 5 rows done, and add any new rows for Plans 6–7.

- [ ] **Step 1:** Update the docs. Record honestly that no Rust OKX path has run against real OKX Demo yet, and that the switch-over Demo test is still the owner's decision (spec section "切換前的 OKX 實測").
- [ ] **Step 2:** Run every check, including both e2e runs. Record the counts.
- [ ] **Step 3:** Final whole-branch review on the most capable model. Then run the CodeRabbit CLI locally against `origin/main` (clone into WSL, `--base-commit <merge-base>`) and fix the valid findings. Push the branch and open a ready PR. Do not merge.
