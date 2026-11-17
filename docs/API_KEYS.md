# How to get API keys

This guide walks you through the keys MochiPrep can use. Demo Mode needs no key at all. Get keys only when you want a real AI model or want to connect OKX.

> **DYOR: do your own research.** MochiPrep is not financial advice. AI output can be wrong, and you are responsible for your own trades.

Provider dashboards change their menus from time to time. If what you see differs from this guide, follow the provider's own page.

## Start here: where keys are stored

| Key | Without a master password | With a master password |
| --- | --- | --- |
| AI provider | Local SQLite, **unencrypted**, kept across restarts | Encrypted in SQLite, loaded when you unlock after a restart |
| OKX | Backend memory only; enter it again after a restart | Same by default. Tick the box when connecting to save it encrypted; it reconnects after you unlock (a real account still has to be armed again) |
| Extra headers | Unencrypted | **Still unencrypted** |

- Treat keys like passwords. Do not paste them into chats or screenshots, and never commit them to git.
- Without a master password, AI keys are stored as plain text in MochiPrep's data folder (`.mochiprep/` for a source build; see [the launcher guide](LAUNCHER.md#where-things-live) for installed apps). Do not keep that folder in iCloud, Dropbox or another sync folder.
- Set the master password in **Settings > Master password**. Neither it nor the data key is ever written to disk, so **a forgotten master password cannot be recovered**. You can only delete the saved keys and enter them again; your accounts and trade history are not affected. Backups made before you set a master password still contain the keys in plain text.
- The master password protects database files that are copied, synced or backed up. While MochiPrep is unlocked, malware on the same computer could still read the keys from memory.
- If a key leaks, delete it in the provider's dashboard right away and create a new one.

## AI providers

Most keys are **shown only once, when you create them**, so copy them right away. AI APIs are usually billed by usage, separately from the provider's chat subscription. An agent calls the model once per cycle, so a shorter interval costs more. Set a spending limit in each provider's dashboard.

The general steps: once you have a key, go to **Settings > AI providers > Add provider** in MochiPrep, paste the key, press **Save key**, then press **Test** to check the connection.

Enter the model name as the full identifier listed in the provider's documentation. To see cost estimates, you can enter the provider's current price per million tokens. It works without them too.

### OpenAI

1. Sign in to the [API keys page](https://platform.openai.com/api-keys).
2. Press **Create new secret key** and copy the key.
3. In MochiPrep, choose the type **OpenAI** and leave Base URL empty.

### Anthropic Claude

1. Sign in to the [API keys page of the Claude Console](https://platform.claude.com/settings/keys).
2. Create a key, give it a name and choose when it expires. The key starts with `sk-ant-`.
3. In MochiPrep, choose the type **Anthropic Claude** and leave Base URL empty.

If the Create key button is greyed out, your role in the organization may not allow it. Ask an admin.

### Google Gemini

1. Sign in to [Google AI Studio](https://aistudio.google.com/apikey).
2. Create an API key and copy it.
3. In MochiPrep, choose the type **Google Gemini** and leave Base URL empty.

### DeepSeek

1. Sign in to the [API keys page of the DeepSeek Platform](https://platform.deepseek.com/api_keys).
2. Create a key and copy it.
3. In MochiPrep, choose the type **DeepSeek** and leave Base URL empty. The default model is `deepseek-flash`.

### OpenCode

1. Following the [Zen documentation](https://opencode.ai/docs/zen/), sign in to the OpenCode Console, add billing details and credit, and copy the API key.
2. In MochiPrep, choose the type **OpenCode** and leave Base URL empty. The default model is `deepseek-v4.1-flash`.

MochiPrep uses OpenCode's Chat Completions endpoint. OpenCode says other text models can be used through it too. Free models, whose names end in `-free`, work only in OpenCode's own clients, so choose a paid model here. The full model list is in the [Zen documentation](https://opencode.ai/docs/zen/).

### OpenAI-compatible services

For local models or any other service compatible with the OpenAI API, choose the type **OpenAI-compatible** and enter the Base URL up to `/v1`, for example `http://localhost:11434/v1` for Ollama. For a local service that needs no key, put any value in the key field.

### Services that need extra headers

Some gateways require extra HTTP headers, such as `HTTP-Referer` or an organization ID. Enter them under **Extra headers** when you add the provider, one per line:

```text
HTTP-Referer: https://example.com
X-Title: MochiPrep
```

- Like API keys, they are stored locally without encryption. The app shows only their names, never their values.
- When editing, leaving the field empty keeps the current headers, and entering new ones replaces the whole set.
- Headers that MochiPrep sets itself, such as `Authorization`, `x-api-key` and `Content-Type`, cannot be overridden.

## OKX

> **Real money warning:** MochiPrep's OKX order handling has only been tested against a simulated exchange. It has never placed an order on real OKX. Verify it on Demo Trading first, step by step, and read [Trading on OKX](OKX_TRADING.md).

You get three values, and you need all of them: the **API key**, the **Secret key** and the **Passphrase**.

You choose the Passphrase yourself, and OKX cannot recover it. If you forget it, you have to create a new key.

### Account settings

Check these in OKX before connecting, or MochiPrep refuses the connection:

- The account mode is **Futures**, not Spot.
- The position mode is **one-way** (net mode), not hedge mode.

### Demo Trading key (do this first)

Demo Trading uses virtual funds, so no real money is involved, and demo keys do not expire when idle.

1. Sign in to OKX.
2. Go to **Trade > Demo Trading**.
3. Open **Personal Center > Demo Trading API**.
4. Press **Create Demo Trading API Key**, tick the **Trade** permission, and set a Passphrase.
5. Copy the three values.
6. In MochiPrep, go to **Settings > OKX trading > Connect OKX** and choose the **Demo Trading** environment.

Demo keys and real keys are not interchangeable.

### Real account key

1. Make sure you have finished the [checks before your first real order](OKX_TRADING.md#before-your-first-real-order) on Demo Trading.
2. Open OKX's [API management page](https://www.okx.com/account/my-api) and create a new API key.
3. Tick **only Read and Trade. Never tick Withdraw.** MochiPrep refuses any key that can withdraw.
4. Set a Passphrase. Binding the key to your IP address is recommended.
5. Go to **Settings > OKX trading > Connect OKX**, choose the **Live** environment, and start with small limits.
6. After connecting, press **Arm** and type `TRADE REAL MONEY`. Only then are real orders turned on.

Keep two things in mind:

- **The Trade permission also covers transferring funds and changing account settings**, not just placing orders, so guard this key just as carefully.
- **A key without a bound IP address expires after 14 days of inactivity.** Each key can be bound to up to 20 IP addresses. A home internet connection's IP address can change, and the key then stops connecting until you update it in OKX.

Consider opening a separate OKX sub-account that holds only money you can afford to lose, and create the key there. That limits the damage if the key leaks or the software makes a mistake.

## Troubleshooting

| Message | Cause | Fix |
| --- | --- | --- |
| `must contain the word 'json'` | A bug in an older version of the Test button | Update to the latest version |
| HTTP 401 / 403 (AI provider) | The key is wrong or has been revoked | Paste it again, or create a new key |
| HTTP 429 | Rate limit reached, or the account is out of credit | Check your balance and usage, or lengthen the agent interval |
| OKX `50119` | The key does not exist, often because the wrong environment was chosen | Use Demo Trading for a demo key and Live for a real key |
| OKX `50123` | The demo key lacks the Trade permission, or was not created in Demo Trading | Create it again in Demo Trading with Trade ticked |
| OKX `50102` | Your computer's clock is off by more than 30 seconds | Correct the system time |
| `has withdrawal permission` | The key can withdraw | Create a key without the Withdraw permission |
| `Set OKX position mode to one-way` | The account is in hedge mode | Switch it to one-way mode in OKX |
| `is in Spot mode` | The account is in Spot mode | Switch it to Futures mode in OKX |

## Official documentation

- [OKX API documentation](https://www.okx.com/docs-v5/en/)
- [OKX API FAQ](https://www.okx.com/en-eu/help/api-faq)
- [OpenAI: Where do I find my API key?](https://help.openai.com/en/articles/4936850-where-do-i-find-my-openai-api-key)
- [Claude: Get an API key](https://platform.claude.com/docs/en/get-api-key)
- [DeepSeek API documentation](https://api-docs.deepseek.com/)
