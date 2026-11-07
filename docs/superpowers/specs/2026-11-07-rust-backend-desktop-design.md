# Rust 後端與桌面 Launcher 設計

日期：2026-11-07
分支：`claude/rust-backend`（從 `main` 的 `bd11fbe` 開出）

## 目的與動機

MochiPrep 的後端目前是 Node/TypeScript，打包後一個 process 約 83 MiB，用 `pnpm dev` 跑則是兩個 process 共約 125 MiB。擁有者希望：

1. **記憶體更少**：把後端整個用 Rust 重寫。
2. **後端能在背景長時間運作**：agent 照排程跑、OKX 倉位持續同步，跟介面開不開無關。
3. **做成桌面 App**：用一個常駐系統匣的 launcher 控制後端，並能單獨開關介面。
4. **開源發佈**：在 GitHub Releases 提供安裝檔，之後可能上 AUR，並支援檢查更新和一鍵更新。擁有者的朋友明確提過「沒有檢查更新和一鍵更新很麻煩」。

## 成功與失敗的定義

擁有者的原話是：**「RAM 佔用更多，或是功能失效」就算失敗**。

- **記憶體**：基準是目前打包版 Node 後端的約 83 MiB（working set，在 `main` 的 `bd11fbe` 量測，介面關閉、後端閒置約 12 秒、live 行情連線中）。
  - Rust 後端在相同條件下的目標是 **≤ 20 MiB**。
  - Launcher 加後端（介面關閉時）必須明顯低於 83 MiB。
  - 介面打開時 WebView 的記憶體另外量、另外報告，不跟後端混算。
  - 每個 Release 都要附上量測數字和量測方法。
- **功能對等**：現有的每一項功能都要在 Rust 後端上運作，判斷標準如下：
  - 現有的 Playwright e2e（`e2e/demo.spec.ts`）**不改測試內容**，直接打 Rust 後端就要通過。
  - 現有 279 個 TypeScript 單元與整合測試涵蓋的每個情境（帳務、風控、模擬撮合、清算、資金費、OKX 的各種失敗與未知結果、主密碼、AI provider 解析與重試、新聞、指標、回放），在 Rust 端都要有對應的測試。可以重新改寫，但不能少。
- **反目標**：
  - 不能改 React 前端來遷就後端。HTTP API 和 SSE 事件必須與現在完全相容。
  - 不能為了記憶體犧牲安全規則（見「不可妥協的規則」）。
  - 不能宣稱還沒實作的功能已經可用。

## 範圍

**要做的**
- Rust 後端：取代 `apps/server` 和 `packages/{core,market-data,agents,exchanges,ai-providers}` 的執行期行為。
- Tauri 2 launcher：系統匣、首次啟動的條款頁、介面視窗、更新。
- 跨平台的 CI build 與 GitHub Releases：Windows、macOS、Linux，各自含 x86_64 和 aarch64，共 6 種組合。
- README：安裝、未簽章版本的放行方式、更新方式。

**不做的（這一版）**
- macOS 和 Windows 的程式碼簽章與公證（擁有者決定先發佈未簽章版）。
- AUR 套件：可以等第一個 Release 之後再做，但設計上要讓 PKGBUILD 能直接取用 Release 的 Linux 產物。
- 新增交易所、新市場或任何新的交易功能。
- 前端改寫。

## 擁有者已經決定的部分

### 運作模型
- **Launcher 和後端同時開、同時關**。開 launcher 就是啟動後端；結束 launcher 就是停止後端。擁有者的理由是：後端沒跑，前端也沒意義。
- **介面可以單獨開關**。關掉介面視窗不影響 launcher 和後端，agent 照常運作。
- 後端在背景運作代表：已經 arm 的真實帳戶，可能在沒人看著的時候下單。現有規則維持不變：**後端每次重啟，真實帳戶一律 disarm**，主密碼回到鎖定。

### Launcher（擁有者選的方案 B：以系統匣為主）
- 平常只有系統匣圖示和右鍵選單，沒有常駐的主視窗。
- **系統匣選單**：
  - 後端狀態（Running／Starting／Stopped／Error）
  - OKX Live 是否有 armed 的帳戶（有的話要醒目標示）
  - Open interface
  - Restart backend（擁有者要求加的）
  - Disarm all
  - Risk & disclaimer…
  - Update to vX.Y.Z（有新版時才出現）
  - Quit（同時停止後端）
- **首次啟動與條款更新時**：先顯示整頁的風險警告和免責條款，要捲到最底，並勾選「了解可能損失全部資金」和「接受條款」兩項才能繼續。**勾選前不啟動後端**。條款要有版本號，版本改變時要求重新同意。
- **Quit、Restart、更新**：如果有 agent 在跑，或任一真實帳戶是 armed，要先跳出確認視窗，說明 agent 會停止、帳戶會 disarm，而倉位和停損單會留在 OKX 上。

### 更新
- 用 **Tauri updater** 做檢查更新和一鍵更新，從 GitHub Releases 讀取更新資訊。
- launcher 啟動時檢查一次，之後每天一次。有新版時系統匣顯示提示，選單出現「Update to vX.Y.Z」。設定可以關掉自動檢查。
- 更新檔用 Tauri 的更新簽章金鑰簽章並驗證。私鑰放在 GitHub Secrets，**絕不 commit**。這把金鑰跟 OS 的程式碼簽章無關。
- Windows（安裝檔）、macOS（`.app`）、Linux AppImage 可以一鍵更新。`.deb` 和 AUR 版本只提示有新版，實際更新交給套件管理器。
- 更新等於重啟後端，套用上面 Quit／Restart 的確認規則。

### 發佈
- GitHub Releases，產物包含 Windows 的 `.msi`／`.exe`、macOS 的 `.dmg`（x86_64 和 aarch64 各一，或 universal），以及 Linux 的 `.deb`、`.AppImage`。
- **未簽章**。README 要寫明 macOS 怎麼在「隱私與安全性」裡放行，以及 Windows SmartScreen 怎麼選「仍要執行」。
- aarch64 一定要支援（擁有者明確要求）。

### 遷移方式
- 重寫的基準點是目前的 `main`，包含 PR #2（主密碼）和記憶體優化。
- Rust 後端開發期間，Node 後端保留為預設。等 Rust 版**同時**通過 e2e、功能完全對等、記憶體達標之後，才切換過去，並移除 Node 後端和不再需要的 TS 套件。前端還會用到的部分保留，見下文。

### 第一個 Release
擁有者決定：**等 Rust 後端達到功能對等之後才發佈第一版**，不先發佈搭配 Node 後端的 launcher。所以：
- 不需要「launcher 帶 Node 子程序」這條路徑。
- 也不需要讓更新程式負責「從 Node 後端換到 Rust 後端」。
- launcher、CI 和發佈流程可以跟後端重寫同時開發，但第一個公開的 Release 要等切換條件全部達成。

### 維護者
擁有者「會一點 Rust」，之後會自己維護。程式碼要以好讀為優先：
- 使用社群主流的 crate。
- 少用複雜的泛型、trait 技巧和自訂巨集。
- 註解說明為什麼這樣寫（沿用 AGENTS.md 的規則）。

## 不可妥協的規則（從 AGENTS.md 移植，Rust 版同樣適用）

- 帳務一律用 decimal（建議 `rust_decimal`），不准用 `f64` 算錢。只有指標計算和回放 tape 可以用浮點數。
- 每一筆錢包或保證金的變動都經過 ledger，並在同一個 SQLite transaction 裡完成。
- AI 輸出不可信任：只有 agent runner 能把它轉成訂單，而且要先經過 schema 驗證和風控引擎。
- 模擬撮合不能憑空造價格：成交價只能來自實際看到的報價，不能用 OHLC 路徑推算。
- AI key 存在 SQLite（有主密碼時只存密文），不能出現在 log、錯誤訊息或瀏覽器裡。
- OKX 憑證只放在記憶體，除非擁有者選擇用主密碼加密保存。絕不以明文寫進磁碟，也絕不給模型看。主密碼和資料金鑰（DEK）都不寫進任何地方。
- 解鎖後重連已存的 OKX key，要經過完整的連線檢查，而且**絕不自動 arm**。
- 結果未知的訂單，絕不重送，要用查詢去確認。每一筆 OKX 訂單都要經過 pre-trade refusal 檢查。
- 合約規格只從 OKX 取得，不准猜。`SymbolId` 不設上限，回放只有 `DEFAULT_SYMBOLS`。
- 能提現的 key 一律拒絕。disarm 時仍然可以平倉。
- 模擬結果絕不能包裝成預測。

## 相容性約束

### HTTP API 與 SSE
Rust 後端必須提供跟現在一模一樣的 API，包括路徑、方法、request 和 response 的 JSON 形狀、HTTP 狀態碼、錯誤格式 `{ "error": "..." }`。路由清單以 `apps/server/src/app.ts` 為準，目前有：

session、health、state、events（SSE），market（candles、quotes），news 與 news sources，instruments，accounts（及其 positions、closed-positions、orders、fills、ledger、equity、audit、reset），orders，positions（close、protection），providers（及 key、test），agents（及 start／pause／resume／stop、analyze、run、stop-all），vault（建立、unlock、password、disable、reset），exchange（status、okx connect、disconnect、arm、disarm），signals（execute、dismiss），decisions，arena，demo（start、reset、exit），replay。

其他行為也要一致：
- **安全檢查**：Host 和 Origin 白名單、每次啟動重新產生的 bearer token、`/api/session` 只發給同源請求、`/api/events` 接受 `?token=`。
- **錯誤密碼回 403**，因為前端遇到 401 會當成 token 過期並重試。
- **SSE**：事件型別（`quote`、`feed`、`replay`、`invalidate`、`toast`、`heartbeat`）、節流間隔、15 秒 heartbeat 都要一樣。

### 前端共用的 TypeScript
`apps/web` 會從 `@mochiprep/shared` 匯入型別和少量執行期程式：`INSTRUMENTS`、`registerInstrument`、`DEFAULT_SYMBOLS`、`TIMEFRAMES`、`TIMEFRAME_MS`、`ARM_PHRASE`、`VAULT_RESET_PHRASE`、`DEFAULT_LIVE_LIMITS`、`DEFAULT_RISK_SETTINGS`、`isDefaultSymbol`，以及指標函式 `ema`、`macd`、`rsi`。

`packages/shared` 因此要保留給前端使用。Rust 端的型別與它之間要有**自動化的契約檢查**，避免兩邊對不上；具體做法由實作者決定。

### 資料
- 沿用同一個 SQLite schema 和 migrations（001 到 007）。Node 版建立的資料庫，Rust 版要能直接開啟，反之在切換前也要能開。
- 主密碼的格式必須逐位元相容：scrypt N=2^17、r=8、p=1，keylen 32，密碼先做 NFC 正規化；密文格式是 AES-256-GCM 的 `v1.<iv>.<tag>.<body>`，AAD 為 `provider-key:<id>`、`okx-credentials:<accountId>`、`mochiprep:vault-key`。Node 版加密的資料，Rust 版要能解開。
- `secure_delete` 和加密後的 WAL checkpoint 行為要保留。

### 回放 tape
回放必須與現在**完全相同**，因為 demo 要能重現，而且有 hash 測試。mulberry32 PRNG、tick 取整、`toFixed` 的格式都要在 Rust 裡精確重現。特別注意：JS 的 `Math.round` 遇到 .5 是往正無窮方向取整，Rust 的 `f64::round` 則是遠離零。驗收方式是：以 `packages/market-data/test/market-data.test.ts` 的 hash，以及三個完整 tape 加上回放輸出的 hash 做比對（見記憶體優化 commit `bd11fbe` 的做法）。

### AI 提示詞
agent 的 system prompt 和 context 格式要與現在一致，換後端不能改變模型行為。

## 切換前的 OKX 實測（尚未決定）

用 mock 的 OKX 跑單元測試，可以涵蓋各種失敗與結果未知的情境。但簽章、WebSocket 倉位同步、拒絕有提現權限的 key 這些環節，只有實際連到 OKX 模擬盤才能確認。

擁有者決定**等測試做完再決定**要不要把「OKX 模擬盤實測」列為切換條件。在 Rust 版通過全部 mock 測試和 e2e 之後，要把這個問題重新提出來，並在擁有者回覆之前，暫停切換。這項實測需要擁有者自己的模擬盤 API key，所以只能手動執行，不放進 CI。

## 架構方向（我的提議，擁有者可推翻）

- **Cargo workspace** 放在 repo 根目錄，crate 放在 `crates/` 底下，例如 core、market-data、agents、exchanges、ai-providers、server、launcher。怎麼切由實作者決定。
- **後端**是獨立的執行檔，由 launcher 以子程序方式啟動。Restart 就是重開子程序；後端當掉不會拖垮 launcher；記憶體也能分開量。
- **建議的 crate**：axum 和 tokio（HTTP 與 SSE），rusqlite（bundled SQLite），rust_decimal，reqwest（rustls），tokio-tungstenite（OKX WebSocket），aes-gcm 和 scrypt，serde，以及一個 schema 驗證方案取代 Zod。
- **前端靜態檔**：React build 出來的檔案嵌入後端執行檔，由後端提供，跟現在 `apps/web/dist` 的行為一樣。launcher 的介面視窗直接載入 `http://127.0.0.1:<port>`。
- **資料目錄**：安裝版用各作業系統的應用程式資料目錄。開發模式仍然可以用 `MOCHIPREP_DATA_DIR`。

## 我替擁有者做的決定（請擁有者確認）

- 開機時自動啟動 launcher：提供選項，**預設關閉**。
- 舊資料遷移：如果偵測到原本 repo 裡的 `.mochiprep/` 資料庫，首次啟動時詢問是否匯入（複製過去，不是搬走）。
- 主密碼在哪裡解鎖：沿用前端現有的解鎖列，launcher 不另外做解鎖畫面。
- 第一版不做自動更新的「靜默更新」：每次更新都要使用者按下去才執行。
- **記憶體怎麼量**：驗收門檻只用跟基準相同的條件判定，也就是 Windows x86_64 的 working set，量測方式和 `bd11fbe` 那次相同（介面關閉、閒置約 12 秒、live 行情連線中）。其他 5 種組合也要量並寫進 Release notes，但只作為參考，不當門檻：Linux 看 RSS，macOS 看 `footprint`。量測要寫成腳本，在每次 Release 的 CI 自動執行。
- **固定 port**：預設固定用 8787，可在設定中更改。原因是前端的 `localStorage`（主題、資料來源等偏好）綁定 origin，port 一變偏好就會消失。launcher 限制只能同時開一個；port 被佔用時，系統匣顯示錯誤並說明是哪個 port，不會自動換 port。開發時的 `pnpm dev` 仍可用 `MOCHIPREP_PORT` 改 port，避開衝突。
- **舊資料匯入的位置**：已安裝的 App 不會知道 repo 放在哪裡，所以不自動掃描。首次啟動時提供「Import existing data…」按鈕，讓使用者自己選擇 `.mochiprep` 資料夾，選好後把資料複製過去；直接略過也可以。
- **沒有系統匣的 Linux 桌面**（例如沒裝擴充功能的 GNOME）：偵測不到系統匣時，launcher 改顯示一個小視窗，內容和系統匣選單相同。關掉這個小視窗等於 Quit，按之前要先確認。
- **條款同意紀錄存在哪裡**：存在 launcher 自己的設定檔（作業系統的設定目錄），不存在後端的 SQLite。這樣後端還沒啟動前就能判斷要不要顯示條款頁。條款內容的版本號一旦提高，就要求重新同意。

## 留給實作者決定

- crate 的切分方式與模組結構。
- 錯誤處理的方式（例如 thiserror 或 anyhow）、log 方式（log 內容不得含密鑰）。
- 契約檢查的具體作法，例如從 Rust 產生 JSON Schema 再跟 Zod schema 比對、產生 TS 型別，或以 API 層級的契約測試取代。
- TypeScript 測試情境搬到 Rust 的順序與分批方式。
- Launcher 與後端之間的溝通方式，例如 stdout 回報 port 和就緒狀態，或用健康檢查輪詢。
- CI 的 workflow 結構與快取策略。

## 附錄：風險警告與免責條款（第 1 版，擁有者已審閱）

首次啟動和條款版本更新時，launcher 會顯示以下英文全文。`{Exchange status line}` 要依每個 Release 的實際測試狀態填寫，絕不能寫得比實際測試更樂觀。正式發佈前，建議請熟悉當地法規的人再看過一次。

> **Risk warning and disclaimer** · Version 1
>
> **MochiPrep is software, not advice.** Nothing it shows, including AI output, suggestions, backtests, replays, or the leaderboard, is financial, investment, legal, or tax advice. Do your own research.
>
> **You can lose all of the money you trade, and in extreme conditions more.** Perpetual futures use leverage, so a small price move can liquidate a position. In a fast or illiquid market, a position can be closed at a price worse than its liquidation price. The loss can then exceed the margin, and your account balance can go negative, leaving you owing the difference to the exchange. Prices can also gap past a stop loss, and an exchange can fail to execute a stop.
>
> **AI decisions can be wrong.** Models make mistakes, misread data, and can be misled by false or manipulative news. The risk engine limits order size and leverage, but it cannot tell a bad decision from a good one.
>
> **Agents trade while you are away.** The backend keeps running when the interface is closed. On an armed real account, agents in automatic mode place orders without asking you. Stop them or disarm the account whenever you are not prepared to accept that.
>
> **The software can have bugs.** {Exchange status line} A bug in MochiPrep, OKX, your network, or an AI provider can cause orders you did not intend, or prevent the exits you did.
>
> **Simulated results are not predictive.** Paper trading, demo mode, and replay use simulated fills. They do not show what would happen with real money.
>
> **Keep your keys safe.** Use exchange keys with trade permission only, never withdrawal permission. Anyone with access to your computer while MochiPrep is unlocked may be able to use them.
>
> **Check your local rules.** Crypto derivatives are restricted or prohibited in some countries. You are responsible for making sure you may use them.
>
> **No warranty, no liability.** MochiPrep is provided "as is" under the GNU AGPL v3, without warranty of any kind. To the extent the law allows, its authors and contributors are not liable for any loss arising from its use.
>
> ☐ I understand that I can lose all of the funds I trade, and in extreme conditions more, including through AI errors or software bugs.
> ☐ I have read and accept this disclaimer. I am responsible for every order placed through MochiPrep.
