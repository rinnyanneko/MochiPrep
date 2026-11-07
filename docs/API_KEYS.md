# 取得 API Key 教學

這份教學帶你申請 MochiPrep 用得到的金鑰。Demo Mode 不需要任何金鑰，想用真實 AI 或連接 OKX 時再來申請就好。

> **DYOR：請自行研究。** MochiPrep 不構成財務建議。AI 輸出可能出錯，你要為自己的交易負責。

各家後台的選單會改版。如果畫面和這裡不同，以官方頁面為準。

## 先看這裡：金鑰存在哪

| 金鑰 | 存放位置 | 重啟後 |
| --- | --- | --- |
| AI provider | 本機 SQLite，**未加密** | 保留 |
| OKX | 只在後端記憶體 | 要重新輸入 |

- 金鑰等同密碼，不要貼到聊天室、截圖或 commit 進 git。
- AI key 明文存在 `.mochiprep/`，不要把這個資料夾放進 iCloud、Dropbox 這類同步資料夾。
- 金鑰外流時，立刻到官方後台刪除，再重新建立。

## AI Provider

大多數金鑰**只在建立時顯示一次**，請當場複製。AI API 通常按用量計費，並和網頁版的訂閱方案分開計算。agent 每個週期都會呼叫一次，間隔越短費用越高，建議到各家後台設定用量上限。

通用流程：取得金鑰後，到 MochiPrep 的 **Settings > AI providers > Add provider** 新增，貼上金鑰按 **Save key**，再按 **Test** 確認能連線。

模型名稱請填官方文件列出的完整識別碼。想看到成本估算的話，可以填入 provider 目前的每百萬 token 價格，不填也能正常使用。

### OpenAI

1. 登入 [API keys 頁面](https://platform.openai.com/api-keys)。
2. 按 **Create new secret key**，複製金鑰。
3. 在 MochiPrep 選類型 **OpenAI**，Base URL 留空。

### Anthropic Claude

1. 登入 [Claude Console 的 API keys 頁面](https://platform.claude.com/settings/keys)。
2. 建立金鑰，取名並選擇到期時間。金鑰以 `sk-ant-` 開頭。
3. 在 MochiPrep 選類型 **Anthropic Claude**，Base URL 留空。

如果 Create key 按鈕是灰的，可能是你的組織角色沒有權限，請找管理員處理。

### Google Gemini

1. 登入 [Google AI Studio](https://aistudio.google.com/apikey)。
2. 建立 API key 並複製。
3. 在 MochiPrep 選類型 **Google Gemini**，Base URL 留空。

### DeepSeek

1. 登入 [DeepSeek Platform 的 API keys 頁面](https://platform.deepseek.com/api_keys)。
2. 建立金鑰並複製。
3. 在 MochiPrep 選類型 **DeepSeek**，Base URL 留空，模型預設為 `deepseek-flash`。

### OpenAI 相容服務

本機模型或其他相容 OpenAI API 的服務，選類型 **OpenAI-compatible**，Base URL 填到 `/v1` 為止，例如 Ollama 的 `http://localhost:11434/v1`。不需要金鑰的本機服務，金鑰欄隨便填一個值即可。

## OKX

> **真實資金警告：** MochiPrep 的 OKX 下單只用模擬交易所測試過，從未在真實 OKX 上下過單。請先照順序用模擬盤驗證，並閱讀 [Trading on OKX](OKX_TRADING.md)。

你會拿到三個值，全部都要：**API key**、**Secret key**、**Passphrase**。

Passphrase 是你自己設定的，OKX 無法找回。忘記就只能重新建立一組。

### 帳戶設定

連線前先在 OKX 確認，否則 MochiPrep 會拒絕連線：

- 帳戶模式是 **Futures**，不能是 Spot。
- 持倉模式是**單向持倉**（net mode），不能是雙向持倉。

### 模擬盤金鑰（先做這個）

模擬盤用虛擬資金，不會動到真錢，金鑰也不會因為閒置而失效。

1. 登入 OKX。
2. 進入 **Trade > Demo Trading**。
3. 開啟 **Personal Center > Demo Trading API**。
4. 按 **Create Demo Trading API Key**，權限勾選 **Trade**，設定 Passphrase。
5. 複製三個值。
6. 到 MochiPrep 的 **Settings > OKX trading > Connect OKX**，環境選 **Demo Trading**。

模擬盤金鑰和真實金鑰不能互用。

### 真實帳戶金鑰

1. 確認模擬盤已照 [上線前檢查](OKX_TRADING.md#before-your-first-real-order) 驗證完成。
2. 開啟 OKX 的 [API 管理頁面](https://www.okx.com/account/my-api)，建立新的 API key。
3. 權限**只勾 Read 和 Trade，絕對不要勾 Withdraw**。有提領權限的金鑰會被 MochiPrep 拒絕。
4. 設定 Passphrase，並建議綁定你的 IP。
5. 到 **Settings > OKX trading > Connect OKX**，環境選 **Live**，先把上限設小。
6. 連線後按 **Arm**，輸入 `TRADE REAL MONEY` 才會開啟真實下單。

請注意兩件事：

- **Trade 權限也包含資金劃轉和變更設定**，不只是下單，所以這組金鑰一樣要嚴格保管。
- **沒綁 IP 的金鑰，閒置 14 天後會失效。** 每把金鑰最多可綁 20 個 IP。家用網路的 IP 可能會變動，變動後金鑰就連不上，要到 OKX 更新。

建議另開一個 OKX 子帳戶，只放你願意承受損失的資金，再用這個子帳戶建立金鑰。這樣即使金鑰外流或程式出錯，影響範圍也有限。

## 常見問題

| 訊息 | 原因 | 解法 |
| --- | --- | --- |
| `must contain the word 'json'` | 舊版 Test 按鈕的問題 | 更新到最新版 |
| HTTP 401 / 403（AI provider） | 金鑰錯誤或已撤銷 | 重新貼上，或建立新金鑰 |
| HTTP 429 | 超過速率限制，或帳戶額度用完 | 檢查餘額和用量，或拉長 agent 間隔 |
| OKX `50119` | 金鑰不存在，常見於環境選錯 | 確認模擬盤金鑰選 Demo Trading，真實金鑰選 Live |
| OKX `50123` | 模擬盤金鑰沒有 Trade 權限，或不是在模擬盤建立的 | 在模擬盤重新建立並勾選 Trade |
| OKX `50102` | 電腦時間誤差超過 30 秒 | 校正系統時間 |
| `has withdrawal permission` | 金鑰可以提領 | 建立一組沒有 Withdraw 權限的金鑰 |
| `Set OKX position mode to one-way` | 目前是雙向持倉 | 到 OKX 改成單向持倉 |
| `is in Spot mode` | 帳戶是現貨模式 | 到 OKX 改成 Futures 模式 |

## 官方文件

- [OKX API 文件](https://www.okx.com/docs-v5/en/)
- [OKX API FAQ](https://www.okx.com/en-eu/help/api-faq)
- [OpenAI：找到你的 API key](https://help.openai.com/en/articles/4936850-where-do-i-find-my-openai-api-key)
- [Claude：取得 API key](https://platform.claude.com/docs/en/get-api-key)
- [DeepSeek API 文件](https://api-docs.deepseek.com/)
