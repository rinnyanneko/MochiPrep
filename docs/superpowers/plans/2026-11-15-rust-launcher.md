# Rust Launcher Implementation Plan (Plan 6 of 7)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Tauri 2 tray launcher that shows the risk terms, starts and supervises the Rust backend as a child process, opens the web interface in its own window, and confirms before stopping agents or disarming real accounts.

**Architecture:** Two new crates. `mochi-launcher-core` holds everything testable without a window: settings and consent file, data import, backend supervisor (spawn, readiness, stop), the HTTP client for the backend's existing API, and the menu/confirmation model. `mochi-launcher` is the thin Tauri binary: tray, windows, plugins. The backend is the unchanged `mochi-server` binary, started with `MOCHIPREP_PORT`, `MOCHIPREP_DATA_DIR` and a new `MOCHIPREP_PARENT_STDIN=1`; it stops when its stdin closes, so it also stops if the launcher dies. The launcher gets the session token from `/api/session` like any same-origin client, and reads state from `/api/agents` and `/api/exchange/status`.

**Tech Stack:** Rust 1.99, Tauri 2 with `tauri-plugin-single-instance`, `tauri-plugin-autostart`, `tauri-plugin-dialog`, `tauri-plugin-opener`; tokio (`process`, `io-util`), reqwest (workspace), serde, rusqlite (same version as `mochi-store`, plus the `backup` feature); `zbus` on Linux only. Launcher pages are plain HTML/JS in `crates/mochi-launcher/ui/`, no bundler.

**Spec:** `docs/superpowers/specs/2026-11-07-rust-backend-desktop-design.md` (sections "Launcher", "運作模型", "我替擁有者做的決定", appendix). Roadmap: `docs/superpowers/plans/2026-11-07-rust-foundation.md`.

## Global Constraints

- Node stays the default backend for `pnpm dev`/`start`/`test:e2e`. React code and the HTTP API do not change; the only backend changes are Task 1's.
- Launcher and backend start and stop together. Closing the interface window never stops the backend. Restarting the backend disarms real accounts and pauses agents (existing behaviour); the launcher never arms anything and never unlocks the vault (the web unlock bar stays the only place).
- The backend is not started until the terms are accepted. Terms text is the spec appendix, verbatim, Version 1; `{Exchange status line}` is exactly: `The OKX integration in this version has been tested only against a simulated exchange, not against OKX Demo Trading or a real OKX account.` Terms count as accepted only when the stored version equals `TERMS_VERSION = 1`.
- Consent and settings live in the launcher's own file `launcher.json` in the OS config dir (Tauri `app_config_dir`), never in the backend's SQLite.
- Port defaults to 8787, configurable in Settings (1024–65535). Never pick another port automatically; a busy port shows an error naming it.
- Data dir: `MOCHIPREP_DATA_DIR` from the launcher's own environment if set (dev), else Tauri `app_data_dir`. Bundle identifier `io.github.rinnyanneko.mochiprep`, product name `MochiPrep`.
- Autostart is offered and off by default. Only one launcher runs at a time.
- Quit, Restart and a port change that restarts: if any agent is running, any `OKX_LIVE` account is armed, or the backend state cannot be read while its process is alive, first show: title `Stop MochiPrep's backend?`, text `Running agents will stop and real accounts will be disarmed. Open positions and their stop orders stay on OKX.`, buttons `Continue` / `Cancel`.
- The interface window loads `http://127.0.0.1:<port>` only, gets no Tauri capability, and opens any other http(s) link in the system browser. Windows are destroyed on close, not hidden, so no WebView runs while the interface is closed.
- No secret (session token, keys) in launcher logs, errors or `Debug`. Comments explain why. Readable Rust: mainstream crates, no macros or trait tricks.
- No updater in this plan (Plan 7). Do not add placeholder menu items or code for it.
- AGENTS.md phase workflow: CodeRabbit, verification, commit, push, ready PR; do not merge.

## Review Focus

1. **Port already in use** (another app or a dev backend on 8787): the backend exits; the tray shows `Error: port 8787 is in use`, offers Restart and Settings, and does not retry in a loop. Test in Task 1 (exit code) and Task 3 (contract test).
2. **Launcher killed or crashed**: the backend sees stdin close and exits cleanly within 10 s, so the port and database are freed. Test in Task 1.
3. **State unreadable at Quit** (backend hung, token lost): Quit still asks for confirmation rather than assuming nothing is armed. Test in Task 2.
4. **Corrupt, missing or older `launcher.json`**: loads as defaults with terms not accepted, never crashes, never treats a missing version as accepted. Test in Task 2.
5. **Import into an existing data dir, from an empty folder, or from the target itself**: refused with nothing written; a source the Node backend still has open is copied as a consistent snapshot. Test in Task 2.

---

### Task 1: Backend child-process mode

**Files:**
- Modify: `crates/mochi-server/src/main.rs`, `crates/mochi-server/src/config.rs`, `crates/mochi-server/src/lib.rs`
- Test: `crates/mochi-server/tests/child_process.rs`

**Interfaces:**
- Produces: `pub const READY_PREFIX: &str = "MochiPrep backend on http://127.0.0.1:";` and `pub const EXIT_PORT_IN_USE: u8 = 3;` exported from `mochi_server`. The ready line is `READY_PREFIX` + the port actually bound + ` (data: <dir>)`. `Config.parent_stdin: bool` from `MOCHIPREP_PARENT_STDIN=1`.

- [ ] **Step 1:** Write `tests/child_process.rs` using `env!("CARGO_BIN_EXE_mochi-server")`, `MOCHIPREP_OFFLINE=1`, a tempdir data dir and a free port found by binding `127.0.0.1:0`:
  - `prints_the_ready_line_with_the_bound_port`: first stdout line starts with `READY_PREFIX` followed by the port.
  - `exits_cleanly_when_stdin_closes_in_parent_mode`: with `MOCHIPREP_PARENT_STDIN=1`, drop the child's stdin after the ready line; it exits with success within 10 s and the port can be bound again.
  - `ignores_stdin_without_parent_mode`: without the variable, closing stdin does not stop it (still answers `/api/health` with `Host: 127.0.0.1:<port>` after 1 s); kill it after.
  - `port_in_use_exits_with_code_3`: hold the port with a `TcpListener`; exit code is `EXIT_PORT_IN_USE`.
- [ ] **Step 2:** Run `cargo test -p mochi-server --test child_process`; expect failures.
- [ ] **Step 3:** Implement: `shutdown_signal` also completes when parent mode is on and stdin reaches EOF (read stdin to the end on a tokio task); print the bound port from `listener.local_addr()`; return `ExitCode::from(EXIT_PORT_IN_USE)` when bind fails with `AddrInUse`, other bind errors stay `FAILURE`. Flush stdout after the ready line.
- [ ] **Step 4:** Run the test file, then `cargo test -p mochi-server`; expect pass. Confirm `pnpm test:e2e:rust` assumptions are unaffected (Playwright waits on the port, not stdout).
- [ ] **Step 5:** Commit.

### Task 2: `mochi-launcher-core` — settings, terms consent, import, menu model

**Files:**
- Create: `crates/mochi-launcher-core/{Cargo.toml,src/lib.rs,src/settings.rs,src/import.rs,src/model.rs}`
- Test: unit tests in each module (tempdirs)

**Interfaces:**
- `settings.rs`: `pub const TERMS_VERSION: u32 = 1; pub const DEFAULT_PORT: u16 = 8787;`
  `#[serde(rename_all = "camelCase", default)] pub struct LauncherSettings { pub accepted_terms_version: Option<u32>, pub port: u16, pub autostart: bool, pub import_offered: bool }` (Default: None, 8787, false, false).
  `pub fn load_settings(path: &Path) -> LauncherSettings` (missing or unparseable → default; an out-of-range port → 8787), `pub fn save_settings(path: &Path, s: &LauncherSettings) -> std::io::Result<()>` (write temp file in the same dir, then rename; create the dir), `pub fn terms_accepted(s: &LauncherSettings) -> bool`, `pub fn validate_port(text: &str) -> Result<u16, String>` (error `Port must be a number from 1024 to 65535`).
- `import.rs`: `pub enum ImportError { NoDatabase, TargetExists, SameFolder, NotMochiPrep, Io(String) }` with `Display` texts `That folder has no mochiprep.db.`, `MochiPrep already has data here, so nothing was imported.`, `That is MochiPrep's own data folder.`, `mochiprep.db is not a MochiPrep database.`, `Import failed: <io>`; `pub fn import_data(source_dir: &Path, target_dir: &Path) -> Result<(), ImportError>`.
  Approach: compare canonicalized dirs; open `source/mochiprep.db` read-only; require a `schema_migrations` table; copy with the SQLite online backup API into `target/mochiprep.db.importing`, then rename to `mochiprep.db`; create the target dir with 0700 on Unix and the file 0600; on any error remove the partial file.
- `model.rs`: `pub enum BackendState { Starting, Running, Stopped, Error(String) }`, `pub struct BackendSnapshot { pub agents_running: usize, pub armed_live_accounts: Vec<String> }`,
  `pub fn needs_confirmation(state: &BackendState, process_alive: bool, snapshot: Option<&BackendSnapshot>) -> bool`,
  `pub struct MenuModel { pub status_label: String, pub armed_label: Option<String>, pub can_open_interface: bool, pub can_disarm_all: bool }`, `pub fn menu_model(state: &BackendState, snapshot: Option<&BackendSnapshot>) -> MenuModel`,
  and the confirmation strings from Global Constraints as `pub const CONFIRM_TITLE` / `CONFIRM_TEXT`.

- [ ] **Step 1:** Write tests:
  - settings: `missing_file_is_default_not_accepted`, `corrupt_file_is_default_not_accepted` (write `{not json`), `older_terms_version_is_not_accepted` (`acceptedTermsVersion: 0`), `newer_terms_version_is_not_accepted` (2), `current_version_is_accepted`, `unknown_keys_are_ignored`, `save_then_load_roundtrips`, `validate_port` cases `"8787"`→8787, `"80"`, `"70000"`, `"x"` → the error text.
  - import: `copies_a_mochiprep_database` (create a source db with a `schema_migrations` row and a WAL-mode write left uncheckpointed with a second connection open; after import the target has the row), `refuses_when_target_has_data` (target file unchanged), `refuses_a_folder_without_db`, `refuses_the_target_itself`, `refuses_a_non_mochiprep_sqlite_file`, `leaves_no_partial_file_on_failure`.
  - model: `quit_needs_confirmation_when_an_agent_runs`, `…_when_a_live_account_is_armed`, `no_confirmation_when_idle`, `confirms_when_state_is_unknown_but_process_alive` (snapshot None, alive true → true), `no_confirmation_when_backend_stopped` (Stopped/Error and not alive → false); menu: Running + armed 2 → `status_label == "Backend: Running"`, `armed_label == Some("Real account armed (2)")`, `can_disarm_all`; Error("port 8787 is in use") → `"Backend: Error: port 8787 is in use"`, `can_open_interface == false`; Starting → `"Backend: Starting"`; Stopped → `"Backend: Stopped"`.
- [ ] **Step 2:** Run `cargo test -p mochi-launcher-core`; expect failures (missing crate).
- [ ] **Step 3:** Implement the three modules.
- [ ] **Step 4:** Run `cargo test -p mochi-launcher-core`, `cargo clippy -p mochi-launcher-core --all-targets -- -D warnings`; expect pass.
- [ ] **Step 5:** Commit.

### Task 3: `mochi-launcher-core` — supervisor and backend client

**Files:**
- Create: `crates/mochi-launcher-core/src/supervisor.rs`, `src/client.rs`
- Test: `crates/mochi-server/tests/launcher_contract.rs` (mochi-server gains `mochi-launcher-core` as a dev-dependency, so the test drives the real binary); unit tests for parsing in `client.rs`

**Interfaces:**
- Consumes: Task 1's `READY_PREFIX`, `EXIT_PORT_IN_USE` (duplicate them as constants in `supervisor.rs`; the contract test asserts they equal `mochi_server`'s), Task 2's `BackendSnapshot`.
- `supervisor.rs`: `pub struct BackendCommand { pub program: PathBuf, pub port: u16, pub data_dir: PathBuf, pub offline: bool }`; `pub enum StartError { PortInUse(u16), Exited { code: Option<i32>, stderr_tail: String }, Timeout, Spawn(String) }` with `Display` (`port <p> is in use`, `backend exited (code <c>): <tail>`, `backend did not start within 20 s`, `cannot start backend: <e>`); `pub struct Backend`; `pub async fn start(cmd: &BackendCommand) -> Result<Backend, StartError>` (env `MOCHIPREP_PORT`, `MOCHIPREP_DATA_DIR`, `MOCHIPREP_PARENT_STDIN=1`, `MOCHIPREP_OFFLINE=1` when `offline`; stdin/stdout/stderr piped; `CREATE_NO_WINDOW` on Windows; `kill_on_drop(true)`; waits up to 20 s for the ready line, then for `/api/health` to answer); `impl Backend { pub fn port(&self) -> u16; pub fn is_alive(&mut self) -> bool; pub async fn stop(self) }` (close stdin, wait 10 s, then kill and wait). Keep the last 2 KiB of stderr for errors; drain stdout/stderr continuously so the pipes never fill.
- `client.rs`: `pub struct BackendClient`; `pub fn new(port: u16) -> Self`; `pub async fn snapshot(&self) -> Result<BackendSnapshot, ClientError>` (GET `/api/agents` counting `running == true`, GET `/api/exchange/status` collecting `accountId` where `venue == "OKX_LIVE" && armed`); `pub async fn disarm_all_live(&self) -> Result<usize, ClientError>` (POST `/api/exchange/okx/{id}/disarm` with body `{}` for each); token from GET `/api/session`, cached, refetched once on 401. Every request sets `Host: 127.0.0.1:<port>`, no `Origin`, 5 s timeout, no proxy. `ClientError` `Display` never contains the token. `impl Debug` for `BackendClient` redacts the token.

- [ ] **Step 1:** Write `launcher_contract.rs` (offline, tempdir, free port): `start_reports_ready_and_health`, `snapshot_of_a_fresh_backend_is_idle` (0 agents, no armed), `stop_frees_the_port` (bind succeeds after `stop`), `busy_port_is_port_in_use` (`StartError::PortInUse(port)`), `missing_program_is_spawn_error`, `constants_match_the_server`, `token_is_refetched_after_restart` (one client, restart backend on the same port, `snapshot` still works). Unit tests in `client.rs` parse sample `/api/agents` and `/api/exchange/status` JSON (an `OKX_DEMO` armed account is not counted; `OKX_LIVE` disarmed not counted).
- [ ] **Step 2:** Run `cargo test -p mochi-server --test launcher_contract` and `cargo test -p mochi-launcher-core`; expect failures.
- [ ] **Step 3:** Implement `supervisor.rs` and `client.rs`.
- [ ] **Step 4:** Run both test commands and clippy; expect pass.
- [ ] **Step 5:** Commit.

### Task 4: `mochi-launcher` — terms gate, tray, interface window, quit and restart

**Files:**
- Create: `crates/mochi-launcher/{Cargo.toml,build.rs,tauri.conf.json,capabilities/local.json,icons/*,ui/terms.html,ui/launcher.js,ui/launcher.css,src/main.rs,src/app.rs,src/tray.rs,src/windows.rs}`
- Test: unit tests in `src/windows.rs` for the navigation rule; `src/main.rs` test that `ui/terms.html` contains `Version 1` and the exact exchange status line

**Interfaces:**
- Consumes: Tasks 2–3 (`load_settings`, `save_settings`, `terms_accepted`, `supervisor::start`, `Backend`, `BackendClient`, `menu_model`, `needs_confirmation`, `CONFIRM_*`).
- Produces for Task 5: `struct AppState { settings: Mutex<LauncherSettings>, settings_path: PathBuf, data_dir: PathBuf, backend: tokio::sync::Mutex<Option<Backend>>, state: Mutex<BackendState>, snapshot: Mutex<Option<BackendSnapshot>> }` managed by Tauri; `async fn start_backend(app: &AppHandle)`, `async fn stop_backend(app: &AppHandle)`, `async fn confirm_stop(app: &AppHandle) -> bool` (uses `needs_confirmation` and the dialog plugin), `fn refresh_menu(app: &AppHandle)`, `fn open_interface(app: &AppHandle)`, window labels `terms`, `interface`.
- `pub fn allowed_navigation(url: &Url, port: u16) -> Navigation` with `enum Navigation { Allow, OpenExternally, Deny }`: same scheme/host `127.0.0.1`/port → Allow; other `http`/`https` → OpenExternally; anything else → Deny.

- [ ] **Step 1:** Write tests: `allowed_navigation` for `http://127.0.0.1:8787/x` (Allow), `http://localhost:8787/` (OpenExternally: not the origin the token and storage belong to), `http://127.0.0.1:9999/` (OpenExternally), `https://news.example/a` (OpenExternally), `file:///etc/passwd` and `javascript:alert(1)` (Deny); terms html test.
- [ ] **Step 2:** Run `cargo test -p mochi-launcher`; expect failures.
- [ ] **Step 3:** Implement:
  - `tauri.conf.json`: identifier and product name from Global Constraints, `frontendDist: "ui"`, no windows declared (created in code), `withGlobalTauri: true`; `capabilities/local.json` grants the launcher commands and dialog permissions to windows `terms`, `settings`, `import`, `panel` only, no `remote` entry. Icons generated from `apps/web/public/mochi.svg` (`icon.ico`, `icon.png`, 32/128 px PNGs) plus `tray.png` and `tray-armed.png` (same mark with a red dot).
  - `main.rs`: `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`; single-instance plugin registered first (a second launch opens the interface if the backend runs, else focuses the terms window); dialog, opener and autostart plugins.
  - Startup: load settings; if `!terms_accepted`, open the `terms` window (scroll to the bottom enables the two checkboxes; both checked enables `Continue`; `Quit` exits without starting anything) and wait; on accept save `acceptedTermsVersion = 1`, then `start_backend` (Task 5 inserts the import step before it).
  - Tray: menu items `Backend: …` (disabled), the armed label when present (disabled; tray icon switches to `tray-armed.png` and tooltip `MochiPrep — real account armed`), `Open interface`, `Restart backend`, `Disarm all` (enabled only when `can_disarm_all`), `Risk & disclaimer…` (opens `terms` read-only: checkboxes hidden, `Close`), `Quit`. A 3 s poll updates the snapshot and menu while Running and detects a backend that exited (state becomes `Error("backend stopped unexpectedly")`; no automatic restart).
  - `open_interface`: create or focus the `interface` window at `http://127.0.0.1:<port>`, `on_navigation` per `allowed_navigation` (OpenExternally → opener `open_url`, return false), new-window requests handled the same way; closing destroys it.
  - Restart and Quit: `confirm_stop`, then `stop_backend` (then `start_backend` / `app.exit(0)`); the interface window is closed before a restart. `RunEvent::ExitRequested` is prevented unless Quit started it, so closing the last window never quits. Disarm all calls `disarm_all_live` and shows a dialog only on error.
- [ ] **Step 4:** Run `cargo test -p mochi-launcher`, `cargo clippy --workspace --all-targets -- -D warnings`, then build `cargo build -p mochi-server -p mochi-launcher` and run `target/debug/mochi-launcher` with `MOCHIPREP_DATA_DIR` set to a temp dir: terms appear, accepting starts the backend, the tray shows Running, the interface loads the UI, closing it keeps the tray, Quit exits and the port is free. Record what you observed in the report.
- [ ] **Step 5:** Commit.

### Task 5: Settings, data import, no-tray fallback, docs

**Files:**
- Create: `crates/mochi-launcher/ui/{settings.html,import.html,panel.html}`, `crates/mochi-launcher/src/{settings_window.rs,import_window.rs,panel.rs}`, `docs/LAUNCHER.md`
- Modify: `crates/mochi-launcher/src/{app.rs,tray.rs}`, `AGENTS.md` (crate list, run command), `docs/ARCHITECTURE.md` (Rust backend section: one paragraph on the launcher)

**Interfaces:**
- Consumes: Task 4's `AppState`, `start_backend`, `stop_backend`, `confirm_stop`, `refresh_menu`; Task 2's `validate_port`, `import_data`, `ImportError`.
- Tauri commands: `get_settings() -> SettingsView { port, autostart }`, `save_settings(port: String, autostart: bool) -> Result<(), String>`, `choose_import_folder() -> Option<String>`, `run_import(folder: String) -> Result<(), String>`, `skip_import()`, `panel_action(action: String)` with actions `open`, `restart`, `disarm`, `terms`, `settings`, `quit`.
- `fn tray_available() -> bool`: on Linux, true only if the D-Bus session bus name `org.kde.StatusNotifierWatcher` has an owner (zbus, blocking, 1 s timeout; no bus → false); other OSes true.

- [ ] **Step 1:** Write tests: `tray_available` is true on non-Linux (cfg test); command-level tests for `save_settings` logic factored as `fn apply_settings(current: &LauncherSettings, port: &str, autostart: bool) -> Result<(LauncherSettings, bool /*port changed*/), String>` (bad port → the Task 2 error, unchanged settings; same port → false; new port → true).
- [ ] **Step 2:** Run `cargo test -p mochi-launcher`; expect failures.
- [ ] **Step 3:** Implement:
  - Tray item `Settings…` (above Quit) opens `settings`: port field with the note `Changing the port resets the interface's saved preferences, because they belong to the old address.`, checkbox `Start MochiPrep when I log in` (autostart plugin enable/disable; off by default), `Save`. A changed port is saved, then asks `Restart the backend now to use port <p>?`; yes runs the Restart flow (with `confirm_stop`).
  - Import: after terms and before the first backend start, if `!import_offered` and `<data_dir>/mochiprep.db` does not exist, open `import` with text `Import existing data from a .mochiprep folder? Your data is copied; the original stays where it is.`, buttons `Import existing data…` (folder picker, then `run_import`, error shown inline, success continues) and `Skip`. Either outcome sets `import_offered = true`. Closing the window counts as Skip.
  - Fallback: if `!tray_available()`, show `panel` (small window, same items as the tray as buttons, live status and armed text, refreshed by the same poll) instead of a tray icon; its close request runs the Quit flow (prevented if the user cancels).
  - `docs/LAUNCHER.md`: how to run in development (`cargo build -p mochi-server` then `cargo run -p mochi-launcher`; the launcher finds `mochi-server` next to its own executable), Linux build packages (`libwebkit2gtk-4.1-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`), where settings and data live, and a manual checklist per OS covering: terms gate (backend not started before accepting; decline quits), read-only terms, tray states (Starting, Running, Error with busy port 8787), armed highlight with an OKX Demo account is NOT shown (Demo is not real money) and with a real account would be, Open interface, close interface keeps backend (agents keep running), second launch opens the interface instead of a second backend, Restart/Quit confirmation with a running paper agent, Disarm all, Settings port change and autostart, import from a Node `.mochiprep` folder (copy, original untouched), refused import into existing data, Linux GNOME without AppIndicator shows the panel and closing it asks to quit, killing the launcher process stops the backend, interface window cannot call any launcher command (`window.__TAURI_INTERNALS__.invoke('get_settings')` from its devtools is refused), memory with the interface closed noted (working set of launcher + backend).
- [ ] **Step 4:** Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `pnpm test`; run the Windows rows of the checklist that need no OKX account and record results in the report.
- [ ] **Step 5:** Commit.
