# 交易所模組化、子帳戶與新交易所（Bybit、Binance、Pionex）設計

狀態：草稿，待擁有者核准。擁有者的需求（2026-11-27）：「把交易所支援變成模組化 需要哪些裝哪些，加入支援子帳戶，加入支援幣安 Pionex ByBit」。

## 目的

現在 OKX 是寫死在程式裡的唯一交易所。擁有者希望：

1. 交易所變成可以個別安裝的模組，只裝自己要用的。
2. 支援子帳戶。
3. 新增 Bybit、Binance、Pionex，產品和現在一樣只做 USDT 本位永續合約。

AGENTS.md 的「不可彎曲的規則」對每個交易所都適用，而且因為擁有者允許第三方外掛，這些規則必須由核心執行，不能交給外掛。

## 擁有者已決定的事（2026-11-27）

- **模組化的形式：執行期外掛。** 每個交易所是一個獨立的外掛程式（子行程），伺服器啟動它、用 stdin/stdout 傳 JSON 溝通。不用 WASM，不用 DLL。
- **第三方外掛也可以裝。** 官方外掛用 MochiPrep 的發行金鑰簽章；第三方外掛第一次使用時要擁有者核准。
- **OKX 在第一階段就變成外掛。**
- **子帳戶：每個子帳戶用它自己的金鑰連線，介面上依主帳戶分組，Agent 選要交易哪一個。** 不用主帳戶金鑰管理或代下單（三家交易所都不允許主帳戶金鑰替子帳戶下單；Bybit 管理子帳戶金鑰需要有轉帳或提幣權限的主金鑰，違反「可提幣的金鑰一律拒絕」）。
- **Pionex：縮減版支援。** 沒有交易所端的停損停利，由程式自己盯價格，只在程式開著時有效；擁有者手動確認金鑰沒有轉帳或提幣權限。
- **推出順序，每一步一個 PR 到 `dev`：** 抽象層（OKX 行為不變）→ Bybit → Binance → 子帳戶分組 → Pionex。
- **行情來自各自的交易所。** 外掛也提供公開報價與合約清單；每個帳戶的 Agent、風控、停損都用它自己交易所的價格與規格。模擬帳戶維持用 OKX。
- **只拒絕可提幣的金鑰**，和 OKX 現在一樣。有轉帳權限（子帳戶劃轉、萬向劃轉）的金鑰可以連線；連線畫面要提醒這類金鑰能把錢轉出這個帳戶。
- **OKX 外掛在升級時自動安裝。** 安裝包不附任何外掛；升級後若已有 OKX 帳戶，自動下載官方 OKX 外掛。

## 目標、非目標、反目標

目標：
- 交易所程式碼不再編進伺服器；伺服器只認得「外掛協定」。
- 每個外掛都有自己的假交易所（fake）供測試。
- 現有所有測試與 `crates/golden` 的固定資料繼續通過；OKX 的行為、錯誤文字、資料庫內容都不變。

非目標：
- 現貨、幣本位合約、期權、網格機器人。
- 主帳戶金鑰的子帳戶管理（建立子帳戶、劃轉、建立子帳戶金鑰）。
- 外掛市集或評分系統。

反目標（就算能動也算失敗）：
- 任何安全規則（武裝、下單前拒絕、未知結果不重送、帳本、拒絕可提幣金鑰）依賴外掛自律。
- 外掛拿得到主密碼、資料金鑰、AI 金鑰，或其他交易所的金鑰。
- 未簽章或簽章不符的外掛被執行。
- 把 Pionex 的程式端停損說成和交易所端停損一樣可靠。

## 現況（2026-11-27 調查）

- `ExecutionEngine`（`crates/mochi-server/src/runtime/execution.rs:55`）本身已經不綁定交易所。綁定 OKX 的是：`ExecutionTarget::Okx(Arc<OkxEngine>)`、`ExchangeSession`（`engine: Arc<OkxEngine>`、三段式 `secrets`）、`OkxOptions`、`okx_venue`、`connect_okx` 與 `check_key`（`runtime/exchange.rs`）。
- `AccountVenue { Paper, OkxDemo, OkxLive }`（`crates/mochi-domain/src/accounting.rs:164`）存在 SQLite 裡，也出現在網頁 API。
- `OkxEngine`（`crates/mochi-okx/src/engine/`，約 6,400 行）同時包含兩種東西：
  - 不分交易所的邏輯：下單前拒絕（`refusal.rs`）、未知結果的查詢解決、保護單對帳（`protection.rs`）、同步狀態、帳本與 store 寫入（`StoreAccess`/`StoreWork`）。
  - OKX 的 REST 呼叫：引擎只透過約 13 個 `OkxClient` 方法碰 OKX（持倉、餘額、下單、依 clOrdId 查單、撤單、algo 掛單/查詢/撤銷、成交、持倉歷史、槓桿、帳戶設定、合約規格）。
- PR #26（多幣對 Agent、OKX arena、拒絕後重問、持倉變動觸發）已合併進 `dev`。核心引擎的介面必須涵蓋 `runtime/position_watch.rs` 與 `runtime/exchange_sync.rs` 今天用到的部分：`engine.positions()`、`sync`、`spawn_position_history`、`store`，以及 `prepare_exchange_sync` 回傳的 `(pass, sessions)` 與 `after_sync(pass)`。`execute_decision` 記錄 Agent 自己訂單前後狀態的邏輯留在核心。
- 帳戶以 (venue, exchange_uid) 為鍵，所以現在就能同時連好幾個 OKX 帳戶；OKX 子帳戶的金鑰就是另一個 uid、另一個帳戶。缺的是分組與標示。

### 各交易所 API（依官方文件，均未實測）

| | OKX | Bybit V5 | Binance USDT-M | Pionex |
|---|---|---|---|---|
| 模擬環境 | Demo Trading | Demo Trading | `demo-fapi.binance.com` | 無 |
| 查金鑰權限（能否提幣） | `account/config` 的 `perm` | `/v5/user/query-api` | `/sapi/v1/account/apiRestrictions` | **無** |
| 交易所端停損停利 | algo 單 | 有 | 條件單 | **無文件** |
| 依自訂單號查單 | 有 | 有 | 有 | 有，但「只限一段時間內」 |
| 子帳戶 | 子帳戶自己的金鑰 | 子帳戶自己的金鑰 | 子帳戶自己的金鑰 | 只有機構版（內部） |
| 合約規格 | ctVal、lotSz | 有 | 有 | 有 step/最小量，**沒有合約面值欄位** |

另外，有第三方專案回報 Pionex 用一般金鑰下永續單會回 `TRADE_INVALID_SYMBOL`，尚未證實。

## 考慮過的做法

1. **獨立外掛程式（採用）。** 每個交易所一個簽章過的執行檔，伺服器以子行程啟動，JSON 透過 stdin/stdout。安全規則全在核心。外掛當掉不影響伺服器與模擬交易。代價：每個執行中的外掛約多 5–10 MB 記憶體；外掛會拿到該交易所的 API 密鑰（它要簽請求）。
2. **WebAssembly 模組。** 沙盒最強，連網與簽章都由宿主做，外掛看不到密鑰。代價：wasmtime 約多 15–20 MB 與大量依賴，OKX 引擎的非同步與計時要大改。擁有者選了 1。
3. **原生動態函式庫。** 沒有隔離，被竄改的 DLL 讀得到金庫記憶體，Rust 也沒有穩定 ABI。排除。

## 設計

### 1. 核心與外掛的分界

把 `OkxEngine` 拆成兩半：

- **核心交易引擎**（新 crate `mochi-exchange`）：`mochi-okx/src/engine/` 裡不分交易所的部分搬過來，改成依賴一個 `VenueAdapter` trait，不再依賴 `OkxClient`。包含：下單前拒絕、未知結果查詢、保護單對帳、同步、`StoreAccess`、武裝判斷、帳本寫入。
- **`VenueAdapter`**：引擎需要的交易所操作，大致對應現在那 13 個 `OkxClient` 方法，但參數與回傳改成中性型別（`VenuePosition`、`VenueOrder`、`VenueFill`、`VenueBalance`、`VenueAlgo`、`ContractSpec`、`KeyCheck`），不再是 OKX 的 `Row`。
- **外掛**只做 `VenueAdapter`：把中性型別翻成該交易所的 REST 呼叫並簽章。外掛不做任何判斷，不寫資料庫，不知道武裝狀態。

這樣第三方外掛就算寫錯或惡意，也不能繞過下單前拒絕、不能在未知結果後重送（重送與否由核心決定），也不能動帳本。它能做壞事的範圍是：拿著金鑰在交易所上亂下單、或對核心謊報狀態。這點必須在安裝畫面講清楚（見第 4 節）。

**能力宣告（capabilities）**：外掛在握手時宣告自己支援什麼，核心依此調整，而不是假設：
- `native_protection`：交易所端停損/停利/移動停損各自是否支援。不支援時，該種保護單被拒絕；只有 Pionex 這種完全沒有交易所端保護的外掛，才改用「程式端保護」（第 7 節）。
- `key_permissions`：能不能查金鑰權限。不能時走第 7 節的手動確認。
- `demo`：有沒有模擬環境。
- `client_order_lookup_window`：依自訂單號查單的有效時間（Pionex 有限制）。核心在這時間內解決未知結果，超過就把該單標成需要人工處理，並停止該帳戶開新倉，**絕不重送**。

### 2. 外掛協定

- 新 crate `mochi-plugin-protocol`：訊息型別與版本號，核心與外掛共用。外掛作者用 Rust 寫時直接依賴它；其他語言照 JSON schema 實作（schema 隨 crate 產生並放進 `docs/`）。
- 傳輸：每行一個 JSON（JSON Lines），請求帶 `id`，回應帶同一個 `id`。stderr 是外掛的日誌，核心讀進來前先過一次密鑰遮罩（`runtime/redact.rs`），再寫進日誌。
- 握手：`hello { protocol_version }` → 外掛回 `manifest`（id、名稱、版本、環境清單、需要的憑證欄位如 key/secret/passphrase、capabilities、會連的網域）。協定主版本不符就拒絕。
- 一個交易所一個子行程，同一交易所的所有帳戶共用（`open_session { account_ref, environment, credentials }` 之後的呼叫都帶 `session`）。這樣連五個 Bybit 子帳戶也只多一個行程。
- 憑證只透過 stdin 傳給外掛，外掛只放記憶體。核心這邊照舊：只在 `Runtime` 記憶體，或擁有者選擇時用主密碼封存。
- 逾時：每個請求有逾時。下單請求逾時視為「結果未知」，走既有的查詢解決流程。外掛行程掛掉也一樣：所有進行中的下單都當成未知，重啟外掛後先查詢。
- 金額、價格、數量在協定裡一律是十進位字串，兩端都用 `mochi_decimal::Dec` 解析，不經過浮點數。

### 2b. 行情與合約規格（每個交易所各自一份）

- 協定除了交易呼叫，還有公開行情：`list_instruments`（合約規格：面值、數量步進、價格步進、最小量、最大槓桿）、`subscribe_quotes` / 報價推送、K 線歷史（Agent 提示詞用的指標要用）。公開行情不需要金鑰，外掛裝好、啟用、且至少有一個該交易所的帳戶或 Agent 用到時才啟動。
- 合約登錄表（`crates/mochi-domain/src/registry.rs`）改成「每個交易所一份」，鍵是 (交易所, 幣對)。`InstrumentSpecification` 裡 OKX 專用的 `venue_contract_value` / `venue_lot_size` 改成中性欄位；OKX 的序列化與 golden 不變（OKX 那份照舊叫這兩個名字）。
- 每個帳戶的 Agent 提示詞資料、風控的報價檢查（報價年齡等）、下單前拒絕、程式端停損，都讀**該帳戶交易所**的報價。模擬帳戶與回放（replay）維持用 OKX 公開行情，`DEFAULT_SYMBOLS` 不變。
- Agent 選的幣對在該帳戶交易所不存在時：Agent 設定頁直接不讓選；已存在的設定在那個幣對上跳過並記錄原因，不改用別家的價格。
- 現在的 `OkxPublicFeed`（`crates/mochi-market/src/okx_public.rs`）在第 1b 階段搬進 OKX 外掛；模擬帳戶的 OKX 行情改由 OKX 外掛提供。沒有裝 OKX 外掛時，模擬帳戶只能用回放。（這是我的判斷：要嘛 OKX 行情留在核心當成內建，要嘛一致外掛化；擁有者選了 OKX 也外掛化，所以一致處理。）
- `MOCHIPREP_OFFLINE=1`：不啟動任何外掛的公開行情，也不自動下載外掛；回放照舊。
- AGENTS.md 的「Contract specs come from OKX」改成「合約規格來自該帳戶的交易所，絕不猜」。

### 3. 外掛的安裝、簽章與信任

- 安裝位置：資料夾下的 `plugins/<id>/<version>/`，裡面有執行檔、`manifest.json`、簽章檔。
- **官方外掛**：用 MochiPrep 發行金鑰（ed25519，和更新程式同一把，公鑰編在程式裡）簽「執行檔 + manifest」的雜湊。`release.yml` 另外產生一份簽章過的 `plugins.json` 目錄，列出每個官方外掛每個平台的下載網址與雜湊。設定頁的「交易所」分頁讀這份目錄，擁有者按「安裝」才下載。
- **第三方外掛**：擁有者從檔案安裝（不從網頁連結自動安裝）。外掛必須帶作者自己的 ed25519 簽章。第一次安裝時顯示作者公鑰指紋與警告（下方），擁有者打字確認後才記下這把公鑰；之後同一外掛的更新必須是同一把公鑰簽的，換鑰匙就當成新外掛重新核准。
- 每次啟動外掛前都重新驗雜湊與簽章，不符就不啟動，並在設定頁顯示原因。
- 第三方外掛的警告內容（大意）：「這個外掛不是 MochiPrep 做的。它會拿到你這個交易所的 API 金鑰，可以用它下任何單，也可能謊報持倉或金鑰權限。MochiPrep 的風控只能管它『透過 MochiPrep』下的單。只用你信任的外掛，並使用沒有提幣權限、資金有限的子帳戶。」
- 移除外掛：先斷線並解除武裝該交易所的所有帳戶，再刪檔。帳戶與歷史紀錄保留，重新安裝後可再連線。
- **升級時的 OKX**（擁有者選擇自動安裝）：第 1b 階段後第一次啟動，若資料庫裡有 OKX 帳戶而 OKX 外掛沒裝，就從 `plugins.json` 下載官方 OKX 外掛、驗簽章、安裝，然後照舊解鎖後重新連線已存的金鑰。下載失敗（離線、GitHub 無法連線、`MOCHIPREP_OFFLINE=1`）時：OKX 帳戶顯示「需要 OKX 外掛」，設定頁提供重試與從檔案安裝。**這段期間交易所上的持倉無法透過 MochiPrep 平倉**，交易所端已掛的停損停利仍有效；畫面與 `docs/EXCHANGES.md` 要明講，請擁有者必要時到交易所網站平倉。
- **外掛當掉**：核心自動重啟（有次數上限與退避），重啟後先查詢所有未知結果的訂單，再恢復同步。重啟期間同樣無法平倉，畫面顯示原因。

### 4. `AccountVenue` 與資料庫

- `AccountVenue` 從固定列舉改成 `Paper` 或 `Exchange { exchange: ExchangeId, environment: Demo | Live }`。`ExchangeId` 是外掛 manifest 的 id（例如 `okx`、`bybit`）。
- 序列化格式維持 `"<EXCHANGE>_<ENV>"`，所以既有的 `OKX_DEMO`、`OKX_LIVE` 讀寫完全不變，golden 固定資料不用動；新的值會是 `BYBIT_DEMO`、`BINANCE_LIVE` 等。
- 新 migration `019_exchange_plugins.sql`（dev 已到 018）：帳戶加 `master_uid`（可空）與 `group_label`（可空）；新表 `exchange_plugins`（id、版本、publisher 公鑰、是否官方、啟用與否、核准時間）。
- 帳戶的交易所沒裝外掛時：帳戶照樣顯示、歷史照樣可看，但不能連線也不能交易，畫面上說明要先裝哪個外掛。

### 5. 連線檢查

`check_key` 變成核心的通用流程，外掛只回報事實：
1. 外掛回 `KeyCheck { uid, master_uid?, permissions: Known{trade, withdraw, transfer} | Unknown, position_mode, account_mode_ok, contract_specs }`。
2. 核心決定：可提幣 → 拒絕（擁有者決定只拒絕提幣；有轉帳權限的金鑰可以連線，但連線畫面顯示警告「這把金鑰能把資金轉出這個帳戶」）；沒交易權限 → 拒絕；不是單向持倉 → 拒絕；合約規格和登錄表不符 → 拒絕（文字沿用 OKX 現有的，只把「OKX」換成交易所名稱，OKX 的文字逐字不變）。
3. `permissions: Unknown` 只在外掛宣告不能查權限時接受，並走第 7 節的手動確認。
4. 重新連線（解鎖後）照舊跑完整檢查且不武裝；武裝永遠要打字確認。

### 6. 子帳戶

- 每個子帳戶各自用自己的金鑰連線，就是一個獨立帳戶（現在 OKX 已經這樣運作）。
- 外掛若查得到主帳戶 uid（OKX `account/config` 的 `mainUid`、Bybit `query-api` 回傳的主帳戶資訊等），就填 `master_uid`；查不到時擁有者可以手動填分組名稱。
- 設定頁與帳戶選單依「交易所 → 主帳戶 → 子帳戶」分組顯示。Agent 設定裡選帳戶時也分組。
- 武裝、風控限制、每日虧損都是每個子帳戶各自算，不合併。
- 不做：主帳戶金鑰、劃轉、建立子帳戶。

### 7. Pionex（縮減版）

依現有文件，Pionex 缺三樣東西，設計上這樣處理：
- **沒有交易所端停損停利** → 「程式端保護」：核心用即時報價盯價格，觸發就送 reduce-only 市價單。只在 MochiPrep 開著且報價正常時有效。UI、Agent 提示詞與文件都要明講這點，持倉畫面標示「程式端停損，程式關閉時不會觸發」。
- **查不到金鑰權限** → 連線時要擁有者打字確認「這把金鑰沒有提幣與轉帳權限」。這和 AGENTS.md「可提幣的金鑰一律拒絕」不同：程式無法驗證。AGENTS.md 會補一句：交易所查不到權限時，以擁有者的打字確認代替，並在帳戶上永久標示「權限未經驗證」。
- **沒有模擬環境** → 只有實盤；只能對假交易所測試。設定頁連線時再警告一次。
- **合約面值**：文件的 `size` 看起來是以幣計量。外掛必須從交易所文件或規格確認，確認不了就拒絕那個交易對，不猜。
- **一般金鑰能否下永續單**：寫任何 Pionex 交易程式碼前，先由擁有者用一把有交易權限、沒有提幣與轉帳權限的金鑰，在只放小額資金的帳戶上實測確認（或 Pionex 官方回覆）。若確認不能，Pionex 這一階段就停，回報擁有者。

### 8. 階段

每個階段：CodeRabbit 審查、驗證、commit、push、開 PR 到 `dev`，不自行合併。

- **第 1a 階段：核心引擎抽出（OKX 仍在行程內）。** 新增 `mochi-exchange`（核心引擎 + `VenueAdapter` + 中性型別），`mochi-okx` 只剩 client 與 `OkxAdapter`；`AccountVenue` 一般化；migration 019。OKX 行為、錯誤文字、golden 全部不變，所有測試通過。
- **第 1b 階段：外掛化。** `mochi-plugin-protocol`、伺服器端的外掛宿主（啟動、握手、逾時、當掉處理、簽章驗證、安裝/移除、第三方核准）、`mochi-exchange-okx` 外掛執行檔（含搬進去的 OKX 公開行情）、每個交易所一份的合約登錄表、升級時自動安裝 OKX 外掛、`release.yml` 建置與簽章外掛並產生 `plugins.json`、設定頁「交易所」分頁、`docs/OKX_TRADING.md` 改寫成 `docs/EXCHANGES.md` 加各交易所附錄、AGENTS.md 更新路徑。OKX 的假交易所搬進外掛（`fake` feature 時外掛可指向測試用的假端點）。
- **第 2 階段：Bybit 外掛**，含自己的假交易所。
- **第 3 階段：Binance 外掛**，含自己的假交易所。
- **第 4 階段：子帳戶分組**（UI 分組、`master_uid` 回報、手動分組名稱）。
- **第 5 階段：Pionex 外掛**（縮減版，含程式端保護與手動權限確認），先完成第 7 節的實測確認。

和 OKX 一樣，新交易所都只對假交易所測過，文件的「未確認」清單每個交易所各一份。

## 留給實作者決定的事（標示為我的決定，擁有者可推翻）

- crate 名稱（`mochi-exchange`、`mochi-plugin-protocol`、`mochi-exchange-<id>`）。
- 第一次開設定頁時列出官方外掛讓擁有者選。離線使用者可從檔案安裝官方外掛。
- manifest 裡「會連的網域」只用來顯示給擁有者看，核心不強制（跨平台限制子行程網路需要代理全部 HTTP 或各 OS 沙盒，工程太大；擁有者已接受外掛能濫用金鑰）。第三方外掛的警告會明講「MochiPrep 無法限制它連到哪裡」。
- OKX 專用的表（`okx_close_intents`、`okx_close_selections` 等，migration 012、013）與 OKX 的封存憑證格式都**不動**，維持和 golden 相容；新交易所用新的通用表（以 `exchange` 欄位區分）與新的封存格式（外掛宣告的憑證欄位的 JSON），放在舊的旁邊。
- 帳戶與保證金模式：每個新交易所只接受和現在 OKX 路徑相同的模式（單向持倉、與 OKX 相同的全倉/逐倉設定，以便風控的保證金與強平計算不變），其他模式在連線檢查拒絕並說明怎麼改。Bybit 的 Unified/Classic、Binance 的多資產模式與統一帳戶，在各自階段查清楚後寫進該階段的計畫。
- 程式端保護只用在 Pionex。Bybit、Binance 若不支援某種交易所端保護（例如移動停損），就拒絕那種保護單，不退回程式端保護。
- 外掛子行程只要還有任何私有連線，或任何公開資料使用者（模擬帳戶、Agent、使用其報價的圖表），就保持執行；兩者都沒有時才關閉，以符合記憶體門檻。
- 序列化沿用 `<EXCHANGE>_<ENV>` 字串，不改 API 形狀。
- 中性型別的確切欄位、`VenueAdapter` 的方法切分、逾時秒數。
- 第三方外掛的作者簽章工具：附一個 `mochi-plugin sign` 子指令或文件說明用 minisign。
