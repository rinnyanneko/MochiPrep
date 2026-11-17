# Development

This is the developer setup. For what the app does and how to use it, see the [README](../../README.md). For the code layout, read [Architecture](ARCHITECTURE.md). AI agents working in this repository should also read [AGENTS.md](../../AGENTS.md).

## Requirements

- Node.js 22.13 or newer, with pnpm 9 or newer (the repository pins pnpm 11). Node is only used for the web UI and the scripts.
- A Rust toolchain. The backend is Rust.
- Google Chrome, only for `pnpm test:e2e`.

## Run from source

```sh
pnpm install
pnpm dev
```

`pnpm dev` runs the Rust server (through cargo) and the vite dev server. Open <http://localhost:5173>. The backend binds to `127.0.0.1:8787` and keeps its SQLite database in `.mochiprep/` in the repository.

Set `MOCHIPREP_PORT` or `MOCHIPREP_WEB_PORT` to change the ports.

To run only the release server, which embeds the built UI:

```sh
pnpm build
pnpm start
```

It serves the UI at <http://localhost:8787>.

## Build the installers

```sh
pnpm bundle -- --target <triple>
```

Releases are built by CI from a `vX.Y.Z` tag that matches the workspace version. The desktop launcher's own development notes are in [Desktop launcher (development)](LAUNCHER.md).

On Linux, the launcher needs the WebKitGTK and tray libraries:

```sh
sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev
```

## Tests

```sh
pnpm test                # web unit tests (vitest), offline, no API key
pnpm typecheck
cargo test --workspace   # Rust crates
pnpm test:e2e            # Playwright through the real UI, needs Google Chrome installed
```

`pnpm test:e2e` builds the release `mochi-server` and runs the browser tests against it on its own port (`MOCHIPREP_E2E_API_PORT`, default 8877) with a throwaway database.

`mochi-launcher` is a workspace member, so `cargo test --workspace` needs the Linux libraries above. Without them, run `cargo test --workspace --exclude mochi-launcher`.

## Environment variables for the launcher

Development only:

- `MOCHIPREP_DATA_DIR`: the backend's data folder instead of the OS data folder.
- `MOCHIPREP_OFFLINE=1`: passed on to the backend, with no OKX public feed and no news.
