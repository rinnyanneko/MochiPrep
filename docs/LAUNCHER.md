# Desktop launcher

`mochi-launcher` is a tray app that runs the Rust backend (`mochi-server`) as a child process and opens the web interface in its own window. The testable parts (settings, import, tray model, supervisor, backend client) are in `mochi-launcher-core`; `mochi-launcher` is the Tauri shell around them.

## Running in development

    cargo build -p mochi-server
    cargo run -p mochi-launcher

The launcher starts the `mochi-server` executable in its own folder, so build the server first: both land in `target/debug`. The server embeds `apps/web/dist`, so run `pnpm build` first if you want the interface to show the UI.

Environment variables read by the launcher, for development only:

- `MOCHIPREP_DATA_DIR`: the backend's data folder instead of the OS data dir.
- `MOCHIPREP_OFFLINE=1`: passed on to the backend (no OKX public feed, no news).

Linux builds need the WebKitGTK and tray libraries. On Debian or Ubuntu:

    sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev

`mochi-launcher` is a workspace member, so `cargo test --workspace` needs them too. Without them, run `cargo test --workspace --exclude mochi-launcher`. The Linux-only tray check (`panel.rs`, zbus) has not yet been compiled on Linux; build it there once before a release.

## Where things live

The bundle identifier is `io.github.rinnyanneko.mochiprep`.

| What | Windows | macOS | Linux |
|---|---|---|---|
| `launcher.json` (terms consent, port, autostart, import offered, update checks) | `%APPDATA%\io.github.rinnyanneko.mochiprep` | `~/Library/Application Support/io.github.rinnyanneko.mochiprep` | `~/.config/io.github.rinnyanneko.mochiprep` |
| Backend data (`mochiprep.db`), unless `MOCHIPREP_DATA_DIR` is set | `%APPDATA%\io.github.rinnyanneko.mochiprep` | `~/Library/Application Support/io.github.rinnyanneko.mochiprep` | `~/.local/share/io.github.rinnyanneko.mochiprep` |

The interface window's WebView keeps its own data (the interface's saved preferences) apart from these; on Windows it is `%LOCALAPPDATA%\io.github.rinnyanneko.mochiprep\EBWebView`.

Consent and launcher settings never go into the backend's SQLite. A missing or corrupt `launcher.json` loads as defaults, with the terms not accepted. The interface's saved preferences belong to `http://127.0.0.1:<port>`, so changing the port starts them fresh.

## Behaviour

- The backend does not start until the current terms version is accepted. Declining, or closing the terms window, quits.
- Before the first start, if the data folder has no `mochiprep.db`, the launcher offers once to import a `.mochiprep` folder (for example the former Node backend's). The database is copied as a consistent snapshot; the original is not changed. Import into existing data is refused. A copy that cannot finish within 60 s (a source another program keeps writing to) gives up with an error, and the offer stays open.
- The port defaults to 8787 and is set in Settings (1024 to 65535). The launcher never picks another port: a busy port shows `Error: port <p> is in use`.
- Closing the interface window stops only the window; the backend and its agents keep running.
- Disarm all is enabled while the backend runs and a real account is armed, or while its state cannot be read: disarming is always safe.
- Quit, Restart and a port change that restarts ask `Stop MochiPrep's backend?` first if an agent is running, a real (`OKX_LIVE`) account is armed, or the backend's state cannot be read.
- On macOS the launcher has no Dock icon and its menu has no Quit (only Edit, for copy and paste), so Cmd+Q does nothing and Quit is only in the tray, where it asks first.
- On Linux without a tray (GNOME without the AppIndicator extension) a small panel window with the same items replaces the tray icon. Closing the panel is Quit.
- Killing the launcher closes the backend's stdin, and the backend shuts down cleanly.

## Updates

Release builds look for a new version at `https://github.com/rinnyanneko/MochiPrep/releases/latest/download/latest.json` once at start and then every 24 hours while the launcher runs. A failed check is logged to stderr and tried again a day later. Debug builds never check. Turning off `Check for updates automatically` in Settings stops the checks (the setting is `checkUpdates` in `launcher.json`, on by default).

A check only finds out what exists: nothing is downloaded or installed until the owner clicks the update item, which shows in the tray (and the no-tray panel) only when a newer release is known.

- Windows installs, the macOS `.app` and a Linux AppImage update themselves: the item reads `Update to v<version>`. Clicking it downloads the update while the backend still runs (Restart and Quit keep working meanwhile; a download not done within 15 minutes gives up), then asks `Stop MochiPrep's backend?` when Quit would, stops the backend, installs and relaunches. A failed download stops nothing; a failed install starts the backend again. Either shows an error dialog. A check that gets no answer within 30 s counts as failed. On Windows the installer runs in passive mode (a progress bar, no questions).
- Any other Linux install (a `.deb` or a distribution package) belongs to the package manager: the item reads `v<version> available` and opens `https://github.com/rinnyanneko/MochiPrep/releases/latest`.

The updater's signing public key is in `tauri.conf.json` (`plugins.updater.pubkey`); an update not signed by its private key is refused.

## Manual checklist

Run each row on Windows, macOS and Linux before a release. Rows marked OKX need an account; never use one that can withdraw.

| # | Check | Expected |
|---|---|---|
| 1 | First launch with no `launcher.json` | The risk warning opens; no `mochi-server` process runs and the data folder stays empty. |
| 2 | Scroll the terms to the end, tick both boxes, Continue | The backend starts (after the import offer on a new data folder). |
| 3 | Quit on the terms, and separately close the terms with X | The launcher exits; no backend was started. |
| 4 | Tray `Risk & disclaimer…` after acceptance | A working read-only terms window opens: no boxes, no Continue. |
| 5 | `Close` on the read-only terms | The window closes; the launcher and backend keep running. |
| 6 | Tray states | `Backend: Starting`, then `Backend: Running`. With another program on port 8787: `Backend: Error: port 8787 is in use`, Restart and Settings still offered, no retry loop. |
| 7 | OKX: arm an OKX Demo account | No armed highlight: Demo is not real money. |
| 8 | OKX: arm a real account | Tray icon and tooltip change; `Real account armed (1)` shows; Disarm all is enabled. |
| 9 | Open interface | The UI loads from `http://127.0.0.1:<port>`; news links open in the system browser. |
| 10 | Close the interface with a paper agent running | The tray stays; the agent keeps running (its runs continue when the interface is reopened). |
| 11 | Second launch while the backend runs | The interface opens or comes to the front; no second backend starts. |
| 12 | Second launch while the backend is stopped or in error | A dialog shows the backend status and points to the tray. |
| 13 | Restart and Quit with a running paper agent | `Stop MochiPrep's backend?` asks; Cancel keeps everything running; Continue stops the agent. |
| 14 | OKX: Disarm all with a real account armed | The account is disarmed; the armed highlight goes away. |
| 15 | Settings: enter `80`, Save | `Port must be a number from 1024 to 65535` shows; nothing is saved. |
| 16 | Settings: change the port, Save, answer Yes | `Restart the backend now to use port <p>?` asks; Restart (with its confirmation if needed) brings the backend up on the new port. |
| 17 | Settings: tick `Start MochiPrep when I log in`, Save, log out and in | MochiPrep starts at login; unticking removes it again. |
| 18 | Import from a `.mochiprep` folder made by the former Node backend on a new data folder | Accounts and history appear in the interface; the source folder's files are unchanged. |
| 19 | With the import offer open, copy any `mochiprep.db` into the data folder, then import a valid folder | `MochiPrep already has data here, so nothing was imported.`; the copied-in file is unchanged. With a `mochiprep.db` already there at launch, no offer is shown. |
| 20 | Import from an empty folder, then Skip | The error shows inline and the offer stays open; Skip starts the backend on empty data. |
| 21 | Linux: GNOME without AppIndicator | The panel replaces the tray icon, shows live status, and closing it asks to quit (when an agent runs). |
| 22 | Kill the launcher process (Task Manager, `kill -9`) | `mochi-server` exits within 10 s and the port is free. |
| 23 | In the interface's devtools: `window.__TAURI_INTERNALS__.invoke('get_settings')` | Refused: the interface window has no launcher command. |
| 24 | A launcher window fails to build | An error dialog names the window; at first launch a failed terms window quits without starting the backend. |
| 25 | Memory with the interface closed | Note the working set of the launcher plus `mochi-server`. |
| 26 | macOS: after launch, with a paper agent running | No Dock icon; Cmd+Q in the interface does not quit; copy and paste work in the interface; the tray's Quit asks first. |
| 27 | Quit right after launch, while the tray shows `Backend: Starting` | `Stop MochiPrep's backend?` shows at once; Continue quits once the start finishes. |
| 28 | Close the import offer while its folder picker is open, then pick a folder | The import is refused (the offer is closed); the backend runs on the empty data folder. |
| 29 | A backend that dies after starting (kill `mochi-server` alone) | Within 3 s the tray shows `Backend: Error: backend stopped unexpectedly`, followed by the end of its stderr if it wrote any. |
| 30 | Terms with the keyboard only: Tab into the text, scroll it with the arrow keys or Page Down to the end | The boxes unlock and can be ticked with Space; Continue starts the backend. |
| 31 | Release build older than the latest release, `Check for updates automatically` on | Shortly after launch the tray (or panel) shows `Update to v<version>`; with no newer release there is no update item. Clicking it with nothing running downloads, restarts once, and the new version runs. |
| 32 | Linux `.deb` install with a newer release out | The item reads `v<version> available`; clicking it opens the release page and changes nothing. |
| 33 | Click the update item with the network cut | An error dialog names the download failure; the backend keeps running throughout and the interface still works. |
| 34 | Click the update item with a paper agent running | Once the download finishes, `Stop MochiPrep's backend?` asks; Cancel changes nothing and the agent keeps running. During the download, Quit and Restart still work. |
| 35 | Untick `Check for updates automatically`, Save, restart the launcher | No request goes to GitHub (watch with a proxy or firewall log) and no update item appears. |

## Owner's pre-release checks

These need the owner's machines and cannot run in CI. They are not done yet.

| # | Check | Expected |
|---|---|---|
| 36 | Launcher plus backend memory with the interface closed: start the launcher, wait 12 s, then `node scripts/measure-memory.mjs --binary <mochi-server> --pid <launcher pid>` | The launcher tree is at most 66 MiB working set on Windows x86_64. |
| 37 | Install and launch each unsigned artifact (Windows `.msi` and NSIS `.exe`, macOS `.dmg` for each architecture, Linux `.deb` and `.AppImage`) | The OS warning in README "Unsigned builds" appears and the documented steps get past it; the launcher starts and the terms window opens. |
| 38 | One real update: publish a test release with a higher version from the draft, then update an older install from the tray | The tray shows `Update to v<version>`; clicking it downloads, installs and relaunches on the new version. |
