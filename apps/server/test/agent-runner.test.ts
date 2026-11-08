import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { DatabaseSync } from 'node:sqlite';
import { ProviderError, type AiProvider } from '@mochiprep/ai-providers';
import { ProtectionChangeError } from '@mochiprep/core';
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

  it('turns a risk rejection into no order at all', async () => {
    const unprotected = JSON.stringify({ ...JSON.parse(long), stopLoss: null });
    complete = async () => ({ text: unprotected, inputTokens: null, outputTokens: null });
    const record = await runtime.runner.runCycle(agentId, 'RUN_ONCE');
    expect(record!.outcome).toBe('RISK_REJECTED');
    expect(runtime.store.listOrders(runtime.store.getAgent(agentId)!.accountId, 10)).toHaveLength(0);
  });

  it('blocks a rejected CLOSE too, which needs no sizing to reach the engine', async () => {
    const accountId = runtime.store.getAgent(agentId)!.accountId;
    runtime.engine.placeOrderSync({ accountId, clientOrderId: 'open', symbol: 'BTCUSDT', side: 'BUY', type: 'MARKET', quantity: '0.01', leverage: 3 });
    const expired = JSON.stringify({ ...JSON.parse(hold), action: 'CLOSE', expiresAt: '2020-01-01T00:00:00Z' });
    complete = async () => ({ text: expired, inputTokens: null, outputTokens: null });
    const record = await runtime.runner.runCycle(agentId, 'RUN_ONCE');
    expect(record!.outcome).toBe('RISK_REJECTED');
    expect(runtime.store.getOpenPosition(accountId, 'BTCUSDT')).toBeDefined();
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

  describe('PROTECT and trailing stops', () => {
    const trailing = { kind: 'RATIO' as const, callback: '0.02', activationPrice: null };
    const accountId = () => runtime.store.getAgent(agentId)!.accountId;
    const last = () => Number(runtime.replay.getQuote('BTCUSDT')!.last);
    const open = (extra: { stopLoss?: string; trailingStop?: typeof trailing } = {}) =>
      runtime.engine.placeOrderSync({
        accountId: accountId(), clientOrderId: 'open', symbol: 'BTCUSDT', side: 'BUY', type: 'MARKET', quantity: '0.01', leverage: 3, ...extra,
      });
    const reply = (decision: object) => {
      complete = async () => ({ text: JSON.stringify({ ...JSON.parse(hold), action: 'PROTECT', ...decision }), inputTokens: null, outputTokens: null });
    };

    it('PROTECT tightens the stop in AUTO and records before and after', async () => {
      const loose = String(Math.round(last() * 0.95));
      const tight = Math.round(last() * 0.98);
      open({ stopLoss: loose });
      reply({ stopLoss: tight });
      const record = (await runtime.runner.runCycle(agentId, 'RUN_ONCE'))!;
      expect(record).toMatchObject({ outcome: 'EXECUTED', outcomeDetail: 'Protection updated' });
      expect(record.protectionBefore).toEqual({ stopLoss: loose, takeProfit: null, trailingStop: null });
      expect(record.protectionAfter).toEqual({ stopLoss: String(tight), takeProfit: null, trailingStop: null });
      expect(runtime.engine.protectionOfSync(accountId(), 'BTCUSDT').stopLoss).toBe(String(tight));
      expect(runtime.store.getDecision(record.id)!.protectionAfter).toEqual(record.protectionAfter);
    });

    it('PROTECT without trailingStop keeps an existing trailing stop when the switch is off', async () => {
      open({ stopLoss: String(Math.round(last() * 0.95)), trailingStop: trailing });
      const tight = Math.round(last() * 0.98);
      reply({ stopLoss: tight });
      const record = (await runtime.runner.runCycle(agentId, 'RUN_ONCE'))!;
      expect(record.outcome).toBe('EXECUTED');
      expect(record.risk!.checks.map((c) => c.id)).not.toContain('trailing-allowed');
      expect(record.protectionAfter).toEqual({ stopLoss: String(tight), takeProfit: null, trailingStop: trailing });
      expect(runtime.engine.protectionOfSync(accountId(), 'BTCUSDT').trailingStop).toEqual(trailing);
    });

    it('PROTECT with trailingStop null is risk rejected when the switch is off', async () => {
      const stop = String(Math.round(last() * 0.95));
      open({ stopLoss: stop, trailingStop: trailing });
      reply({ stopLoss: Number(stop), trailingStop: null });
      const record = (await runtime.runner.runCycle(agentId, 'RUN_ONCE'))!;
      expect(record.outcome).toBe('RISK_REJECTED');
      expect(record.outcomeDetail).toContain('Trailing stops allowed');
      expect(runtime.engine.protectionOfSync(accountId(), 'BTCUSDT')).toEqual({ stopLoss: stop, takeProfit: null, trailingStop: trailing });
    });

    it('PROTECT asking for the current protection changes nothing', async () => {
      const stop = String(Math.round(last() * 0.95));
      open({ stopLoss: stop });
      reply({ stopLoss: Number(stop) });
      const before = runtime.store.listOrders(accountId(), 50).length;
      const record = (await runtime.runner.runCycle(agentId, 'RUN_ONCE'))!;
      expect(record).toMatchObject({ outcome: 'HOLD', outcomeDetail: 'Protection already as requested' });
      expect(runtime.store.listOrders(accountId(), 50)).toHaveLength(before);
    });

    it('PROTECT that the engine finds already in force is a HOLD, not an update', async () => {
      // The engine aligned the model's price to the tick and found nothing to change.
      const stop = String(Math.round(last() * 0.95));
      open({ stopLoss: stop });
      reply({ stopLoss: Number(stop) + 0.04 });
      runtime.engine.setProtection = async () => ({
        before: { stopLoss: stop, takeProfit: null, trailingStop: null },
        after: { stopLoss: `${stop}.0`, takeProfit: null, trailingStop: null },
        pending: false,
      });
      const record = (await runtime.runner.runCycle(agentId, 'RUN_ONCE'))!;
      expect(record).toMatchObject({ outcome: 'HOLD', outcomeDetail: 'Protection already as requested' });
      expect(record.protectionAfter).toEqual({ stopLoss: `${stop}.0`, takeProfit: null, trailingStop: null });
    });

    it('records a thrown protection change as rejected, with secrets redacted', async () => {
      open({ stopLoss: String(Math.round(last() * 0.95)) });
      reply({ stopLoss: Math.round(last() * 0.98) });
      runtime.engine.setProtection = async () => { throw new Error('refused custom-secret-agent-key'); };
      const record = (await runtime.runner.runCycle(agentId, 'RUN_ONCE'))!;
      expect(record.outcome).toBe('ORDER_REJECTED');
      expect(record.outcomeDetail).toContain('refused');
      expect(record.outcomeDetail).not.toContain('custom-secret-agent-key');
    });

    it('records an unanswered protection change as placed and awaiting confirmation', async () => {
      const loose = String(Math.round(last() * 0.95));
      open({ stopLoss: loose });
      reply({ stopLoss: Math.round(last() * 0.98) });
      const held = { stopLoss: loose, takeProfit: null, trailingStop: null };
      runtime.engine.setProtection = async () => ({ before: held, after: held, pending: true });
      const record = (await runtime.runner.runCycle(agentId, 'RUN_ONCE'))!;
      expect(record).toMatchObject({ outcome: 'ORDER_PLACED', outcomeDetail: 'Protection change sent to OKX, awaiting confirmation' });
      expect(record.protectionAfter).toEqual(held);
    });

    it('records a change refused part-way with the protection actually in force', async () => {
      open({ stopLoss: String(Math.round(last() * 0.95)) });
      const tight = String(Math.round(last() * 0.98));
      reply({ stopLoss: Number(tight) });
      const inForce = { stopLoss: tight, takeProfit: null, trailingStop: null };
      runtime.engine.setProtection = async () => { throw new ProtectionChangeError('OKX 51000: Parameter error', inForce); };
      const record = (await runtime.runner.runCycle(agentId, 'RUN_ONCE'))!;
      expect(record).toMatchObject({ outcome: 'ORDER_REJECTED', outcomeDetail: 'OKX 51000: Parameter error' });
      expect(record.protectionAfter).toEqual(inForce);
    });

    it('LONG with a trailing stop attaches it on fill', async () => {
      runtime.store.saveAgent({ ...runtime.store.getAgent(agentId)!, allowTrailingStop: true });
      complete = async () => ({
        text: JSON.stringify({ ...JSON.parse(long), stopLoss: Math.round(last() * 0.97), takeProfit: null, trailingStop: { kind: 'RATIO', callback: 0.02, activationPrice: null } }),
        inputTokens: null, outputTokens: null,
      });
      const record = (await runtime.runner.runCycle(agentId, 'RUN_ONCE'))!;
      expect(record.outcome).toBe('EXECUTED');
      expect(runtime.engine.protectionOfSync(accountId(), 'BTCUSDT').trailingStop).toEqual(trailing);
    });
  });
});
