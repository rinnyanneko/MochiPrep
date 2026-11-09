# Rust Server and Feeds Implementation Plan (Plan 3 of 7)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust backend binary, `mochi-server`, that serves the same HTTP API, SSE stream, auth and web UI as the Node server for everything except agents, AI providers, the vault runtime and OKX trading. It is proven by byte-level API goldens recorded from Node and by the unchanged `e2e/display.spec.ts` running against it.

## Completion handoff — 2026-11-09

Tasks 1–11 are implemented on `claude/rust-server-feeds`; Task 11's implementation ends at `08df677`. The original task steps below are retained as the implementation recipe. The final handoff was recovered after the previous session reached its usage limit.

Fresh verification on this worktree:
- `pnpm test`: 410 tests passed across 26 files.
- `pnpm typecheck`, `cargo fmt --all -- --check`, and `cargo clippy --workspace --all-targets -- -D warnings`: exit 0.
- `cargo test --workspace`: all tests passed.
- `pnpm test:e2e`: all 5 Node browser tests passed.
- `pnpm test:e2e:rust -- e2e/display.spec.ts e2e/paper.spec.ts`: release build succeeded and both Rust browser tests passed.

The initial sandboxed Node and Rust runs could not connect to their loopback test servers; both full suites passed with localhost access. Cargo emitted incremental-cache hard-link fallback warnings, but no Clippy diagnostics. The installed pnpm 11.25.0 ran the checks; Corepack could not download the pinned 11.24.0 in the sandbox.

The saved CodeRabbit review covered the committed diff from `f6866d9` through `ffc900f`. Two minor findings were fixed in `08df677`. The remaining handshake-timeout finding is carried in the accounting plan's follow-up table: it affects both backends and is deferred to preserve the agreed parity scope. The continuation also corrected a `toFixed` documentation comment; runtime behaviour is unchanged.

Plan 3 is complete within that scope. Node remains the default; agents/providers and demo execution belong to Plan 4, while vault runtime and OKX trading belong to Plan 5. The existing news URL validation gap, Windows Ctrl-Break shutdown limitation, and other accepted differences remain documented.

The owner subsequently instructed that every completed phase must receive a CodeRabbit review, commit, push, and ready-for-review PR, superseding Task 11's original no-push instruction. A fresh CodeRabbit review against `f6866d9` completed on 2026-11-09 with one minor finding and no critical or warning findings: `href_attr` rescans repeated unterminated `<link` openers. Source inspection confirmed the existing quadratic path; it was initially carried in the accounting plan's cleanup row and disclosed in the PR; the subsequent review-fix pass below resolves it. No runtime changes were made during that publication step.

**Architecture:**
- A new `crates/mochi-server` crate (lib + bin) on axum/tokio holds the routes, auth, SSE, validation and a `Runtime` actor. The actor owns the synchronous `PaperExecutionEngine`, the replay feed and the settings store on one task and talks to routes through messages.
- Live OKX public data and news go into `crates/mochi-market` behind reqwest/tokio-tungstenite.
- Routes that belong to Plans 4 and 5 are either read-only ports (their GETs) or return HTTP 501 with an honest error (their writes).
- The Node backend stays the default and unchanged.

**Tech Stack:** Rust stable, axum 0.8, tokio, tower, reqwest (rustls), tokio-tungstenite (rustls), rust-embed, serde/serde_json; TypeScript/vitest for the golden generator; Playwright.

**Spec:** `docs/superpowers/specs/2026-11-07-rust-backend-desktop-design.md`, especially "相容性約束" (HTTP API and SSE).

Server facts are in `docs/superpowers/plans/2026-11-08-rust-server-feeds-facts.md` (routes, auth, SSE, Runtime, OKX public, news, e2e). The controller hands each task the sections it needs. The TypeScript sources it cites (`apps/server/src/app.ts`, `apps/server/src/runtime.ts`, `packages/market-data/src/{okx,news,replay}.ts`) are the reference behaviour.

## Roadmap change (controller call, owner may overrule)

Two of the three tests in `e2e/demo.spec.ts` need agents and the mock AI, and demo start creates agents. So `demo.spec.ts` moves to Plan 4's proof, and demo routes return 501 here. Plan 3 is proven by:
- the API goldens;
- `e2e/display.spec.ts` against the Rust server;
- a new `e2e/paper.spec.ts` that trades a paper account without the demo, which runs against both backends.

## Global Constraints

- **Node stays the default.** Nothing in `apps/server/src` or `packages/*/src` changes behaviour. The generator and tests may grow.
- **Readable Rust** for a maintainer who "knows some Rust": mainstream crates, no custom macros, no clever generics. Comments explain why.
- **Money and accounting** use `mochi_decimal::Dec` and the existing core types, never `f64`.
- **Identical HTTP surface:**
  - Same paths, methods, JSON shapes, status codes and `{ "error": string }` bodies.
  - Every success is 200.
  - Zod-equivalent validation rejects unknown keys, with byte-identical error messages (`<path or 'input'>: <message>`, joined with `; `).
  - Explicit `Unknown …` errors are 404, a busy cycle is 409, and a wrong vault password is 403.
- **Auth:**
  - The token is 32 random bytes as hex, made fresh per process. It is issued by unauthenticated `GET /api/session` and compared in constant time.
  - It is sent as `Authorization: Bearer`. Only `/api/events` also accepts `?token=`, and the header wins.
  - Missing or wrong token: 401 `{"error":"Unauthorized"}`.
  - Every path checks Host against the allowlist (403 `Host not allowed`), then Origin when present (403 `Origin not allowed`), then the token for `/api/*`.
  - `/api/session` refuses cross-site requests (`sec-fetch-site` not `same-origin`/`none` and no Origin) with 403 `Cross-site request refused`.
  - Allowed origins: `http://localhost:5173`, `http://127.0.0.1:5173`, `http://localhost:<PORT>`, `http://127.0.0.1:<PORT>`, plus comma-separated `MOCHIPREP_ALLOWED_ORIGINS`.
- **Network config:** binds `127.0.0.1` only. `MOCHIPREP_PORT` (default 8787) and `MOCHIPREP_DATA_DIR` (default `<repo>/.mochiprep`, db file `mochiprep.db`). Body limit 256 KiB. An empty POST with no content-type is accepted.
- **SSE:**
  - Messages are `data: <JSON>\n\n` with no `event:`, `id:` or `retry:` fields.
  - Headers `content-type: text/event-stream`, `cache-control: no-cache, no-transform`.
  - On connect it sends feed snapshots (REPLAY, then LIVE), then the replay state. The heartbeat `{"type":"heartbeat"}` runs every 15 s per connection.
  - Live quotes are throttled to 250 ms per symbol, invalidate is coalesced over 200 ms, and replay emits the last step's quotes once per batch.
  - Shutdown closes open streams.
- **Runtime timing:** replay batch timer 100 ms, credit `speed × 0.1` steps per tick, no credit while a batch is in flight. Equity every 15 tape-minutes (replay) and every 60 s (live). Live tick 1 s. `replay.cursor` and `replay.speed` (default 20) persist in `settings`.
- **Routes from later plans:**
  - Writes return 501 `{"error":"Not available in the Rust backend yet"}`.
  - Their GETs that the UI refetches on SSE open return the real data from the database (read-only). They never fabricate.
- **Tests run offline.** OKX and news are tested against local fake servers. Secrets never appear in logs, panics or `Debug`.
- **Goldens:** a TypeScript change that alters a golden regenerates it with `pnpm golden` in the same commit. Every task runs `pnpm test`, `pnpm typecheck`, `cargo test --workspace`, `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`.
- **Commits** end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. On Windows, `LNK1104` is a transient lock: retry.

## Review Focus

1. **A browser on another site** (foreign Origin, or `sec-fetch-site: cross-site` without Origin) must get 403 from every route, including `/api/session` and static files. DNS rebinding (an unknown Host) must also get 403. Covered in Task 2.
2. **An SSE client that disconnects mid-stream, or never reads** must not block quote handling or other clients. Each connection gets a bounded channel, and a lagging client is dropped. Covered in Task 5.
3. **The replay tape reaching its end while playing:** it stops with `atEnd` true, persists the cursor, and survives a restart at the end. Covered in Task 4.
4. **OKX WebSocket silent for 30 s, or closing:** state goes DEGRADED with REST polling every 5 s, reconnects with 1→30 s backoff, and recovers to CONNECTED. Quotes keep reaching the paper engine throughout. Covered in Task 8.
5. **A news feed that redirects to `http://`, a private host, or more than 3 hops:** refused at every hop, and a too-large body is refused. The previous cache is kept when every source fails. Covered in Task 9.

---

### Task 1: Pure split of `engine.rs` (carried from Plan 2)

**Files:**
- Create: `crates/mochi-core/src/paper/{types.rs,fills.rs,protection.rs}`
- Modify: `crates/mochi-core/src/paper/{engine.rs,mod.rs}`

**Interfaces:**
- Produces: no API change. Every `pub` item stays reachable at the same path (`mochi_core::paper::…`).
- Moves:
  - events, errors, deps types and helpers go to `types.rs`;
  - `fill_order`, posting and `refresh_liquidation` go to `fills.rs`;
  - replace/cancel protection and `close_order` go to `protection.rs`.

- [ ] **Step 1:** Move code only: no renames, no logic edits, no new `pub`.
- [ ] **Step 2:** Run `cargo test --workspace`. Expected: identical pass counts to before, and both scenario suites green. Also run `git diff --stat` and confirm only the files above changed.
- [ ] **Step 3:** Commit with a subject like `Split the paper engine into modules (pure move)`.

### Task 2: Server crate, config, auth and errors

**Files:**
- Create: `crates/mochi-server/{Cargo.toml,src/lib.rs,src/main.rs,src/config.rs,src/auth.rs,src/error.rs,src/routes/mod.rs}`
- Create: `crates/mochi-server/tests/auth.rs`
- Modify: root `Cargo.toml` (workspace deps: axum, tokio with `rt-multi-thread,macros,time,sync,signal`, tower, http-body-util, rand, subtle or a constant-time compare, tracing)

**Interfaces:**
- Produces:
  - `pub struct Config { port: u16, data_dir: PathBuf, allowed_origins: Vec<String> }`.
  - `pub fn load_config() -> Config`, which reads the three env vars as the Global Constraints say.
  - `pub fn app(state: AppState) -> axum::Router`.
  - `pub struct ApiError { status: StatusCode, message: String }` with `IntoResponse` producing `{"error": message}`, plus constructors `bad_request`, `not_found`, `conflict`, `forbidden`, `unauthorized`, `not_available()` (501, the exact text from Global Constraints).
  - `AppState { token: String, config: Arc<Config>, runtime: RuntimeHandle }`. `RuntimeHandle` is a placeholder until Task 4.
  - Routes `GET /api/session` → `{"token"}` and `GET /api/health` → `{"ok":true}`.
  - `main.rs` prints `MochiPrep backend on http://127.0.0.1:<port> (data: <dir>)` on start and shuts down gracefully on Ctrl-C/SIGTERM.

- [ ] **Step 1: Write the tests in `tests/auth.rs`**, using `tower::ServiceExt::oneshot`, mirroring `apps/server/test/api-integration.test.ts:152-160`:
  - `missing_token_is_401` (Host `localhost:8787`).
  - `unknown_host_is_403_even_for_static`.
  - `foreign_origin_with_valid_token_is_403`.
  - `own_origin_is_200`.
  - `session_refuses_cross_site_without_origin` (`sec-fetch-site: cross-site` gives 403 with the exact text).
  - `events_accepts_query_token_but_header_wins`.
  - `body_over_256_kib_is_413_with_error_json`.
  - `empty_post_without_content_type_is_accepted` (against a test route).
  - `extra_allowed_origins_from_env_are_trusted`.
- [ ] **Step 2:** Run `cargo test -p mochi-server`. Expected: FAIL.
- [ ] **Step 3:** Implement. A single middleware checks Host, then Origin, then the token, in that order. An unmatched `/api/*` with no token is 401; with a token it is 404 `{"error":"Not found"}`. Record that 404 body as a deliberate difference from Fastify's default shape, with a comment.
- [ ] **Step 4:** Run the tests again. Expected: PASS. Then run every check in Global Constraints and commit.

### Task 3: Shared API types and Zod-equivalent validation

**Files:**
- Create: `crates/mochi-domain/src/server.rs` (`ServerEvent`, `Topic`, `FeedStatus`, `FeedState`, `ReplayState`, `VaultStatus`, `NewsSource`, `NewsItem`)
- Create: `crates/mochi-server/src/validate.rs`
- Modify: `apps/server/golden/generate.ts` (a new `validation.json`)
- Create: `crates/mochi-server/tests/golden_validation.rs`

**Interfaces:**
- Produces:
  - The serde types, mirroring `packages/shared/src/domain.ts:116-124, 618-637`. `ServerEvent` is tagged `type` and serialised exactly as Node does.
  - `pub fn parse_body<T>(schema: Schema, value: &serde_json::Value) -> Result<T, ApiError>` for every body and query schema used by ported routes:
    - `CreateAccountSchema`
    - `PlaceOrderSchema` (with `trailingStop`)
    - `ClosePositionSchema`
    - `ProtectionSchema`
    - `AddInstrumentSchema`
    - `NewsSourceInputSchema`
    - `NewsSourceToggleSchema`
    - `ReplayControlSchema`
    - `CandleQuery`, `SourceQuery`, `LimitQuery`, `DecisionQuery`
    - `SymbolSchema` as a path param
  - Hand-written per schema in plain code. No macro DSL.
- `validation.json`: for each schema, a corpus of about 15 inputs, each with the verdict and the exact error message Zod v4 produces. The corpus covers:
  - valid inputs;
  - missing keys, unknown keys and wrong types;
  - bounds (`limit` 0/1001; leverage 0/101; fraction 0/1.0000001);
  - decimal regex and over-40-character strings;
  - explicit `null` against `nullish` and `optional` (the carried "OrderRequest explicit-null" item);
  - `trailingStop` absent vs null;
  - coerce-int query strings (`"5"`, `"5.5"`, `"abc"`).

- [ ] **Step 1:** Add the corpus to `generate.ts` (it runs the real Zod schemas from `packages/shared/src/schemas.ts`). Run `pnpm golden` and write `golden_validation.rs` to assert each case. Run it. Expected: FAIL.
- [ ] **Step 2:** Implement `validate.rs` until every case matches byte for byte (verdict, parsed value and message). Run all checks and commit.

### Task 4: Runtime actor, settings store and replay clock

**Files:**
- Create: `crates/mochi-server/src/runtime/{mod.rs,replay_clock.rs,events.rs}`
- Create: `crates/mochi-store/src/settings.rs` (`get_setting`, `set_setting`, JSON values in table `settings`) and `instruments` accessors (`list_instruments`, `save_instrument`, `delete_instrument`)
- Test: `crates/mochi-server/tests/runtime.rs`, `crates/mochi-store/tests/settings.rs`

**Interfaces:**
- Produces:
  - `RuntimeHandle` (Clone, Send). Routes `await` its methods, and each method is a message to one actor task that owns `PaperExecutionEngine<SqlitePaperStore>`, `ReplayMarketData`, the instrument registry (kept in sync with the engine's `instruments_mut()`), the settings and the live feed handle.
  - `subscribe() -> broadcast::Receiver<ServerEvent>`.
  - `invalidate(&[Topic])`: coalesced over 200 ms into one event.
  - `replay_state()`, `set_playing(bool)`, `set_speed(u32)`, `step_replay(n)`: serialised; an HTTP step waits for its batch.
  - `state()`: the `/api/state` body, with `demoActive` from setting `demo.active`, `vault` from whether `vault_keys` has a row, `feeds`, and the whole `instruments` map.
  - `shutdown()`.
- Startup order mirrors `runtime.ts:118-189`:
  - open the DB;
  - register every row of `instruments` (and track it on the live feed from Task 8);
  - restore `replay.cursor` and `replay.speed`;
  - the replay is not playing after a restart.
- Engine deps:
  - `get_quote`/`now` read the feed for the source (replay `now` = tape time + 60 000);
  - `new_id` gives `<prefix>_<uuid v4>`, as TS does;
  - `on_event` invalidates `accounts, orders, positions, arena`, and on a liquidation also emits the toast `<account>: <side> <symbol> was liquidated (simulated)` (level error).
- Carried items settled here, each with a test:
  - `on_quote` error semantics: events flush per committed step and are dropped on rollback, consistent with `set_protection`.
  - No quote-age check in `on_quote` (the 60 s rule stays inside order placement only).
  - All writes go through `PaperStore::transaction`.
  - `StoreFailure` keeps the rusqlite error kind, so BUSY is detectable.
  - `ReplayMarketData::new` validates `minutes > WARMUP_MINUTES` and returns `Result`.
  - `Tape` column lengths cannot diverge.

- [ ] **Step 1: Tests:**
  - `replay_batch_steps_floor_of_credit` (speed 20 → 2 steps per 100 ms tick, using tokio paused time).
  - `no_credit_while_a_batch_is_in_flight`.
  - `end_of_tape_stops_playing_and_persists_cursor` (Review Focus 3, with a short tape).
  - `restart_restores_cursor_and_speed_not_playing`.
  - `invalidate_coalesces_within_200ms`.
  - `equity_recorded_every_15_tape_minutes`.
  - `settings_round_trip_json`.
  - `instruments_rows_register_in_engine_registry`.
- [ ] **Step 2:** Run them. Expected: FAIL. Then implement and run again. Expected: PASS. Run all checks and commit.

### Task 5: SSE `/api/events`

**Files:**
- Create: `crates/mochi-server/src/routes/events.rs`
- Test: `crates/mochi-server/tests/sse.rs`

**Interfaces:**
- Consumes: `RuntimeHandle::subscribe`, `state()`.
- Produces: the stream described in Global Constraints. Each connection forwards from a bounded channel (capacity 256). A client that lags past it is closed, and the stream ends rather than blocking the runtime (Review Focus 2). The live quote throttle (250 ms per symbol) lives in the runtime, so all clients see the same quotes.

- [ ] **Step 1: Tests:**
  - `connect_sends_feed_replay_snapshot_in_order`.
  - `heartbeat_every_15s` (paused time).
  - `messages_are_data_only_lines`.
  - `slow_client_is_dropped_without_blocking_others`.
  - `shutdown_closes_open_streams`.
  - `live_quotes_throttled_per_symbol_250ms`.
- [ ] **Step 2:** Run them. Expected: FAIL. Then implement and run again. Expected: PASS. Run all checks and commit.

### Task 6: Paper, market, replay, state and instrument-read routes, with byte-level API goldens

**Files:**
- Create: `crates/mochi-server/src/routes/{accounts.rs,orders.rs,market.rs,replay.rs,state.rs,instruments.rs}`
- Modify: `apps/server/golden/generate.ts` (a new `api-script.json`)
- Create: `crates/mochi-server/tests/golden_api.rs`

**Interfaces:**
- Produces these routes, exactly as in `app.ts`. Error texts are verbatim and engine refusals come back as REJECTED orders in the result, not as HTTP errors:
  - `GET/POST /api/accounts`
  - `POST /api/accounts/:id/reset`
  - `DELETE /api/accounts/:id`:
    - 400 `Delete the agents using this account first` when the agents table references it;
    - otherwise deletes the paper data;
    - for a non-PAPER account it returns 501, because disconnecting is Plan 5.
  - `GET /api/accounts/:id/{positions,closed-positions,orders,fills,ledger,equity,audit}`
  - `POST /api/orders`, `DELETE /api/orders/:id`
  - `POST /api/positions/:accountId/:symbol/close`
  - `PUT /api/positions/:accountId/:symbol/protection`:
    - an absent `trailingStop` keeps the current one;
    - the response is `{ok, pending:false, before, after}`;
    - `ProtectionResult` gains `pending` (carried 3/4).
  - `GET /api/market/candles`, `GET /api/market/quotes`
  - `POST /api/replay`, `GET /api/state`, `GET /api/instruments`
- Non-PAPER venues on order, position and protection routes return 501 (Plan 5).
- `api-script.json`:
  - A scripted session against a Node `buildApp` built with a fresh in-memory DB and `replayMinutes: 6500`, driven by `app.inject`.
  - For each step it records the request (method, path, body) and the response (status + exact body text).
  - The script covers:
    - creating accounts;
    - stepping the replay;
    - market and limit orders, with SL, TP and trailing;
    - close;
    - protection keep and remove;
    - ledger, equity and audit reads;
    - candles and quotes;
    - every validation error path;
    - unknown ids (404);
    - REPLAY `assertTradable` errors.
  - Ids and timestamps that come from randomness or the wall clock are normalised (`acct_<n>`, `ord_<n>` in first-seen order; wall-clock `createdAt`/`updatedAt` only where the TS uses `Date.now()`). This is done by a shared normaliser that is written once in TS and mirrored in Rust, and the fixture records which fields it normalised.

- [ ] **Step 1:** Write the script in `generate.ts`, run `pnpm golden`, and write `golden_api.rs`, which replays the same requests through `app()` with a matching fresh runtime and compares normalised bodies byte for byte. Run it. Expected: FAIL.
- [ ] **Step 2:** Implement the routes until it passes. Document where untrusted strings first reach `Dec` (the carried `Dec::parse` note: `0x…`/Infinity/NaN are refused, exponents are capped, and leading-zero exponents are accepted). Run all checks and commit.

### Task 7: Read-only GETs for later plans' areas, 501 writes, and embedded web assets

**Files:**
- Create: `crates/mochi-server/src/routes/{later.rs,static_files.rs}`
- Create: `crates/mochi-store/src/rows.rs` (read-only mappers for `agents`, `providers` (view without extra-header values), `decisions` (JSON `body` passthrough), `vault_keys` presence, non-PAPER accounts)
- Test: `crates/mochi-server/tests/later_and_static.rs`, plus `api-script.json` additions for these GETs (Node DB seeded with one agent, one provider and one decision through store calls)

**Interfaces:**
- Produces GETs:
  - `/api/agents`, `/api/providers`, `/api/signals`, `/api/decisions`, `/api/arena`, `/api/vault`, `/api/exchange/status`;
  - `/api/news/sources` and `/api/instruments/available` come in Tasks 8-9.
- Arena is computed exactly as `runtime.ts:929-969` from the agents and the paper balances.
- `exchange/status` reports every non-PAPER account as not connected and not armed, which is the truth for a server with no OKX sessions.
- Every write in the A/P/V/X/demo areas returns `not_available()`. That means every route not listed in Tasks 6, 8 and 9.
- Static: the web build is embedded with rust-embed from `apps/web/dist`.
  - Missing at build time means "no UI", and the server still runs, as Node does.
  - `/` serves `index.html`. There is no SPA fallback (the UI uses hash routing).
  - Headers are `Cache-Control: public, max-age=0`, with ETag.
  - Host/Origin checks apply and the token check does not.

- [ ] **Step 1: Tests:**
  - the GETs match the golden script;
  - every write route on the 501 list returns the exact body;
  - `/` and an asset are served with the headers;
  - `unknown_host_static_403`.
- [ ] **Step 2:** Run them. Expected: FAIL. Then implement and run again. Expected: PASS. Run all checks and commit.

### Task 8: Live OKX public market data and instrument add/remove

**Files:**
- Create: `crates/mochi-market/src/okx_public.rs` (and `instrument_parse.rs` mirroring `parseOkxInstrument`)
- Modify: `crates/mochi-market/Cargo.toml` (reqwest rustls, tokio-tungstenite rustls, tokio)
- Modify: `apps/server/golden/generate.ts` (`okx-public.json`: `parseOkxInstrument`, `parseOkxCandles` and quote building over recorded OKX sample payloads)
- Create: `crates/mochi-market/tests/{golden_okx_public.rs,okx_live_feed.rs}`
- Create: `crates/mochi-server/src/routes/instruments.rs` (extend with available/add/remove)

**Interfaces:**
- Produces `OkxPublicFeed`:
  - `start()`, `stop()`, `track(symbol)`, `untrack(symbol)`, `get_quote(symbol)`, `get_candles(symbol, tf, limit)` (capped at 300, 3 s cache), `list_instruments(fresh)` (5 min cache), `status() -> FeedStatus` (`provider: "OKX public"`).
  - `quotes() -> mpsc::Receiver<Quote>` into the runtime, which hands every quote to `engine.on_quote` and throttles SSE.
- Constants and endpoints exactly as in `okx.ts`:
  - REST base, WS URL, bar map, `instId` mapping;
  - 8 s timeout;
  - `ping` every 20 s;
  - 30 s silence closes the socket;
  - backoff `min(1000·2^n, 30000)`;
  - REST poll every 5 s only while not CONNECTED, plus once at start and on a new track;
  - quotes emitted only by ticker messages;
  - `mark` falls back to the JS-formatted `(bid+ask)/2` with `markEstimated: true`.
- Routes:
  - `GET /api/instruments/available`.
  - `POST /api/instruments`: fresh list lookup; texts `X is already added` / `X is not a live USDT crypto perpetual on OKX`; saves to `instruments`, registers, tracks, and invalidates `instruments`.
  - `DELETE /api/instruments/:symbol`: all five refusal texts. `Agents still use this pair` reads the agents table.
  - LIVE candles in `/api/market/candles`.

- [ ] **Step 1:** Golden the parsers from TS with fixed OKX payloads. Write the feed tests against a local axum REST server and a local WebSocket server:
  - `connects_subscribes_and_emits_ticker_quotes`
  - `silence_30s_degrades_polls_rest_and_reconnects` (Review Focus 4, paused time)
  - `backoff_caps_at_30s`
  - `rest_poll_only_while_not_connected`
  - `track_after_start_subscribes_and_polls`
  - `candle_cache_3s`
  - `instrument_add_and_remove_texts`
- [ ] **Step 2:** Run them. Expected: FAIL. Then implement and run again. Expected: PASS. Run all checks and commit.

### Task 9: News service and news routes

**Files:**
- Create: `crates/mochi-market/src/news.rs`
- Modify: `apps/server/golden/generate.ts` (`news.json`: `sanitizeHeadline`, `parseFeed`, `selectNews`, `validateFeedUrl` over a corpus that includes hostile titles, entities, CDATA, Atom links, future and old dates, and coin-name matches)
- Create: `crates/mochi-market/tests/{golden_news.rs,news_fetch.rs}`
- Create: `crates/mochi-server/src/routes/news.rs`

**Interfaces:**
- Produces:
  - `NewsService::for_symbol(symbol, limit) -> NewsResult { items, error }`:
    - a lazy 5-minute cache, with shared in-flight refresh;
    - the previous cache is kept when all sources fail;
    - `lastError` reads `Could not read: <names>`.
  - Fetch:
    - 8 s per hop, at most 3 redirects;
    - every hop re-validated, with errors `<problem>: <url>`;
    - the user-agent and accept headers verbatim;
    - `feed too large` above 3 000 000 characters after the full read.
  - Built-in feeds and the `news.sources` settings overlay exactly as in `runtime.ts:472-486`.
  - Routes: `GET /api/news`, `GET/POST /api/news/sources`, `PUT/DELETE /api/news/sources/:id`, all with the exact texts.

- [ ] **Step 1:** Golden the pure functions from TS. Write the fetch tests against a local server:
  - `redirect_to_http_refused`
  - `redirect_to_private_host_refused`
  - `fourth_redirect_refused`
  - `too_large_refused`
  - `all_sources_fail_keeps_cache`
  - `sources_change_invalidates_cache`

  Together these cover Review Focus 5.
- [ ] **Step 2:** Run them. Expected: FAIL. Then implement and run again. Expected: PASS. Run all checks and commit.

### Task 10: Run e2e against the Rust server

**Files:**
- Modify: `playwright.config.ts`. When `MOCHIPREP_E2E_BACKEND=rust`, `webServer[0]` runs `cargo run --release -p mochi-server` with the same env and port. The default stays Node.
- Create: `e2e/paper.spec.ts`. It creates a REPLAY paper account in the UI and steps the replay. It places a market order, sets a 1% trailing stop in the dialog, sees its trigger, closes the position and sees the ledger audit pass. It runs on both backends and doesn't use demo or agents.
- Modify: root `package.json` (script `test:e2e:rust`)

- [ ] **Step 1:** Write `paper.spec.ts`. Run `pnpm test:e2e` (Node). Expected: PASS for display, paper and demo.
- [ ] **Step 2:** Run `pnpm test:e2e:rust -- e2e/display.spec.ts e2e/paper.spec.ts`. Expected: PASS. Fix server gaps it finds in the task that owns them, recording each in the report.
- [ ] **Step 3:** Run all checks and commit.

### Task 11: Remaining carried items, docs and wrap-up

**Files:**
- Modify: `crates/mochi-domain/src/jsnum.rs`:
  - `js_to_fixed` returns JS strings for NaN/±Infinity, with `debug_assert!(digits <= 20)`;
  - `js_round` decides −0, matching what reaches API output.
- Modify: `AGENTS.md` (Rust section: `mochi-server`, how to run it, that it is not the default), `docs/ARCHITECTURE.md` (Rust paragraph: what serves, what returns 501)
- Modify: `docs/superpowers/plans/2026-11-07-rust-accounting-core.md` (mark the Plan 3 rows done and add any new rows for Plans 4-7)

- [ ] **Step 1:** Add jsnum tests for NaN/±Infinity and −0 output, with expectations from TS added to `jsnum.json`. Implement.
- [ ] **Step 2:** Update the docs. Run every check plus `pnpm test:e2e` and `pnpm test:e2e:rust -- e2e/display.spec.ts e2e/paper.spec.ts`. Record the counts.
- [ ] **Step 3:** Run the CodeRabbit CLI locally against `origin/main`. It's in WSL. Clone into WSL, check out the branch, run `coderabbit review --agent -t committed --base-commit <merge-base>`, then delete the clone. Fix the valid findings, then commit. Do not push.


## PR #7 review fixes — 2026-11-09

The owner authorized fixing the PR feedback in both backends. Trailing root dots are now removed from parsed hostnames before local-host checks, including encoded/Unicode dot forms; public HTTPS names with root dots remain accepted. Tests exercise registration validation, persisted sources and redirects without making unsafe requests. This closes the reported hostname-normalization bypass, not the separate DNS-resolution limitation.

Stored decision-cost strings now use the shared JavaScript Number conversion helper. The Rust e2e instructions name the supported display and paper specs, and the runner launches the installed Playwright CLI through the current Node executable without a shell. The wildcard import is explicit, duplicate tracing fields have distinct names, and the HTML test fixture declares its language.

Hostile Unicode fixtures retain their original parsed values but use visible escapes in source and generated JSON. The ISO/legacy date parsers and non-string validation checks were split into smaller helpers while preserving their golden outputs. Documentation now describes the compatibility-sensitive parser, validation and runtime contracts. The malformed-link scan skips previously exhausted runs; its 20,000-opener regression exceeded three seconds before the fix.

Verification after review fixes: 422 TypeScript tests and 197 Rust tests passed; typecheck, rustfmt and Clippy passed; all five Node and both Rust browser tests passed. The Rust suite used a fresh target/pr7-verification build directory after repeated Windows linker locks in the original target directory. CodeRabbit's review of the fix patch completed with zero findings.
