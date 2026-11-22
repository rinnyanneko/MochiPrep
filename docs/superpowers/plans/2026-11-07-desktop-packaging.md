# Desktop Packaging (Background Server + Tauri Client) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship MochiPrep on Windows as a background server process (keeps agents trading when the window is closed) plus a Tauri desktop client with a system-tray icon.

**Architecture:** The server stays exactly what it is today: Fastify on `127.0.0.1`, serving the built web UI same-origin, with the per-start session token. It is bundled with esbuild into one `server.mjs` and run by a copy of `node.exe` shipped inside the installer. The Tauri app is a thin supervisor: it finds a running server through `<dataDir>/server.json` or spawns one detached, then opens a window on `http://127.0.0.1:<port>`. Closing the window hides it; the tray's quit item stops the server gracefully through a shutdown route.

**Tech Stack:** Node >= 22.13, esbuild, Tauri 2 (Rust, WebView2), `tauri-plugin-single-instance`, `tauri-plugin-opener`, `tauri-plugin-dialog`, `ureq`, `serde_json`.

**Spec:** No separate spec file. The design was agreed in conversation on 2026-11-07 and is restated here:
- Server and client run on the same computer. No remote access, no change to the bind address.
- Closing the client window leaves the server and its agents running in the background.
- Windows first. macOS/Linux are not in scope and get no placeholder code.
- No Flutter; the client reuses the existing React UI unchanged.

## Global Constraints

- The server still binds only `127.0.0.1`. `ServerConfig.host` stays the literal type `'127.0.0.1'`.
- OKX credentials stay in `Runtime` memory only. `server.json` and logs must never contain OKX credentials, AI keys or the session token.
- The session token flow (`/api/session`, same-origin only) is unchanged. The client loads the server's own origin, so no new allowed origin is added.
- Only one server may run per data directory: two `Runtime`s on one SQLite file would both trade.
- `pnpm test` stays offline; new tests use loopback only (see `vitest.setup.ts`).
- The packaged data directory is Tauri's `app_data_dir()` (`%APPDATA%\com.mochiprep.desktop`). The dev default `.mochiprep/` is unchanged.
- Version: root `package.json` `version` and `apps/desktop/src-tauri/tauri.conf.json` `version` must be equal.
- User-facing quit copy must say honestly that agents stop, open OKX positions stay open on the exchange, and OKX credentials must be entered again after restart.

## Review Focus

1. **Two clients launched at once** (double-click twice): only one server starts; the second spawn exits with code 3 and the client attaches to the first. Test: Task 1 `refuses a second live instance`.
2. **Server killed by a crash or Windows logoff** leaves a stale `server.json`: the next launch must replace it, not refuse forever. Test: Task 1 `replaces a record whose process is gone`, and `replaces a record whose port does not answer`.
3. **Port 8787 already taken by another program:** the client picks a free port instead of failing. Test: Task 5 `pick_port_skips_an_occupied_port`.
4. **Client upgraded while the old server is still running:** the user is asked before the old server is restarted; it is never killed silently and never left serving the old UI unnoticed. Test: Task 5 `classify_reports_version_mismatch`.
5. **News link clicked inside the window:** opens in the system browser; the app window never navigates away from the server origin. Test: Task 6 `only_server_origin_stays_in_window`.

---

## File Structure

| Path | Responsibility |
|---|---|
| `apps/server/src/instance.ts` (new) | Single-instance record `server.json`: acquire, detect stale, release. |
| `apps/server/src/version.ts` (new) | `APP_VERSION` read from root `package.json`. |
| `apps/server/src/config.ts` | Add `webDir` (`MOCHIPREP_WEB_DIR`). |
| `apps/server/src/app.ts` | Use `config.webDir`; add `POST /api/system/shutdown`. |
| `apps/server/src/main.ts` | Acquire instance before listen, release on shutdown, exit code 3 when already running. |
| `apps/server/scripts/bundle.mjs` (new) | esbuild bundle + copy migrations, web dist and `node.exe` into `apps/server/dist/server/`. |
| `apps/server/scripts/smoke-bundle.mjs` (new) | Runs the bundle and checks health, session, UI and shutdown. |
| `apps/desktop/src-tauri/` (new) | Tauri app: `main.rs` (setup, tray, window), `server.rs` (discover/spawn/stop), `nav.rs` (link policy). |
| `apps/desktop/package.json` (new) | `tauri` CLI scripts. |
| `docs/DESKTOP.md` (new), `README.md`, `AGENTS.md` | Build and usage docs, layout entry. |

---

### Task 1: Single-instance record

**Files:**
- Create: `apps/server/src/instance.ts`
- Create: `apps/server/src/version.ts`
- Test: `apps/server/test/instance.test.ts`

**Interfaces:**
- Produces:
  ```ts
  export const INSTANCE_FILE = 'server.json';
  export interface InstanceRecord { pid: number; port: number; version: string; controlToken: string; startedAt: string }
  export interface InstanceProbes { alive(pid: number): boolean; healthy(port: number): Promise<boolean> }
  export const defaultProbes: InstanceProbes; // alive: process.kill(pid, 0) in try/catch; healthy: fetch http://127.0.0.1:<port>/api/health, 1 s AbortSignal.timeout
  export type Acquired =
    | { ok: true; record: InstanceRecord; release(): void }
    | { ok: false; running: InstanceRecord };
  export async function acquireInstance(dataDir: string, port: number, probes?: InstanceProbes): Promise<Acquired>;
  // version.ts
  export const APP_VERSION: string; // root package.json "version", via import ... with { type: 'json' }
  ```

- [ ] **Step 1: Write the failing tests** in `apps/server/test/instance.test.ts`, each with a fresh `mkdtempSync` data dir and fake probes:
  - `writes a record with this pid, port, version and a 64-hex control token` → `ok === true`, file `server.json` parses to `{ pid: process.pid, port: 9100, version: APP_VERSION }`, `controlToken` matches `/^[0-9a-f]{64}$/`.
  - `refuses a second live instance` → existing record `{pid: 4242, port: 9100}`, probes `alive: () => true, healthy: async () => true` → `{ ok: false, running.pid === 4242 }`, file unchanged.
  - `replaces a record whose process is gone` → `alive: () => false` → `ok === true`, file now has `process.pid`.
  - `replaces a record whose port does not answer` → `alive: () => true, healthy: async () => false` → `ok === true`.
  - `replaces an unreadable record` → file contents `"{"` → `ok === true`.
  - `release removes only its own record` → acquire, overwrite file with another pid, `release()` → file still exists; acquire again, `release()` → file gone.

- [ ] **Step 2: Run** `pnpm vitest run apps/server/test/instance.test.ts` — Expected: FAIL, module `../src/instance` not found.

- [ ] **Step 3: Implement** `version.ts` and `instance.ts`. Write the record with `writeFileSync(path, json, { flag: 'wx', mode: 0o600 })` so two racing starts cannot both win: on `EEXIST` read the existing record, probe it, and if stale `rmSync` it and retry once; a second `EEXIST` means another start won the race → return `{ ok: false, running }` with whatever record is now on disk. `controlToken` is `randomBytes(32).toString('hex')`. Add a why-comment that the control token only authorizes shutdown (Task 2) and is not the session token.

- [ ] **Step 4: Run** the same command — Expected: 6 passed.

- [ ] **Step 5: Commit**
  ```bash
  git add apps/server/src/instance.ts apps/server/src/version.ts apps/server/test/instance.test.ts
  git commit -m "Allow only one server per data directory"
  ```

### Task 2: Shutdown route, configurable web dir, main wiring

**Files:**
- Modify: `apps/server/src/config.ts:7-28`
- Modify: `apps/server/src/app.ts:50` (signature), `:62-76` (auth hook), `:405-406` (static root)
- Modify: `apps/server/src/main.ts`
- Test: `apps/server/test/system-shutdown.test.ts`

**Interfaces:**
- Consumes: `acquireInstance`, `InstanceRecord`, `APP_VERSION` (Task 1).
- Produces:
  ```ts
  // config.ts
  interface ServerConfig { /* existing */ webDir: string } // MOCHIPREP_WEB_DIR, default join(REPO_ROOT, 'apps/web/dist')
  // app.ts
  export interface AppHooks { controlToken?: string; onShutdown?: () => void }
  export function buildApp(runtime: Runtime, config: ServerConfig, hooks?: AppHooks): { app: FastifyInstance; token: string };
  // POST /api/system/shutdown → 202 { stopping: true }, then onShutdown() on setImmediate.
  //   Authorized by the normal Bearer token OR header `x-mochiprep-control: <controlToken>`.
  //   The control header authorizes this one path only.
  // GET /api/health → { ok: true, version: APP_VERSION }
  ```
- `main.ts` exit codes: `0` clean stop, `3` another server already running for this data dir (prints its port), `1` anything else.

- [ ] **Step 1: Write the failing tests** in `apps/server/test/system-shutdown.test.ts`, using `openDatabase(':memory:')`, `new Runtime({ db })`, `buildApp(runtime, loadConfig({}), { controlToken: 'c'.repeat(64), onShutdown })` and `app.inject` with `host: 'localhost:8787'`:
  - `shutdown with the control header returns 202 and calls onShutdown once` (await a tick before asserting the spy).
  - `shutdown with the session bearer token is accepted`.
  - `shutdown without credentials is 401 and does not call onShutdown`.
  - `control header does not authorize other routes` → `GET /api/state` with only `x-mochiprep-control` → 401.
  - `shutdown is 404 when no onShutdown hook is given` (plain `buildApp(runtime, config)`, request sent with the session bearer token so auth passes).
  - `health reports the app version` → body `{ ok: true, version: APP_VERSION }`.
  - `serves index.html from MOCHIPREP_WEB_DIR` → temp dir with `index.html` containing `mochi-test`, `loadConfig({ MOCHIPREP_WEB_DIR: dir })`, `GET /` body contains `mochi-test`.

- [ ] **Step 2: Run** `pnpm vitest run apps/server/test/system-shutdown.test.ts` — Expected: FAIL.

- [ ] **Step 3: Implement** in `config.ts` and `app.ts`. In the `onRequest` hook, check the control header before the bearer check only when `path === '/api/system/shutdown'` and `hooks.controlToken` is set, using the existing `sameToken`. Register the route only when `hooks.onShutdown` exists.

- [ ] **Step 4: Rewire `main.ts`:** `loadConfig` → `acquireInstance(config.dataDir, config.port)`; if `!ok`, `console.error` with the running port and `process.exit(3)`; otherwise open DB, build app with `{ controlToken: record.controlToken, onShutdown: shutdown }`, listen. If `listen` throws, `release()` and exit 1. `shutdown` runs once (guard flag), stops runtime, closes app and DB, calls `release()`, exits 0. Keep the SIGINT/SIGTERM handlers. Make sure `mkdirSync(config.dataDir, { recursive: true })` happens before `acquireInstance`.

- [ ] **Step 5: Run** `pnpm test` and `pnpm typecheck` — Expected: all pass (existing `buildApp` callers still compile because `hooks` is optional).

- [ ] **Step 6: Manual check:** `pnpm start` in two terminals → second prints the first one's port and exits with code 3; `curl -X POST -H "x-mochiprep-control: <token from .mochiprep/server.json>" -H "host: 127.0.0.1:8787" http://127.0.0.1:8787/api/system/shutdown` → first exits and `.mochiprep/server.json` is gone.

- [ ] **Step 7: Commit**
  ```bash
  git add apps/server/src apps/server/test/system-shutdown.test.ts
  git commit -m "Add graceful shutdown route and single-instance startup"
  ```

### Task 3: Bundle the server

**Files:**
- Create: `apps/server/scripts/bundle.mjs`, `apps/server/scripts/smoke-bundle.mjs`
- Modify: `apps/server/package.json` (add `esbuild` devDependency, scripts `bundle`, `smoke:bundle`), root `package.json` (script `bundle:server` = `pnpm build && pnpm --filter @mochiprep/server bundle`)

**Interfaces:**
- Produces the directory Task 5 ships as a Tauri resource:
  ```
  apps/server/dist/server/
    server.mjs        # esbuild: entry src/main.ts, platform node, format esm, target node22, bundle: true
    migrations/*.sql  # copied from src/db/migrations (database.ts resolves them next to import.meta.url)
    web/              # copied from apps/web/dist
    node.exe          # copy of process.execPath of the building Node (must be >= 22.13)
  ```
  The server is started as `node.exe server.mjs` with env `MOCHIPREP_PORT`, `MOCHIPREP_DATA_DIR`, `MOCHIPREP_WEB_DIR=<dir>/web`.

- [ ] **Step 1: Write `smoke-bundle.mjs` first** (it is the test): start `dist/server/node.exe dist/server/server.mjs` with a temp data dir and a free port; poll `/api/health` up to 15 s; assert `version` equals root `package.json` version, `GET /api/session` returns a 64-hex token, `GET /` returns HTML containing `<div id="root">`, the DB file and `server.json` exist in the data dir; while it is still running, start a second copy against the same data dir and another free port and assert it exits with code 3; then `POST /api/system/shutdown` with the control token from `server.json` and assert the first exits with code 0 and `server.json` is removed. Print `SMOKE OK` and exit 0, else exit 1. Always send `host: 127.0.0.1:<port>`.

- [ ] **Step 2: Run** `pnpm --filter @mochiprep/server smoke:bundle` — Expected: FAIL, `dist/server/server.mjs` missing.

- [ ] **Step 3: Implement `bundle.mjs`.** Add an esbuild banner `import { createRequire } from 'node:module'; const require = createRequire(import.meta.url);` because bundled CommonJS dependencies (Fastify's) call `require`. Mark `node:*` external. Fail with a clear message if `apps/web/dist/index.html` is missing (run `pnpm build` first) or if `process.versions.node` is below 22.13. Clean `dist/server` before writing.

- [ ] **Step 4: Run** `pnpm bundle:server && pnpm --filter @mochiprep/server smoke:bundle` — Expected: `SMOKE OK`.

- [ ] **Step 5: Commit**
  ```bash
  git add apps/server/scripts apps/server/package.json package.json pnpm-lock.yaml
  git commit -m "Bundle the server into a self-contained folder"
  ```

### Task 4: Tauri shell with tray and hide-on-close

**Files:**
- Create: `apps/desktop/package.json` (name `@mochiprep/desktop`, devDependency `@tauri-apps/cli` 2.x, scripts `dev: tauri dev`, `build: tauri build`)
- Create: `apps/desktop/src-tauri/Cargo.toml`, `build.rs`, `tauri.conf.json`, `capabilities/default.json`, `icons/` (generate with `pnpm tauri icon` from `apps/web/public` logo), `src/main.rs`
**Interfaces:**
- `tauri.conf.json`: `identifier: "com.mochiprep.desktop"`, `productName: "MochiPrep"`, `version` = root version, `app.windows: []` (the window is created in code once the server answers), `bundle.targets: ["nsis"]`, `bundle.resources: { "../../server/dist/server/": "server/" }`, `build.beforeBuildCommand: "pnpm -w bundle:server"`, no `frontendDist` dev server.
- `main.rs` produces: `fn open_main_window(app: &AppHandle, port: u16) -> tauri::Result<()>` (creates or shows+focuses window label `main`, `WebviewUrl::External("http://127.0.0.1:<port>/")`, title `MochiPrep`, 1440×900).
- Tray menu items, ids and labels: `open` "開啟 MochiPrep", `quit` "停止伺服器並結束". Left-click on the tray icon = `open`.
- Window `CloseRequested` → `api.prevent_close()` and `window.hide()`.
- `tauri-plugin-single-instance` callback → `open_main_window` with the known port.

- [ ] **Step 1: Scaffold** the Cargo project with `tauri = { version = "2", features = ["tray-icon"] }`, the three plugins, `serde`, `serde_json`, `ureq`. In this task `setup` opens the window on a hard-coded port read from env `MOCHIPREP_DEV_PORT` (default 8787) so the shell can be checked against `pnpm start`; Task 5 replaces this.

- [ ] **Step 2: Verify:** `rustup` MSVC toolchain present; with `pnpm start` running, `pnpm --filter @mochiprep/desktop dev` → window shows the dashboard and live data updates (SSE works through WebView2), closing the window leaves the tray icon, tray left-click brings it back, launching a second copy focuses the first. `quit` may just `app.exit(0)` for now.

- [ ] **Step 3: Commit**
  ```bash
  git add apps/desktop pnpm-lock.yaml
  git commit -m "Add Tauri desktop shell with tray"
  ```

### Task 5: Server supervisor (discover, spawn, version check)

**Files:**
- Create: `apps/desktop/src-tauri/src/server.rs`
- Modify: `apps/desktop/src-tauri/src/main.rs` (setup uses the supervisor)

**Interfaces:**
- Consumes: `server.json` format and exit code 3 (Tasks 1–2), resource layout (Task 3), `open_main_window` (Task 4).
- Produces:
  ```rust
  #[derive(Deserialize, Clone)] #[serde(rename_all = "camelCase")]
  pub struct InstanceRecord { pub pid: u32, pub port: u16, pub version: String, pub control_token: String }
  pub struct Paths { pub data_dir: PathBuf, pub server_dir: PathBuf, pub log_file: PathBuf }
  pub enum Found { None, Running(InstanceRecord), VersionMismatch(InstanceRecord) }
  pub fn read_record(data_dir: &Path) -> Option<InstanceRecord>;               // None on missing or unparsable
  pub fn classify(record: Option<InstanceRecord>, healthy: bool, app_version: &str) -> Found;
  pub fn healthy(port: u16) -> bool;                                            // GET /api/health, 1 s timeout
  pub fn pick_port(preferred: u16) -> u16;                                      // preferred if bindable on 127.0.0.1, else an OS-assigned port
  pub fn spawn(paths: &Paths, port: u16) -> std::io::Result<std::process::Child>;
  pub fn ensure_server(paths: &Paths, app_version: &str) -> Result<Found, String>; // returns Running after spawning and waiting for health
  pub fn stop(record: &InstanceRecord) -> Result<(), String>;                   // POST /api/system/shutdown with x-mochiprep-control, wait ≤ 15 s for health to fail
  ```
- `log_file` = `<data_dir>/logs/server.log` (append). `spawn` uses `creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)` (`0x8 | 0x200 | 0x08000000`) so the server outlives the client and shows no console.

- [ ] **Step 1: Write the failing Rust unit tests** in `server.rs` `#[cfg(test)]`:
  - `read_record_parses_server_json` (temp dir, camelCase JSON) and `read_record_none_when_garbage`.
  - `classify_none_when_no_record`, `classify_none_when_record_but_unhealthy`, `classify_running_when_same_version`.
  - `classify_reports_version_mismatch` → record version `0.0.9`, app `0.1.0`, healthy → `VersionMismatch`.
  - `pick_port_returns_preferred_when_free` (pick a free port first, then ask for it).
  - `pick_port_skips_an_occupied_port` → hold a `TcpListener` on 127.0.0.1:P, `pick_port(P) != P` and is bindable.

- [ ] **Step 2: Run** `cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml` — Expected: FAIL (functions missing).

- [ ] **Step 3: Implement.** `ensure_server`: read + `healthy` + `classify`; on `None`, `spawn(pick_port(8787))`, poll `healthy` every 250 ms up to 20 s; if the child exits with code 3, re-read the record and classify again (another client won the race); on timeout return `Err` naming the log file. `main.rs` setup runs `ensure_server` on a background thread, then on the main thread: `Running` → `open_main_window`; `VersionMismatch` → dialog "背景伺服器是舊版本 (vX)。要重新啟動伺服器嗎？代理會停止，OKX 倉位仍留在交易所，重啟後需重新輸入 OKX 金鑰。" with 重新啟動 / 先用舊版; restart = `stop` then `ensure_server` again; keep using old = open window on its port; `Err` → error dialog with the message, then exit.

- [ ] **Step 4: Run** `cargo test ...` — Expected: all pass.

- [ ] **Step 5: Manual check** with `pnpm --filter @mochiprep/desktop dev` and no server running: a `node.exe` process appears without a console, the window opens, `%APPDATA%\com.mochiprep.desktop\server.json` exists. Kill the Tauri process in Task Manager → `node.exe` keeps running and agents keep cycling; relaunch → attaches without spawning a second `node.exe`.

- [ ] **Step 6: Commit**
  ```bash
  git add apps/desktop/src-tauri/src
  git commit -m "Find or start the background server from the desktop client"
  ```

### Task 6: Quit flow and link policy

**Files:**
- Create: `apps/desktop/src-tauri/src/nav.rs`
- Modify: `apps/desktop/src-tauri/src/main.rs`

**Interfaces:**
- Consumes: `stop`, `read_record` (Task 5).
- Produces: `pub fn is_server_origin(url: &url::Url, port: u16) -> bool` — true only for scheme `http`, host `127.0.0.1`, that port. Used in `WebviewWindowBuilder::on_navigation`: false → open with `tauri-plugin-opener` and return false. Also handle `on_new_window` / `target="_blank"` the same way (NewsPanel links use `target="_blank"`).
- Tray `quit`: confirm dialog "停止伺服器並結束？所有代理會停止。OKX 上的倉位與掛單仍會留在交易所。重新啟動後需要再次輸入 OKX 金鑰。" with 停止並結束 / 取消; on confirm `stop(&record)` then `app.exit(0)`; if `stop` fails, show the error and do not exit.

- [ ] **Step 1: Write the failing tests** in `nav.rs`: `only_server_origin_stays_in_window` → true for `http://127.0.0.1:8787/markets`; false for `http://127.0.0.1:9999/`, `https://127.0.0.1:8787/`, `http://localhost:8787/`, `https://www.okx.com/`.
- [ ] **Step 2: Run** `cargo test ...` — Expected: FAIL.
- [ ] **Step 3: Implement** `nav.rs` and wire navigation and the quit flow in `main.rs`.
- [ ] **Step 4: Run** `cargo test ...` — Expected: pass. Manual: click a news headline → opens in the default browser, app window stays put; tray quit → confirm → `node.exe` gone and `server.json` removed; cancel → nothing changes.
- [ ] **Step 5: Commit**
  ```bash
  git add apps/desktop/src-tauri/src
  git commit -m "Confirm before stopping the server and open external links outside the app"
  ```

### Task 7: Version sync test and docs

**Files:**
- Test: `apps/server/test/desktop-version.test.ts`
- Create: `docs/DESKTOP.md`
- Modify: `README.md` (short "Desktop app" section linking `docs/DESKTOP.md`), `AGENTS.md` (layout line `apps/desktop`: Tauri client and background-server supervisor; commands `pnpm bundle:server`, `pnpm --filter @mochiprep/desktop build`)

- [ ] **Step 1: Write the test** `desktop version matches the app version` → parse `apps/desktop/src-tauri/tauri.conf.json` and root `package.json`, expect equal `version`.
- [ ] **Step 2: Run** `pnpm vitest run apps/server/test/desktop-version.test.ts` — Expected: PASS (fails later if someone bumps only one).
- [ ] **Step 3: Write `docs/DESKTOP.md`:** prerequisites (Rust MSVC toolchain, WebView2 is built into Windows 11), build commands, where data and `logs/server.log` live, that the server keeps running after the window closes and how to stop it (tray → 停止伺服器並結束), that OKX credentials are not persisted so they must be re-entered after a server restart, and that `server.json` holds only pid, port, version and a shutdown-only token.
- [ ] **Step 4: Final verification:** `pnpm test`, `pnpm typecheck`, `cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml`, `pnpm --filter @mochiprep/desktop build` → NSIS installer in `apps/desktop/src-tauri/target/release/bundle/nsis/`; install it, run through the Task 5 and Task 6 manual checks on the installed copy.
- [ ] **Step 5: Commit**
  ```bash
  git add apps/server/test/desktop-version.test.ts docs/DESKTOP.md README.md AGENTS.md
  git commit -m "Document the desktop app"
  ```
