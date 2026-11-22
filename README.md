# MochiPrep

*Your cute little AI trading companion.*

MochiPrep is a local AI trading terminal for USDT-margined crypto perpetuals. You bring your own AI key, and AI agents trade for you on a simulated paper account, on OKX Demo Trading, or on a real OKX account. Everything runs on your own computer.

> **DYOR: do your own research.** MochiPrep is not financial advice. AI output can be wrong, and you are responsible for your own trades. Simulated results are not predictions of future performance.

> **Real money:** OKX trading has only been tested against a simulated exchange, not OKX Demo Trading and not with real funds. Read [Trading on OKX](docs/OKX_TRADING.md) before connecting a real account.

## Install

Download the build for your system from the [latest GitHub release](https://github.com/rinnyanneko/MochiPrep/releases/latest) and start MochiPrep. A tray app starts the backend and opens the interface in its own window. On first launch you read and accept the risk warning; nothing runs before that.

| System | Download |
| --- | --- |
| Windows | `.msi` or the NSIS `.exe` installer |
| macOS | `.dmg`, one per CPU type (Apple silicon or Intel) |
| Linux | `.deb` (`sudo apt install ./MochiPrep_*.deb`) or `.AppImage` (`chmod +x MochiPrep_*.AppImage`, then run it) |

### Arch Linux

The [PKGBUILD](packaging/arch/PKGBUILD) builds MochiPrep from the release source. It is not on the AUR yet. Download it into an empty folder, then run:

```bash
sudo pacman -S --needed base-devel
```

```bash
makepkg -si
```

`makepkg` installs everything the build needs from the official repositories, then builds and installs the package. The build compiles Rust code, so it takes several minutes.

Your data lives in your operating system's application data folder. See [the launcher guide](docs/LAUNCHER.md#where-things-live) for the exact paths. Nothing else is required: no Docker, no account, no paid service.

### Unsigned builds

The installers are not code-signed by Windows or Apple, so your system warns you the first time.

- **Windows:** SmartScreen shows "Windows protected your PC". Choose **More info**, then **Run anyway**.
- **macOS:** the first launch is blocked. Open **System Settings > Privacy & Security**, scroll to the message about MochiPrep, and choose **Open Anyway**.
- **Linux:** no prompt. Mark the AppImage executable first.

### Updating

- Windows, macOS and the Linux AppImage update from the tray. When a newer release exists, the menu shows an update item. Clicking it downloads, installs and restarts.
- A `.deb` updates through your package manager. The tray only shows that a new version exists and opens the release page.
- The Arch package updates when you run `makepkg -si` again with a newer PKGBUILD. The tray only shows that a new version exists.
- The launcher checks for a new version at start and every 24 hours. Turn this off with **Check for updates automatically** in Settings.

## Build from source

You need Node.js 22.13 or newer, pnpm 12 (pinned in `package.json`), and a Rust toolchain. Node is only used for the web UI; the backend is Rust.

```sh
git clone https://github.com/rinnyanneko/MochiPrep.git
cd MochiPrep
pnpm install
pnpm build
pnpm start
```

Then open <http://localhost:8787>. The release server embeds the UI and listens on `127.0.0.1` only.

To run it in Docker instead, see [Self-hosting with Docker](docs/SELF_HOSTING.md). The image is built from source with `docker compose up -d`.

The data folder for a source build is `.mochiprep/` in the repository. For development setup, tests and installer builds, see [Development](docs/dev/DEVELOPMENT.md).

## Getting started

Press **Try Demo Mode** on the Dashboard. No API key and no internet connection are needed. Each mock agent starts with 10,000 virtual USDT. Press play in the top bar to run the replay. Demo Mode uses a bundled, seeded price tape and mock agents, so each run repeats the same way. Its trades, PnL and leaderboard results are simulations, not forecasts.

## AI providers

Add a provider in **Settings**, paste its key, and pick it when you create an agent. Supported providers:

- OpenAI and OpenAI-compatible services, including local models such as Ollama
- Anthropic Claude
- Google Gemini
- DeepSeek
- OpenCode

Step-by-step instructions for getting each key are in [How to get API keys](docs/API_KEYS.md).

By default, AI keys are saved **unencrypted** in a local database, so they survive restarts. To protect them, set a **master password** in **Settings**. AI keys are then stored encrypted, and you unlock once after each restart. There is no password recovery. See [How to get API keys](docs/API_KEYS.md) for details.

Model output is checked strictly, and it must pass the risk checks before any order is placed. Keys are never shown in the browser or written to logs.

## Agents: suggest or trade automatically

Each agent has one of two modes:

- **Suggest to me** (the default for new agents). The agent shows its entry, stop and target on the chart and in a list. Nothing is ordered until you accept. Accepting runs the risk checks again and resizes the order at current prices. Suggestions expire after one agent interval.
- **Trade automatically.** The agent places approved orders without asking you first.

## Agent templates

A template is a saved set of agent settings: strategy instructions, timeframe, schedule, execution mode, news, trailing stops and risk limits. When you create an agent, pick one under **Start from template** to fill in the form, then change anything you like before saving. The template only fills in the form. The agent's limits are checked when it is created, and every order still goes through the risk checks. Provider, model, pair and account are chosen per agent, so templates do not store them.

MochiPrep includes four built-in templates: **Conservative**, **Balanced**, **Trend following** and **Range reversion**. They are starting points, not proven strategies. All of them use Suggest mode, require a stop loss, and keep leverage and risk per trade at or below the defaults.

Open **AI Agents > Templates** to manage them:

- **New template** creates your own from scratch.
- **Duplicate** copies any template, including a built-in one, so you can change it.
- **Edit** changes your own templates. Built-in templates cannot be edited, so updates to MochiPrep can improve them. Duplicate one to change it.
- **Delete** removes your own template. Deleting a built-in only hides it, and **Restore built-ins** brings back every hidden one.
- **Export** (on a row) or **Export mine** (all your own templates) shows a `.json` file you can download or copy. It holds only template settings, never keys or account details.
- **Import** reads an exported file, either picked from disk or pasted as text, and adds its templates as your own. A name that already exists gets a copy suffix. Each imported template is checked like one you create by hand, and the file is refused if it is not a MochiPrep template export. Only import files you trust, and review the templates before using them: strategy text is sent to the AI model, and an imported template may use Trade automatically.

Changing or deleting a template does not change agents already created from it.

## Paper trading and live prices

Switch the top bar to **Live OKX** to paper trade against OKX public market data. No exchange key is needed. Paper orders are simulated locally. See [How paper trading works](docs/PAPER_ENGINE.md) for the rules.

On **Markets** and **Trading**, charts show the selected account's trades: `B` and `S` markers, the entry line with live PnL, and the stop, take-profit, trailing stop, liquidation and limit levels. Hover a marker to see the fill's size, price and fee.

## Trading pairs

BTC, ETH and SOL are built in. To trade another coin, add it in **Settings > Trading pairs**. Any live USDT crypto perpetual listed on OKX can be added, and its contract specifications are read from OKX. Added pairs work on Live OKX accounts only, because the replay includes just the three built-in pairs.

## News

On Live OKX, an agent can read recent headlines from RSS feeds. Turn this on per agent, and choose feeds in **Settings > News sources**. Agents see up to 8 headlines from the last 24 hours, with headlines about their coin first. Headlines are untrusted input: they can be wrong, or written to mislead an AI. The risk checks still limit every order, but they cannot tell a misled decision from a sound one. On a real account, use Suggest mode when news is on.

## Trading on OKX

Connect an account in **Settings**, using Demo Trading or a real account. Your OKX key needs **Trade** permission and must **not** have Withdraw permission. Keys that can withdraw are refused. Step-by-step instructions are in [How to get API keys](docs/API_KEYS.md), and the safety rules are in [Trading on OKX](docs/OKX_TRADING.md).

## Your data

- MochiPrep keeps its database and launcher settings in your operating system's application data folder. Only your user account should be able to read them. See [the launcher guide](docs/LAUNCHER.md#where-things-live).
- **AI keys** are unencrypted unless you set a master password. Anyone who can read the database, including backups and copies, can read unencrypted keys. Do not keep the data folder in iCloud, Dropbox or other sync folders.
- **OKX keys** are kept only in memory by default, so they are gone after a restart. You can save them encrypted with your master password when you connect.
- Keys are never sent to a model or shown in the browser.
- Requests leave your computer only to the AI provider you chose, to OKX when you use it, and to the news feeds you turned on.

## Current limits

- OKX is the only supported exchange. Only linear USDT crypto perpetuals with isolated margin and one-way position mode are supported. OKX's stock, commodity and pre-market contracts are excluded.
- OKX order handling has only been tested against a simulated exchange, not OKX Demo Trading and not a real account. See [Trading on OKX](docs/OKX_TRADING.md) for what is still unverified.
- Paper trading uses one 0.5% maintenance rate for every pair, while OKX uses tiers. Simulated liquidation is less accurate on small coins.
- Paper fills, funding and liquidation are simplified. There is no order-book queue, partial liquidity, cross margin, or exact exchange-matching liquidation.
- Trailing stops and other protection changes on OKX have not been verified on OKX Demo Trading.
- Other exchanges are planned but not available.

## License

MochiPrep is licensed under the [GNU Affero General Public License](LICENSE), version 3 or (at your option) any later version. If you run a modified version as a network service, you must offer its users the source.

Charts use [TradingView Lightweight Charts](https://www.tradingview.com/lightweight-charts/) (Apache-2.0), copyright TradingView, Inc.

## Documentation

- [How to get API keys](docs/API_KEYS.md)
- [Trading on OKX](docs/OKX_TRADING.md)
- [How paper trading works](docs/PAPER_ENGINE.md)
- [The desktop launcher](docs/LAUNCHER.md)
- [Self-hosting with Docker](docs/SELF_HOSTING.md)

For developers:

- [Development](docs/dev/DEVELOPMENT.md)
- [Architecture](docs/dev/ARCHITECTURE.md)
- [Desktop launcher (development)](docs/dev/LAUNCHER.md)
- [Roadmap](docs/dev/ROADMAP.md)
