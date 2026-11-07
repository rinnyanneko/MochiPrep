# Rust Backend Foundation Implementation Plan (Plan 1 of 7)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the Rust workspace and the four pieces every later plan builds on (domain types with JS-exact number formatting, the SQLite store, the vault crypto, and the deterministic replay feed), each proven identical to the TypeScript implementation by golden fixtures.

**Architecture:** A Cargo workspace at the repo root with crates under `crates/`. A TypeScript generator in `apps/server/golden/` runs the existing TS code and writes JSON fixtures to `crates/golden/`. Rust tests must reproduce them exactly, and a vitest test fails when the committed fixtures go stale. The Node backend stays the default and is not changed in behavior.

**Tech Stack:** Rust stable (edition 2021), serde/serde_json, rusqlite (`bundled`), aes-gcm, scrypt, base64, sha2, unicode-normalization, thiserror; TypeScript/vitest/tsx for the generator.

**Spec:** `docs/superpowers/specs/2026-11-07-rust-backend-desktop-design.md`

## Roadmap (each later plan is written when the previous one is done)

| Plan | Delivers | Proven by |
|---|---|---|
| **1. Foundation (this plan)** | workspace, domain types, JS number formatting, store and migrations, vault crypto, replay feed | golden fixtures from TS |
| 2. Accounting core | decimal parity, perp math, fill model, risk engine, paper engine, ledger audit, store CRUD | ported `paper-engine`/`recovery`/core tests and golden scenarios |
| 3. Server and feeds | axum API and SSE with identical routes, auth, embedded web assets, OKX public data, news, instruments, demo and replay | Playwright e2e against the Rust server, unchanged |
| 4. Agents | AI providers, retries, redaction, identical prompts, agent runner, suggestions | ported agent and provider tests |
| 5. Vault runtime and OKX trading | lock/unlock, sealed keys, OKX execution engine, connect checks, unknown-outcome resolution, arming, API contract check against Zod | ported `okx-live` and `vault` tests |
| 6. Launcher | Tauri 2 tray app, terms page, interface window, restart/quit confirmations, no-tray fallback, autostart, data import, single instance | launcher tests and a manual checklist per OS |
| 7. Release and switch-over | 6-target CI, GitHub Releases, updater, memory measurement, README, OKX demo decision, Node backend removal | CI artifacts, memory report, e2e |

## Global Constraints

- Node backend stays the default; nothing in `apps/server/src` or `packages/*/src` changes behavior in this plan.
- Readable Rust for a maintainer who "knows some Rust": mainstream crates, no custom macros, no clever generics or trait tricks, comments explain why.
- Money is never `f64`. Only indicators and the replay tape may use floats (no money appears in this plan).
- Same SQLite schema: migrations are the files in `apps/server/src/db/migrations/`, applied in name order, recorded in `schema_migrations (name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL)`.
- Pragmas on open: `journal_mode = WAL`, `foreign_keys = ON`, `synchronous = NORMAL`, `secure_delete = ON`. New data dirs `0700`, db/wal/shm files `0600` (Unix only).
- Vault format, byte-compatible with `apps/server/src/vault.ts`: scrypt `N=2^17, r=8, p=1`, key length 32, password NFC-normalized; AES-256-GCM with 12-byte IV; sealed string `v1.<iv b64>.<tag b64>.<body b64>` (standard base64 with padding); AAD contexts `mochiprep:vault-key`, `provider-key:<id>`, `okx-credentials:<accountId>`; the wrapped plaintext is the **base64 text** of the 32-byte data key.
- Replay is bit-identical to TS: same PRNG, same float operation order, JS `Math.round` (ties toward +∞) and JS `toFixed` (ties away from zero) semantics.
- Tests run offline. Secrets never appear in panics, `Debug` output, or logs.

## Review Focus

1. **A password that is not NFC-normalized** (e.g. `e` + U+0301 combining accent): must unwrap a vault created from the NFC form. Test in Task 4.
2. **A database Node created at an older schema** (001–006 applied): opening applies only `007_vault.sql` and keeps existing rows. Test in Task 3.
3. **A migration that fails midway**: no partial schema and no `schema_migrations` row for it. Test in Task 3.
4. **Exact-tie rounding** (`0.125.toFixed(2)`, `2.5` with `Math.round`, `-2.5`): matches JS, not Rust's default. Tests in Task 2.
5. **Replay at its edges**: seek below warm-up clamps to 5760, seek past the end clamps to the last bar, `step()` at the end returns nothing, `get_candles` with a limit beyond the data returns what exists. Tests in Task 6.

---

### Task 1: Toolchain, workspace, golden harness, domain types

**Files:**
- Create: `Cargo.toml` (workspace), `rust-toolchain.toml`, `crates/mochi-domain/Cargo.toml`, `crates/mochi-domain/src/lib.rs`, `crates/mochi-domain/src/instruments.rs`, `crates/mochi-domain/src/market.rs`, `crates/mochi-domain/tests/golden_instruments.rs`
- Create: `apps/server/golden/generate.ts`, `apps/server/test/golden.test.ts`, `crates/golden/instruments.json` (generated)
- Modify: `.gitignore` (add `target/`), `apps/server/package.json` (script `"golden": "tsx golden/generate.ts"`), root `package.json` (script `"golden": "pnpm --filter @mochiprep/server golden"`), `AGENTS.md` (Layout: `crates/`; Commands: `cargo test --workspace`, `pnpm golden`)

**Interfaces:**
- Produces (Rust, `mochi_domain`):
  - `pub const DEFAULT_SYMBOLS: [&str; 3] = ["BTCUSDT", "ETHUSDT", "SOLUSDT"]`
  - `pub struct InstrumentSpecification` with every field of the TS interface, `#[serde(rename_all = "camelCase")]`, decimal fields as `String`, `max_leverage: u32`, `funding_interval_hours: u32`, `market_type: String`
  - `pub fn default_instruments() -> Vec<InstrumentSpecification>` (order = `DEFAULT_SYMBOLS`)
  - `pub fn default_instrument(symbol: &str) -> Option<InstrumentSpecification>`
  - `pub enum DataSource { Replay, Live }` serialized `"REPLAY"`/`"LIVE"`
  - `pub enum Timeframe { M1, M5, M15, H1, H4 }` serialized `"1m" "5m" "15m" "1h" "4h"`, `pub fn ms(self) -> i64` (60_000, 300_000, 900_000, 3_600_000, 14_400_000)
  - `pub struct Candle { time: i64, open: f64, high: f64, low: f64, close: f64, volume: f64, closed: bool }` (camelCase serde, all fields `pub`)
  - `pub struct Quote { source: DataSource, symbol: String, bid: String, ask: String, last: String, mark: String, mark_estimated: bool, funding_rate: Option<String>, next_funding_time: Option<i64>, ts: i64 }` (camelCase serde, all fields `pub`)
- Produces (TS, `apps/server/golden/generate.ts`): `export async function buildGolden(): Promise<Record<string, unknown>>`, which maps a file name (e.g. `"instruments.json"`) to its content. Running the file writes each entry to `crates/golden/<name>`: a `Buffer` value as raw bytes, anything else as `JSON.stringify(value, null, 1) + "\n"`. The freshness test skips the names in `export const RANDOMIZED = new Set<string>()`, which holds entries whose bytes legitimately differ per run. Later tasks add entries to the map and to the set.
- Produces (Rust test helper, duplicated per crate as a 3-line fn, not a crate): `fn golden(name: &str) -> serde_json::Value` reading `concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/", name)`.

- [ ] **Step 1: Install Rust** via rustup (stable, default profile). On Windows this needs the MSVC build tools rustup offers. Ask the user before installing. Verify: `cargo --version` prints a version.
- [ ] **Step 2: Create the workspace.** Root `Cargo.toml` with `[workspace] members = ["crates/*"]`, `resolver = "2"`, and `[workspace.dependencies]` pinning serde (derive), serde_json, thiserror. `rust-toolchain.toml`: `channel = "stable"`, components `rustfmt`, `clippy`. Set `rust-version` in `[workspace.package]` to the version installed in Step 1. Add `target/` to `.gitignore`.
- [ ] **Step 3: Write the generator and freshness test.** `generate.ts` puts `"instruments.json": DEFAULT_SYMBOLS.map((s) => INSTRUMENTS[s])` in `buildGolden()`. `golden.test.ts`: for every entry of `await buildGolden()` not in `RANDOMIZED`, the committed file parses to `toEqual` the fresh value, with the failure message `crates/golden/<name> is stale: run pnpm golden`.
- [ ] **Step 4: Run `pnpm golden`**, then `pnpm vitest run apps/server/test/golden.test.ts`. Expected: PASS, and `crates/golden/instruments.json` exists.
- [ ] **Step 5: Write the failing Rust test** `golden_instruments.rs::default_instruments_match_typescript`: `assert_eq!(serde_json::to_value(default_instruments()).unwrap(), golden("instruments.json"))`. Also `timeframe_ms_and_names`: `Timeframe::H4.ms() == 14_400_000` and `serde_json::to_string(&Timeframe::M15).unwrap() == "\"15m\""`.
- [ ] **Step 6: Run `cargo test -p mochi-domain`.** Expected: FAIL (items not defined).
- [ ] **Step 7: Implement the Interfaces block.** Copy the values from `INSTRUMENTS` in `packages/shared/src/domain.ts` (`perp('BTC','0.0001','0.1','0.01')`, `perp('ETH','0.001','0.01','0.1')`, `perp('SOL','0.01','0.01','1')`; `maxLeverage 100`, `maintenanceMarginRate "0.005"`, `fundingIntervalHours 8`, `venueLotSize "0.01"`, `contractSize "1"`, `minQuantity = quantityStep`).
- [ ] **Step 8: Run `cargo test -p mochi-domain` and `pnpm test`.** Expected: both PASS.
- [ ] **Step 9: Commit** with the message `Add Rust workspace, domain types and golden fixture harness`.

### Task 2: JS-exact number formatting

**Files:**
- Create: `crates/mochi-domain/src/jsnum.rs`, `crates/mochi-domain/tests/golden_jsnum.rs`, `crates/golden/jsnum.json` (generated)
- Modify: `apps/server/golden/generate.ts`, `crates/mochi-domain/src/lib.rs` (`pub mod jsnum;`)

**Interfaces:**
- Produces: `pub fn js_round(x: f64) -> f64` (JS `Math.round`), `pub fn js_to_fixed(x: f64, digits: usize) -> String` (JS `Number.prototype.toFixed` for `|x| < 1e21`, `digits <= 20`)

- [ ] **Step 1: Add the generator entry.** `"jsnum.json": { round: ROUND_CASES.map((x) => [x, Math.round(x)]), toFixed: FIXED_CASES.map(([x, d]) => [x, d, x.toFixed(d)]) }` with `ROUND_CASES = [0.5, 1.5, 2.5, -0.5, -1.5, -2.5, 2.4999999999999996, 0.49999999999999994, -0.49999999999999994, 1e15 + 0.5, 0, 7.000000000000001]` and `FIXED_CASES = [[0.125, 2], [0.375, 2], [-0.125, 2], [1.005, 2], [2.5, 0], [-2.5, 0], [68000, 1], [1e-7, 6], [0.0000005, 6], [123.456, 3], [0.1 + 0.2, 17], [-0, 2], [99.995, 2], [0.0005, 6], [-0.0005, 6], [-1e-7, 6], [12345.6789, 0]]`. Run `pnpm golden`.
- [ ] **Step 2: Write the failing test** `golden_jsnum.rs::matches_javascript`: for every round case `assert_eq!(js_round(x).to_bits(), expected.to_bits(), "{x}")` (compare `-0` as `0.0`: assert `js_round(x) == expected`); for every toFixed case `assert_eq!(js_to_fixed(x, d), expected, "{x} {d}")`.
- [ ] **Step 3: Run `cargo test -p mochi-domain jsnum`.** Expected: FAIL (not defined).
- [ ] **Step 4: Implement.** `js_round`: `let f = x.floor(); if x - f >= 0.5 { f + 1.0 } else { f }` (the subtraction is exact). `js_to_fixed`: JS picks the larger `n` on an exact tie, which means away from zero for the magnitude. Rust's `format!` breaks ties to even. Format the exact value and fix only true ties:

```rust
// Rust formats an f64 with its exact decimal expansion, ties to even; JS toFixed takes the
// larger magnitude on an exact tie. A tie is exact only when the expansion ends right after
// a 5 in position digits+1, so check that and round the magnitude up by hand.
let exact = format!("{:.1100}", x.abs());          // exact: an f64 has at most 1074 fraction digits
let (int_part, frac) = exact.split_once('.').unwrap();
let tie = frac.as_bytes()[digits] == b'5' && frac[digits + 1..].bytes().all(|b| b == b'0');
let mut s = if tie { add_one_ulp_of_last_digit(int_part, &frac[..digits]) } else { format!("{:.*}", digits, x.abs()) };
if x < 0.0 { s.insert(0, '-'); }                    // JS: sign whenever x < 0, so -1e-7 -> "-0.000000", -0 -> "0.00"
```

`add_one_ulp_of_last_digit` increments the decimal string `int_part.frac[..digits]` by one unit in its last place, carrying through `9`s. With `digits == 0`, the result has no decimal point, as in JS.
- [ ] **Step 5: Run `cargo test -p mochi-domain`.** Expected: PASS.
- [ ] **Step 6: Commit** with the message `Add JS-exact Math.round and toFixed for Rust`.

### Task 3: SQLite store and migrations

**Files:**
- Create: `crates/mochi-store/Cargo.toml`, `crates/mochi-store/src/lib.rs`, `crates/mochi-store/tests/migrations.rs`, `crates/golden/schema.json` and `crates/golden/node-created.sqlite` (generated)
- Modify: `apps/server/golden/generate.ts`

**Interfaces:**
- Produces (`mochi_store`):
  - `pub const MIGRATIONS: &[(&str, &str)]`, `(file name, include_str!("../../../apps/server/src/db/migrations/<file>"))` for 001–007 in order
  - `pub fn open_database(path: &Path) -> Result<rusqlite::Connection, StoreError>`
  - `pub fn open_in_memory() -> Result<rusqlite::Connection, StoreError>`
  - `pub fn scrub(conn: &Connection) -> Result<(), StoreError>` (`PRAGMA wal_checkpoint(TRUNCATE)`; callers only call it outside a transaction)
  - `pub enum StoreError` (thiserror) with `Sqlite(#[from] rusqlite::Error)`, `Io(#[from] std::io::Error)`, `Migration { name: String, source: rusqlite::Error }`
  - `#[doc(hidden)] pub fn migrate_with(conn: &Connection, migrations: &[(&str, &str)]) -> Result<(), StoreError>`, the migration runner, exposed so tests can feed it a subset or a broken migration

- [ ] **Step 1: Add generator entries.**
  - `"schema.json"`: open `openDatabase(':memory:')` and record `SELECT type, name, tbl_name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name` plus `SELECT name FROM schema_migrations ORDER BY name`.
  - `"node-created.sqlite"`: a special entry the generator writes as a file rather than JSON. Create it with `openDatabase(tmp)`, insert one account row and one provider row (fixed ids `acct_golden`, `prov_golden`, fixed timestamps), run `PRAGMA wal_checkpoint(TRUNCATE)`, close, then copy the main file. Add it to `RANDOMIZED`, since SQLite bytes vary.
  - Run `pnpm golden`.
- [ ] **Step 2: Write the failing tests** in `migrations.rs`:
  - `fresh_schema_matches_node`: `open_in_memory()`, run the same two queries, `assert_eq!` against `schema.json`.
  - `migration_list_matches_directory`: the names in `MIGRATIONS` equal the sorted `.sql` files in `apps/server/src/db/migrations`, so a new migration cannot be forgotten.
  - `opens_node_database_without_reapplying`: copy `node-created.sqlite` to a tempdir, `open_database`, assert 7 rows in `schema_migrations` with the original `applied_at` values unchanged, and assert `acct_golden` is readable.
  - `older_schema_gets_only_the_new_migration`: `migrate_with(conn, &MIGRATIONS[..6])`, insert a row, then `migrate_with(conn, MIGRATIONS)`. Assert that only `007_vault.sql` was added and the row survives.
  - `failed_migration_leaves_nothing`: `migrate_with(conn, &[("999_bad.sql", "CREATE TABLE t(x); SELECT * FROM nope;")])` returns `Err(StoreError::Migration { name, .. })` with `name == "999_bad.sql"`, table `t` does not exist, and no `schema_migrations` row was written.
  - `pragmas_are_set`: `journal_mode` is `wal` (on a file db), `foreign_keys` is 1, `secure_delete` is 1.
  - `scrub_empties_the_wal`: write a row to a file db, `scrub`, and the `-wal` file length is 0.
- [ ] **Step 3: Run `cargo test -p mochi-store`.** Expected: FAIL.
- [ ] **Step 4: Implement.** Use rusqlite with the `bundled` feature. Run each migration inside `BEGIN`/`COMMIT` with `execute_batch`, plus the insert into `schema_migrations` (`applied_at` = Unix ms). On error, roll back and return `Migration`. Create the parent directory with mode 0700 and chmod the db, wal and shm files to 0600 under `#[cfg(unix)]`.
- [ ] **Step 5: Run `cargo test -p mochi-store`.** Expected: PASS.
- [ ] **Step 6: Commit** with the message `Add Rust SQLite store sharing the Node migrations`.

### Task 4: Vault crypto, byte-compatible with Node

**Files:**
- Create: `crates/mochi-vault/Cargo.toml`, `crates/mochi-vault/src/lib.rs`, `crates/mochi-vault/tests/compat.rs`, `crates/golden/vault.json` (generated by TS), `crates/golden/rust-vault.json` (generated by Rust)
- Modify: `apps/server/golden/generate.ts`, `apps/server/test/golden.test.ts`

**Interfaces:**
- Produces (`mochi_vault`):
  - `pub struct KdfParams { pub n: u32, pub r: u32, pub p: u32 }` with serde names `"N"`, `"r"`, `"p"`; `pub const DEFAULT_KDF: KdfParams = KdfParams { n: 131_072, r: 8, p: 1 }`
  - `pub struct PasswordWrap { pub salt: String, pub kdf: KdfParams, pub wrapped_key: String }` (camelCase serde: `wrappedKey`). This is the JSON stored in `vault_keys.params`.
  - `pub type DataKey = [u8; 32]`; `pub fn new_data_key() -> DataKey`
  - `pub fn seal(key: &DataKey, plaintext: &str, context: &str) -> String`
  - `pub fn open(key: &DataKey, sealed: &str, context: &str) -> Result<String, VaultError>`
  - `pub fn wrap_with_password(data_key: &DataKey, password: &str, kdf: KdfParams) -> Result<PasswordWrap, VaultError>`
  - `pub fn unwrap_with_password(wrap: &PasswordWrap, password: &str) -> Result<DataKey, VaultError>`. These two are CPU-heavy, and Plan 5 calls them through `tokio::task::spawn_blocking`.
  - `pub fn provider_context(id: &str) -> String` → `provider-key:<id>`; `pub fn exchange_context(account_id: &str) -> String` → `okx-credentials:<id>`; `pub const WRAP_CONTEXT: &str = "mochiprep:vault-key"`
  - `pub enum VaultError { WrongPassword, Malformed, Decrypt, Kdf }`, where the `Display` for `WrongPassword` is `"Wrong master password"`. No variant carries key or plaintext material.

- [ ] **Step 1: Add the generator entry `"vault.json"`** and add it to `RANDOMIZED`. Use `PASSWORD = 'mochi-pässwörd-é'` and `FAST = { N: 1024, r: 8, p: 1 }`, and record `{ password, passwordNfc: PASSWORD.normalize('NFC'), dataKeyHex, wrap: await wrapWithPassword(dataKey, PASSWORD, FAST), sealed: [{ context: 'provider-key:prov_1', plaintext: 'sk-golden-ai-key', sealed }, { context: 'okx-credentials:acct_1', plaintext: '{"apiKey":"k","secretKey":"s","passphrase":"p","environment":"OKX_DEMO"}', sealed }] }`.
- [ ] **Step 2: Write the failing tests** in `compat.rs`:
  - `opens_what_node_sealed`: unwrap `vault.json`'s wrap with `password` and with `passwordNfc`, get the same key, and `hex(key) == dataKeyHex`. Every sealed entry opens to its plaintext.
  - `wrong_password_is_reported_as_such`: `unwrap_with_password(&wrap, "nope")` returns `Err(WrongPassword)`.
  - `context_and_tampering_are_refused`: opening with the other entry's context, with a flipped body byte, or with `v2.` instead of `v1.` returns an `Err` and does not panic.
  - `writes_fixture_for_node`: when the env var `MOCHI_UPDATE_GOLDEN=1` is set, write `rust-vault.json` with the same shape as `vault.json`, made by Rust from a fresh data key, `FAST` and the same password. Otherwise do nothing.
  - `round_trip`: seal and open with a fresh key, and the sealed string starts with `v1.` and has 4 dot-separated parts.
- [ ] **Step 3: Run `cargo test -p mochi-vault`.** Expected: FAIL.
- [ ] **Step 4: Implement** with the crates `aes-gcm`, `scrypt` (`Params::new(log2(n), r, p, 32)`; `n` must be a power of two, otherwise return `Kdf`), `base64` (`STANDARD`), `unicode-normalization` (`.nfc()`) and `rand`/`getrandom` for the IV and key. Map a failed GCM tag check during unwrap to `WrongPassword`. Make `Debug` for any struct holding a key print `[redacted]`.
- [ ] **Step 5: Run `MOCHI_UPDATE_GOLDEN=1 cargo test -p mochi-vault`, then `cargo test -p mochi-vault`.** Expected: PASS, and `crates/golden/rust-vault.json` exists.
- [ ] **Step 6: Add the reverse check** to `golden.test.ts` as `'Node opens what Rust sealed'`: `unwrapWithPassword(rustVault.wrap, rustVault.passwordNfc)` hex-equals `rustVault.dataKeyHex`, and each sealed entry opens with `open` from `apps/server/src/vault.ts`. Run `pnpm vitest run apps/server/test/golden.test.ts`. Expected: PASS.
- [ ] **Step 7: Commit** with the message `Add Rust vault crypto compatible with Node's format`.

### Task 5: Deterministic replay tape

**Files:**
- Create: `crates/mochi-market/Cargo.toml`, `crates/mochi-market/src/lib.rs`, `crates/mochi-market/src/tape.rs`, `crates/mochi-market/tests/golden_tape.rs`, `crates/golden/tape.json` (generated)
- Modify: `apps/server/golden/generate.ts`

**Interfaces:**
- Consumes: `mochi_domain::{Candle, default_instrument, jsnum::{js_round, js_to_fixed}}`
- Produces (`mochi_market::tape`):
  - `pub const TAPE_START: i64 = 1_767_225_600_000` (2026-01-01T00:00:00Z); `pub const TAPE_MINUTES: usize = 25_920`; `pub const WARMUP_MINUTES: usize = 5_760`
  - `pub struct Tape { pub open: Vec<f64>, pub high: Vec<f64>, pub low: Vec<f64>, pub close: Vec<f64>, pub volume: Vec<f64> }` with `pub fn len(&self) -> usize`, `pub fn time(&self, i: usize) -> i64`, `pub fn candle(&self, i: usize) -> Candle`
  - `pub fn generate_tape(symbol: &str, minutes: usize) -> Option<Tape>`: `None` for non-default symbols

- [ ] **Step 1: Add the generator entry `"tape.json"`.** Record `{ head: { [symbol]: first 64 candles of generateTape(symbol) }, sha256 }`. The `sha256` is computed over, for each symbol in `DEFAULT_SYMBOLS` and each column in the order open, high, low, close, volume of the full default tape, the raw little-endian bytes of that column's `Float64Array`. Run `pnpm golden`. The 64-candle head of BTCUSDT also has to keep hashing to `ed818a88…` in the existing market-data test, which is a sanity check.
- [ ] **Step 2: Write the failing tests** in `golden_tape.rs`:
  - `head_matches_typescript`: for every symbol and `i < 64`, `serde_json::to_value(tape.candle(i))` field values equal the golden ones. Compare floats with `==`, not by formatting, because serde writes `68000.0` where JS writes `68000`.
  - `full_tape_hash_matches_typescript`: the same SHA-256 over `f64::to_le_bytes` of the columns.
  - `unknown_symbol_has_no_tape`: `generate_tape("DOGEUSDT", 10).is_none()`.
- [ ] **Step 3: Run `cargo test -p mochi-market`.** Expected: FAIL.
- [ ] **Step 4: Implement by porting `packages/market-data/src/synthetic.ts` line by line.** The seeds are BTC `0x6d6f6368`/68 000/42, ETH `0x69707265`/3 400/520 and SOL `0x70303031`/150/6 400. Keep every float expression in the TS operand order, since Rust does not fuse multiply-adds by default and the bits only match if the order matches. Pitfalls to port exactly:
  - `mulberry32` uses `i32` `wrapping_add` and `wrapping_mul`, with `>>>` as a `u32` logical shift on the reinterpreted bits, and returns `(t >>> 0) / 4294967296` as `f64`.
  - `regimeLeft-- <= 0` compares first, then decrements.
  - The 4 sub-steps each call `gaussian()`, which is 6 `random()` calls summed left to right, minus 3.
  - `roundTick(v) = js_to_fixed(js_round(v / tick) * tick, decimals).parse::<f64>()`. The volume is `js_to_fixed(volume, 3).parse()`.
- [ ] **Step 5: Run `cargo test -p mochi-market`.** Expected: PASS.
- [ ] **Step 6: Commit** with the message `Port the deterministic replay tape to Rust`.

### Task 6: Replay feed (quotes, candles, funding, cursor)

**Files:**
- Create: `crates/mochi-market/src/replay.rs`, `crates/mochi-market/tests/golden_replay.rs`, `crates/golden/replay.json` (generated)
- Modify: `apps/server/golden/generate.ts`, `crates/mochi-market/src/lib.rs`

**Interfaces:**
- Consumes: `tape::*` and `mochi_domain::{Quote, Candle, Timeframe, DataSource, DEFAULT_SYMBOLS}`
- Produces (`mochi_market::replay::ReplayMarketData`):
  - `pub fn new(minutes: Option<usize>) -> Self`, where the cursor starts at `WARMUP_MINUTES`
  - `pub fn len(&self) -> usize`, `pub fn position(&self) -> usize`, `pub fn at_end(&self) -> bool`, `pub fn first_playable_time(&self) -> i64`, `pub fn end_time(&self) -> i64`, `pub fn now(&self) -> i64`
  - `pub fn seek(&mut self, index: i64)`, clamped to `[WARMUP_MINUTES, len - 1]`
  - `pub fn step(&mut self) -> Vec<Quote>`, which is empty at the end and otherwise has one quote per default symbol in `DEFAULT_SYMBOLS` order. Listener fan-out is Plan 3's concern.
  - `pub fn get_quote(&self, symbol: &str) -> Option<Quote>`
  - `pub fn get_candles(&self, symbol: &str, timeframe: Timeframe, limit: usize) -> Vec<Candle>`

- [ ] **Step 1: Add the generator entry `"replay.json"`.** Use `r = new ReplayMarketData()` and record:
  - `meta`: `len`, `firstPlayableTime`, `endTime`, `nowAtStart`
  - `steps`: three `r.step()` results from a fresh instance
  - `snapshots`: for each cursor in `[5760, 9000, 20000, 25919]`, `seek(cursor)`, then record `{ cursor, now, quotes: DEFAULT_SYMBOLS.map(getQuote), candles: { BTCUSDT: {tf: getCandles('BTCUSDT', tf, 50)}, SOLUSDT: {…} } }` for every timeframe
  - `edges`: the positions after `seek(-5)` and after `seek(99999)`, the length of `step()` after seeking to the end, and `getCandles('BTCUSDT','4h',100000).length` at cursor 5760
  - Run `pnpm golden`.
- [ ] **Step 2: Write the failing tests** in `golden_replay.rs`: `meta_matches`, `steps_match`, `snapshots_match` (quotes compared as whole JSON values, since they are all strings and integers; candles compared field by field with float `==`), and `edges_match`.
- [ ] **Step 3: Run `cargo test -p mochi-market replay`.** Expected: FAIL.
- [ ] **Step 4: Implement by porting `packages/market-data/src/replay.ts`.** Constants are `FUNDING_MS = 28_800_000` and `SPREAD_FRACTION = 0.00005`. Pitfalls:
  - `half = max(tick, js_round(close * SPREAD_FRACTION / tick) * tick)`, keeping the TS grouping `(close * SPREAD) / tick`.
  - `bid` and `ask` use `js_to_fixed(close ∓ half, decimals)`, where `decimals` is the number of fraction digits of `priceTick`.
  - The funding rate is `js_to_fixed(clamp(0.0001 + change * 0.01, -0.0005, 0.0005), 6)` with `back = cursor.saturating_sub(480)`.
  - `next_funding_time = (ts / FUNDING_MS + 1) * FUNDING_MS` in `i64`.
  - In `get_candles`, the volume is summed from the newest bar backward, in exactly the TS order, then `js_to_fixed(sum, 3).parse()`. `closed = bucket + size <= now`. The output is reversed to oldest first.
- [ ] **Step 5: Run `cargo test --workspace` and `pnpm test`.** Expected: both PASS.
- [ ] **Step 6: Commit** with the message `Port the replay feed to Rust with golden parity`.

### Task 7: Foundation wrap-up

**Files:**
- Modify: `AGENTS.md` (a Rust section: crates under `crates/`, golden fixtures, and the rule "a TS change that alters a golden file must regenerate it and keep the Rust port in step"), `docs/ARCHITECTURE.md` (a short "Rust backend (in progress)" paragraph that claims nothing beyond what exists)

- [ ] **Step 1: Run the full verification:** `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, `pnpm test`, `pnpm typecheck`. Expected: all clean.
- [ ] **Step 2: Update the docs** as listed in Files.
- [ ] **Step 3: Commit** with the message `Document the Rust foundation and golden fixtures`.

---

## Carried forward from Plan 1 reviews

Real findings that nothing in Plan 1 builds on. The plan named for each must pick it up.

| Plan | Item |
|---|---|
| 2 | `migrations.rs` `failed_migration_leaves_nothing`: also assert `conn.is_autocommit()` |
| 3 | `js_to_fixed`: return JS strings for NaN/±Infinity instead of panicking; `debug_assert!(digits <= 20)` |
| 3 | `js_round` returns +0 where JS gives -0 for results in (-0.5, 0); decide before it reaches API output |
| 3 | `ReplayMarketData::new`: validate `minutes > WARMUP_MINUTES`, return `Result`; test a short tape stepped to its last index (Runtime passes `replayMinutes`, Node tests use 5_800) |
| 3 | `Tape` public columns can diverge in length; `candle()` indexes unchecked |
| 5 | `unwrap_with_password` maps malformed wrap data to `Malformed`; Node maps every post-derivation failure to `WrongPasswordError` (HTTP 403). Make the API responses identical |
| 5 | `DataKey` is a bare `[u8; 32]`, so `Debug` prints key bytes: make it a newtype with a redacting `Debug`, add `zeroize` for password copies and derived keys |
| 5 | Cap the scrypt cost read from stored params (a tampered DB could abort on a huge allocation); accept every value Node can write |
| 5 | Tamper tests assert only `is_err()`: assert the variant; comment that `open` is deliberately stricter than Node |
| 7 | Pin `rust-toolchain.toml` to an exact version when CI lands (`clippy -D warnings` breaks on new lints) |
| 7 | CI: regenerate `rust-vault.json` (`MOCHI_UPDATE_GOLDEN=1 cargo test -p mochi-vault`) before the Node golden test; make the writer test `#[ignore]` + `--ignored` |
| 7 | Run the Unix permission tests (`new_database_files_are_private`, `sqlite_gives_wal_and_shm_the_main_file_mode`) on Linux and macOS CI. They passed once on Linux (WSL) when added; the 0700 data-directory path and macOS are still unexercised |
| any | `golden_tape.rs` head test compares `Option<i64>` times; comment host-endian hashing in generate.ts; `from_utf8_lossy` in jsnum; mochi-market lib.rs doc |
