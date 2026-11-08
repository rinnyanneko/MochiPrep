# Plan 3 facts: porting HTTP server / SSE / auth / static / OKX public data / news / instruments / demo / replay to Rust

Worktree root = C:\Users\rinny\Documents\GitHub\MochiPrep\.claude\worktrees\db-password-encryption-d6b0ca (all paths below relative to it). HEAD ed51a60.
No crate in crates/ depends on axum/tokio/reqwest yet (crates/*/Cargo.toml: not found).
Needs-legend: A = agents (AgentRunner / agents+decisions tables), P = AI provider rows/keys, V = vault runtime state, X = OKX private trading session, S = SQLite tables (settings/instruments) the Rust store does not wrap yet.
Existing Rust backend design spec: docs/superpowers/specs/2026-11-07-rust-backend-desktop-design.md (28: API/SSE must be fully compatible, no React changes; 111-119 API areas + security; 147-148 axum/tokio, static assets embedded in binary; 158 default fixed port 8787).

## 1. Routes (apps/server/src/app.ts)

Global: Fastify 5, fastify({ logger: { level: 'warn' }, bodyLimit: 256 * 1024 }) (app.ts:56). All successes are HTTP 200 JSON (no 201/204 anywhere).

Error format (setErrorHandler, app.ts:82-92), always JSON { "error": "<string>" }:
- ZodError -> 400, message = issues.map("<path.join('.') or 'input'>: <issue.message>").join('; ') (zod v4 wording, shown verbatim by UI).
- WrongPasswordError (apps/server/src/vault.ts:21-25, "Wrong master password") -> 403 (not 401: web client treats 401 as expired token and retries; app.ts:87).
- Otherwise message = runtime.redact(error.message) (masks every in-memory API key / extra-header value / OKX secret, runtime.ts:574-582), status = error.statusCode ?? 400; >=500 logged. So every plain "throw new Error(..)" is a 400. Fastify's own body-parser errors (bad JSON 400, too large 413, unsupported type 415) pass through this handler (framework behaviour, not tested in repo).
- Explicit 404 (Object.assign(new Error('Unknown ...'), {statusCode:404})): account app.ts:172, order 221, provider 253, agent 312, suggestion 404.
- 409: requireSignal when outcome !== 'SIGNAL' ("This suggestion is no longer pending", app.ts:405); analyze/run when a cycle is in flight: reply.code(409).send({error:'A cycle is already running for this agent'}) (app.ts:345).
- Unknown paths: no custom notFound handler; Fastify default 404 body ({message,error,statusCode}; framework default, not verified by running). The onRequest hook runs first: unknown /api/* without token is 401; bad Host is 403.

Query schemas (app.ts:36-44): CandleQuery{source REPLAY|LIVE, symbol SymbolSchema, timeframe in 1m,5m,15m,1h,4h, limit coerce int 1..1000 default 300}; SourceQuery{source}; LimitQuery{limit coerce int 1..2000 default 200}; DecisionQuery = LimitQuery + optional agentId. Body schemas: packages/shared/src/schemas.ts (all z.strictObject: unknown keys rejected). SymbolSchema = /^[A-Z0-9]{1,24}USDT$/ (schemas.ts:35). decimalString = /^\d+(\.\d+)?$/ max 40 (schemas.ts:33).

### session / health / state / events
- GET /api/session (app.ts:95-100). No schema. -> {token}. Skips auth. 403 "Cross-site request refused" if sec-fetch-site is present and not same-origin/none AND there is no Origin header. Needs: none.
- GET /api/health (102). -> {ok:true}. Requires bearer token (only /api/session is exempt). none.
- GET /api/state (104-110). Calls isDemoActive(), vaultStatus(), replayState(), feeds(), global INSTRUMENTS. -> {demoActive, vault:'OFF'|'LOCKED'|'UNLOCKED', replay:ReplayState, feeds:[replayStatus, liveStatus], instruments: Record<symbol,InstrumentSpecification>}. NOTE instruments is the whole map (UI type ignores it, apps/web/src/store.ts:170). Needs: V (vault status = does table vault_keys have a row), S (settings demo.active).
- GET /api/events (112-131). ?token=. SSE, see section 3.

### market
- GET /api/market/candles (134-138). CandleQuery. assertTradable(symbol,source) then candles(). -> Candle[] oldest first {time,open,high,low,close,volume,closed}. 400 "X is not an added pair. Add it in Settings first." / "X has no replay data. Use a Live OKX account." (runtime.ts:465-468). LIVE: OKX REST; provider caps limit at 300 (okx.ts:292). none.
- GET /api/market/quotes (139). SourceQuery. -> Quote[] for each symbol in INSTRUMENTS that currently has a quote (runtime.ts:921). none.

### news
- GET /api/news (142-145). query {symbol} -> runtime.newsFor(symbol, 12) -> {items:NewsItem[], error:string|null}. none (public https).
- GET /api/news/sources (146). -> NewsSource[] (code built-ins with saved enabled overlay + user sources). S (settings key news.sources).
- POST /api/news/sources (147-150). NewsSourceInputSchema{name trim 1..40, url z.url max300} -> addNewsSource -> new NewsSource {id:'news_<uuid>',name,url,enabled:true,builtIn:false}; 400 on validateFeedUrl problem or "This feed is already added". S.
- PUT /api/news/sources/:id (151-154). NewsSourceToggleSchema{enabled} -> {ok:true}; 400 "Unknown news source". S.
- DELETE /api/news/sources/:id (155-158). -> {ok:true}; 400 "Unknown news source" / "Built-in sources can be turned off, not removed". S.

### instruments
- GET /api/instruments (161). -> Object.values(INSTRUMENTS) (3 built-in + added). none.
- GET /api/instruments/available (162). -> runtime.availableInstruments(): OKX list (5-min cache) minus those already in INSTRUMENTS. none (public OKX REST).
- POST /api/instruments (163). AddInstrumentSchema{symbol} -> addInstrument: fresh OKX list lookup; 400 "X is already added" / "X is not a live USDT crypto perpetual on OKX"; saveInstrument (table instruments), registerInstrument, live.trackSymbol, invalidate('instruments'); returns the spec. S.
- DELETE /api/instruments/:symbol (164-167). param parsed by SymbolSchema -> removeInstrument; {ok:true}; 400: "X is built in", "X is not an added pair", "Agents still use this pair", "<acct> has an open X position", "<acct> has open X orders". A (reads agents), S.

### accounts / positions / orders / ledger / equity / audit / reset
accountView(id) = {...store.getAccount(id), balance: runtime.balanceOf(id)} (app.ts:170). requireAccount -> 404 "Unknown account".
- GET /api/accounts (176). -> AccountView[]. none (non-PAPER with no session falls back to stored equity points, runtime.ts:878-898).
- POST /api/accounts (177-181). CreateAccountSchema{name trim 1..60, source REPLAY|LIVE, initialBalance decimalString default '10000'} -> engine.createAccount; invalidate('accounts') -> AccountView. none.
- POST /api/accounts/:id/reset (182-186). engine.resetAccount; invalidate(accounts,orders,positions,decisions,arena) -> AccountView. Engine refuses non-PAPER ("Exchange accounts cannot be reset", Rust engine.rs:~413). none.
- DELETE /api/accounts/:id (187-194). 400 "Delete the agents using this account first" if any agent has accountId; then disconnectExchange(id) + deleteAccountData(id,false) in one tx; invalidate(accounts,orders,positions); {ok:true}. A (reads agents table), X (disconnectExchange touches exchange sessions/sealed creds/pauses agents).
- GET /api/accounts/:id/positions (195). positionsOf -> PositionView[]. none for PAPER; exchange venue returns [] when not connected.
- GET /api/accounts/:id/closed-positions (196-198). LimitQuery -> Position[]. none.
- GET /api/accounts/:id/orders (199). LimitQuery -> Order[]. none.
- GET /api/accounts/:id/fills (200). LimitQuery -> Fill[]. none.
- GET /api/accounts/:id/ledger (201). listLedger(id).slice(-limit).reverse() -> LedgerEvent[] newest first. none.
- GET /api/accounts/:id/equity (202). listEquity(id, limit) -> EquityPoint[] oldest first. none.
- GET /api/accounts/:id/audit (203-209). core auditLedger -> {consistent, issues}; non-PAPER -> {consistent:true, issues:[], notApplicable:true}. none.

### orders / positions (paper or exchange via runtime.execution(accountId))
- POST /api/orders (212-218). PlaceOrderSchema (strict): accountId, clientOrderId<=128, symbol, side BUY|SELL, type MARKET|LIMIT, quantity decimal, price? nullish decimal, leverage int 1..100, reduceOnly?, stopLoss?, takeProfit?, trailingStop? {kind RATIO|SPREAD, callback decimal, activationPrice decimal|null}. Flow: requireAccount, assertTradable(symbol, account.source), execution(accountId).placeOrder, invalidate(accounts,orders,positions,arena). -> OrderResult {order, fills, duplicate} (UI reads .order); engine rejections come back as a REJECTED order in the result, not as HTTP errors. none for PAPER; X for exchange venue.
- DELETE /api/orders/:id (219-225). 404 "Unknown order"; execution(order.accountId).cancelOrder; {ok:true}. none PAPER; X otherwise.
- POST /api/positions/:accountId/:symbol/close (226-233). ClosePositionSchema{fraction >0..<=1, clientOrderId}; symbol param via SymbolSchema -> execution.closePosition(accountId,symbol,fraction,clientOrderId) -> OrderResult. none PAPER; X otherwise.
- PUT /api/positions/:accountId/:symbol/protection (234-248). ProtectionSchema{stopLoss decimal|null, takeProfit decimal|null, trailingStop? TrailingStop|null}. Absent trailingStop keeps engine.protectionOf().trailingStop; null removes it. Calls engine.setProtection(acct, symbol, {stopLoss,takeProfit,trailingStop:kept}, {clientOrderId:'manual:<uuid>', agentId:null, decisionId:null}); invalidate(orders,positions). -> {ok:true, pending, before, after}. Refusals throw -> 400 with reason. Rust ProtectionResult (crates/mochi-core/src/paper/engine.rs:56-59) has only before/after: no pending, and set_protection has no meta argument. none PAPER; X otherwise.

### AI providers (all need provider rows; key routes need V)
- GET /api/providers (261). -> ProviderView[] (config minus extraHeaders, plus headerNames, requiresKey, hasKey). The 'mock' row (id 'mock', name 'Mock AI (offline demo)', kind MOCK) is seeded in Runtime ctor. P, S.
- POST /api/providers (262-268). ProviderInputSchema{name, kind OPENAI_COMPATIBLE|OPENAI|ANTHROPIC|GEMINI|DEEPSEEK|OPENCODE, extraHeaders? (<=20, reserved names blocked, lowercased), baseUrl url|null, defaultModel, inputPricePerMTok/outputPricePerMTok >=0|null}; id prov_<uuid> -> ProviderView. P.
- PUT /api/providers/:id (269-276). same schema; 400 "The mock provider is built in". P.
- DELETE /api/providers/:id (277-284). 400 "Agents still use this provider"; clears key; {ok:true}. P, A.
- PUT /api/providers/:id/key (285-289). ProviderKeySchema{apiKey trim 1..1000} -> runtime.setKey: LOCKED -> 400 "Unlock MochiPrep with your master password first"; UNLOCKED -> sealed row; OFF -> plaintext row; {ok:true}. P, V.
- DELETE /api/providers/:id/key (290-293). {ok:true}. P.
- POST /api/providers/:id/test (294-307). optional body {model?} (uses request.body ?? {}); MOCK short-circuits to {ok:true, sample:'Mock provider needs no connection.'}; others make an HTTPS call to the vendor. P (external AI).

### agents (all need A)
- GET /api/agents (316). Agent[]. A.
- POST /api/agents (317). AgentInputSchema{name, providerId, model, symbol, timeframe, strategy<=8000, accountId, risk (default DEFAULT_RISK_SETTINGS), intervalSec int 10..86400, executionMode AUTO|SIGNAL default SIGNAL, useNews default false, allowTrailingStop default false} -> createAgent. A, P.
- PUT /api/agents/:id (318-325). same schema -> saved Agent. A.
- DELETE /api/agents/:id (326-332). 400 "Stop the agent first". {ok:true}. A.
- POST /api/agents/:id/{start,pause,resume,stop} (334-342, one loop). start/resume first runtime.assertCanRun(agent) (exchange accounts must be connected, and armed if executionMode AUTO); returns the Agent. A (+X for exchange accounts).
- POST /api/agents/:id/analyze and /run (343-349). runner.runCycle(id,'ANALYZE'|'RUN_ONCE') -> DecisionRecord or 409. A, P.
- POST /api/agents/stop-all (350-356). runner.stopAll(), retireAllSignals(), runtime.setPlaying(false), runtime.disarm() -> {stopped:number}. A (but side effects on replay + exchange arm state).

### signals / decisions / arena
- GET /api/signals (408). store.listPendingSignals -> DecisionRecord[]. A (decisions table).
- POST /api/signals/:id/execute (409-413). requireSignal (404/409), assertCanExecuteSignal, runner.executeSignal -> DecisionRecord. A (+X for OKX_LIVE).
- POST /api/signals/:id/dismiss (414). -> DecisionRecord. A.
- GET /api/decisions (416-419). DecisionQuery -> DecisionRecord[]. A.
- GET /api/arena (420). runtime.arena() (runtime.ts:929-969): ArenaRow per agent; [] with no agents. A.
NOTE for a server without agents: on SSE open the UI refetches accounts, agents, providers, signals, decisions, instruments, vault, exchange/status, arena (apps/web/src/store.ts:104,167-208,267) using Promise.allSettled, so those endpoints should return []/defaults rather than 404.

### vault (V; crypto in mochi-vault)
- GET /api/vault (359). {status}.
- POST /api/vault (360-363). VaultCreateSchema{password 10..200} -> enableVault (scrypt N=2^17,r=8,p=1; seals existing plaintext provider keys, transaction, store.scrub()) -> {status}.
- POST /api/vault/unlock (364-367). VaultPasswordSchema{password 1..200} -> unlock(): opens wrap, loads sealed AI keys, then reconnects saved OKX credentials via connectOkx -> {status, failed:string[]}. Wrong password 403. V, X.
- POST /api/vault/password (368-372). VaultChangeSchema{current, next 10..200} -> {status}.
- POST /api/vault/disable (373-376). VaultPasswordSchema -> disableVault (unseals AI keys to plaintext, deletes sealed OKX creds) -> {status}.
- POST /api/vault/reset (377-380). VaultResetSchema{confirmation<=100} must equal 'DELETE SAVED KEYS' (domain.ts:643) else 400 "Type exactly: DELETE SAVED KEYS"; deletes all sealed data -> {status}.
Concurrent vault actions -> 400 "Another master password action is in progress" (runtime.ts:592-600).

### exchange / OKX (all X)
- GET /api/exchange/status (383). -> ExchangeStatus[]; [] if no non-PAPER accounts (runtime.ts:856-875).
- POST /api/exchange/okx/connect (384-387). OkxConnectSchema{apiKey, secretKey, passphrase (trim 1..200), name, environment OKX_DEMO|OKX_LIVE, limits RiskSettings default DEFAULT_LIVE_LIMITS, remember default false} -> AccountView. X, V.
- POST /api/exchange/okx/:id/disconnect (388). {ok:true}.
- POST /api/exchange/okx/:id/arm (392-395). ArmSchema{confirmation} must equal 'TRADE REAL MONEY' (domain.ts:605). {ok:true}.
- POST /api/exchange/okx/:id/disarm (396-399). {ok:true}.

### demo & replay
- POST /api/demo/start (423-426). startDemo() sync -> {demoActive:true}. A+P(mock): creates agents with provider 'mock'.
- POST /api/demo/reset (427-430). await resetDemo() -> {demoActive:true}. A.
- POST /api/demo/exit (431-434). await exitDemo() -> {demoActive:false}. A.
- POST /api/replay (435-441). ReplayControlSchema{playing? bool, speed? int 1..600, steps? int 1..1440}. Order: setSpeed, setPlaying, then await stepReplay(steps); -> replayState(). none (agent tick is a no-op without agents).

### static (app.ts:443-444)
if (existsSync(webDist)) void app.register(fastifyStatic, { root: webDist }), webDist = REPO_ROOT/apps/web/dist. See section 5.

## 2. Auth / network / limits

- Token: randomBytes(32).toString('hex') generated inside buildApp on every process start (app.ts:58); not persisted. Issued by GET /api/session as {token} (app.ts:95-100). No cookies anywhere (grep cookie in apps/server/src: not found).
- Comparison: constant-time sameToken = equal byte length && crypto.timingSafeEqual (app.ts:49-53).
- Supplying it: header "Authorization: Bearer <token>" (strip regex ^Bearer , app.ts:77); ONLY for path === '/api/events' the query param ?token= is also accepted (app.ts:78). If an Authorization header exists it wins (header ?? query). Missing/wrong -> 401 {error:'Unauthorized'}.
- Auth applies only to paths starting /api/ except exactly /api/session (app.ts:75-76); path = request.url.split('?')[0]. Static files and / need no token.
- Host check (app.ts:67-71): allowedHosts = set of new URL(origin).host for each allowed origin; request Host header must be in it else 403 {error:'Host not allowed'} (DNS-rebinding guard). Applies to ALL paths including static. Exact compare including port.
- Origin check (app.ts:72-73): if Origin header present and not exactly in config.allowedOrigins -> 403 {error:'Origin not allowed'}. Absent Origin is fine.
- Order inside the single onRequest hook: Host -> Origin -> (api only) token.
- /api/session extra: sec-fetch-site in {same-origin, none, absent} = same-origin; otherwise requires an Origin header (already allowlisted) else 403 "Cross-site request refused" (app.ts:96-98).
- Allowed origins (config.ts:21-27): http://localhost:5173, http://127.0.0.1:5173, http://localhost:PORT, http://127.0.0.1:PORT, plus comma-separated MOCHIPREP_ALLOWED_ORIGINS (trimmed, empties dropped). Allowed Hosts therefore: localhost:5173, 127.0.0.1:5173, localhost:PORT, 127.0.0.1:PORT (+extras).
- CORS: no CORS plugin, no Access-Control-* headers anywhere (grep: not found). Protection is the Origin allowlist + token; the browser app is same-origin (vite proxy in dev, direct in prod). OPTIONS preflights are not handled.
- Env vars (apps/server/src/config.ts:14-29, .env.example): MOCHIPREP_PORT (default 8787; Number(env.MOCHIPREP_PORT ?? 8787), no validation), MOCHIPREP_DATA_DIR (default <REPO_ROOT>/.mochiprep, resolve()d), MOCHIPREP_ALLOWED_ORIGINS. apps/web/vite.config.ts also reads MOCHIPREP_PORT for its proxy. No other env var read by the server.
- Bind: host typed as the literal '127.0.0.1' (config.ts:8), app.listen({host, port}) (main.ts:13). Never 0.0.0.0.
- Body limit: 256 KiB (app.ts:56). Compression: none. Timeouts: Fastify defaults (not configured).
- Rate limits: none on the HTTP server (grep not found). Only outbound/caching limits: OKX candle cache 3 s, instrument cache 5 min, news cache 5 min, vault ops serialized.
- Security headers (CSP, X-Frame-Options, ...): none set (grep not found).
- Shutdown (main.ts:16-23): SIGINT/SIGTERM -> runtime.stop(); app.close(); db.close(); exit(0). A preClose hook ends all open SSE responses so close() does not hang (app.ts:63-65). Startup log: "MochiPrep backend on http://127.0.0.1:<port> (data: <dir>)" (main.ts:14).
- Tested behaviours (apps/server/test/api-integration.test.ts:152-160): missing token with valid Host localhost:8787 -> 401; Host evil.invalid -> 403; foreign Origin with valid token -> 403; Origin http://localhost:8787 -> 200.

## 3. SSE /api/events

Handler app.ts:112-131. reply.hijack(); writeHead(200, {content-type: text/event-stream, cache-control: 'no-cache, no-transform', connection: keep-alive}). No retry: line, no id:, no named event: field. Every message is "data: <JSON>\n\n" (default "message" event; client uses source.onmessage, apps/web/src/store.ts:269-272).

On connect, in order: subscribe to runtime; start per-connection heartbeat setInterval(15_000) sending {type:'heartbeat'} (first beat after 15 s, none immediately); add stream to the streams set; then immediately send {type:'feed',status} for each of runtime.feeds() (REPLAY status first, then LIVE) and {type:'replay',state:replayState()}. On request.raw 'close': clear heartbeat, unsubscribe, remove from set.

ServerEvent union (packages/shared/src/domain.ts:629-635):
- {type:'heartbeat'}
- {type:'quote', quote:Quote}; Quote = {source:'REPLAY'|'LIVE', symbol, bid, ask, last, mark (strings), markEstimated:bool, fundingRate:string|null, nextFundingTime:number|null, ts:number ms} (domain.ts:101-114)
- {type:'feed', status:FeedStatus}; FeedStatus = {source, provider, state:'CONNECTED'|'CONNECTING'|'DEGRADED'|'DISCONNECTED'|'OFFLINE_REPLAY', lastMessageAt:number|null, detail?:string} (domain.ts:116-124)
- {type:'replay', state:ReplayState}; ReplayState = {loaded:true, playing, speed, now, startTime, endTime, atEnd, progress 0..1} (domain.ts:618-627, built runtime.ts:274-288; loaded is always true)
- {type:'invalidate', topics:Topic[]}
- {type:'toast', level:'info'|'warn'|'error', message}
Topic (domain.ts:637): 'accounts'|'orders'|'positions'|'agents'|'decisions'|'providers'|'arena'|'exchange'|'instruments'|'news'|'vault'. (Web ALL_TOPICS omits 'news', store.ts:104.)

Emission and throttling (apps/server/src/runtime.ts):
- quote LIVE: every OKX ticker WS message calls engine.onQuote(quote) unthrottled; the SSE quote is sent only if >= 250 ms since the last broadcast for THAT symbol (QUOTE_BROADCAST_MS=250, per-symbol Map; runtime.ts:235-242).
- quote REPLAY: engine.onQuote runs on every tape step; SSE quote events are emitted once per replay batch, for the quotes of the LAST step processed, one per default symbol (doSteps end, runtime.ts:358).
- invalidate: coalesced. invalidate(...topics) adds to a dirty Set and arms ONE 200 ms timer (INVALIDATE_MS=200; not re-armed while pending); on fire emits one {type:'invalidate', topics:[...dirty]} and clears. Empty set emits nothing (runtime.ts:210-218).
- replay: emitted on setSpeed (always), on setPlaying only when the value changed, at the end of every doSteps batch (even with 0 quotes), and at startDemo.
- feed: emitted by the live provider on every state change (CONNECTING/CONNECTED/DEGRADED/DISCONNECTED) via live.onStatus (runtime.ts:186). Replay feed onStatus is a no-op, so the REPLAY feed status is only in the per-connection snapshot and /api/state.
- toast: unthrottled direct emit. Sources: runner onNotice, liquidation ("<account>: <side> <symbol> was liquidated (simulated)", level error, runtime.ts:222-230), onSignal ("<agent> suggests <action> <symbol>", info), live-tick error (error), "Replay stopped: ..." (error), "<acct>: lost sync with OKX. Agents paused. ..." (error), vault unlock OKX reconnect failure (error).
- heartbeat: 15 s per connection, emitted by the route only.
- Client expectations (apps/web/src/store.ts:250-300): token via ?token=; silence > 40 s (STREAM_SILENCE_MS, checked every 5 s) = dead stream; on error closes, forgets token, re-fetches /api/session and reconnects after 2 s; on open it refetches state + all topics.
- Fan-out is a synchronous Set of listeners (runtime.ts:206-208); per-client writes go straight to raw.write with no buffering/backpressure handling.

## 4. Runtime (apps/server/src/runtime.ts, 970 lines)

Constants (runtime.ts:43-53): MOCK_PROVIDER_ID='mock'; REPLAY_BATCH_MS=100; QUOTE_BROADCAST_MS=250; INVALIDATE_MS=200; REPLAY_EQUITY_EVERY_MS=15*60_000 (tape minutes); LIVE_EQUITY_EVERY_MS=60_000; DEFAULT_SPEED=20; EXCHANGE_SYNC_MS=5_000; PASSWORD_WRAP_ID='password'.

Startup order (main.ts:7-13; Runtime ctor runtime.ts:118-155):
1. loadConfig(); openDatabase(<dataDir>/mochiprep.db) (db/database.ts:11-30): mkdir 0700, WAL + foreign_keys + synchronous NORMAL + secure_delete ON, chmod 0600 on db/-wal/-shm, apply migrations from the migrations dir sorted by name, each in its own tx recorded in schema_migrations.
2. new Runtime({db}): SqliteStore; keys = empty Map if a vault exists else all plaintext provider keys; kdf; new ReplayMarketData(replayMinutes?) (3 tapes, 18*1440 min default); live = new OkxMarketData(); providerFactory; NewsService(() => this.newsSources(), newsFetch); PaperExecutionEngine; AgentRunner; seedMockProvider() (insert provider row 'mock' if absent); for every row of the instruments table: registerInstrument(spec) + live.trackSymbol(symbol); restore().
3. buildApp(runtime, config) (hooks, routes, static).
4. runtime.start() (runtime.ts:184-189): live.onQuote(->onLiveQuote), live.onStatus(->emit feed), live.start() (OKX WS + REST poll), liveTimer = setInterval(liveTick, 1000).
5. app.listen.

restore() (runtime.ts:176-182): setting replay.cursor -> replay.seek(cursor); replay.speed (default 20); lastReplayEquityAt = replay.now(); runner.disarmAfterRestart() (agents with running/ANALYZING/EXECUTING -> running=false, status PAUSED, statusDetail "Paused because the backend restarted"; packages/agents/src/runner.ts:122-133). Replay is NOT playing after restart (playing is memory only). demo.active persists via settings.

Settings storage: table settings(key,value JSON) (db/store.ts:526-534). Keys used by runtime: replay.cursor, replay.speed, demo.active, news.sources. Rust SqlitePaperStore (crates/mochi-store/src/paper.rs) wraps ONLY the PaperStore trait; settings, instruments, providers, agents, decisions, vault, sealed_* tables have no Rust accessor (only raw SQL through SqlitePaperStore::connection()).

Data dir: MOCHIPREP_DATA_DIR or <REPO_ROOT>/.mochiprep (REPO_ROOT = 3 levels above apps/server/src, config.ts:5). DB file mochiprep.db.

Instruments registry persistence: migration apps/server/src/db/migrations/003_instruments.sql: CREATE TABLE instruments (symbol TEXT PRIMARY KEY, spec TEXT NOT NULL, added_at INTEGER NOT NULL); spec = JSON of InstrumentSpecification. store.listInstruments ORDER BY symbol; saveInstrument = upsert (db/store.ts:446-459). Live registry = module-global INSTRUMENTS in packages/shared/src/domain.ts:55-74: built-ins BTCUSDT (quantityStep 0.0001, tick 0.1, venueContractValue 0.01), ETHUSDT (0.001, 0.01, 0.1), SOLUSDT (0.01, 0.01, 1); all venueLotSize 0.01, maxLeverage 100, maintenance 0.005, fundingIntervalHours 8, minQuantity = step. unregisterInstrument never removes defaults. Rust: mochi_domain::registry::Instruments is NOT global; the engine owns one (PaperExecutionEngine::instruments_mut) so a Rust server has to keep its own registry and the engine's in sync.

Paper engine wiring (runtime.ts:129-134):
- store: SqliteStore (Rust: SqlitePaperStore)
- getQuote: (source, symbol) => this.market(source).getQuote(symbol); market('REPLAY') = ReplayMarketData, market('LIVE') = OkxMarketData
- now: (source) => this.market(source).now() (replay: tape time of cursor + 60_000; live: Date.now())
- newId: not passed in TS; engine default "<prefix>_<crypto.randomUUID()>" (packages/core/src/paper/engine.ts:87), prefixes acct, ord, fill, pos. Rust deps require new_id: Box<dyn FnMut(&str)->String+Send>.
- onEvent: onEngineEvent -> invalidate('accounts','orders','positions','arena'); for kind 'liquidation' also runner.pauseAccountAgents(accountId, "Position liquidated on <symbol>") + toast error.
- Rust PaperEngineDeps (crates/mochi-core/src/paper/engine.rs:80-87): {store, instruments, get_quote, now, new_id, on_event} (types in section 9).

Quote fan-out:
- LIVE: OKX ticker WS message -> OkxMarketData.applyTicker -> quoteListeners -> Runtime.onLiveQuote: engine.onQuote(quote) (resting orders, liquidations, funding) then throttled SSE emit.
- REPLAY: ReplayMarketData.step() fires its own quoteListeners but Runtime does not subscribe to them; doSteps calls engine.onQuote itself per quote, then awaits runner.tick('REPLAY').

Quote age rules: engine hard limit MAX_FILL_QUOTE_AGE_MS = 60_000: now - quote.ts > 60_000 -> OrderRejection MARKET_DATA_STALE "quote is <N>s old" (packages/core/src/paper/engine.ts:36,312-314; Rust crates/mochi-core/src/paper/engine.rs:27,680-687, seconds via js_round). Missing quote -> "NO_MARKET_DATA: no quote for X". Separate per-agent maxQuoteAgeSec (risk settings; DEFAULT_LIVE_LIMITS 10 s) is enforced in the risk engine (packages/core/src/risk/engine.ts:116,247) for agent decisions only. Rust balance_of counts a position with no quote as flat (views.rs:31-33).

Replay clock (runtime.ts:272-362):
- Memory state: playing=false, speed (setting replay.speed, default 20 tape-minutes/second), stepCredit=0, stepping Promise|null, replayTimer.
- setPlaying(true) starts setInterval(onReplayTimer, 100); false clears it; both emit replay (only when value changes).
- onReplayTimer: if a batch is in flight return (no credit accrues: "the tape waits for agents"); else stepCredit += speed*100/1000; steps = floor(stepCredit); if <1 return; stepCredit -= steps; void stepReplay(steps).
- stepReplay(n): while (stepping) await stepping; then stepping = doSteps(n).finally(clear). Concurrent manual POST /api/replay {steps} serialize behind it and the HTTP call awaits completion.
- doSteps: signalRaised=false; loop i<n && !signalRaised: quotes = replay.step() (break if empty); engine.onQuote for each; await runner.tick('REPLAY') (Promise.all of ALL due agents' cycles, so a slow model blocks the tape); if replay.now() - lastReplayEquityAt >= 15 tape-minutes: lastReplayEquityAt = replay.now(), recordEquity('REPLAY') (PAPER REPLAY accounts), invalidate('arena','accounts'). Exception: setPlaying(false) + toast "Replay stopped: <redacted>". After the loop: setSetting('replay.cursor', replay.position); emit quote for each of the last quotes; if replay.atEnd setPlaying(false); emit replay; invalidate('accounts','positions').
- onSignal(record): for REPLAY decisions sets signalRaised=true and setPlaying(false); toast info.
- stop(): setPlaying(false); await in-flight step; clear liveTimer + invalidateTimer; live.stop().
- ReplayMarketData (packages/market-data/src/replay.ts): cursor starts at WARMUP_MINUTES=4*1440; step() cursor++ and one Quote per default symbol; atEnd = cursor >= length-1; now() = time(cursor)+60_000; firstPlayableTime = time(WARMUP)+60_000; endTime = time(last)+60_000; TAPE_START = Date.UTC(2026,0,1) = 1_767_225_600_000; tape from seeded mulberry32 (synthetic.ts). Already ported in Rust (section 9).
- replayState.progress = clamp((now-start)/(end-start), 0, 1).

Live tick (runtime.ts:244-264) every 1000 ms, re-entrancy guarded: if >=5 s since last: syncExchanges() (each OKX session engine.sync(); first failure pauses its agents + toast); then await runner.tick('LIVE'); every 60 s recordEquity('LIVE') for LIVE PAPER accounts + invalidate('arena'). Errors -> toast error (redacted). For a no-OKX-trading, no-agents server only the equity recording remains relevant.

Demo (runtime.ts:366-428):
- isDemoActive = setting demo.active === true.
- startDemo (sync): no-op if active. replay.seek(0) (clamps to WARMUP), persist replay.cursor, lastReplayEquityAt = replay.now(). Creates 3 REPLAY isDemo accounts ("Momentum account", "Reversion account", "Reckless account", 10000 default) each with a demo agent via createAgent(..., isDemo=true): names Momentum Mochi / Reversion Daifuku / Reckless Rice Cake, providerId 'mock', models MOCK_MODELS.{momentum,reversion,reckless}, BTCUSDT, 15m, intervalSec 900, executionMode AUTO, DEFAULT_RISK_SETTINGS; plus a 4th REPLAY isDemo account "Demo terminal". Sets demo.active=true; invalidate(accounts,agents,arena,decisions,orders,positions); emits replay. So UI total equity = 4 x 10,000 = 40,000.00 (e2e asserts it).
- resetDemo: setPlaying(false); await in-flight step; clearDemo(); startDemo(). exitDemo: same without startDemo, invalidate same topics.
- clearDemo (one tx): delete agents with isDemo; for accounts with isDemo deleteAccountData(id,false); demo.active=false. User data untouched.
- Determinism test apps/server/test/demo-reset.test.ts (replayMinutes 6500, 600 steps twice around resetDemo, equal balances and economic fills) needs agents with the mock provider to produce fills.
- Demo cannot be completed without the agents plan: it creates agents, needs provider row 'mock', and decisions come from the in-process mock AI (packages/ai-providers/src/mock.ts, no network).

Misc: balanceOf (runtime.ts:878) never throws (PAPER -> engine.balanceOf; exchange venue -> cached sync balance, else last equity point, else initialBalance with zeros). quotes(source) = listSymbols().flatMap(getQuote). feeds() = [replay.status(), live.status()]. execution(accountId) returns the paper engine for venue PAPER, else the OKX session engine or throws "<name> is not connected to OKX. Enter its API key in Settings.". OkxMarketData.getQuote is undefined until a ticker with bid, ask, last, ts has arrived.

## 5. Static web assets

- Build: pnpm build -> pnpm --filter @mochiprep/web build -> vite build (apps/web/package.json:7-8; root package.json "build"). vite.config.ts sets no build.outDir, so output is apps/web/dist (does not exist in this worktree right now).
- Serving: @fastify/static ^9 (9.3.0 installed) registered with only { root: REPO_ROOT/apps/web/dist }, and only if that dir exists when buildApp runs (app.ts:443-444). In the esbuild bundle apps/server/dist/main.mjs, REPO_ROOT (resolve(dirname(import.meta.url), '../../..')) is the same depth as from apps/server/src, so it still resolves to the repo root.
- No SPA fallback (no setNotFoundHandler, wildcard default). The web app uses hash routing (location.hash in apps/web/src/App.tsx:26 and store.ts:122,133; e2e uses /#settings), so only / and hashed assets are needed. Files: index.html (links /mochi.svg and /src/main.tsx in source), assets/*, mochi.svg (apps/web/public/mochi.svg).
- Cache headers: none set by the repo. @fastify/static/send defaults: Cache-Control: public, max-age=0, ETag, Last-Modified, Accept-Ranges: bytes; index.html for /; dotfiles 'allow' (index.js:41). (Package defaults, not verified by running.)
- CSP / security headers: none. Static responses still pass the Host/Origin onRequest hook (Host must be allowed), but not the token check.
- Rust design spec wants the React build embedded in the binary and served the same way (spec line 148).

## 6. Public OKX market data adapter (packages/market-data/src/okx.ts, 359 lines)

Constants (okx.ts:14-27): REST_BASE https://www.okx.com; WS_URL wss://ws.okx.com/ws/v5/public; BARS {1m:'1m',5m:'5m',15m:'15m',1h:'1H',4h:'4H'}; PING_MS=20_000; SILENCE_LIMIT_MS=30_000; REST_POLL_MS=5_000; CANDLE_CACHE_MS=3_000; MAX_BACKOFF_MS=30_000; INSTRUMENT_CACHE_MS=300_000; CRYPTO_CATEGORY='1'. instId: BTCUSDT -> BTC-USDT-SWAP (toInstId, okx.ts:23). HTTP timeout 8 s (AbortSignal.timeout(8_000), okx.ts:259). Uses global fetch and global WebSocket (Node 22). No auth/API key.

REST (private get<T>(path); envelope {code,msg,data}; non-2xx -> "OKX HTTP <status>"; code !== '0' -> "OKX error <code>: <msg>"):
- Candles: GET /api/v5/market/candles?instId=<id>&bar=<bar>&limit=<min(limit,300)>. Rows are string arrays newest-first [ts,o,h,l,c,vol,volCcy,volCcyQuote,confirm]; parseOkxCandles: time=Number(r[0]), OHLC numbers, volume=Number(r[6]) (base-ccy size; r[5] would be contracts), closed = r[8]==='1', sorted ascending (okx.ts:60-73). Cache key "<symbol>:<timeframe>:<limit>" valid 3 s (okx.ts:287-297). The HTTP route allows limit up to 1000 but the provider returns at most 300.
- Polling per tracked symbol, 3 requests in parallel (Promise.allSettled across symbols, okx.ts:266-285): GET /api/v5/market/ticker?instId=, GET /api/v5/public/mark-price?instType=SWAP&instId=, GET /api/v5/public/funding-rate?instId=. Fields: ticker bidPx/askPx/last/ts; mark markPx; funding fundingRate, fundingTime.
- Instruments: GET /api/v5/public/instruments?instType=SWAP (okx.ts:304). listInstruments(fresh=false) cached 5 min unless fresh; rows -> parseOkxInstrument -> sorted by symbol (localeCompare). There is NO per-instrument getInstrument call in the public adapter: Runtime.addInstrument does (await live.listInstruments(true)).find(s => s.symbol === symbol) (runtime.ts:519-529). (The private OKX client has getContractValue(symbol) used by connectOkx, packages/exchanges/src/okx-client.ts; X only.)
- parseOkxInstrument eligibility (okx.ts:34-57): instId matches ^([A-Z0-9]+)-USDT-SWAP$; settleCcy==='USDT' && ctType==='linear' && state==='live' && ruleType==='normal' && instCategory==='1' && ctValCcy===base; and ctVal, lotSz, minSz, tickSz all present. Spec: symbol <base>USDT, marketType CRYPTO_PERP, base/quote/settlement USDT, contractSize '1', quantityStep = (ctVal*lotSz) decimal toFixed(), minQuantity = ctVal*minSz, priceTick = tickSz, maxLeverage = Number(lever)||1, maintenanceMarginRate '0.005' (hard-coded), fundingIntervalHours 8 (hard-coded), venueContractValue = ctVal, venueLotSize = lotSz.
- Instrument validation otherwise: SymbolSchema regex at the HTTP edge; addInstrument requires not already in INSTRUMENTS and present in the fresh list; removeInstrument refusals listed in section 1.

WebSocket (okx.ts:172-249):
- connect() on start(); on open: attempts=0, subscribe {op:'subscribe', args:[{channel:'tickers'|'mark-price'|'funding-rate', instId}] for each tracked symbol}, state CONNECTED, ping interval 20 s sending the text "ping" (reply text "pong" is ignored in handleMessage). Heartbeat closes the socket if no message for 30 s.
- Messages: JSON {arg:{channel,instId}, data:[row]}; only data[0] used. tickers -> applyTicker (bid/ask/last/ts) and emits a Quote when bid, ask, last and ts are all present; mark-price -> state.mark = markPx (no emit); funding-rate -> fundingRate, nextFundingTime = Number(fundingTime) (no emit). So quotes are emitted only by ticker messages.
- Quote build (okx.ts:88-103): mark = markPx, else JS ((bid+ask)/2).toString() with markEstimated=true; fundingRate / nextFundingTime null when unknown.
- onerror sets detail "WebSocket error"; onclose -> scheduleReconnect(detail ?? "Connection closed") if running: state DEGRADED, detail "<reason>; reconnecting, REST polling meanwhile"; delay min(1000 * 2**attempt, 30000) (1,2,4,...,30 s), attempts reset on successful open; at most one pending reconnect timer.
- REST fallback: setInterval 5 s runs pollRest() only while connection !== 'CONNECTED'; pollRest also runs once on start() and when a symbol is newly tracked while running.
- trackSymbol: add to set, send subscribe if socket OPEN, pollRest. untrackSymbol: send unsubscribe, delete symbol and cached state. Default tracked BTCUSDT/ETHUSDT/SOLUSDT; Runtime adds every row of the instruments table at startup.
- status(): {source:'LIVE', provider:'OKX public', state, lastMessageAt, detail?}; lastMessageAt updated on every WS message and every successful REST poll; now() = Date.now().
- No explicit rate limiter; bounded by candle cache 3 s, instrument cache 5 min, 3 REST calls per symbol per 5 s while WS is down.
- Provider interface (packages/market-data/src/provider.ts): source, name, start(), stop(), getCandles(symbol,tf,limit):Promise<Candle[]>, getQuote(symbol):Quote|undefined, now(), status(), onQuote(l)->unsubscribe, onStatus(l)->unsubscribe, optional listInstruments(fresh?), trackSymbol, untrackSymbol.
- Injection points for tests (OkxOptions): fetchImpl, WebSocketImpl, restBase, wsUrl, backoff. Tests: packages/market-data/test/market-data.test.ts. apps/server/test/okx-live.test.ts concerns the private client (X).

## 7. News service (packages/market-data/src/news.ts, 192 lines)

- Built-in feeds (news.ts:3-10): coindesk "CoinDesk" https://www.coindesk.com/arc/outboundfeeds/rss (on); cointelegraph https://cointelegraph.com/rss (on); decrypt https://decrypt.co/feed (on); theblock "The Block" https://www.theblock.co/rss.xml (on); bitcoinmagazine "Bitcoin Magazine" https://bitcoinmagazine.com/feed (OFF); cryptoslate https://cryptoslate.com/feed/ (OFF). Built-ins come from code; only enabled is overlaid from saved settings (runtime.ts:472-477). User sources are saved in settings key news.sources (NewsSource[], builtIn:false).
- Polling: NO background timer. NewsService.forSymbol refreshes lazily when now - fetchedAt >= REFRESH_MS (5 min); concurrent callers share one in-flight refreshing promise. invalidate() sets fetchedAt=0 (called whenever sources change, runtime.ts:479-483). Cache is memory only (nothing written to SQLite); if every source fails (zero items) while some are enabled the previous cache is kept; lastError = "Could not read: <names>" or null.
- Fetch rules (news.ts:146-170): 8 s timeout per hop (FETCH_TIMEOUT_MS); max 3 redirects (MAX_REDIRECTS=3, loop hop 0..3, then "Too many redirects"); redirect:'manual' and EVERY hop incl. the first is re-validated with validateFeedUrl (error "<problem>: <url>"); Location resolved with new URL(location, url); 3xx without Location returns the response; non-ok -> "HTTP <status>"; headers user-agent "MochiPrep/0.1 (RSS reader)", accept "application/rss+xml, application/atom+xml, application/xml, text/xml"; size check AFTER the full read: text.length > 3_000_000 -> "feed too large".
- validateFeedUrl (news.ts:80-93): must parse as URL ("Not a valid URL"); protocol must be https: ("Feeds must use https"); hostname lowercased with [] stripped is rejected as "Feeds must be on a public host" if: localhost, ends with .local or .localhost, has no dot, matches ^(127|10|0)\., ^192\.168\., ^169\.254\., ^172\.(1[6-9]|2\d|3[01])\., or contains ':'. Text-only check, no DNS resolution, so a public name resolving to a private IP is not blocked.
- sanitizeHeadline (news.ts:38-44): replace every \p{Cc}, \p{Cf}, '<' and '>' with a space; collapse \s+ to one space; trim; slice to 200 chars (MAX_TITLE_CHARS). parseFeed first strips <[^>]*> tags from the title (news.ts:71).
- parseFeed (news.ts:68-77): regex over <(item|entry)[\s>]...</\1> blocks (case-insensitive); CDATA unwrapped; entities decoded (&lt; &gt; &quot; &apos; &nbsp; &amp; and numeric &#..; / &#x..;, code points > 0x10FFFF dropped); date from the first present of pubDate, published, updated, dc:date via Date.parse (NaN -> dropped); link from <link> text or <link href="...">; url kept only if ^https?:// (<=500 chars) else ''; item = {id:"<source>:<title>", source (feed NAME), title, url, publishedAt}.
- selectNews (news.ts:102-120): drop items with publishedAt > now or age > 24 h; newest first; items mentioning the coin first (ticker regex (^|[^A-Za-z0-9])BASE([^A-Za-z0-9]|$) or coin-name word match from COIN_NAMES: BTC bitcoin; ETH ethereum/ether; SOL solana; DOGE dogecoin; XRP ripple; ADA cardano; BNB binance coin; LTC litecoin; AVAX avalanche; SHIB shiba inu), then the rest; max 3 per source (PER_SOURCE_LIMIT); dedupe by lowercased title; default limit 8 (HTTP route passes 12).
- baseCurrency = INSTRUMENTS[symbol]?.baseCurrency ?? symbol.replace(/USDT$/, '') (runtime.ts:486).
- Storage: sources in settings key news.sources; headlines never persisted. Tests: packages/market-data/test/news.test.ts.

## 8. Playwright e2e

Config: playwright.config.ts at repo root (pnpm test:e2e = playwright test). testDir 'e2e' (specs are in e2e/, NOT apps/web/e2e), timeout 90 s, use {baseURL http://127.0.0.1:5199, channel 'chrome', viewport 1440x900}. WEB_PORT=5199, API_PORT=8877. Env passed to BOTH web servers: MOCHIPREP_PORT=8877, MOCHIPREP_DATA_DIR=mkdtempSync(tmpdir()/mochiprep-e2e-*) (throwaway DB), MOCHIPREP_ALLOWED_ORIGINS=http://127.0.0.1:5199. webServer[0]: command "pnpm --filter @mochiprep/server start" (= node build.mjs && node ... dist/main.mjs), readiness = TCP port 8877, reuseExistingServer false. webServer[1]: "pnpm --filter @mochiprep/web exec vite --port 5199", port 5199. Vite proxies /api -> http://127.0.0.1:<MOCHIPREP_PORT> with changeOrigin:true (vite.config.ts:14-16), so the backend sees Host 127.0.0.1:8877 and, for non-GET same-origin fetches, Origin http://127.0.0.1:5199 (whitelisted by the env var). To swap in a Rust server change webServer[0] only; the binary must honour MOCHIPREP_PORT, MOCHIPREP_DATA_DIR, MOCHIPREP_ALLOWED_ORIGINS and listen before Playwright's port probe times out. e2e needs no built apps/web/dist (vite serves the UI).

e2e/demo.spec.ts (3 tests, one shared backend/DB, serial in file order):
1. "market data to agent decision to risk to paper order to position to PnL in the UI" (lines 5-84). Calls: GET /api/session, /state, /events (SSE), topic refetches (accounts, agents, providers, signals, decisions, instruments, vault, exchange/status, arena); POST /api/demo/start; Markets page: GET /api/market/candles + /market/quotes (REPLAY is the UI default source, store.ts:131; asserts chart canvas and ticker-last > 0); AI Agents page: 3 agent cards, POST /api/agents/:id/start x3, status "Waiting" x3; replay speed select 240 + Play -> POST /api/replay; Trading Arena: GET /api/arena (3 rows, a minus sign, replay-time reaches 2026-01-06..09 within 45 s); POST /api/agents/stop-all; Trade History: GET /api/decisions?limit=1000, fills, ledger, closed-positions, GET /api/accounts/:id/audit ("Ledger balances match"), expects EXECUTED and RISK_REJECTED decisions with risk-check text; Trading page: accounts "Reckless account" (fills > 0) and "Demo terminal" (equity 10,000.00), POST /api/orders, positions, POST /api/positions/:accountId/:symbol/close; Settings "Reset demo" -> POST /api/demo/reset; Dashboard total-equity 40,000.00 again. NEEDS: agents + mock AI provider (in-process, no network, no keys), risk engine, paper engine. Does NOT need vault, external AI, or OKX private trading. The live OKX public feed runs in the background but nothing asserts on it.
2. "an agent in suggestion mode waits for the user, then the suggestion becomes a position" (86-113): PUT /api/agents/:id (executionMode SIGNAL via "Suggest to me"), POST agents/:id/start, POST /api/replay (speed 60, play); expects a pending suggestion (GET /api/signals) within 30 s, the tape stops (replay playing=false via signalRaised), POST /api/signals/:id/execute, a position row appears. NEEDS agents + mock provider + signals.
3. "a 1% trailing stop set in the protection dialog shows its trigger price in the position row" (115-136): "Demo terminal" paper account, POST /api/orders, PUT /api/positions/:accountId/:symbol/protection with trailingStop {kind RATIO, callback '1', activationPrice null}; asserts trailing-trigger text below mark and > 97% of mark. Needs paper engine + trailing stops, but the account comes from POST /api/demo/start (creates agents) so it depends on the demo module.

e2e/display.spec.ts (1 test, lines 5-35): "time zone and color flip apply across the UI and survive a reload": opens /#settings; needs the replay-time element populated (GET /api/state replay + SSE replay event); everything else is localStorage-driven UI (time zone, color flip, theme). The Settings page mounts VaultCard (GET /api/vault), ExchangeCard (GET /api/exchange/status), PairsCard (GET /api/instruments, GET /api/instruments/available -> live OKX REST), NewsSourcesCard (GET /api/news/sources, GET /api/news?symbol=), providers list; unasserted, but they should answer. No agents/AI/vault/OKX trading required.

Other server tests (vitest, root vitest.config.ts, run by pnpm test): apps/server/test/api-integration.test.ts (194 lines; app.inject; quote->AI decision->risk->order->position->PnL walk, protection keep/remove trailing, host/token/origin checks, shutdown with open SSE stream, vault HTTP flow incl. 400 short password, 403 wrong password, 400 bad reset phrase), demo-reset.test.ts, instruments.test.ts, vault.test.ts, recovery.test.ts, protection.test.ts, okx-live.test.ts, agent-*.test.ts, provider-*.test.ts, paper-engine.test.ts, golden.test.ts.

## 9. Existing Rust crates (names/signatures only)

Workspace Cargo.toml: members crates/*, exclude crates/golden (fixtures), resolver 2, edition 2021, rust-version 1.99, workspace deps serde(derive), serde_json(float_roundtrip, raw_value), thiserror 2, sha2, tempfile, num-bigint, num-traits. Crates: mochi-decimal, mochi-domain, mochi-core, mochi-market, mochi-store, mochi-vault. rust-toolchain.toml present.

mochi-domain (crates/mochi-domain/src/lib.rs). Re-exports: accounting::{default_account_config, default_live_limits, default_risk_settings, AccountBalance, AccountVenue, AiAction, AiDecisionData, AiTrailingStop, EntryOrderType, EquityPoint, ExternalProtection, ExternalProtectionKind, Fill, LedgerEvent, LedgerEventType, Liquidity, Order, OrderRequest, OrderResult, OrderSide, OrderStatus, OrderType, PaperAccount, PaperAccountConfig, Position, PositionSide, PositionStatus, PositionView, Protection, RiskCheck, RiskResult, RiskSettings, SizedTrade, TrailingKind, TrailingLimits, TrailingStopSpec, TrailingStopView, TRAILING_LIMITS}; instruments::{default_instrument(&str)->Option<InstrumentSpecification>, default_instruments()->Vec<InstrumentSpecification>, InstrumentSpecification{symbol, market_type:String, base_currency, quote_currency, settlement_currency, contract_size, quantity_step, min_quantity, price_tick, max_leverage:u32, maintenance_margin_rate, funding_interval_hours:u32, venue_contract_value, venue_lot_size}, DEFAULT_SYMBOLS:[&str;3]}; market::{Candle{time:i64, open,high,low,close,volume:f64, closed:bool}, DataSource{Replay,Live} (serde UPPERCASE), Timeframe{M1,M5,M15,H1,H4} (serde "1m","5m","15m","1h","4h"; fn ms(self)->i64), Quote{source, symbol, bid, ask, last, mark: String, mark_estimated:bool, funding_rate:Option<String>, next_funding_time:Option<i64>, ts:i64} (camelCase)}. pub mod jsnum (js_round, js_to_fixed, js_number_string, ...), pub mod registry: Instruments::{with_defaults(), get(&self,&str)->Option<&InstrumentSpecification>, register(&mut self, spec), unregister(&mut self,&str)} (defaults never removed). NOT in Rust (grep of crates/mochi-domain/src: not found): ServerEvent, Topic, FeedStatus, ConnectionState, ReplayState, VaultStatus, NewsSource, NewsItem, Agent, ProviderConfig, request-body validation (zod equivalents).

mochi-market (crates/mochi-market/src/lib.rs: pub mod replay; pub mod tape;)
- tape.rs: const TAPE_START: i64 = 1_767_225_600_000; TAPE_MINUTES: usize = 18*1440; WARMUP_MINUTES: usize = 4*1440; pub struct Tape{pub open,high,low,close,volume: Vec<f64>} with len(), is_empty(), time(i:usize)->i64, candle(i:usize)->Candle; pub fn generate_tape(symbol:&str, minutes:usize)->Option<Tape>.
- replay.rs: pub struct ReplayMarketData { new(minutes: Option<usize>)->Self, len(), is_empty(), position()->usize, at_end()->bool (cursor+1 >= len), first_playable_time()->i64, end_time()->i64, now()->i64, seek(&mut self, index:i64) (clamped), step(&mut self)->Vec<Quote> (empty at end), get_quote(&self,&str)->Option<Quote>, get_candles(&self,&str, Timeframe, limit:usize)->Vec<Candle> }. No listeners, no feed status, no ReplayState (header comment: listeners and feed status belong to the server runtime).
- Tests: tests/golden_replay.rs, tests/golden_tape.rs against crates/golden/replay.json, tape.json.
- Not in Rust: OKX live adapter (REST/WS), news, any LIVE market data (grep: not found).

mochi-store (crates/mochi-store/src/lib.rs, paper.rs; deps rusqlite 0.37 bundled, serde, serde_json, thiserror, mochi-core, mochi-domain)
- pub const MIGRATIONS: &[(&str,&str)] (the 8 TS .sql files, 001..008, embedded via include_str! from apps/server/src/db/migrations)
- pub enum StoreError{Sqlite(rusqlite::Error), Io(std::io::Error), Migration{name,source}, CheckpointBlocked}
- pub fn open_database(path:&Path)->Result<Connection,StoreError> (creates dir 0700 / file 0600, same PRAGMAs as Node, runs migrations); pub fn open_in_memory()->Result<Connection,StoreError>; pub fn scrub(conn:&Connection)->Result<(),StoreError> (WAL checkpoint TRUNCATE, error if blocked); #[doc(hidden)] pub fn migrate_with(conn:&Connection, migrations:&[(&str,&str)])->Result<(),StoreError>
- pub use paper::SqlitePaperStore: struct{conn:Connection, depth:Cell<u32>}; new(conn:Connection)->Self; connection(&self)->&Connection; impl PaperStore. Single connection, not Sync (Cell) but Send. No wrappers for settings, instruments, providers, agents, decisions, vault, sealed_* tables.
- Tests: tests/migrations.rs, paper_engine.rs, paper_recovery.rs, paper_scenarios.rs, paper_store.rs.

mochi-core (crates/mochi-core/src/lib.rs: mod error (CoreError); pub mod fill_model, ledger, paper, performance, perp, risk)
- paper/mod.rs re-exports: AccountConfigOverrides, Clock, CreateAccountArgs, EngineError, EngineEvent, EventSink, IdSource, PaperEngineDeps, PaperExecutionEngine, ProtectionResult, QuoteLookup, PaperStore, StoreFailure, advance_extreme, trailing_trigger, validate_trailing, DailyRisk.
- trait PaperStore (paper/store.rs; synchronous): transaction<T,E:From<StoreFailure>>(&self, f: impl FnOnce()->Result<T,E>)->Result<T,E>; insert_account, save_account, get_account(id)->Option, list_accounts, delete_account_data(id, keep_account_row:bool); insert_order, save_order, get_order, get_order_by_client_id(account,client_id), list_open_orders(account), list_open_orders_for_market(source,symbol), list_orders(account,limit); insert_fill, list_fills_by_order, list_fills(account,limit); insert_position, save_position, get_open_position(account,symbol), list_open_positions, list_open_positions_for_market, list_closed_positions(account,limit); append_ledger(&LedgerEvent), list_ledger(account, since_ts:Option<i64>); upsert_equity_point(&EquityPoint), list_equity(account,limit) (latest limit, oldest first). All return Result<_, StoreFailure(pub String)>.
- PaperExecutionEngine<S: PaperStore> (paper/engine.rs:285+; synchronous; mutating methods take &mut self; uses RefCell internally): new(PaperEngineDeps<S>); store()->&S; instruments()->&Instruments; instruments_mut()->&mut Instruments; create_account(CreateAccountArgs)->Result<PaperAccount,EngineError>; reset_account(&str)->Result<PaperAccount,EngineError>; place_order(OrderRequest)->Result<OrderResult,EngineError>; cancel_order(&str)->Result<(),EngineError>; close_position(account_id:&str, symbol:&str, fraction:f64, client_order_id:&str)->Result<OrderResult,EngineError>; set_protection(account_id, symbol, &Protection)->Result<ProtectionResult{before,after},EngineError>; protection_of(account_id, symbol)->Result<Protection,EngineError>; on_quote(&mut self,&Quote)->Result<(),EngineError> (market.rs:45); balance_of(account_id)->Result<AccountBalance,EngineError>; position_views(account_id)->Result<Vec<PositionView>,EngineError>; open_orders(account_id); daily_risk(account_id)->Result<DailyRisk{daily_pnl,day_start_equity},EngineError>; record_equity(&mut self, account_id)->Result<(),EngineError>.
- PaperEngineDeps<S>{store:S, instruments:Instruments, get_quote:QuoteLookup, now:Clock, new_id:IdSource, on_event:EventSink}; QuoteLookup=Box<dyn Fn(DataSource,&str)->Option<Quote>+Send>; Clock=Box<dyn Fn(DataSource)->i64+Send>; IdSource=Box<dyn FnMut(&str)->String+Send>; EventSink=Box<dyn FnMut(EngineEvent)+Send>.
- CreateAccountArgs{name:String, source:DataSource, initial_balance:Option<String>, is_demo:Option<bool>, config:Option<AccountConfigOverrides>}; EngineError{Invalid(String), Store(StoreFailure)} (Display = TS message); EngineEvent serde-tagged "kind": Fill{fill,order}, Liquidation{position}, Funding{account_id,symbol,amount}, Order{order}.
- MAX_FILL_QUOTE_AGE_MS = 60_000 (engine.rs:27; private const).
- Not in Rust: async ExecutionEngine trait shared by paper+OKX, AgentRunner, ledger audit exists as a function in the ledger module (crates/mochi-core/src/ledger.rs; not individually listed), risk engine exists (risk.rs). set_protection lacks meta and a "pending" flag; there is no "keep trailing stop when absent" logic (route-level in TS, app.ts:240).
- Tests: tests/golden_math.rs, golden_risk.rs.

mochi-vault (crates/mochi-vault/src/lib.rs; deps aes-gcm, base64, rand, scrypt, unicode-normalization)
- const WRAP_CONTEXT = "mochiprep:vault-key"; type DataKey = [u8;32]; struct KdfParams{n:u32 (serde "N"), r:u32, p:u32}; const DEFAULT_KDF{n:131_072,r:8,p:1}; struct PasswordWrap{salt:String, kdf:KdfParams, wrapped_key:String} (serde camelCase wrappedKey; the JSON stored in vault_keys.params); enum VaultError{WrongPassword ("Wrong master password"), Malformed, Decrypt, Kdf}.
- fn provider_context(id:&str)->String ("provider-key:{id}"); fn exchange_context(account_id:&str)->String ("okx-credentials:{id}"); fn new_data_key()->DataKey; fn seal(key:&DataKey, plaintext:&str, context:&str)->String; fn open(key:&DataKey, sealed:&str, context:&str)->Result<String,VaultError>; fn wrap_with_password(data_key:&DataKey, password:&str, kdf:KdfParams)->Result<PasswordWrap,VaultError>; fn unwrap_with_password(wrap:&PasswordWrap, password:&str)->Result<DataKey,VaultError> (both slow, ~128 MiB: run in spawn_blocking).
- Not in Rust: the vault state machine (OFF/LOCKED/UNLOCKED, exclusive-lock, sealed-key storage, scrub orchestration), i.e. Runtime.enableVault/unlock/changePassword/disableVault/resetVault.

## 10. Build / scripts / frontend API discovery

- Root package.json scripts: dev = "pnpm --parallel --filter @mochiprep/server --filter @mochiprep/web dev"; build = "pnpm --filter @mochiprep/web build"; start = "pnpm --filter @mochiprep/server start"; test = "vitest run"; test:watch; test:e2e = "playwright test"; typecheck = "pnpm -r --parallel typecheck"; golden = "pnpm --filter @mochiprep/server golden". engines node >=22.13.0; packageManager pnpm@11.24.0.
- apps/server/package.json scripts: dev = "tsx watch --clear-screen=false --max-semi-space-size=1 --disable-warning=ExperimentalWarning src/main.ts"; build = "node build.mjs"; start = "node build.mjs && node --max-semi-space-size=1 --enable-source-maps --disable-warning=ExperimentalWarning dist/main.mjs"; typecheck = "tsc -p tsconfig.json"; golden = "tsx golden/generate.ts". Runtime deps: fastify ^5.12, @fastify/static ^9, zod ^4.6 + workspace packages (agents, ai-providers, core, exchanges, market-data, shared). Uses node:sqlite DatabaseSync, global fetch/WebSocket.
- apps/server/build.mjs: esbuild bundles src/main.ts -> dist/main.mjs (platform node, esm, target node22, sourcemap); externals = the server's own third-party package.json dependencies (fastify, @fastify/static, zod; not @mochiprep/*); workspace TS packages and decimal.js are bundled; then cpSync src/db/migrations -> dist/migrations (database.ts reads migrations next to itself). It does NOT build the web app, so `pnpm start` serves apps/web/dist only if `pnpm build` ran earlier.
- Dev topology: web on http://127.0.0.1:5173 (vite, strictPort, host 127.0.0.1) proxying /api to http://127.0.0.1:<MOCHIPREP_PORT ?? 8787> with changeOrigin; production-like: backend serves apps/web/dist on 8787.
- How the frontend finds the API: always same-origin relative URLs; no base-URL setting, env var or absolute URL (grep fetch( in apps/web/src: only lib/api.ts). apps/web/src/lib/api.ts: GET /api/session -> token kept in a module variable -> every call sends "authorization: Bearer <token>"; content-type application/json only when a body exists (POSTs without body, e.g. /api/demo/start, /api/agents/:id/start, /api/agents/stop-all, send no content-type and no body, so the server must accept empty POSTs); on 401 it clears the token and retries once; error text = json.error, fallback "Request failed (<status>)". SSE: new EventSource("/api/events?token=<token>") (store.ts:257). Consequence for Rust: serve SPA and /api from one origin/port, /api/session unauthenticated, keep {error:string} bodies, keep 401 = expired token (hence 403 for wrong vault password).

## Gaps / not found (quick list)
- No axum/tokio/reqwest/tokio-tungstenite in any Cargo.toml yet.
- No Rust: ServerEvent/Topic/ReplayState/FeedStatus types, OKX public adapter, news service, settings/instruments store accessors, runtime orchestration, demo (needs agents), vault state machine, request-body validation, static asset embedding.
- No CORS, rate limiting, CSP, cookies, SPA fallback, compression in the TS server (so nothing to port).
- apps/web/dist not built in this worktree.
