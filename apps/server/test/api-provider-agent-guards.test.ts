import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import type { DatabaseSync } from 'node:sqlite';
import { createRemoteProvider } from '@mochiprep/ai-providers';
import { DEFAULT_RISK_SETTINGS } from '@mochiprep/shared';
import { buildApp } from '../src/app';
import { openDatabase } from '../src/db/database';
import { Runtime } from '../src/runtime';

const providerInput = {
  name: 'Gateway', kind: 'OPENAI_COMPATIBLE', baseUrl: 'https://gateway.test/v1', defaultModel: 'm',
  inputPricePerMTok: null, outputPricePerMTok: null,
};
const response = (text: string) => new Response(JSON.stringify({ choices: [{ message: { content: text } }] }));
const deferred = () => {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => { resolve = done; });
  return { promise, resolve };
};

describe('provider samples and agent mutation guards', () => {
  let db: DatabaseSync;
  let runtime: Runtime;
  let app: ReturnType<typeof buildApp>['app'];
  let token: string;
  let transport: typeof fetch;

  const call = (method: 'POST' | 'PUT' | 'DELETE', url: string, payload?: object) =>
    app.inject({ method, url, headers: { host: 'localhost:8787', authorization: `Bearer ${token}` }, ...(payload ? { payload } : {}) });

  beforeEach(() => {
    db = openDatabase(':memory:');
    transport = async () => response('{"ok":true}');
    runtime = new Runtime({ db, replayMinutes: 5_800, providerFactory: (config, key) => createRemoteProvider(config, key, transport) });
    runtime.store.insertProvider({ ...providerInput, kind: 'OPENAI_COMPATIBLE', id: 'remote', createdAt: 1 });
    runtime.setKey('remote', 'original-gateway-secret');
    ({ app, token } = buildApp(runtime, { host: '127.0.0.1', port: 8787, dataDir: '.', allowedOrigins: ['http://localhost:8787'] }));
  });

  afterEach(async () => {
    await app.close();
    await runtime.stop();
    db.close();
  });

  it('redacts an echoed secret before cutting the connection sample at 120 characters', async () => {
    transport = async () => response('x'.repeat(112) + 'original-gateway-secret');
    const result = await call('POST', '/api/providers/remote/test', {});
    expect(result.statusCode).toBe(200);
    expect(result.json().sample).toBe('x'.repeat(112) + '[REDACTE');
    expect(result.body).not.toContain('original');
  });

  it('redacts both the old and current keys and headers after edits during a connection test', async () => {
    const entered = deferred();
    const release = deferred();
    await call('PUT', '/api/providers/remote', { ...providerInput, extraHeaders: { 'x-org': 'old-header-secret' } });
    transport = async () => {
      entered.resolve();
      await release.promise;
      return response('original-gateway-secret current-gateway-secret old-header-secret current-header-secret');
    };
    const pending = call('POST', '/api/providers/remote/test', {}).then((result) => result);
    await entered.promise;
    try {
      await call('PUT', '/api/providers/remote/key', { apiKey: 'current-gateway-secret' });
      await call('PUT', '/api/providers/remote', { ...providerInput, extraHeaders: { 'x-org': 'current-header-secret' } });
    } finally {
      release.resolve();
    }
    expect((await pending).json()).toEqual({ ok: true, sample: '[REDACTED] [REDACTED] [REDACTED] [REDACTED]' });
  });

  const setupAgent = () => {
    const accountId = runtime.engine.createAccount({ name: 'paper', source: 'REPLAY' }).id;
    const input = {
      name: 'a', providerId: 'remote', model: 'm', symbol: 'BTCUSDT', timeframe: '15m' as const, strategy: '', accountId,
      risk: DEFAULT_RISK_SETTINGS, intervalSec: 900, executionMode: 'SIGNAL' as const,
    };
    return { input, agent: runtime.createAgent(input) };
  };

  const holdDecision = () => {
    const entered = deferred();
    const release = deferred();
    const last = Number(runtime.replay.getQuote('BTCUSDT')!.last);
    transport = async () => {
      entered.resolve();
      await release.promise;
      return response(JSON.stringify({
        symbol: 'BTCUSDT', action: 'LONG', orderType: 'MARKET', entryPrice: null, stopLoss: last * 0.98,
        takeProfit: last * 1.04, reason: 'held', confidence: 0.8, expiresAt: null,
      }));
    };
    return { entered, release };
  };

  it('rejects edits and deletion while busy, preserving validation precedence and the pending settings', async () => {
    const { agent, input } = setupAgent();
    const { entered, release } = holdDecision();
    const pending = call('POST', `/api/agents/${agent.id}/run`).then((result) => result);
    await entered.promise;
    const edited = { ...input, executionMode: 'AUTO', risk: { ...input.risk, maxLeverage: 1 } };
    try {
      expect((await call('PUT', `/api/agents/${agent.id}`, {})).statusCode).toBe(400);
      expect((await call('PUT', `/api/agents/${agent.id}`, { ...edited, providerId: 'missing' })).statusCode).toBe(400);
      expect((await call('PUT', '/api/agents/missing', edited)).statusCode).toBe(404);
      const edit = await call('PUT', `/api/agents/${agent.id}`, edited);
      expect(edit.statusCode).toBe(409);
      expect(edit.json().error).toMatch(/cycle.*running.*try again/i);
      expect((await call('DELETE', `/api/agents/${agent.id}`)).statusCode).toBe(409);
      expect(runtime.store.getAgent(agent.id)).toMatchObject({ executionMode: 'SIGNAL', risk: DEFAULT_RISK_SETTINGS });
    } finally {
      release.resolve();
      await pending;
    }
    expect((await pending).json().outcome).toBe('SIGNAL');
    expect(runtime.store.listOrders(input.accountId, 50)).toHaveLength(0);
    expect((await call('PUT', `/api/agents/${agent.id}`, edited)).statusCode).toBe(200);
    expect(runtime.store.getAgent(agent.id)).toMatchObject({ executionMode: 'AUTO', risk: { maxLeverage: 1 } });
    expect((await call('DELETE', `/api/agents/${agent.id}`)).statusCode).toBe(200);
  });

  it('keeps the running-delete error first and permits Pause and Stop during a cycle', async () => {
    const { agent } = setupAgent();
    runtime.runner.start(agent.id);
    const { entered, release } = holdDecision();
    const pending = call('POST', `/api/agents/${agent.id}/run`).then((result) => result);
    await entered.promise;
    try {
      const deletion = await call('DELETE', `/api/agents/${agent.id}`);
      expect(deletion.statusCode).toBe(400);
      expect(deletion.json().error).toBe('Stop the agent first');
      expect((await call('POST', `/api/agents/${agent.id}/pause`)).statusCode).toBe(200);
      expect((await call('POST', `/api/agents/${agent.id}/stop`)).statusCode).toBe(200);
      expect((await call('DELETE', `/api/agents/${agent.id}`)).statusCode).toBe(409);
    } finally {
      release.resolve();
      await pending;
    }
    expect((await call('DELETE', `/api/agents/${agent.id}`)).statusCode).toBe(200);
  });
});
