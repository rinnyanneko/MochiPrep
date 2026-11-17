# Architecture

MochiPrep is a local-first, BYOK trading terminal. The server owns secrets and trading state; the browser is an untrusted display and control client.

## Crate boundaries

The backend is Rust, in the `crates/` workspace. `mochi-server` is a single binary that embeds the web UI.

- `packages/shared` defines the TypeScript domain types, Zod schemas, instruments and indicators the web UI uses. It is the UI's API contract.
- `mochi-decimal` and `mochi-domain` hold decimal arithmetic and the domain types and instrument registry.
- `mochi-core` owns perpetual math, fill and risk models, the paper engine, and ledger audit.
- `mochi-market` exposes the OKX public feed, the deterministic replay tape and feed, and news.
- `mochi-ai` adapts model APIs, builds prompts and indicators, and redacts secrets.
- `mochi-okx` holds the signed OKX client and `OkxEngine`, with an in-process fake OKX behind the `fake` feature. See [Trading on OKX](../OKX_TRADING.md).
- `mochi-store` is SQLite persistence and its migrations (`crates/mochi-store/migrations/`); `mochi-vault` is the master-password crypto.
- `mochi-server` composes them behind axum routes and the `Runtime` actor. Its agent cycle parses model output strictly, evaluates risk, and is the only layer that turns a decision into an order.
- `mochi-launcher-core` and `mochi-launcher` are the desktop launcher. See [the launcher guide](LAUNCHER.md).
- `apps/web` is the React client and must not receive API keys.

Rust doc comments that name `packages/...` or `apps/server/...` refer to the Node backend. It was removed when Rust became the backend and is still in git history before that change. `crates/golden` holds fixtures frozen from it.

## Instruments

The instrument registry (`mochi-domain`) holds the three built-in pairs plus any the user adds, stored in the `instruments` table and registered at startup. An added pair's specs come from OKX at the moment of adding, and connecting an exchange account rechecks every registered pair. Replay has only the built-in pairs, so `assert_tradable` keeps added pairs on live data.

`parse_okx_instrument` decides what is tradable. Its crypto filter uses `instCategory: 1`, a meaning inferred from OKX's data rather than documentation: 3 holds stocks and 4 holds commodities.

## Agent cycle

An agent cycle snapshots a fresh quote and closed candles, builds indicators and a prompt, calls the selected provider, strictly validates the response, then passes it through every risk check. Only an approved, sized result reaches the account's `ExecutionEngine`, which is the paper engine or the OKX engine. An exchange account's ceilings override the agent's own limits during sizing, and the OKX engine checks them again. Invalid output, stale data, provider failure, or risk rejection produces a decision record but no order. Analyze-only cycles never submit orders. A per-agent in-flight guard prevents overlapping cycles, and pausing or stopping during model work is checked before execution.

AI output is untrusted data throughout this path. It cannot select another account, bypass sizing, change configured limits, or directly call the engine.

## Protection

`Protection` is `{ stopLoss, takeProfit, trailingStop }`, the whole protective state of a position. `ExecutionEngine.setProtection` replaces it in one call and returns what was in force before and after, plus `pending` when an exchange has not yet confirmed. The paper engine applies it in one store transaction and holds the trailing extreme on the order row. The OKX engine places every changed leg as an algo order before cancelling any, and applies the armed rule from `classify_protection_change`. A `PROTECT` decision from an agent and the position dialog's `PUT /api/positions/:accountId/:symbol/protection` both end here; for an agent, only the agent cycle (`crates/mochi-server/src/runtime/agent_cycle.rs`) does that, after strict parsing (`crates/mochi-server/src/agent_parse.rs`) and the risk engine. An absent `trailingStop` keeps the current one and `null` removes it. Orders the owner placed on OKX are exposed read-only as `externalProtection`.

## Execution modes

In `AUTO`, an approved decision goes straight to the engine. In `SIGNAL`, the cycle stops at outcome `SIGNAL` with an expiry on the feed's clock. `execute_signal` trusts only the idea: it re-runs the risk engine on current data, which also resizes the order. A new suggestion from the same agent replaces the old one, and Stop All clears them all. Replay stops at each suggestion, or it would expire before anyone could read it.

## News

`NewsService` reads enabled RSS feeds at most every five minutes. Feed text is hostile input: `sanitize_headline` removes angle brackets and control or format characters, so a title cannot close the `<news>` block in the prompt, and titles are capped in length. The system prompt says headlines are data and never instructions. That lowers prompt injection risk without removing it, so the risk engine stays the real bound on any order. News failures never stop a cycle. Feeds must be public https without a username or password in the URL, and redirects are followed by hand so each hop passes that check, because the backend fetches them from the user's machine. A feed host must also resolve only to public addresses: on every hop the name is resolved, refused if any address is loopback, private, link-local, CGNAT, multicast or otherwise non-public (IPv4-mapped and NAT64 addresses are judged by their IPv4 address), and the connection is pinned to the checked addresses, so DNS rebinding cannot swap in a private one. A non-public IP literal is refused by the text check instead (public IPv4 literals are allowed) (`is_public_address` in `crates/mochi-market/src/news.rs`, enforced by a custom reqwest resolver).

## Replay clock

Replay is a seeded, bundled minute tape with a warm-up region. Its clock advances only through an explicit step. The runtime publishes one quote per symbol and waits for quote handling and due agent cycles to complete before advancing again. Slow model calls therefore pause tape time instead of skipping simulated minutes. Reset restores the deterministic cursor and account state, so equal inputs produce equal numeric outcomes; random database IDs are not part of determinism.

## Persistence and recovery

SQLite stores accounts, orders, positions, fills, ledger events, agents, decisions, and replay cursor. Financial mutations use one synchronous store transaction, avoiding an `await` boundary inside accounting. On restart, durable paper state and replay position are recovered, agents are forced to paused/not-running, and exchange accounts come back disconnected and disarmed. Without a master password, AI provider keys are reloaded from SQLite and OKX credentials are absent, because they never reach disk. With one, sealed AI keys and any OKX credentials the owner chose to save stay unavailable until unlock; unlocking reconnects saved OKX accounts through the full connect checks, and a real account must still be armed again before it can trade. The paper simulator processes only accounts whose venue is PAPER; before a fix in both backends it could cancel an OKX account's order rows on a quote, including live stops, and the next sync would then cancel the stop on OKX.

## Rust backend

`mochi-server` is a Rust HTTP server on axum and tokio. A `Runtime` actor owns the paper engine, replay clock, live OKX public feed, settings and agent state. Routes and asynchronous model/candle/news completions communicate with that actor, so stalled models do not block HTTP or live quotes. Replay awaits due agents at each bar before advancing. The server provides auth, SSE, the embedded UI, paper accounts/trading, market data, instruments, news, provider CRUD/keys/tests, agent lifecycle/analysis, suggestions, demo lifecycle, the vault and OKX trading. Demo reset removes only demo-owned data and repeats economic results from the first bar. `MOCHIPREP_OFFLINE=1` disables the OKX public feed and news fetching; the e2e config sets it.

Golden fixtures in `crates/golden` were generated by the former Node backend and are now frozen. They keep the Rust paper engine, tape, vault format and API behaviour as that backend had them. A test shows a reopened file database restores paper state.

The vault covers enable, unlock, change, disable and reset (enabling scrubs the old plaintext and warns if a reader blocks the WAL truncation). OKX trading covers connect checks, arming, the 5 second sync, unknown-outcome lookup, exchange protection with place-before-cancel and cleanup, and reconnect of saved credentials on unlock. The agent runner drives paper and OKX accounts through one `ExecutionEngine` trait. All of the OKX paths are tested against an in-process fake OKX only: none has run against OKX Demo or a real account. The UNCONFIRMED behaviours in `OKX_TRADING.md` are the owner's recommended check before trusting real money. The owner decided on 2026-11-16 that this check is not a condition of the switch to Rust.

The desktop launcher is two crates. `mochi-launcher-core` holds the testable parts: `launcher.json` (terms consent, port, autostart, import offer) in the OS config dir, the one-time data import, the tray menu model, a supervisor that runs `mochi-server` as a child process and a small HTTP client for its state. `mochi-launcher` is the Tauri tray app around it. It shows the risk warning and starts no backend until the current terms version is accepted, offers the import once before the first start, and opens the web interface in a window that loads only `http://127.0.0.1:<port>` and has no Tauri access. Closing that window keeps the backend and its agents running; Quit, Restart and a port change ask first when an agent runs or a real account is armed. The backend exits when the launcher's pipe to its stdin closes, so a killed launcher frees the port and database. Where no tray exists (GNOME without AppIndicator) a small panel window takes its place. See `LAUNCHER.md`.

Differences from the former Node backend, none on a path the web UI depends on:
- A feed date with no time zone is read as UTC; Node reads it as the machine's local time.
- A lone UTF-16 surrogate in feed text becomes U+FFFD, where Node's JSON writes a `\udXXX` escape. Lengths and cuts match.
- Malformed OKX envelopes and hostile JSON (a lone-surrogate escape, `1e400`, nesting past 128 levels, URLs the `url` crate and Node parse differently) get the same status with Rust's own error text.
- An unknown path or wrong method answers 404 `{"error":"Not found"}`, not Fastify's `{message,error,statusCode}`. On such paths a 415 or 413 can come before the 404, because bodies are checked before routing.
- Static files carry no `Last-Modified`, a different `ETag` and no percent-decoding, unlike `@fastify/send`.
- Playwright runs one worker against one shared backend, so spec files depend on their name order: `demo.spec.ts`'s 40,000.00 total equity holds only before `paper.spec.ts` creates its account.
- Rust fixes review findings the Node backend had: stops covering every fill of a stacked position, resting entry orders counted toward the position limit, disarming before any fallible work, retried lost-sync pauses, and races between overlapping closes, protection edits, connects, unlock reconnects and disconnects, and sync's order and fill writes. A full-position stop never takes a looser level than a per-entry one.
- The server does not handle Ctrl-Break on Windows (the Node backend did not either), so Ctrl-Break ends the process without a clean shutdown. On Ctrl-C the Rust server closes open event streams and the database cleanly.

## Rust carryovers

| Area | Current boundary / next work |
| --- | --- |
| OKX Demo test | The vault and OKX trading are tested against a fake OKX only. The owner's Demo test of the UNCONFIRMED behaviours in `OKX_TRADING.md` is recommended before trusting real money; it is not a release condition (owner decision 2026-11-16). |
| Exchange protection tests | Done in Plan 5: pending confirmations, partial `ProtectionChangeError.after` and post-alignment results are ported and tested against the fake OKX. Paper protection changes stay synchronous, return pending=false and do not align stop prices. |
| Untrusted JSON | Numeric overflow, isolated UTF-16 surrogate escapes and JSON deeper than serde_json's recursion bound are safely rejected with Rust invalid-JSON text; Node may accept the JSON and produce a different validation error. Valid Unicode and finite numeric decisions retain parity. |
| Text cutoffs | Rust preserves UTF-8 at response/sample cutoffs and drops an astral character that would cross the UTF-16 boundary; JavaScript can retain an isolated surrogate. Secrets are redacted before cutting. |

## Security model

By default the server binds `127.0.0.1` and uses an authorization token plus Host and Origin checks. `MOCHIPREP_HOST` changes the bind address, and it is opt-in and unsafe on a network you do not fully trust. The token is issued by `/api/session` to any client whose `Host` header is allowed, with no Origin requirement, so a client that can reach the port and forge that header can use the whole API. The Host check defends browsers against DNS rebinding, not against network clients. The desktop launcher strips `MOCHIPREP_HOST` from the backend's environment, so launcher-run backends stay on loopback. Users are pointed to [Self-hosting](../SELF_HOSTING.md) for the risk. AI keys are stored unencrypted in the `provider_keys` table, by the owner's choice, in a database only the owner can read. They are omitted from API views. OKX credentials exist only in `Runtime` memory. An optional master password changes this: a random data key seals AI keys (`sealed_provider_keys`) and, when the owner opts in, OKX credentials (`sealed_exchange_credentials`) with AES-256-GCM, bound to their row. The password only wraps the data key through scrypt (`vault_keys`), and neither is ever stored. Until unlocked after a restart, sealed keys are not loaded. `secure_delete` and a WAL checkpoint scrub the plaintext rows that sealing replaces. Extra provider headers stay unencrypted. Known and key-shaped secrets are redacted from provider errors and raw responses before persistence or return. Exchange credentials are never part of a model's context. Public OKX market data needs no key. MochiPrep has no withdrawal path, and it refuses keys that can withdraw.
