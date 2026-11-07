import { createServer } from 'node:net';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import type { DatabaseSync } from 'node:sqlite';
import { DEFAULT_RISK_SETTINGS } from '@mochiprep/shared';
import { openDatabase } from '../src/db/database';
import { buildApp } from '../src/app';
import { Runtime } from '../src/runtime';

const freePort = () =>
  new Promise<number>((resolve) => {
    const probe = createServer().listen(0, '127.0.0.1', () => {
      const { port } = probe.address() as { port: number };
      probe.close(() => resolve(port));
    });
  });

describe('API decision-to-position integration', () => {
  let db: DatabaseSync;
  let runtime: Runtime;
  let app: ReturnType<typeof buildApp>['app'];
  let token: string;
  const headers = () => ({ host: 'localhost:8787', authorization: `Bearer ${token}` });

  beforeEach(() => {
    db = openDatabase(':memory:');
    runtime = new Runtime({
      db, replayMinutes: 5_800,
      kdf: { N: 2 ** 10, r: 8, p: 1 },
      providerFactory: () => ({
        kind: 'OPENAI',
        async complete(request) {
          const last = Number(request.context.quote.last);
          return {
            text: JSON.stringify({
              symbol: 'BTCUSDT', action: 'LONG', orderType: 'MARKET', entryPrice: null,
              stopLoss: last * 0.98, takeProfit: last * 1.04, reason: 'integration', confidence: 0.8, expiresAt: null,
            }),
            inputTokens: 10, outputTokens: 10,
          };
        },
      }),
    });
    ({ app, token } = buildApp(runtime, {
      host: '127.0.0.1', port: 8787, dataDir: '.',
      allowedOrigins: ['http://localhost:8787'],
    }));
  });

  afterEach(async () => {
    await app.close();
    await runtime.stop();
    db.close();
  });

  it('walks quote -> AI decision -> risk -> order -> position -> PnL through app.inject', async () => {
    const quoteResponse = await app.inject({ method: 'GET', url: '/api/market/quotes?source=REPLAY', headers: headers() });
    expect(quoteResponse.statusCode).toBe(200);
    const quote = quoteResponse.json().find((q: { symbol: string }) => q.symbol === 'BTCUSDT');

    const accountResponse = await app.inject({
      method: 'POST', url: '/api/accounts', headers: headers(),
      payload: { name: 'api', source: 'REPLAY', initialBalance: '10000' },
    });
    const account = accountResponse.json();
    const providerResponse = await app.inject({
      method: 'POST', url: '/api/providers', headers: headers(),
      payload: { name: 'provider', kind: 'OPENAI', baseUrl: null, defaultModel: 'model', inputPricePerMTok: null, outputPricePerMTok: null },
    });
    const provider = providerResponse.json();
    await app.inject({ method: 'PUT', url: `/api/providers/${provider.id}/key`, headers: headers(), payload: { apiKey: 'integration-secret-key' } });
    const agentResponse = await app.inject({
      method: 'POST', url: '/api/agents', headers: headers(),
      payload: {
        name: 'agent', providerId: provider.id, model: 'model', symbol: 'BTCUSDT', timeframe: '15m', strategy: '',
        accountId: account.id, risk: DEFAULT_RISK_SETTINGS, intervalSec: 900, executionMode: 'AUTO',
      },
    });
    const agent = agentResponse.json();

    const decisionResponse = await app.inject({ method: 'POST', url: `/api/agents/${agent.id}/run`, headers: headers() });
    expect(decisionResponse.statusCode).toBe(200);
    const decision = decisionResponse.json();
    expect(decision.outcome).toBe('EXECUTED');
    expect(decision.risk.approved).toBe(true);

    const positionsResponse = await app.inject({ method: 'GET', url: `/api/accounts/${account.id}/positions`, headers: headers() });
    const positions = positionsResponse.json();
    expect(positions).toHaveLength(1);
    expect(positions[0]).toMatchObject({ side: 'LONG', markPrice: quote.mark });
    expect(Number(positions[0].unrealizedPnl)).toBeLessThan(0);

    const audit = await app.inject({ method: 'GET', url: `/api/accounts/${account.id}/audit`, headers: headers() });
    expect(audit.json()).toEqual({ consistent: true, issues: [] });
  });

  it('enforces host and token checks', async () => {
    expect((await app.inject({ method: 'GET', url: '/api/accounts', headers: { host: 'localhost:8787' } })).statusCode).toBe(401);
    expect((await app.inject({ method: 'GET', url: '/api/accounts', headers: { ...headers(), host: 'evil.invalid' } })).statusCode).toBe(403);
  });

  it('refuses a foreign Origin even with a valid token', async () => {
    const get = (origin: string) => app.inject({ method: 'GET', url: '/api/accounts', headers: { ...headers(), origin } });
    expect((await get('http://evil.invalid')).statusCode).toBe(403);
    expect((await get('http://localhost:8787')).statusCode).toBe(200);
  });

  it('shuts down with an event stream still open', async () => {
    const port = await freePort();
    const origin = `http://127.0.0.1:${port}`;
    const server = buildApp(runtime, { host: '127.0.0.1', port, dataDir: '.', allowedOrigins: [origin] });
    await server.app.listen({ host: '127.0.0.1', port });
    const stream = await fetch(`${origin}/api/events?token=${server.token}`);
    expect(stream.status).toBe(200);

    const outcome = await Promise.race([
      server.app.close().then(() => 'closed'),
      new Promise((resolve) => setTimeout(() => resolve('hung'), 3000)),
    ]);
    expect(outcome).toBe('closed');
  });

  it('manages the master password over HTTP without echoing it', async () => {
    const post = (url: string, payload: Record<string, string>) => app.inject({ method: 'POST', url, headers: headers(), payload });
    expect((await app.inject({ method: 'GET', url: '/api/state', headers: headers() })).json().vault).toBe('OFF');

    expect((await post('/api/vault', { password: 'short' })).statusCode).toBe(400);
    const created = await post('/api/vault', { password: 'long-enough-password' });
    expect(created.json()).toEqual({ status: 'UNLOCKED' });

    const wrong = await post('/api/vault/password', { current: 'guess-123456', next: 'another-long-password' });
    // 403, not 401: the browser client treats 401 as a stale session token and retries.
    expect(wrong.statusCode).toBe(403);
    expect(wrong.body).not.toContain('guess-123456');

    expect((await post('/api/vault/reset', { confirmation: 'nope' })).statusCode).toBe(400);
    expect((await post('/api/vault/reset', { confirmation: 'DELETE SAVED KEYS' })).json()).toEqual({ status: 'OFF' });
  });
});
