# 分工：Codex 任務單

分工原則是按檔案切開，兩邊不會改到同一批檔案。

| 負責 | 範圍 |
| --- | --- |
| Codex | `packages/*/test/**`、`apps/server/test/**`、`docs/**`、`README.md` |
| Claude Code | `apps/web/**`、`e2e/**`、最終驗收 |

Codex 的工作分支：`codex/tests-docs`，完成後開 PR 到 `main`，不要直接 push `main`。

## 目前狀態

後端和前端都已實作完成，但**單元與整合測試仍是 0 個**，這正是任務 1、2 要補的。已驗證過的：

- 全部 package 型別檢查、`pnpm build`、Playwright E2E（`pnpm test:e2e`，需要本機安裝 Chrome）通過，並在全新目錄從 `pnpm install` 重跑過。
- E2E 走完 Demo → 行情 → agent → 風控核准與拒絕 → 手動下單 → 部位 → PnL → 平倉 → Demo reset。
- 手動驗證：重啟後資料與 replay 時間點保留、agent 變 PAUSED；OKX 即時行情模擬下單與重複單防護；用本地假 OpenAI 相容伺服器走完 BYOK；金鑰未寫入 SQLite。
- 這些手動驗證都還沒變成自動化測試。

只手動走過或完全沒測過的路徑，不要因為程式碼看起來合理就當作正確：限價單成交、部位翻轉、強平、資金費邊界、OKX 斷線重連、Anthropic 與 Gemini 的真實 API。

驗證過程已修掉、請補回歸測試的 bug：

- `mock-reckless-v1` 用分鐘編號取餘數，15 分鐘排程永遠落在同一分支，改用週期編號。
- SSE 長連線讓 `app.close()` 卡住，後端無法正常關閉。現在由 `preClose` 結束串流。

## 修 bug 的規則

測試是為了找出 bug，不是為了配合現有行為。

1. 測試結果和 `docs/PAPER_ENGINE.md` 的規則或一般永續合約的數學不符時，就是 bug。先寫失敗的測試，再修 `src`。
2. 可以修 `packages/*/src` 和 `apps/server/src`，一個 bug 一個 commit，訊息寫清楚錯在哪。
3. **不要改 API 契約**：`packages/shared/src/domain.ts`、`schemas.ts`、`apps/server/src/app.ts` 的路由和回傳形狀。前端正依賴這些。真的需要改時，先在 PR 或對話裡提出。
4. 判斷不了是 bug 還是設計時，不要改，列在 PR 描述裡。

## 任務 1：交易引擎測試

檔案：`packages/core/test/*.test.ts`（純函式）、`apps/server/test/paper-engine.test.ts`（引擎加真實 SQLite）。

必須涵蓋：

- 多單、空單 PnL；槓桿與保證金計算；部分平倉（保證金按比例釋放、剩餘部位的強平價更新）
- 手續費：taker 與 maker 費率、進出場都收
- 資金費：多空方向、正負費率、邊界只收一次
- 止損與止盈觸發；同一個 quote 同時觸發時止損優先；跳空時用下一個 quote 成交
- 強平：用 mark price；損失上限等於隔離保證金，跳空穿價也一樣；強平後取消該部位的 reduce-only 單
- 重複下單：同一個 `clientOrderId` 回傳原單、`duplicate: true`，不重複成交
- 保證金不足：拒單，且交易完全回滾，ledger 無殘留，只留下 REJECTED 訂單
- 限價單：可立即成交時走 taker 且不差於限價；掛單保留保證金；取消後釋放；成交時走 maker
- reduce-only：數量裁切到持倉量；無持倉時拒單
- 部位翻轉：平倉和開倉兩段的損益、手續費、保證金都正確
- ledger 一致性：跑一段隨機但固定種子的交易序列後，`auditLedger` 必須為空
- 風控：`evaluateRisk` 每個 check 的通過與失敗、部位大小換算、止損在強平價之後時拒單

## 任務 2：Agent、資料源、復原測試

檔案：`apps/server/test/*.test.ts`、`packages/market-data/test/*.test.ts`、`packages/agents/test/*.test.ts`、`packages/ai-providers/test/*.test.ts`。

必須涵蓋：

- 無效 AI 回應被拒：非 JSON、多餘欄位、缺欄位、信心值超界、LIMIT 沒有價格
- AI 請求失敗時不開倉；連續 3 次失敗或金鑰錯誤時 agent 自動暫停
- 過期行情被拒（無持倉時不呼叫 AI）
- 同一 agent 的週期不重疊：`runCycle` 進行中再呼叫回傳 `null`
- `Analyze Now` 永遠不下單；模型思考途中按 Stop，排程週期不下單
- 重啟復原：用**檔案型** DB 關閉後重開，帳戶、部位、掛單、replay 游標都還在；agent 變 PAUSED；金鑰消失
- Demo reset：同一段 replay 跑兩次，每個帳戶的權益和成交序列完全相同（id 是隨機的，比數值）
- Replay tape 決定性：固定 hash，避免 generator 不小心被改動
- OKX：`parseOkxCandles`、`buildQuote`、`handleMessage`；用假 WebSocket 測斷線重連與 REST 備援；缺 mark price 時 `markEstimated` 為 true
- `redactSecrets`：金鑰不會出現在錯誤訊息、`rawResponse`、API 回應中；SQLite 檔案內搜不到金鑰
- API 整合：用 `app.inject` 走完 行情 → agent 決策 → 風控 → 下單 → 部位 → PnL

## 任務 3：文件

- `README.md`：定位、截圖留位置、先決條件、`pnpm install` 和 `pnpm dev`、Demo Mode、BYOK、限制
- `docs/ARCHITECTURE.md`：package 邊界、agent 週期、replay 時鐘（tape 會等 agent 做完）、安全模型
- `docs/PAPER_ENGINE.md`：成交模型、滑價、強平、資金費、ledger。必須依程式碼的實際行為寫，並明寫「模擬強平不保證與交易所規則相同」
- `docs/ROADMAP.md`：milestone 順序是 OKX live adapter 第一，之後才是 Bybit、Binance 等其他交易所，再來是 Tauri、FX/CFD。列出 live 上線前的條件：預設關閉、禁止提領權限、下單前必過風控、防重複送單、斷線對帳

先寫 `docs/PAPER_ENGINE.md`，任務 1 要用它當規格。UI 細節等前端完成後由 Claude Code 補。

## 測試提示

```ts
import { openDatabase } from '../src/db/database';
import { Runtime } from '../src/runtime';
import { buildApp } from '../src/app';

const db = openDatabase(':memory:');
const runtime = new Runtime({ db, live: fakeFeed, providerFactory: () => fakeProvider });
```

- 引擎測試：`new SqliteStore(db)` 加 `PaperExecutionEngine`，用自己的 Map 提供 `getQuote` 和 `now`。
- `Runtime` 沒呼叫 `start()` 就不會連網。測試不可依賴真實網路。
- `replayMinutes` 必須大於 5,760（`WARMUP_MINUTES`）。
- `app.inject` 要帶 `headers: { host: 'localhost:8787', authorization: ... }`，否則會被 Host 檢查擋成 403。
- 用 `stepReplay(n)` 推進時間，不要用 `setPlaying`，後者依賴真實計時器。

## 完成標準

`pnpm test` 全綠、不需網路、也不需 API key。PR 描述列出修了哪些 bug，以及尚未解決的疑點。
