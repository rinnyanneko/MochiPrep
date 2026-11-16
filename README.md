# MochiPrep

*Your cute little AI trading companion.*

MochiPrep is a local-first, bring-your-own-key AI trading terminal for USDT-margined crypto perpetuals. AI agents place orders on their own, on a simulated paper account, on OKX Demo Trading, or on a real OKX account. It combines deterministic replay, OKX market data, strict risk checks, an auditable paper ledger, and multiple model providers. Simulated performance is not predictive.

> **DYOR: do your own research.** MochiPrep is not financial advice. AI output can be wrong, and you are responsible for your own trades.

> **Real money:** OKX trading has only been tested against a simulated exchange, not OKX Demo Trading and not with real funds. Read [Trading on OKX](docs/OKX_TRADING.md) first. AI decisions can be wrong, and you are responsible for every order.

## Install

Download the build for your system from the [latest GitHub release](https://github.com/rinnyanneko/MochiPrep/releases/latest) and start MochiPrep. It runs a tray app that starts the backend and opens the interface in its own window. On first launch you read and accept the risk warning; nothing runs before that.

| System | Download |
| --- | --- |
| Windows | `.msi` or the NSIS `.exe` installer |
| macOS | `.dmg`, one per CPU architecture (Apple silicon or Intel) |
| Linux | `.deb` (`sudo apt install ./MochiPrep_*.deb`) or `.AppImage` (`chmod +x MochiPrep_*.AppImage`, then run it) |

Your data (the SQLite database and the launcher settings) lives in your operating system's application data folder. See [the launcher guide](docs/LAUNCHER.md) for the exact paths. Nothing else is required: no Docker, no account, no paid service.

### Unsigned builds

The installers are not code-signed by Windows or Apple, so your system warns you the first time. The updater files are signed separately; that signature is what checks updates.

- **Windows:** SmartScreen shows "Windows protected your PC". Choose **More info**, then **Run anyway**.
- **macOS:** the first launch is blocked. Open **System Settings > Privacy & Security**, scroll to the message about MochiPrep, and choose **Open Anyway**.
- **Linux:** no prompt. Mark the AppImage executable first.

### Updating

- Windows, macOS and the Linux AppImage update from the tray: when a newer release exists the menu shows an update item, and clicking it downloads, installs and restarts.
- A `.deb` updates through the package manager, as would a distribution package if one is published. The tray only says a version is available and opens the release page.
- The launcher checks for a new version at start and every 24 hours. Turn that off with **Check for updates automatically** in Settings.

## Memory use

The backend is a single Rust binary. With the interface closed, idle for 12 seconds and the live OKX feed connected, its Windows x86_64 working set measured 19.49, 19.74 and 19.76 MiB over three runs, against 83 MiB for the Node backend it replaced (measured at `bd11fbe`). The release gate is 20 MiB or less. It was reached by tuning, with 2 tokio worker threads and a release profile of `lto = "fat"` and `codegen-units = 1`, not by an allocator change.

Measure it yourself:

```sh
cargo build --release -p mochi-server
node scripts/measure-memory.mjs --binary target/release/mochi-server.exe --gate
```

Linux and macOS are measured in CI for reference only (Linux resident set, macOS `footprint`); the gate applies to Windows only. The web interface's own WebView memory is separate and not included. The launcher plus backend has a budget of 66 MiB with the interface closed; that is a manual check (`--pid <launcher pid>`) that has not been done yet.

## Development

Needs Node 22.13 or newer with pnpm 9 or newer (the repository pins pnpm 11), and a Rust toolchain. Node is only for the web UI and the scripts; the backend is Rust.

```sh
pnpm install
pnpm dev
```

`pnpm dev` runs the Rust server (through cargo) and the vite dev server. Open <http://localhost:5173>. The backend binds to `127.0.0.1:8787` and keeps its SQLite database in `.mochiprep/` in the repository. Set `MOCHIPREP_PORT` or `MOCHIPREP_WEB_PORT` to change the ports.

For everyday use from source, build the web app once and run the release server alone. It embeds the UI and serves it at <http://localhost:8787>:

```sh
pnpm build
pnpm start
```

To build the installers locally, run `pnpm bundle -- --target <triple>`. Releases are built by CI from a `vX.Y.Z` tag that matches the workspace version.

## Test

```sh
pnpm test                # web unit tests (vitest), offline, no API key
pnpm typecheck
cargo test --workspace   # Rust crates
pnpm test:e2e            # Playwright through the real UI, needs Google Chrome installed
```

`pnpm test:e2e` builds the release `mochi-server` and runs the browser tests against it on its own port (`MOCHIPREP_E2E_API_PORT`, default 8877) with a throwaway database. On Linux, `mochi-launcher` needs `libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev`; otherwise add `--exclude mochi-launcher` to the cargo command.

## Demo Mode

Press **Try Demo Mode** on the Dashboard. It needs no API key and no internet: each mock agent gets 10,000 virtual USDT, then press play in the top bar to run the tape. Demo Mode uses a bundled, seeded replay tape and mock agents. Time advances deterministically and waits for agent work, making equal runs reproducible by values and fill sequence. Demo trades, PnL, rejections, and leaderboard results are simulations—not forecasts or investment claims.

## Suggest or trade automatically

Each agent has a mode. **Suggest to me** shows the entry, stop and target on the chart and in a list, and nothing is ordered until you accept. Accepting runs the risk engine again and resizes the order at current prices. Suggestions expire after one agent interval, and the replay tape stops at each one so you can read it. **Trade automatically** places approved orders with no confirmation. New agents start in Suggest mode.

## News

On Live OKX, an agent can read recent headlines from RSS feeds. Turn it on per agent, and choose feeds in **Settings > News sources**. Agents get up to 8 headlines from the last 24 hours, with their coin first. Only titles are used, and each decision records the ones the model saw. Headlines are untrusted: they can be wrong, or written to mislead an AI. The risk engine still bounds every order, but it cannot tell a misled decision from a sound one, so prefer Suggest mode with news on a real account.

## Live OKX prices

Switch the top bar to **Live OKX** to paper trade against OKX public market data. No exchange key is involved: orders on a paper account are simulated locally.

## Trades on the chart

Charts on **Markets** and **Trading** show the selected account's trades: `B` and `S` markers with realized PnL on closing fills, the entry line with live PnL and return on margin, TP, SL, liquidation, and resting limits. Hover a bar with a marker to see each fill's size, price and fee. Markers cover the account's latest 500 fills and appear on loaded bars only.

## Trading pairs

BTC, ETH and SOL are built in. To trade another coin, add it in **Settings > Trading pairs**: any live USDT crypto perpetual listed on OKX works, and its contract specs come straight from OKX. Added pairs work on Live OKX accounts only, because the replay tape has just the three built-in pairs.

## Trading on OKX

Connect an account in **Settings**, using Demo Trading or a real account. See [how to get API keys](docs/API_KEYS.md). Agents on that account then trade it automatically, with every order inside the account's ceilings. See [Trading on OKX](docs/OKX_TRADING.md).

## BYOK providers

Add a provider in **Settings**, paste its key, then pick it when creating an agent. See [how to get API keys](docs/API_KEYS.md). You can configure OpenAI-compatible, OpenAI, Anthropic, Gemini, DeepSeek, or OpenCode providers, and add extra request headers for gateways that need them. AI keys and extra headers are saved **unencrypted** in the local SQLite file, so they survive restarts. The database lives in your application data folder (`.mochiprep/` when run from source), which only your user can read. Anyone who can read that file, including backups of it, can read the keys. To prevent that, set a **master password** in Settings: AI keys are then stored encrypted, OKX keys can optionally be saved encrypted too, and you unlock once after every restart. There is no password recovery. Extra headers stay unencrypted. Keys are never logged or returned to the browser. Model output is strictly validated and must pass backend risk checks before the paper engine can accept an order.

## Current limits

- OKX is the only exchange. Linear USDT crypto perpetuals only, isolated margin, one-way position mode. OKX's stock, commodity and pre-market contracts are excluded.
- OKX order handling has only been tested against a simulated exchange, not OKX Demo Trading and not a real account.
- Paper trading uses one 0.5% maintenance rate for every pair, while OKX uses tiers, so simulated liquidation is less accurate on small coins.
- Simplified fills, funding, and isolated liquidation; no order-book queue, partial liquidity, maintenance tiers, cross margin, or exchange-identical liquidation.
- Arming always ends on restart. OKX credentials are kept only in memory unless you set a master password and choose to save them, in which case they are stored encrypted and you unlock after each restart. AI keys persist across restarts, encrypted when a master password is set.

## License

MochiPrep is licensed under the [GNU Affero General Public License](LICENSE), version 3 or (at your option) any later version. If you run a modified version as a network service, you must offer its users the source.

Charts use [TradingView Lightweight Charts](https://www.tradingview.com/lightweight-charts/) (Apache-2.0), copyright TradingView, Inc.

See [Trading on OKX](docs/OKX_TRADING.md), [paper engine rules](docs/PAPER_ENGINE.md), [architecture](docs/ARCHITECTURE.md), and [roadmap](docs/ROADMAP.md).
