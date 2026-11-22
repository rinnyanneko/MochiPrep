import type { APIRequestContext } from '@playwright/test';

/** Where `fake-ai.mjs` listens; the Playwright config starts it. */
export const AI_PORT = Number(process.env.MOCHIPREP_E2E_AI_PORT || 8879);

/** The agents the specs trade with, one replay paper account each, plus a manual account. */
export const AGENTS = [
  { name: 'Momentum Mochi', model: 'momentum', account: 'Momentum account' },
  { name: 'Reversion Daifuku', model: 'reversion', account: 'Reversion account' },
  { name: 'Reckless Rice Cake', model: 'reckless', account: 'Reckless account' },
] as const;
export const TERMINAL = 'Terminal';

async function api(request: APIRequestContext) {
  const { token } = (await (await request.get('/api/session')).json()) as { token: string };
  const headers = { Authorization: `Bearer ${token}` };
  const call = async <T>(method: 'GET' | 'POST' | 'PUT', path: string, data?: unknown): Promise<T> => {
    const response = await request.fetch(`/api${path}`, { method, headers, data });
    if (!response.ok()) throw new Error(`${method} ${path}: ${response.status()} ${await response.text()}`);
    return (await response.json()) as T;
  };
  return call;
}

/** Stops every agent, so nothing trades into the next test sharing this backend. */
export async function stopAll(request: APIRequestContext) {
  const call = await api(request);
  await call('POST', '/agents/stop-all');
}

/**
 * A fresh account with one Auto agent on it, named `name`. For a test that needs an agent whose
 * account no other test has traded on.
 */
export async function freshAgent(request: APIRequestContext, name: string, model: string) {
  const call = await api(request);
  const providers = await call<{ id: string; name: string }[]>('GET', '/providers');
  const provider = providers.find((p) => p.name === 'E2E model')!;
  const account = await call<{ id: string }>('POST', '/accounts', { name: `${name} account`, source: 'REPLAY', initialBalance: '10000' });
  await call('POST', '/agents', {
    name,
    providerId: provider.id,
    model,
    symbols: ['BTCUSDT'],
    timeframe: '15m',
    strategy: '',
    accountId: account.id,
    intervalSec: 900,
    executionMode: 'AUTO',
  });
}

/** The sidebar's Trading button, whose name gains a count while suggestions wait. */
export const TRADING_NAV = /^Trading( \d+)?$/;

/**
 * Creates the specs' world once per backend: an AI provider pointing at the fake model, three
 * Auto agents on 10,000 USDT replay accounts, and a terminal account for manual trades. The
 * backend and database are shared by every spec, so a second call finds it and does nothing.
 */
export async function seed(request: APIRequestContext) {
  const call = await api(request);
  const accounts = await call<{ name: string }[]>('GET', '/accounts');
  if (accounts.some((a) => a.name === TERMINAL)) return;
  const provider = await call<{ id: string }>('POST', '/providers', {
    name: 'E2E model',
    kind: 'OPENAI_COMPATIBLE',
    baseUrl: `http://127.0.0.1:${AI_PORT}`,
    defaultModel: 'momentum',
    inputPricePerMTok: null,
    outputPricePerMTok: null,
  });
  await call('PUT', `/providers/${provider.id}/key`, { apiKey: 'e2e-key' });
  for (const agent of AGENTS) {
    const account = await call<{ id: string }>('POST', '/accounts', { name: agent.account, source: 'REPLAY', initialBalance: '10000' });
    await call('POST', '/agents', {
      name: agent.name,
      providerId: provider.id,
      model: agent.model,
      symbols: ['BTCUSDT'],
      timeframe: '15m',
      strategy: '',
      accountId: account.id,
      intervalSec: 900,
      executionMode: 'AUTO',
    });
  }
  await call('POST', '/accounts', { name: TERMINAL, source: 'REPLAY', initialBalance: '10000' });
}
