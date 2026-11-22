#!/usr/bin/env node
// An OpenAI-compatible chat endpoint for the e2e specs, so agents run through the real remote
// provider code without a real model. It reads the market data MochiPrep puts in the prompt and
// answers by the agent's model name:
//
//   momentum   opens with the EMA trend (stop 2 ATR, target 3 ATR), closes an open position
//   reversion  opens against the EMA trend, closes an open position
//   reckless   on alternate bars opens without a stop loss, which the risk engine refuses
//
// An agent with several pairs gets one decision per pair, in the `decisions` shape.
import { createServer } from 'node:http';

const PORT = Number(process.env.MOCHIPREP_E2E_AI_PORT || 8879);

/** The JSON that follows a "Market data" heading in the user prompt. */
function marketData(user) {
  const start = user.indexOf(':\n') + 2;
  const end = user.indexOf('\n\n', start);
  return JSON.parse(user.slice(start, end === -1 ? undefined : end));
}

function decide(model, pair) {
  const last = Number(pair.market.last);
  const { ema20, ema50, atr14 } = pair.indicators;
  const atr = atr14 ?? last * 0.005;
  const digits = (pair.market.last.split('.')[1] ?? '').length;
  const round = (n) => Number(n.toFixed(digits));
  const base = { symbol: pair.symbol, orderType: 'MARKET', entryPrice: null, expiresAt: null, confidence: 0.6 };
  if (pair.position) return { ...base, action: 'CLOSE', stopLoss: null, takeProfit: null, reason: 'e2e: take the result' };
  const up = (ema20 ?? last) >= (ema50 ?? last);
  if (model.startsWith('reckless')) {
    const bar = Math.floor(Date.parse(pair.currentTime) / 900_000);
    if (bar % 2 === 0) return { ...base, action: 'LONG', stopLoss: null, takeProfit: null, reason: 'e2e: no stop on purpose' };
    return { ...base, action: 'LONG', stopLoss: round(last - 2 * atr), takeProfit: round(last + 3 * atr), reason: 'e2e: reckless long' };
  }
  const long = model.startsWith('reversion') ? !up : up;
  const sign = long ? 1 : -1;
  return {
    ...base,
    action: long ? 'LONG' : 'SHORT',
    stopLoss: round(last - sign * 2 * atr),
    takeProfit: round(last + sign * 3 * atr),
    reason: `e2e: ${model}`,
  };
}

function answer(body) {
  const system = body.messages.find((m) => m.role === 'system')?.content ?? '';
  const user = body.messages.find((m) => m.role === 'user')?.content ?? '';
  // A connection test asks for {"ok":true}.
  if (!user.includes('Market data')) return JSON.stringify({ ok: true });
  const data = marketData(user);
  if (system.includes('"decisions"')) return JSON.stringify({ decisions: data.map((pair) => decide(body.model, pair)) });
  return JSON.stringify(decide(body.model, data));
}

createServer((req, res) => {
  let text = '';
  req.on('data', (chunk) => (text += chunk));
  req.on('end', () => {
    if (req.method !== 'POST' || !req.url.endsWith('/chat/completions')) {
      res.writeHead(404).end();
      return;
    }
    const content = answer(JSON.parse(text));
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ choices: [{ message: { content } }], usage: { prompt_tokens: 100, completion_tokens: 20 } }));
  });
}).listen(PORT, '127.0.0.1', () => console.log(`fake AI on http://127.0.0.1:${PORT}`));
