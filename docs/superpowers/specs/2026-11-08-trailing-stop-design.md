# Agent 移動停損（Trailing Stop）與可調整的保護單 設計

狀態：草稿，待擁有者核准。來源：擁有者朋友的 feature request，擁有者於 2026-11-08 確認範圍。

## 目的

讓 Agent 能替持倉設定移動停損：價格往有利方向走時停損跟著走，回撤到設定距離就平倉。擁有者同時要求停損、停利、移動停損都能在持倉期間調整。這樣 Agent 可以鎖住獲利，不必只靠固定停損或手動平倉。

## 擁有者已決定的事

- 適用所有帳戶：模擬（PAPER）、OKX Demo、OKX 實盤。
- 每個 Agent 的設定裡有一個開關「允許移動停損」。關閉時該 Agent 不能設定移動停損。
- 距離用百分比還是固定價差，由 Agent 每次自己選。
- 停損（SL）、停利（TP）、移動停損都可以調整：開倉時設定，持倉時也能新增、修改、移除。
- 移動停損是「疊加」在固定停損之上，不取代它（見下方）。
- Agent 可選擇設定啟動價（activation price），價格到了才開始追蹤。
- Node 後端與 Rust 模擬引擎一起做，並有對拍情境（parity scenarios）。
- 完成後繼續 Rust 移植工作（Plan 3）。

## 現況（2026-11-08 調查）

這次比「加一個欄位」大，因為目前完全沒有調整保護單的路徑：

- Agent 的輸出只有 `LONG | SHORT | CLOSE | HOLD`（`AiDecisionSchema`，`packages/shared/src/schemas.ts`）。Agent 沒辦法調整既有持倉的 SL/TP。
- 提示詞（`packages/agents/src/prompt.ts`）不會告訴模型目前持倉的 SL/TP 是多少。
- 模擬引擎有 `setProtection`，但只有 UI 路由 `PUT /api/positions/:accountId/:symbol/protection` 用得到，而且只限 PAPER。
- OKX 引擎只在進場時用 `attachAlgoOrds` 掛 SL/TP。沒有任何 algo 單的查詢、修改、取消。OKX 持倉畫面顯示的 SL/TP 其實只是本地進場單當初帶的值，不是交易所上的實際狀態。
- 風控引擎（`packages/core/src/risk/engine.ts`）用固定停損距離計算倉位大小，並檢查「停損在強平前觸發」。它不處理保護單調整。
- 模擬引擎的 SL/TP 存成 `orders` 表裡的 `STOP_MARKET` / `TAKE_PROFIT_MARKET` 子單。`positions` 沒有保護單欄位。

## 目標

1. Agent 開倉時可以帶移動停損（若該 Agent 開關開啟）。
2. Agent 可以用新的動作調整既有持倉的 SL、TP、移動停損。
3. 三種帳戶行為一致：OKX 上由交易所保管保護單，模擬帳戶用同樣的規則模擬。
4. Agent 和 UI 都看得到每個持倉目前真正生效的 SL、TP、移動停損（OKX 從交易所讀回）。
5. Rust 模擬引擎與風控引擎跟 Node 對拍一致。

## 非目標

- 不做部分平倉的保護單（保護單一律平掉整個持倉）。
- 不做多段停利、OCO 以外的進階單種。
- Rust 端不做 OKX 的部分。Rust 還沒有 OKX 交易，那屬於 Plan 5；這次只把要求寫進 Rust 計畫的延續表。
- 不改變「新倉位必須有固定停損」這條規則（`requireStopLoss`）的意義。

## 反目標（做了也算失敗的情況）

- 調整保護單的過程中，持倉有任何一刻在交易所上沒有停損。
- 移動停損在模擬帳戶用了沒觀察到的價格觸發，或用 OHLC 路徑推算。
- 介面顯示的停損和交易所上實際的不一樣，卻沒有告訴使用者。
- 對未知結果的 OKX 請求重送，造成重複的保護單。
- 在 OKX Demo 驗證之前，就宣稱實盤移動停損可用。

## 設計

### 移動停損的定義

每個持倉最多一張移動停損，參數：

- `kind`：`RATIO`（百分比）或 `SPREAD`（固定價差），二選一，Agent 決定。
- `callback`：`RATIO` 時是小數（`0.015` = 1.5%），`SPREAD` 時是價格距離（USDT）。
- `activationPrice`：可選。沒給就立刻開始追蹤。

追蹤規則跟 OKX 文件的公式一樣：

- 多單：追蹤啟動後的最高價 `H`，觸發價 = `H × (1 − ratio)` 或 `H − spread`。最新價 ≤ 觸發價時，以市價平倉。
- 空單：追蹤最低價 `L`，觸發價 = `L × (1 + ratio)` 或 `L + spread`，最新價 ≥ 觸發價時平倉。
- 啟動價：多單在最新價 ≥ 啟動價時開始追蹤，空單在 ≤ 時開始。（OKX 沒寫方向，這是我的判斷，要在 Demo 確認。）
- 觸發價只會往有利方向移動。

移動停損疊加在固定停損上。固定停損仍是風控計算倉位大小、檢查強平的依據。移動停損的觸發價比固定停損更靠近價格時，自然會先觸發。

### Agent 的輸出

`AiDecisionSchema` 增加：

- 新欄位 `trailingStop`：`null` 或 `{ kind: 'RATIO' | 'SPREAD', callback: 正數, activationPrice: 正數 | null }`。
- 新動作 `PROTECT`：調整既有持倉的保護單。用 `stopLoss`、`takeProfit`、`trailingStop` 三個欄位表達「調整後的完整狀態」：給值就是設定或修改，`null` 就是移除。三個都和目前一樣時，視為不必變更。

`LONG`/`SHORT` 也可以帶 `trailingStop`。`HOLD`/`CLOSE` 帶非 null 的 `trailingStop` 算無效輸出。

開關關閉時（我的判斷）：
- 已經掛著的移動停損保留並繼續運作。
- Agent 的 `PROTECT` 不能新增、修改或移除它。這種情況下 `trailingStop` 欄位不在 schema 裡，視為「不變」，所以模型不會誤刪。
- 使用者可以在介面手動移除。
- 開關只限制 Agent。使用者在介面上手動調整不受開關限制。

SIGNAL 模式（我的判斷）：
- `PROTECT` 建議會記下產生建議時看到的保護單狀態。
- 使用者接受時，如果目前的保護單和當時記下的不同，這個建議就作廢，標為過期，避免悄悄蓋掉之後的變更。
- 沒有不同的話，照常重新跑風控再執行。

提示詞：

- 系統提示詞說明 `PROTECT` 和 `trailingStop`。
- 只有開關開啟時，才告訴模型可以用 `trailingStop`。開關關閉時提示詞完全不提，免得模型去用。
- 使用者提示詞的持倉資訊加上目前生效的 `stopLoss`、`takeProfit`、`trailingStop`（含目前的觸發價）。

### 風控

`evaluateRisk` 增加：

- 開關關閉卻帶了 `trailingStop`：新檢查 `trailing-allowed` 失敗，結果 `RISK_REJECTED`，History 顯示原因。
- `trailing-stop` 檢查：`RATIO` 的 callback 在 `[0.001, 0.3]`、`SPREAD` 的 callback 至少一個 tick 且不超過參考價的 30%。啟動價要在有利的一側（多單 > 參考價，空單 < 參考價）。數值上下限是我的判斷，OKX 的實際範圍只出現在錯誤訊息裡，Demo 驗證時以 OKX 為準再調整。
- `PROTECT` 有自己的檢查路徑：要有持倉、行情新鮮、SL/TP 在正確的一側。另外：
  - 調整後必須還有固定停損，除非 `requireStopLoss` 關閉。
  - 新的固定停損不得超過強平價。
  - 不得放寬到讓每筆風險超過 `maxRiskPerTradePct`（以目前持倉數量和新停損距離計算）。
  - `PROTECT` 不改變倉位大小或槓桿。

### 模擬引擎（Node 與 Rust）

- 新的單型 `TRAILING_STOP_MARKET`，和 SL/TP 一樣存在 `orders` 表，`close_position = 1`、`reduce_only = 1`。
- 遷移 `008_trailing_stop.sql`：
  - `orders` 增加 `trailing_kind`、`trailing_callback`、`trailing_activation_price`、`trailing_extreme`（目前追蹤到的最高或最低價，未啟動時為 null）。
  - `agents` 增加 `allow_trailing_stop INTEGER NOT NULL DEFAULT 0`。預設關閉，既有 Agent 行為不變。
- `onQuote` 的順序：資金費、強平、固定停損、移動停損、停利、限價單。同一個報價上固定停損和移動停損都觸發時，由固定停損平倉，移動停損取消。（我的判斷：比較保守，和「SL 優先於 TP」一致。）
- 每個報價先更新 `trailing_extreme`（用 `quote.last`），再判斷觸發。觸發後跟 SL 一樣，以那個報價的 bid/ask 加滑價成交（taker）。不會用觸發價成交，不會補沒觀察到的價格。
- `setProtection` 擴充為同時處理移動停損。它仍然是先取消再重建，在單一 store transaction 內完成。
- 進場單可以帶 `attachedTrailingStop`，成交後和 SL/TP 一起掛上。
- `PositionView` 增加 `trailingStop`（參數加目前的觸發價）。
- 計算全部用 `D` / `Dec`。觸發價四捨五入到 tick 的方式要記錄在 `docs/PAPER_ENGINE.md`（我的判斷：多單觸發價向下取 tick，空單向上取 tick，對交易者不利的方向）。

### OKX 引擎

- 進場：`attachAlgoOrds` 可以同時帶 SL/TP 和移動停損（OKX 2026-04-13 起支援）。帶不帶得進同一個元素，Demo 驗證；不行就分成兩個元素。
- 新增 `OkxClient` 方法：查詢掛著的 algo 單（`orders-algo-pending`，`conditional`、`oco`、`move_order_stop`）、下 algo 單（`order-algo`）、修改（`amend-algos`，只能改 SL/TP 這類條件單）、取消（`cancel-algos`）、查歷史（`orders-algo-history`）。
- 每張保護單都帶自己產生的 `algoClOrdId`，未知結果時用它查詢，絕不重送。這和現有「未知結果靠查詢解決」的規則一樣。
- 調整保護單：
  - SL/TP：用 `amend-algos` 修改。
  - 移動停損：OKX 不能修改，所以「先下新的、確認生效後再取消舊的」。過程中最多短暫有兩張，兩張都是 reduce-only，不會造成反向開倉。永遠不會先取消再下單，所以不會有沒停損的時刻。
  - 新單失敗：舊單保留，結果記為 `ORDER_REJECTED`，附原因。
- 持倉畫面和 Agent 提示詞的 SL/TP/移動停損，改為從交易所查到的 algo 單讀取，和現有的同步一起更新。
- 每個 OKX 請求都經過 `preTradeRefusal`。保護單調整分兩類（我的判斷，從嚴）：
  - **只會降低風險，不需要 armed**（比照「出場永遠可以」）：
    - 新增或收緊固定停損
    - 新增移動停損
    - 收緊移動停損的 callback
    - 新的移動停損比舊的更緊時，替換它
  - **其他一律需要 armed，並受即時限制約束**：
    - 放寬或移除固定停損
    - 新增、修改、移除停利
    - 放寬或移除移動停損
    - 修改啟動價
  - 判斷不出屬於哪一類時，當作需要 armed。
- 哪些 algo 單算「這個持倉的保護單」（我的判斷）：
  - MochiPrep 只管理自己建立的單：帶 MochiPrep `algoClOrdId` 前綴的，以及進場時附掛、用 `attachAlgoClOrdId` 標記的。
  - 使用者在 OKX App 自己掛的單（外部單）：顯示在持倉畫面並標示「外部」，MochiPrep 永遠不修改、不取消。
  - 提示詞也會告訴 Agent 有外部單存在。
  - 舊版本建立、沒有標記的附掛單：一律視為外部單。
- 同一帳戶同一商品只有一個持倉。不管是哪個 Agent 或手動調整，最後一次調整生效。
- SL/TP 和移動停損一樣，改用「先下新的、確認生效後再取消舊的」。這樣不必依賴 `amend-algos` 能不能拆 OCO，也保證任何時刻都有停損。過程中短暫有兩張 reduce-only 的單，可以接受。
- 持倉被其他單平掉後，剩下的保護單要取消（OKX 的行為文件沒寫，Demo 驗證；不會自動取消的話由同步程式取消）。

### 介面

- Agent 設定頁：「允許移動停損」開關，放在風控設定旁邊。預設關閉。
- 持倉列表：顯示移動停損參數與目前觸發價。
- 手動調整保護單的對話框：增加移動停損欄位，並開放給 OKX 持倉（見下方擁有者的回答）。
- History：`PROTECT` 決策顯示調整前後的值。

### Rust

- `mochi-domain`：`AiAction::Protect`、`TrailingStop` 型別、`OrderType::TrailingStopMarket`、新欄位。
- `mochi-core`：風控新檢查（`risk.json` 金樣本擴充）、模擬引擎的移動停損、`set_protection` 擴充。
- `mochi-store`：遷移 008 透過現有 `include_str!` 自動帶入，`SqlitePaperStore` 讀寫新欄位。
- 對拍情境至少包括：
  - 啟動前後
  - 跳空穿越
  - 和固定停損同時觸發
  - 調整與移除
  - 開關關閉被拒
  - RATIO 與 SPREAD 各一
  - 多空各一
  - 限價進場帶移動停損
  - 資料庫重開後恢復追蹤狀態
- OKX 部分寫進 Plan 5 的延續表。

## 驗證

- Node：vitest 單元測試，包括風控、模擬引擎、解析、OKX 引擎（用假的 OKX 回應）。另外跑 typecheck、e2e。
- Rust：新情境、金樣本、`cargo test`、clippy。
- OKX Demo：上線前要在 Demo 帳戶實測以下 UNCONFIRMED 項目：
  - callback 範圍
  - 啟動價方向
  - attach 能否和 SL/TP 同一元素
  - `cancel-algos` 能否取消移動停損
  - 持倉關閉後 algo 單的去向
  - 追蹤用的是哪種價格
- 本地合併前跑 CodeRabbit CLI。

## 留給實作者決定

- 檔案切分、函式命名。
- 提示詞的具體措辭（要符合上面的規則）。
- `PROTECT` 決策在 History 的版面。

## 擁有者對待決事項的回答（2026-11-08）

- 範圍：全部做。包括移動停損、新的 `PROTECT` 動作、OKX algo 單的查詢、下單、取消與同步，三種帳戶都要。
- OKX 實盤：程式不封鎖。擁有者會先在 Demo 帳戶自己測試上面列的 UNCONFIRMED 項目。
  - 在測試結果出來前，文件（`docs/OKX_TRADING.md`）和介面不得宣稱實盤移動停損已驗證。
  - 文件要列出這些未驗證的項目。
- 手動調整對話框：開放給 OKX 持倉，套用和 Agent 相同的 armed 規則。取代現在的「Change stops on OKX」拒絕訊息。
