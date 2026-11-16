# Release and Switch-over Implementation Plan (Plan 7 of 7)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Measure memory, add the Tauri updater to the launcher, build signed-for-update but OS-unsigned installers for 6 targets in CI, publish them as a draft GitHub Release with memory numbers, then make Rust the only backend and remove the Node backend.

**Architecture:** A dependency-free Node script measures the backend's memory the same way on every OS; CI runs it on each target and the release notes collect the results. The launcher gains `tauri-plugin-updater`; the decision logic (when to check, what the menu says, whether this install can update itself) lives in `mochi-launcher-core` as pure functions. Bundling uses the Tauri CLI with a separate release config that adds `mochi-server` as an `externalBin`, so plain `cargo build`/`cargo test` never needs a staged backend. One reusable workflow builds all 6 targets; CI calls it on pull requests without updater signing, the release workflow calls it on a `v*` tag with signing and uploads to a draft release. Last, the Node backend and the TS packages only it used are deleted, migrations move into `mochi-store`, and every script and doc points at Rust.

**Tech Stack:** Rust 1.99, Tauri 2 (`tauri-plugin-updater` 2), `@tauri-apps/cli` 2 as a root devDependency, GitHub Actions (`tauri-apps/tauri-action@v0`, `Swatinem/rust-cache@v2`, `pnpm/action-setup`, `actions/setup-node`), Node ≥ 22.13 for scripts.

**Spec:** `docs/superpowers/specs/2026-11-07-rust-backend-desktop-design.md` (sections "成功與失敗的定義", "範圍", "更新", "發佈", "遷移方式", "第一個 Release", "切換前的 OKX 實測", "我替擁有者做的決定": memory and port). Roadmap row 7 in `docs/superpowers/plans/2026-11-07-rust-foundation.md`.

## Owner decisions in force

- 2026-11-16: OKX Demo testing is **not** a switch-over condition. The first Release says plainly that the OKX path was tested only against a simulated exchange (the terms text already does). Task 7 records this in the spec.
- Unsigned builds (no OS code signing or notarization). The updater signing key is separate and required.

## Owner input needed before Task 2

The owner generates the updater key pair once, on their own machine (the private key never enters the repo or this session):

    pnpm dlx @tauri-apps/cli@2 signer generate -w ~/.tauri/mochiprep-updater.key

then adds GitHub secrets `TAURI_SIGNING_PRIVATE_KEY` (file contents) and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and gives the controller the contents of the `.pub` file. Task 2 puts that public key in `tauri.conf.json`.

## Global Constraints

- Memory gate (spec): Rust backend working set **≤ 20 MiB** on Windows x86_64, interface closed, idle about 12 s, live OKX market feed connected, measured like `bd11fbe` (baseline 83 MiB). Launcher plus backend with the interface closed must be clearly below 83 MiB; ruling: **≤ 66 MiB** (20 % under) counts as clearly below. Other 5 targets are measured and reported, never gating: Linux `VmRSS`, macOS `footprint`.
- Node removal (Task 6) happens only after Task 1 recorded a passing backend measurement. If it fails, stop before Task 6 and report.
- Updater endpoint, exactly: `https://github.com/rinnyanneko/MochiPrep/releases/latest/download/latest.json`. Release page for package-managed installs: `https://github.com/rinnyanneko/MochiPrep/releases/latest`.
- Update checks: once at launcher start, then every 24 h while it runs; off when the setting `checkUpdates` is false (default true); never in debug builds. Nothing downloads or installs without the user clicking the menu item. Updating applies the Quit/Restart confirmation rule (`needs_confirmation`, `CONFIRM_TITLE`, `CONFIRM_TEXT`).
- Self-updating installs: Windows (NSIS/MSI), macOS `.app`, Linux AppImage (the `APPIMAGE` environment variable is set). Every other Linux install (`.deb`, AUR) only shows that a version is available and opens the release page.
- Menu labels, exactly: `Update to v{version}` (self-updating) and `v{version} available` (package-managed). Shown only when an update is known. No placeholder items otherwise.
- Release artifacts: Windows `.msi` and NSIS `.exe`; macOS `.dmg` and the updater `.app.tar.gz` for x86_64 and aarch64 separately; Linux `.deb` and `.AppImage`. Targets: `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`.
- The release is created as a **draft**; publishing it is the owner's action. The updater private key and password are read only from secrets, never echoed, never written to the repo.
- Tag `vX.Y.Z` must equal `[workspace.package].version` in `Cargo.toml`; the release workflow fails otherwise.
- After Task 6, the HTTP API, SSE events, React code and e2e specs are unchanged; `pnpm test:e2e` runs Playwright against the Rust server.
- Vault format and migrations stay byte-identical; golden fixtures in `crates/golden` become frozen inputs (no generator).
- Rules from AGENTS.md still bind (Dec for money, secrets never logged, unlock never arms, and so on). Comments explain why. No placeholder code.
- AGENTS.md phase workflow: CodeRabbit, verification, commit, push, ready PR; do not merge.

## Review Focus

1. **No network or GitHub down at update check**: the check fails quietly (one `eprintln!`, no dialog, no menu item), the backend is unaffected, and the next check still happens 24 h later. Test in Task 2 (`schedule` and model tests).
2. **Download fails or the update is interrupted** after the user confirmed: the backend is still running (download happens before it stops); if installing fails after the stop, the backend is started again and an error dialog names the failure. Test in Task 2 (`update_steps` order) and a checklist row.
3. **`.deb` install sees an update**: never calls `download_and_install`; the item opens the release page. Test in Task 2 (`install_kind`).
4. **Live feed unreachable during measurement** (CI runner region, outage): the script still reports the number but marks `liveFeed` as not connected, and the gate treats such a Windows run as not passing. Test in Task 1.
5. **Stale references after removal**: a doc, script, config or test that still names `apps/server`, `packages/core`, `@mochiprep/server` and so on fails or misleads. Task 6 ends with a repo-wide search that must come back empty outside history notes.

---

### Task 1: Memory measurement script and the Windows gate

**Files:**
- Create: `scripts/measure-memory.mjs`, `scripts/measure-memory.test.mjs` (run with `node --test`)
- Modify: `package.json` (script `measure:memory`), `vitest.config.ts` only if needed to keep `node --test` files out of vitest

**Interfaces:**
- Produces: `node scripts/measure-memory.mjs --binary <path> [--port 8987] [--idle 12] [--out report.json] [--pid <launcher pid>]`. Report JSON: `{ target: "<os>-<arch>", metric: "workingSet"|"rss"|"footprint", bytes, mib, liveFeed: "connected"|"<other state>"|"unknown", idleSeconds, measuredAt, commit, launcherTree?: { bytes, mib, processes } }`. Exit code 0 always when a number was read; 1 when the backend did not start or no number could be read. A `--gate` flag adds exit code 2 when `metric` is `workingSet` and `mib > 20` or `liveFeed !== "connected"`.
- Exports for tests: `parseFootprint(text) -> bytes`, `parseProcStatus(text) -> bytes`, `parseWorkingSet(text) -> bytes`, `gateFailures(report) -> string[]`.

- [ ] **Step 1:** Write `scripts/measure-memory.test.mjs`: parsers read a captured `footprint <pid>` sample (`phys_footprint` line, units KB/MB), a `/proc/<pid>/status` sample (`VmRSS:  12345 kB`) and a PowerShell integer; `gateFailures` returns `[]` for `{metric:"workingSet", mib:18.4, liveFeed:"connected"}`, names the limit for `mib:21`, and names the feed for `liveFeed:"degraded"`.
- [ ] **Step 2:** Run `node --test scripts/measure-memory.test.mjs`; expect failures.
- [ ] **Step 3:** Implement. Start the binary with `MOCHIPREP_PORT`, a fresh temp `MOCHIPREP_DATA_DIR`, no `MOCHIPREP_OFFLINE`, and the inherited `MOCHIPREP_*` removed; wait for the `READY_PREFIX` line (`MochiPrep backend on http://127.0.0.1:`); fetch the token from `/api/session` (no Origin header); read `/api/events?token=…` (check the route's actual auth in `crates/mochi-server/src/routes/events.rs`) until a feed status for the OKX source arrives or 30 s pass; close the stream; wait `--idle` seconds; sample: Windows `powershell -NoProfile -Command "(Get-Process -Id N).WorkingSet64"`, Linux `/proc/N/status` `VmRSS`, macOS `footprint N`. `--pid` additionally sums the given process and its descendants (Windows `Get-CimInstance Win32_Process` by `ParentProcessId`; Linux `/proc/*/stat`; macOS `pgrep -P`), with the backend started by the launcher instead of the script (then `--binary` is not used). Kill the child and delete the temp dir in `finally`. Print a one-line summary and write `--out`.
- [ ] **Step 4:** Run the tests; expect pass. Then `cargo build --release -p mochi-server` and run `node scripts/measure-memory.mjs --binary target/release/mochi-server.exe --port 8987 --gate --out <scratch>/memory-windows.json` on this Windows x86_64 machine (port 8987: 8787 and 8877 may be the owner's). Record the report in the task report. A failing gate stops the plan before Task 6.
- [ ] **Step 5:** Commit.

### Task 2: Launcher updater

**Files:**
- Create: `crates/mochi-launcher-core/src/update.rs`; `crates/mochi-launcher/src/updater.rs`
- Modify: `crates/mochi-launcher-core/src/{lib.rs,settings.rs,model.rs}`, `crates/mochi-launcher/{Cargo.toml,tauri.conf.json,capabilities/local.json,src/app.rs,src/tray.rs,src/panel.rs,src/main.rs,ui/settings.html,ui/launcher.js}`, `docs/LAUNCHER.md`
- Test: unit tests in `update.rs`, `settings.rs`, `model.rs`

**Interfaces:**
- Produces in `mochi_launcher_core::update`:
  - `pub const UPDATE_ENDPOINT: &str` and `pub const RELEASES_PAGE: &str` (values from Global Constraints).
  - `pub enum InstallKind { SelfUpdating, PackageManaged }` and `pub fn install_kind(os: &str, appimage: Option<&std::ffi::OsStr>) -> InstallKind` (`"linux"` with `None` or empty is `PackageManaged`; everything else `SelfUpdating`).
  - `pub struct UpdateOffer { pub version: String, pub kind: InstallKind }`, `pub fn update_label(offer: &UpdateOffer) -> String`.
  - `pub const CHECK_INTERVAL: Duration = 24 h`; `pub fn check_due(enabled: bool, last_attempt: Option<Instant>, now: Instant) -> bool` (a failed check counts as an attempt).
  - `pub enum UpdateStep { Download, StopBackend, Install, Relaunch }` and `pub fn update_steps() -> [UpdateStep; 4]` in that order; when `Install` fails, the Tauri side starts the backend again.
- `LauncherSettings.check_updates: bool`, serde name `checkUpdates`, default `true`; an old file without the field loads as `true`.
- `menu_model(state, snapshot, update: Option<&UpdateOffer>)`; `MenuModel.update_label: Option<String>`.

- [ ] **Step 1:** Tests: `install_kind("linux", None)` and `Some("")` are `PackageManaged`, `Some("/tmp/MochiPrep.AppImage")` is `SelfUpdating`, `"windows"`/`"macos"` with `None` are `SelfUpdating`; labels `Update to v0.2.0` and `v0.2.0 available`; `check_due` true with no attempt, false 23 h after one, true at 24 h, always false when disabled; `update_steps()` downloads before stopping the backend; a settings file without `checkUpdates` loads with `check_updates == true` and the existing consent; `menu_model` carries the label only when an offer is given.
- [ ] **Step 2:** Run `cargo test -p mochi-launcher-core`; expect failures.
- [ ] **Step 3:** Implement the core module. Then the Tauri side: add `tauri-plugin-updater = "2"`; `tauri.conf.json` gets `plugins.updater` with `endpoints: [UPDATE_ENDPOINT]`, `pubkey` (owner input), and `windows.installMode: "passive"`; `bundle.createUpdaterArtifacts` stays out of this file (Task 3's release config sets it). `updater.rs` runs a thread that, when `check_due`, calls `app.updater()?.check()` and stores `Some(UpdateOffer)` or nothing in app state, then refreshes the menu through the existing serialized main-thread refresh. Release builds only (`cfg!(debug_assertions)` disables it). Clicking the item: `PackageManaged` opens `RELEASES_PAGE` with the opener plugin; `SelfUpdating` asks for confirmation when `needs_confirmation` says so, then follows `update_steps()`: download (error dialog on failure, nothing stopped), stop the backend with the existing stop path, install (on failure start the backend again and show the error), `app.restart()`. The no-tray panel shows the same item. Settings window: a `Check for updates automatically` checkbox bound to `checkUpdates`, disabled while loading or saving like the other inputs. Grant `updater:default` only if the JS side needs it (it should not; the updater runs in Rust).
- [ ] **Step 4:** Run `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`; expect pass. Add `docs/LAUNCHER.md` checklist rows: update item appears for a newer release; `.deb` opens the release page; download failure leaves the backend running; confirmation shows when an agent runs; setting off means no check.
- [ ] **Step 5:** Commit.

### Task 3: Bundling with the backend inside

**Files:**
- Create: `crates/mochi-launcher/tauri.release.conf.json`, `scripts/stage-backend.mjs`
- Modify: root `package.json` (devDependency `@tauri-apps/cli` pinned to an exact 2.x version at least two weeks old; scripts `tauri` and `bundle`), `.gitignore` (`crates/mochi-launcher/binaries/`), `crates/mochi-launcher/tauri.conf.json` only if bundle metadata is missing (`publisher`, `category`, `shortDescription`, `licenseFile` or `license: "AGPL-3.0-or-later"`, Linux `deb.depends` for WebKitGTK 4.1 and AppIndicator)

**Interfaces:**
- Produces: `node scripts/stage-backend.mjs <rust-target-triple>` copies `target/<triple>/release/mochi-server[.exe]` to `crates/mochi-launcher/binaries/mochi-server-<triple>[.exe]` and fails with a clear message if the source is missing. `pnpm bundle -- --target <triple>` runs `pnpm build`, `cargo build --release -p mochi-server --target <triple>`, the stage script, then `tauri build --config crates/mochi-launcher/tauri.release.conf.json --target <triple>`.
- `tauri.release.conf.json`: `bundle.externalBin: ["binaries/mochi-server"]`, `bundle.createUpdaterArtifacts: true`, `bundle.targets` per OS (`nsis`, `msi`, `app`, `dmg`, `deb`, `appimage`). CI passes an extra `--config '{"bundle":{"createUpdaterArtifacts":false}}'` when no signing key exists.

- [ ] **Step 1:** Implement the stage script and release config; the launcher already looks for `mochi-server` next to its own executable (`crates/mochi-launcher/src/app.rs`), which is where `externalBin` installs it on all three OSes, so no launcher code changes.
- [ ] **Step 2:** On this Windows machine run `pnpm bundle -- --target x86_64-pc-windows-msvc` with `--config '{"bundle":{"createUpdaterArtifacts":false}}'`. Expected: an `.msi` and an NSIS `.exe` under `target/x86_64-pc-windows-msvc/release/bundle/`. List the MSI's files (`msiexec /a` to a scratch folder, or `lessmsi` if present) and confirm `mochi-server.exe` sits beside `mochi-launcher.exe`. Do not install it.
- [ ] **Step 3:** `cargo test --workspace` still passes without `binaries/` present (the release config is the only place naming it).
- [ ] **Step 4:** Commit.

### Task 4: CI workflow

**Files:**
- Create: `.github/workflows/ci.yml`, `.github/workflows/build.yml` (reusable, `workflow_call`)

**Interfaces:**
- `build.yml` inputs: `sign-updater: boolean`, `upload-release: boolean`, `release-tag: string` (empty unless releasing). Secrets: `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` (optional). Matrix (runner → target): `windows-latest` → x86_64-pc-windows-msvc, `windows-11-arm` → aarch64-pc-windows-msvc, `macos-15-intel` → x86_64-apple-darwin, `macos-latest` → aarch64-apple-darwin, `ubuntu-22.04` → x86_64-unknown-linux-gnu, `ubuntu-22.04-arm` → aarch64-unknown-linux-gnu. Each leg: checkout, pnpm + Node 22 with pnpm cache, Rust 1.99 with the target, `Swatinem/rust-cache@v2` keyed by target, Linux packages `libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev patchelf`, `pnpm install --frozen-lockfile`, `pnpm build`, release-build `mochi-server`, stage it, `node scripts/measure-memory.mjs` (no `--gate`) uploading `memory-<target>.json` as an artifact, then `tauri-apps/tauri-action@v0` with `projectPath: crates/mochi-launcher`, `args: --target <triple> --config crates/mochi-launcher/tauri.release.conf.json` (plus the no-updater override when `sign-updater` is false), `tagName`/`releaseDraft: true`/`updaterJsonPreferNsis: true`/`includeUpdaterJson: true` only when `upload-release`. Without upload, the bundles are uploaded as workflow artifacts.
- `ci.yml` on `pull_request` and `push` to `main`: job `checks` (ubuntu-22.04): `pnpm typecheck`, `pnpm test`, `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`; job `tests` matrix `windows-latest`, `macos-latest`: `cargo test --workspace`; job `e2e` (ubuntu-22.04, Chrome is preinstalled): `pnpm build`, `pnpm test:e2e:rust` (after Task 6: `pnpm test:e2e`); job `bundle`: calls `build.yml` with `sign-updater: false`, `upload-release: false`. `concurrency` cancels superseded runs per ref. `permissions: contents: read`.

- [ ] **Step 1:** Write both workflows. Pin every action to a major tag; no `pull_request_target`; no secret in `run:` lines (pass as `env:` to the tauri-action step only).
- [ ] **Step 2:** Validate locally with `actionlint` only if it is already installed (do not download it); otherwise say it was skipped. The real check is the PR run: after pushing, all jobs and 6 bundle legs pass. Read failing logs with `gh run view --log-failed` and fix.
- [ ] **Step 3:** Commit.

### Task 5: Release workflow and notes

**Files:**
- Create: `.github/workflows/release.yml`, `scripts/check-version.mjs`, `scripts/release-notes.mjs`, `scripts/release-notes.test.mjs`

**Interfaces:**
- `node scripts/check-version.mjs <tag>`: exit 1 with a message unless `<tag>` is `v` + `Cargo.toml`'s `[workspace.package].version`.
- `node scripts/release-notes.mjs <dir-of-memory-json> > notes.md`: a Markdown section `## Memory` with one table row per target (`target | metric | MiB | live feed`), the method in one paragraph (interface closed, idle 12 s, live OKX feed, the metric per OS, gate only on Windows x86_64 working set, 83 MiB Node baseline from `bd11fbe`), and a line saying runs without a connected feed are not comparable; a section `## Install` linking the README's install and unsigned-app instructions; a section `## OKX` stating the integration was tested only against a simulated exchange.
- `release.yml` on `push: tags: ['v*']`: job `version` runs `check-version`; job `build` calls `build.yml` with `sign-updater: true`, `upload-release: true`, `release-tag: ${{ github.ref_name }}`, secrets passed explicitly; job `notes` downloads the memory artifacts, runs `release-notes.mjs`, and sets the draft's body with `gh release edit <tag> --notes-file notes.md`. `permissions: contents: write` only on jobs that upload.

- [ ] **Step 1:** Test `release-notes.mjs` with two fixture reports (one connected, one `degraded`) and `check-version.mjs` with a matching and a mismatched tag.
- [ ] **Step 2:** Run `node --test scripts/*.test.mjs`; expect failures, implement, expect pass.
- [ ] **Step 3:** Write `release.yml`. Not run in this plan: the first tag is the owner's call after Task 6 merges. Say so in the PR.
- [ ] **Step 4:** Commit.

### Task 6: Switch to Rust and remove the Node backend

Only after Task 1's gate passed.

**Files:**
- Delete: `apps/server/` (including `golden/` and `test/`), `packages/core/`, `packages/market-data/`, `packages/agents/`, `packages/exchanges/`, `packages/ai-providers/`, `e2e/run-rust.mjs`
- Move: `apps/server/src/db/migrations/*.sql` → `crates/mochi-store/migrations/` (same names and bytes)
- Modify: `crates/mochi-store/src/lib.rs` and `crates/mochi-store/tests/migrations.rs` (new path), `package.json` (scripts), `pnpm-workspace.yaml` only if a pattern breaks, `vitest.config.ts` (include only existing test dirs), `playwright.config.ts` (Rust only; API port from `MOCHIPREP_E2E_API_PORT`, default 8877), `pnpm-lock.yaml` (via `pnpm install`), `.github/workflows/ci.yml` (`pnpm test:e2e`, drop the golden staleness check if any), any Rust test that reads a path under `apps/` or `packages/`
- Create: `scripts/dev.mjs`

**Interfaces:**
- Root scripts after the change: `dev` → `node scripts/dev.mjs` (spawns `cargo run -p mochi-server` and the web `vite` dev server, forwards Ctrl+C to both, exits when either exits); `build` unchanged; `start` → `pnpm build && cargo run --release -p mochi-server`; `test` → vitest (`apps/web/test`; `packages/shared` has no tests of its own); `test:e2e` → Playwright against `target/release/mochi-server` after `cargo build --release -p mochi-server`; `golden`, `test:e2e:rust` removed.
- `packages/shared` stays (the web imports `@mochiprep/shared` 20 times); nothing else in `packages/` stays unless the web or shared imports it.

- [ ] **Step 1:** Before deleting, check that the vite dev server's origin is accepted by `mochi-server`'s default allowed origins the way Node accepted it; if not, `scripts/dev.mjs` sets `MOCHIPREP_ALLOWED_ORIGINS` for the vite port.
- [ ] **Step 2:** Move migrations, update `include_str!` paths and the migrations test; `cargo test -p mochi-store` passes; the files are byte-identical (`git diff -M100% --stat` shows pure renames).
- [ ] **Step 3:** Delete the Node backend and the five packages, update scripts and configs, run `pnpm install`.
- [ ] **Step 4:** Verify: `pnpm typecheck`, `pnpm test`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `MOCHIPREP_E2E_API_PORT=8977 pnpm test:e2e` (all pass, e2e specs unchanged), and `pnpm dev` starts both servers and the UI loads from the vite port (then stop it).
- [ ] **Step 5:** `git grep -n -E "apps/server|@mochiprep/(server|core|market-data|agents|exchanges|ai-providers)|packages/(core|market-data|agents|exchanges|ai-providers)|MOCHIPREP_E2E_BACKEND|pnpm golden|test:e2e:rust"` outside `docs/superpowers/` and Rust doc comments: empty, or each hit fixed. Rust doc comments that say "port of `packages/...`" stay; Task 7 adds the note that explains them.
- [ ] **Step 6:** Commit.

### Task 7: README and docs

**Files:**
- Modify: `README.md`, `AGENTS.md`, `docs/ARCHITECTURE.md`, `docs/OKX_TRADING.md`, `docs/API_KEYS.md`, `docs/PAPER_ENGINE.md`, `docs/ROADMAP.md`, `docs/LAUNCHER.md`, `docs/superpowers/specs/2026-11-07-rust-backend-desktop-design.md` (owner decision note only)

- [ ] **Step 1:** README: what MochiPrep is (keep the current intro), install per OS from GitHub Releases; unsigned builds: macOS "System Settings → Privacy & Security → Open Anyway" after the first blocked launch, Windows SmartScreen "More info → Run anyway"; Linux `.deb` (`sudo apt install ./MochiPrep_*.deb`) and AppImage (`chmod +x`); updating (one-click from the tray on Windows, macOS and AppImage; `.deb` and AUR through the package manager); memory numbers and the method, with the Task 1 Windows number; development commands after Task 6; OKX tested only against a simulated exchange.
- [ ] **Step 2:** AGENTS.md: layout without the removed packages; commands (`pnpm dev`, `pnpm test`, `pnpm test:e2e`, `cargo test --workspace`, `pnpm bundle`); remove "Rust backend (in progress)" and "NOT the default backend"; rules that named TS files now name the Rust equivalents (`mochi_decimal::Dec`, `mochi-vault`, `mochi-okx` `refusal.rs`, and so on); migrations live in `crates/mochi-store/migrations/`; `crates/golden` are frozen fixtures from the former Node backend; keep the phase workflow and the publishing-login rule word for word.
- [ ] **Step 3:** ARCHITECTURE.md: Rust is the backend; one paragraph that Rust doc comments naming `packages/...` or `apps/server/...` refer to the Node backend removed in this change, still in git history before it. Other docs: replace references to removed TS files with the Rust crates. Spec: under "切換前的 OKX 實測", add the 2026-11-16 decision (not a condition) in the spec's language.
- [ ] **Step 4:** `docs/LAUNCHER.md`: add the owner's pre-release rows: launcher plus backend memory with `--pid` (≤ 66 MiB), install and launch each unsigned artifact, one real update from a draft-published test release.
- [ ] **Step 5:** Commit.
