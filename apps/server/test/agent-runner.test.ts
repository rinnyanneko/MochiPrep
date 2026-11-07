import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { DatabaseSync } from 'node:sqlite';
import { ProviderError, type AiProvider } from '@mochiprep/ai-providers';
import { DEFAULT_RISK_SETTINGS, type ProviderConfig } from '@mochiprep/shared';
import { openDatabase } from '../src/db/database';
import { Runtime } from '../src/runtime';

const hold = JSON.stringify({
  symbol: 'BTCUSDT', action: 'HOLD', orderType: 'MARKET', entryPrice: null, stopLoss: null,
  takeProfit: null, reason: 'wait', confidence: 0.5, expiresAt: null,
});
const long = JSON.stringify({
  symbol: 'BTCUSDT', action: 'LONG', orderType: 'MARKET', entryPrice: null, stopLoss: 60_000,
  takeProfit: 80_000, reason: 'trend', confidence: 0.8, expiresAt: null,
});

describe('AgentRunner integration', () => {
  let db: DatabaseSync;
  let runtime: Runtime;
  let config: ProviderConfig;
  let complete: AiProvider['complete'];
  let agentId: string;

  beforeEach(() => {
    db = openDatabase(':memory:');
    complete = async () => ({ text: hold, inputTokens: 1, outputTokens: 1 });
    runtime = new Runtime({
      db, replayMinutes: 5_800,
      providerFactory: () => ({ kind: 'OPENAI', complete: (request) => complete(request) }),
    });
    config = {
      id: 'provider', name: 'test', kind: 'OPENAI', baseUrl: null, defaultModel: 'test',
      inputPricePerMTok: null, outputPricePerMTok: null, createdAt: 1,
    };
    runtime.store.insertProvider(config);
    runtime.setKey(config.id, 'custom-secret-agent-key');
    const account = runtime.engine.createAccount({ name: 'agent', source: 'REPLAY' });
    agentId = runtime.createAgent({
      name: 'agent', providerId: config.id, model: 'test', symbol: 'BTCUSDT', timeframe: '15m',
      strategy: 'test', accountId: account.id, risk: DEFAULT_RISK_SETTINGS, intervalSec: 900,
    }).id;
  });

  afterEach(async () => {
    await runtime.stop();
    db.close();
  });

  it('does not overlap cycles for the same agent', async () => {
    let resolve: ((value: { text: string; inputTokens: null; outputTokens: null }) => void) | undefined;
    complete = () => new Promise((r) => { resolve = r; });
    const first = runtime.runner.runCycle(agentId, 'RUN_ONCE');
    await vi.waitFor(() => expect(resolve).toBeTypeOf('function'));
    expect(await runtime.runner.runCycle(agentId, 'RUN_ONCE')).toBeNull();
    resolve!({ text: hold, inputTokens: null, outputTokens: null });
    expect((await first)!.outcome).toBe('HOLD');
  });

  it('Analyze Now never places an order', async () => {
    complete = async () => ({ text: long, inputTokens: null, outputTokens: null });
    const record = await runtime.runner.runCycle(agentId, 'ANALYZE');
    expect(record!.outcome).toBe('ANALYSIS_ONLY');
    expect(runtime.store.listOrders(runtime.store.getAgent(agentId)!.accountId, 10)).toHaveLength(0);
  });

  it('a Stop while the model is thinking prevents a scheduled order', async () => {
    let resolve: ((value: { text: string; inputTokens: null; outputTokens: null }) => void) | undefined;
    complete = () => new Promise((r) => { resolve = r; });
    runtime.runner.start(agentId);
    const cycle = runtime.runner.runCycle(agentId, 'SCHEDULE');
    await vi.waitFor(() => expect(resolve).toBeTypeOf('function'));
    runtime.runner.stop(agentId);
    resolve!({ text: long, inputTokens: null, outputTokens: null });
    const record = await cycle;
    expect(record!.outcome).toBe('ANALYSIS_ONLY');
    expect(record!.outcomeDetail).toContain('stopped');
  });

  it('does not call AI for stale data while flat', async () => {
    let called = false;
    complete = async () => { called = true; return { text: hold, inputTokens: null, outputTokens: null }; };
    const original = runtime.replay.getQuote.bind(runtime.replay);
    runtime.replay.getQuote = ((symbol) => ({ ...original(symbol), ts: runtime.replay.now() - 120_000 })) as typeof runtime.replay.getQuote;
    const record = await runtime.runner.runCycle(agentId, 'RUN_ONCE');
    expect(record!.outcome).toBe('DATA_ERROR');
    expect(called).toBe(false);
  });

  it('auto-pauses after three model failures and immediately on an auth error', async () => {
    complete = async () => { throw new Error('temporary failure'); };
    runtime.runner.start(agentId);
    for (let i = 0; i < 3; i++) await runtime.runner.runCycle(agentId, 'RUN_ONCE');
    expect(runtime.store.getAgent(agentId)).toMatchObject({ running: false, status: 'PAUSED', consecutiveErrors: 3 });

    const second = runtime.createAgent({
      ...runtime.store.getAgent(agentId)!, name: 'auth', accountId: runtime.store.getAgent(agentId)!.accountId,
    }).id;
    complete = async () => { throw new ProviderError('AUTH', 'bad key custom-secret-agent-key'); };
    runtime.runner.start(second);
    const record = await runtime.runner.runCycle(second, 'RUN_ONCE');
    expect(record!.outcomeDetail).not.toContain('custom-secret-agent-key');
    expect(runtime.store.getAgent(second)).toMatchObject({ running: false, status: 'PAUSED' });
  });

  it('redacts secrets from invalid raw responses before persistence', async () => {
    complete = async () => ({
      text: 'not-json custom-secret-agent-key sk-proj_abcdefghijk', inputTokens: null, outputTokens: null,
    });
    const record = await runtime.runner.runCycle(agentId, 'RUN_ONCE');
    expect(record!.outcome).toBe('INVALID_OUTPUT');
    expect(record!.rawResponse).not.toContain('custom-secret-agent-key');
    expect(record!.rawResponse).not.toContain('sk-proj_abcdefghijk');
    const stored = runtime.store.listDecisions({ agentId, limit: 10 })[0]!;
    expect(stored.rawResponse).toBe(record!.rawResponse);
  });
});
