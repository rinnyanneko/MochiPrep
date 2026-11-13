# Rust AI Providers and Agents Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Complete Plan 4 of the Rust backend port: providers, agent analysis/execution, suggestions, scheduling and demo mode, preserving the Node API.

**Architecture:** Provider HTTP work and candle/news fetches run outside the single-owner runtime actor. Analysis snapshots leave the actor; validated results return for current-state risk checks and execution. Replay advances one bar, awaits all due cycles, then advances again; live feeds and HTTP continue during model waits. Small pure modules own indicators, prompt construction, output parsing and mock decisions.

**Tech Stack:** Existing Rust workspace, Tokio, reqwest, Axum, SQLite and TypeScript-generated golden fixtures.

**Spec:** `docs/superpowers/specs/2026-11-07-rust-backend-desktop-design.md`

## Global Constraints

- Node remains the default backend. React and existing e2e assertions stay unchanged.
- Match TypeScript prompt strings, API paths/status/bodies, SSE shapes and persisted SQLite format. Reference implementation is current `packages/ai-providers`, `packages/agents`, and `apps/server/src/{app,runtime}.ts`.
- Rust ledger and money arithmetic uses `mochi_decimal::Dec`, never `f64` or `rust_decimal`. Indicators, mock analysis and model numeric output use numbers as in TypeScript.
- AI output is untrusted. Only the runner may turn it into orders after schema validation and the risk engine.
- Never include provider keys or extra-header values in browser payloads, logs, recorded responses or errors. Redact BEFORE truncation; minimal corresponding Node corrections and cutoff regression tests are authorized when the reference leaks secret prefixes. Sanitize parsed decision text and schema diagnostics before persistence as well. OKX credentials are outside this phase.
- Vault mutation and OKX trading remain Plan 5. Locked vaults fail closed for remote AI; plaintext provider keys remain owner-controlled when vault is OFF.
- Tests use deterministic local transports and replay, never paid models or real trading.
- Follow AGENTS.md phase workflow: CodeRabbit, verification, commit, push, ready PR; do not merge.

## Review Focus

- Model HTTP stalled while replay is stepping: HTTP and live quotes must remain responsive; no duplicate cycles or dropped replay bars.
- Pause/stop/delete/reset while analysis is in flight: lifecycle cannot resurrect a stopped agent or write orders for deleted ownership.
- Untrusted nested/Unicode model output and echoed secret headers: strict schema and redaction before persistence/observability.
- PROTECT suggestions after protection changes: expire stale snapshots, preserve trailing absent/null meaning, report partial/pending outcomes honestly.
- Restart and locked vault: no automatic trading, no leaked loaded/plaintext keys, remote provider failures recorded and auto-paused as Node.

### Task 1: Provider transport and pure analysis primitives

**Files:** Create `crates/mochi-ai/{Cargo.toml,src/lib.rs,src/types.rs,src/remote.rs,src/mock.rs,src/redact.rs,src/indicators.rs,src/prompt.rs,tests/parity.rs,tests/remote.rs}`; modify golden generator and freshness test to add `crates/golden/ai.json`. Additional focused test files permitted. Reuse domain types; export public types from mochi-ai for later runtime tasks. No server/store changes in this task.

**Interfaces:** Produce `ProviderKind`, `ProviderConfig`, `MarketContext`, `AiRequest`, `AiResponse`, `ProviderError` (code/message/critical), object-safe `AiProvider` with boxed Send future `complete`; `RemoteProvider`, `MockProvider`; `redact_secrets`, `indicator_snapshot`, `build_system_prompt(bool)`, `build_user_prompt(&MarketContext,bool)`. Match TS field semantics; typed context uses existing Candle/Quote/PositionView/AccountBalance/RiskSettings/NewsItem, indicator ordered map. Expose deterministic injectable HTTP transport or local mock server seam, and configurable retry delays/timeouts for tests. Do not derive Debug on secret-bearing config/request clients. Use existing JS number/date helpers where applicable.

- [ ] Read source references and write TS fixtures covering indicator warmup/flat/trend/oscillation, each mock strategy/action, complete prompts with news, strategy closing tag, external/cancelling protection and both trailing switches.
- [ ] Write Rust parity tests consuming those fixtures, plus local HTTP tests for all provider request paths, headers/bodies, success extraction, blank/malformed responses, AUTH/429/5xx, retries, timeout and echoed-secret redaction. Observe RED before implementation (missing crate/function is acceptable initial failure).
- [ ] Port the pure primitives and remote protocols from TS. Request timeout 60s; retry NETWORK/RATE_LIMIT/SERVER only with 1s,3s waits; preserve attempt suffix. OpenAI-compatible family JSON mode only OPENAI/DEEPSEEK. Reserved auth/content-type headers override extras. Mock provider stays offline, deterministic and keyless. No accounting calculations here.
- [ ] Run `cargo test -p mochi-ai`, `pnpm golden`, and relevant golden freshness test. Expected: all parity and transport tests pass without external network. Run fmt/clippy for crate. Self-review diff and commit task.

### Task 2: Persisted agent/provider state and strict output parsing

**Files:** Extend `crates/mochi-store/src/rows.rs` (split writes into `ai.rs` if helpful); create `crates/mochi-server/src/agent_parse.rs`; extend server validation with ProviderInput, ProviderKey, ProviderTest and AgentInput schemas; tests in store/server and golden generator.

**Interfaces:** Store get/insert/save/delete agent and provider configuration, provider key load/save/delete, get/insert/save decision JSON with indexed fields matching Node. Keep existing GET wire output intact. Parser `parse_decision(&str) -> Result<AiDecisionData,String>` is strict TS AiDecisionSchema equivalent and can reuse server schema validation. Expose typed AgentInput and ProviderInput results for Task4; preserve extraHeaders omission and AgentInput SIGNAL default versus runtime createAgent AUTO default. Runtime consumes typed rows::Agent and mochi_ai::ProviderConfig; decisions may use typed struct or validated serde_json::Value where preserving omission/order needs it.

- [ ] Write RED roundtrip tests against Node-created database and fixtures: absent/null trailing, unknown keys, fenced JSON, numeric strings rejected, required nullable keys, invalid enums/finite/ranges and exact errors.
- [ ] Add exact API schema golden cases for provider header normalization/reserved names/omission, required nullable prices, agent defaults, numeric bounds and unknown fields. Reuse validate.rs Fields/check helpers; model parse errors use root rather than input for root issues. Implement writes from Node Store SQL; keys never returned by provider views. Provider deletion and agent deletion preserve Node cascade semantics; transactions for grouped state changes. Add parser with exact TS validation/error contract.
- [ ] Run focused store/parser tests and golden freshness. Expected: pass, existing read fixtures unchanged. Commit task after self-review.

### Task 3: Runner and asynchronous scheduling

**Files:** Create `crates/mochi-server/src/runtime/agents.rs` and focused cycle/execution modules; modify runtime/mod.rs and replay integration, add `tests/agents.rs` and test helpers. Core risk fixes only for carried Plan4 issues. Required reference tests: apps/server/test/agent-runner.test.ts, agent-signals.test.ts, agent-news.test.ts; every runner/signal scenario must have a Rust equivalent, with exchange-specific protection failures covered by an execution seam or explicitly carried to Plan5 when no exchange engine exists.

**Interfaces:** RuntimeHandle async `run_cycle(id,trigger)` and `execute_signal(id)`; actor preparation/completion phases preserve in-flight exclusion. Lifecycle start/pause/resume/stop/stop_all, expiry/dismissal and status updates accessible to routes. Use Task1 providers and Task2 rows. Extend RuntimeOptions with injected provider factory for tests. Replay batching awaits due agents at EACH bar without holding runtime actor. Live scheduling ticks each second independently of 60s equity sampling.

- [ ] Write RED tests for analyze-only, auto order, manual suggestion/revalidation/expiry, HOLD, invalid response, AUTH immediate pause, three consecutive failures, no-overlap, stale data, news failure, restart disarm, trailing opt-in, PROTECT unchanged/changed/snapshot expiry, late result lifecycle/deletion and slow provider responsiveness.
- [ ] Port runner.ts exactly including 30 closed candle minimum, 8000 raw-response limit, stale-entry cancellation, signal replacement, cost estimation, finishCycle latest-state logic, effectiveRisk, toOrder and protection behavior. Reject risk `input` failures as errors. Reject nonfinite risk inputs and checked quote-age subtraction as carried from Plan2, with regression tests.
- [ ] Implement actor-safe async workflow, replay due-cycle barriers and manual-signal pause, live tick, liquidation agent pause and shutdown cleanup. Reassess current balances/quotes after model response. No direct order creation outside risk-approved runner.
- [ ] Run runner/runtime/replay/core tests. Expected: pass with deterministic local fixtures and no actor blocking. Commit task after self-review.

### Task 4: API, demo and phase verification

**Files:** Add `crates/mochi-server/src/routes/{agents,providers}.rs` and runtime provider/demo helpers; modify routes/later.rs registration and docs (`AGENTS.md`, README/backend guidance and carryover table). Tests in server API and existing e2e suite.

**Interfaces:** Replace current 501s for provider CRUD/key/test, agents CRUD/lifecycle/analyze/run/stop-all, suggestions execute/dismiss, demo start/reset/exit. Preserve vault/OKX 501s. API schemas, defaults and statuses exactly follow app.ts; runtime refs and busy guards exactly follow runtime.ts. Provider tests use actual transport seam, never production endpoints.

- [ ] Write RED API tests for Node-compatible bodies/status including malformed bodies before missing IDs, provider views exclude secrets, key OFF versus LOCKED behavior, duplicate pair/unknown refs, busy CRUD, stop-all signal retirement, demo lifecycle preserving non-demo data.
- [ ] Port handlers/demo seeds/reset ownership and runtime redaction (all loaded keys + header values); use Task3 async APIs. Port demo-reset.test.ts economic-sequence repeatability test; preserve user data. Full e2e/demo.spec.ts must run unchanged against Rust.
- [ ] Run `pnpm test`, `pnpm typecheck`, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `pnpm test:e2e`, `pnpm test:e2e:rust`. Expected: all pass; document actual counts and any real limitations, never claim Plan5 work implemented.
- [ ] Fresh whole-branch review and CodeRabbit review; fix validated important findings with regression tests. Update docs and carryovers, commit, push branch, open ready PR into main. Node stays default.
