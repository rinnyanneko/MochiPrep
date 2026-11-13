import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import type { DatabaseSync } from 'node:sqlite';
import type { AiProvider } from '@mochiprep/ai-providers';
import { DEFAULT_RISK_SETTINGS, type ExecutionMode } from '@mochiprep/shared';
import { buildApp } from '../src/app';
import { openDatabase } from '../src/db/database';
import { Runtime } from '../src/runtime';

describe('suggestion mode', () => {
  let db: DatabaseSync;
  let runtime: Runtime;
  let accountId: string;
  let price: number;
  let app: ReturnType<typeof buildApp>['app'];
  let token: string;
  let aiExpiresAt: string | null;
  /** Replaces the default LONG with another decision built from the price the model saw. */
  let override: ((price: number) => object) | null;

  const provider: AiProvider = {
    kind: 'OPENAI',
    complete: async (request) => {
      price = Number(request.context.quote.last);
      if (override) return { text: JSON.stringify(override(price)), inputTokens: 1, outputTokens: 1 };
      return {
        text: JSON.stringify({
          symbol: 'BTCUSDT', action: 'LONG', orderType: 'MARKET', entryPrice: null, stopLoss: Math.round(price * 0.98),
          takeProfit: Math.round(price * 1.04), reason: 'test', confidence: 0.7, expiresAt: aiExpiresAt,
        }),
        inputTokens: 1,
        outputTokens: 1,
      };
    },
  };

  const agent = (executionMode: ExecutionMode = 'SIGNAL') =>
    runtime.createAgent({
      name: 'a', providerId: 'remote', model: 'm', symbol: 'BTCUSDT', timeframe: '15m', strategy: '', accountId,
      risk: DEFAULT_RISK_SETTINGS, intervalSec: 900, executionMode,
    });
  const orders = () => runtime.store.listOrders(accountId, 50).filter((o) => o.type === 'MARKET');
  const raise = async () => (await runtime.runner.runCycle(agent().id, 'RUN_ONCE'))!;

  beforeEach(() => {
    aiExpiresAt = null;
    override = null;
    db = openDatabase(':memory:');
    runtime = new Runtime({ db, replayMinutes: 6_000, providerFactory: () => provider });
    runtime.store.insertProvider({ id: 'remote', name: 'r', kind: 'OPENAI', baseUrl: null, defaultModel: 'm', inputPricePerMTok: null, outputPricePerMTok: null, createdAt: 1 });
    runtime.setKey('remote', 'ai-key');
    accountId = runtime.engine.createAccount({ name: 'a', source: 'REPLAY' }).id;
    ({ app, token } = buildApp(runtime, { host: '127.0.0.1', port: 8787, dataDir: '.', allowedOrigins: ['http://localhost:8787'] }));
  });

  afterEach(async () => {
    await app.close();
    await runtime.stop();
    db.close();
  });

  const call = (method: 'GET' | 'POST', url: string) =>
    app.inject({ method, url, headers: { host: 'localhost:8787', authorization: `Bearer ${token}` } });

  const holdAcceptance = async (id: string) => {
    const getPositions = runtime.engine.getPositions;
    let entered!: () => void;
    let release!: () => void;
    const waiting = new Promise<void>((resolve) => { entered = resolve; });
    const held = new Promise<void>((resolve) => { release = resolve; });
    runtime.engine.getPositions = async (...args) => {
      entered();
      await held;
      return getPositions.apply(runtime.engine, args);
    };
    const result = runtime.runner.executeSignal(id);
    await waiting;
    return async () => {
      runtime.engine.getPositions = getPositions;
      release();
      return result;
    };
  };

  it.each(['stop', 'stop-all'] as const)('cancels a held accepted entry on %s even when its agent is idle', async (action) => {
    const signal = await raise();
    expect(runtime.store.getAgent(signal.agentId)?.status).toBe('IDLE');
    const finish = await holdAcceptance(signal.id);
    const stopped = await call('POST', action === 'stop' ? `/api/agents/${signal.agentId}/stop` : '/api/agents/stop-all');
    const result = await finish();
    expect(stopped.statusCode).toBe(200);
    expect(result).toMatchObject({ outcome: 'SIGNAL_EXPIRED', signalExpiresAt: null });
    if (action === 'stop-all') {
      expect(stopped.json().stopped).toBe(1);
      expect(result.outcomeDetail).toBe('Cleared by Stop All');
    }
    expect(orders()).toEqual([]);
    expect(runtime.store.getDecision(signal.id)).toEqual(result);
  });

  it('suggests instead of ordering, with entry, size and an expiry', async () => {
    const signal = await raise();
    expect(signal.outcome).toBe('SIGNAL');
    expect(signal.risk!.approved).toBe(true);
    expect(signal.risk!.sized).not.toBeNull();
    expect(signal.signalExpiresAt).toBe(runtime.replay.now() + 900_000);
    expect(orders()).toHaveLength(0);
    expect(runtime.store.listPendingSignals().map((s) => s.id)).toEqual([signal.id]);
  });

  it('still trades directly in AUTO mode', async () => {
    const record = await runtime.runner.runCycle(agent('AUTO').id, 'RUN_ONCE');
    expect(record!.outcome).toBe('EXECUTED');
    expect(runtime.store.listPendingSignals()).toHaveLength(0);
  });

  it('honours an earlier expiry from the model', async () => {
    aiExpiresAt = new Date(runtime.replay.now() + 60_000).toISOString();
    expect((await raise()).signalExpiresAt).toBe(runtime.replay.now() + 60_000);
  });

  it('Analyze Now never creates a suggestion', async () => {
    const record = await runtime.runner.runCycle(agent().id, 'ANALYZE');
    expect(record!.outcome).toBe('ANALYSIS_ONLY');
    expect(runtime.store.listPendingSignals()).toHaveLength(0);
  });

  it('does not suggest what the risk engine rejects', async () => {
    const provider2 = provider.complete;
    provider.complete = async (request) => {
      const reply = await provider2(request);
      return { ...reply, text: JSON.stringify({ ...JSON.parse(reply.text), stopLoss: null }) };
    };
    try {
      expect((await raise()).outcome).toBe('RISK_REJECTED');
      expect(runtime.store.listPendingSignals()).toHaveLength(0);
    } finally {
      provider.complete = provider2;
    }
  });

  it('places the order on accept, with the stop and target, linked to the decision', async () => {
    const signal = await raise();
    const result = await runtime.runner.executeSignal(signal.id);
    expect(result.outcome).toBe('EXECUTED');
    expect(result.orderId).toBe(orders()[0]!.id);
    expect(result.fillIds).toHaveLength(1);
    const [position] = runtime.engine.positionViews(accountId);
    expect(position).toMatchObject({ side: 'LONG', stopLoss: String(signal.decision!.stopLoss), takeProfit: String(signal.decision!.takeProfit) });
    expect(runtime.store.getDecision(signal.id)!.outcome).toBe('EXECUTED');
    expect(runtime.store.listPendingSignals()).toHaveLength(0);
  });

  it('expires a suggestion after the agent changes to another account for the same pair', async () => {
    const signal = await raise();
    expect(signal.outcome).toBe('SIGNAL');
    const replacement = runtime.engine.createAccount({ name: 'replacement', source: 'REPLAY' }).id;
    runtime.store.saveAgent({ ...runtime.store.getAgent(signal.agentId)!, accountId: replacement });
    const result = await call('POST', `/api/signals/${signal.id}/execute`);
    expect(result.statusCode).toBe(200);
    expect(result.json()).toMatchObject({ outcome: 'SIGNAL_EXPIRED', signalExpiresAt: null, accountId });
    expect(runtime.store.getDecision(signal.id)!.outcome).toBe('SIGNAL_EXPIRED');
    expect(runtime.store.listOrders(replacement, 50)).toEqual([]);
    expect(orders()).toEqual([]);
  });

  it('resizes from current equity rather than the original analysis', async () => {
    const signal = await raise();
    const original = Number(signal.risk!.sized!.quantity);
    db.prepare('UPDATE accounts SET wallet_balance=? WHERE id=?').run('5000', accountId);
    const result = await runtime.runner.executeSignal(signal.id);
    expect(Number(result.risk!.sized!.quantity)).toBeLessThan(original);
    expect(orders()[0]!.quantity).toBe(result.risk!.sized!.quantity);
  });

  it('re-runs risk at accept time and refuses if things changed', async () => {
    const signal = await raise();
    runtime.engine.placeOrderSync({ accountId, clientOrderId: 'manual', symbol: 'BTCUSDT', side: 'BUY', type: 'MARKET', quantity: '0.001', leverage: 3 });
    const result = await runtime.runner.executeSignal(signal.id);
    expect(result.outcome).toBe('RISK_REJECTED');
    expect(result.outcomeDetail).toContain('No existing position');
    expect(orders()).toHaveLength(1);
  });

  it('cannot be executed twice', async () => {
    const signal = await raise();
    await runtime.runner.executeSignal(signal.id);
    await expect(runtime.runner.executeSignal(signal.id)).rejects.toThrow(/no longer pending/);
    expect(orders()).toHaveLength(1);
  });

  it('survives a double click arriving together', async () => {
    const signal = await raise();
    const results = await Promise.allSettled([runtime.runner.executeSignal(signal.id), runtime.runner.executeSignal(signal.id)]);
    expect(results.filter((r) => r.status === 'fulfilled')).toHaveLength(1);
    expect(orders()).toHaveLength(1);
  });

  it('refuses a dismissed suggestion', async () => {
    const signal = await raise();
    expect(runtime.runner.dismissSignal(signal.id).outcome).toBe('SIGNAL_DISMISSED');
    await expect(runtime.runner.executeSignal(signal.id)).rejects.toThrow(/no longer pending/);
    expect(orders()).toHaveLength(0);
  });

  it('expires on the feed clock, and an expired one cannot trade', async () => {
    const signal = await raise();
    runtime.replay.seek(runtime.replay.position + 16);
    const result = await runtime.runner.executeSignal(signal.id);
    expect(result.outcome).toBe('SIGNAL_EXPIRED');
    expect(orders()).toHaveLength(0);
  });

  it('sweeps expired suggestions as time passes', async () => {
    const signal = await raise();
    await runtime.stepReplay(14);
    expect(runtime.store.getDecision(signal.id)!.outcome).toBe('SIGNAL');
    await runtime.stepReplay(1);
    expect(runtime.store.getDecision(signal.id)!.outcome).toBe('SIGNAL_EXPIRED');
  });

  it('keeps one live suggestion per agent', async () => {
    const created = agent();
    const first = (await runtime.runner.runCycle(created.id, 'RUN_ONCE'))!;
    const second = (await runtime.runner.runCycle(created.id, 'RUN_ONCE'))!;
    expect(runtime.store.getDecision(first.id)).toMatchObject({ outcome: 'SIGNAL_EXPIRED', outcomeDetail: 'Replaced by a newer suggestion' });
    expect(runtime.store.listPendingSignals().map((s) => s.id)).toEqual([second.id]);
  });

  it('holds the tape at a new suggestion so it can be read', async () => {
    const created = agent();
    runtime.runner.start(created.id);
    const before = runtime.replay.position;
    runtime.setPlaying(true);
    await runtime.stepReplay(100);
    expect(runtime.replay.position).toBe(before + 1);
    expect(runtime.replayState().playing).toBe(false);
  });

  describe('PROTECT', () => {
    const last = () => Number(runtime.replay.getQuote('BTCUSDT')!.last);
    const protect = (stopLoss: number) => () => ({
      symbol: 'BTCUSDT', action: 'PROTECT', orderType: 'MARKET', entryPrice: null, stopLoss, takeProfit: null,
      reason: 'tighten', confidence: 0.6, expiresAt: null,
    });
    const openWithStop = (stopLoss: string) =>
      runtime.engine.placeOrderSync({ accountId, clientOrderId: 'open', symbol: 'BTCUSDT', side: 'BUY', type: 'MARKET', quantity: '0.01', leverage: 3, stopLoss });

    it('leaves protection unchanged when paused during acceptance', async () => {
      const loose = String(Math.round(last() * 0.95));
      openWithStop(loose);
      override = protect(Math.round(last() * 0.98));
      const signal = await raise();
      expect(signal.outcome).toBe('SIGNAL');
      const finish = await holdAcceptance(signal.id);
      runtime.runner.pause(signal.agentId);
      const result = await finish();
      expect(result).toMatchObject({ outcome: 'SIGNAL_EXPIRED', signalExpiresAt: null });
      expect(runtime.engine.protectionOfSync(accountId, 'BTCUSDT').stopLoss).toBe(loose);
    });

    it('a PROTECT signal expires when protection changed before it was accepted', async () => {
      const loose = String(Math.round(last() * 0.95));
      openWithStop(loose);
      override = protect(Math.round(last() * 0.98));
      const signal = await raise();
      expect(signal.outcome).toBe('SIGNAL');
      expect(signal.protectionBefore).toEqual({ stopLoss: loose, takeProfit: null, trailingStop: null });
      runtime.engine.setProtectionSync(accountId, 'BTCUSDT', { stopLoss: String(Math.round(last() * 0.96)), takeProfit: null, trailingStop: null });
      const result = await runtime.runner.executeSignal(signal.id);
      expect(result).toMatchObject({ outcome: 'SIGNAL_EXPIRED', outcomeDetail: 'Protection changed since this suggestion' });
      expect(runtime.engine.protectionOfSync(accountId, 'BTCUSDT').stopLoss).toBe(String(Math.round(last() * 0.96)));
    });

    it('an unchanged PROTECT signal is re-assessed and executed on accept', async () => {
      openWithStop(String(Math.round(last() * 0.95)));
      const tight = Math.round(last() * 0.98);
      override = protect(tight);
      const signal = await raise();
      const result = await runtime.runner.executeSignal(signal.id);
      expect(result).toMatchObject({ outcome: 'EXECUTED', outcomeDetail: 'Protection updated' });
      expect(result.protectionAfter!.stopLoss).toBe(String(tight));
      expect(runtime.engine.protectionOfSync(accountId, 'BTCUSDT').stopLoss).toBe(String(tight));
    });
  });

  describe('API', () => {
    it('lists, executes, and answers 409 once settled', async () => {
      const signal = await raise();
      expect((await call('GET', '/api/signals')).json().map((s: { id: string }) => s.id)).toEqual([signal.id]);
      const response = await call('POST', `/api/signals/${signal.id}/execute`);
      expect(response.json().outcome).toBe('EXECUTED');
      expect((await call('POST', `/api/signals/${signal.id}/execute`)).statusCode).toBe(409);
      expect((await call('GET', '/api/signals')).json()).toEqual([]);
    });

    it('dismisses, and 404s an unknown id', async () => {
      const signal = await raise();
      expect((await call('POST', `/api/signals/${signal.id}/dismiss`)).json().outcome).toBe('SIGNAL_DISMISSED');
      expect((await call('POST', '/api/signals/nope/execute')).statusCode).toBe(404);
    });

    it('Stop All clears pending suggestions', async () => {
      const signal = await raise();
      await call('POST', '/api/agents/stop-all');
      expect(runtime.store.getDecision(signal.id)).toMatchObject({ outcome: 'SIGNAL_EXPIRED', outcomeDetail: 'Cleared by Stop All' });
    });

    it('saves the trailing-stop switch with PUT', async () => {
      const created = agent();
      expect(created.allowTrailingStop).toBe(false);
      const response = await app.inject({
        method: 'PUT', url: `/api/agents/${created.id}`, headers: { host: 'localhost:8787', authorization: `Bearer ${token}` },
        payload: {
          name: 'a', providerId: 'remote', model: 'm', symbol: 'BTCUSDT', timeframe: '15m', strategy: '', accountId,
          intervalSec: 900, executionMode: 'SIGNAL', allowTrailingStop: true,
        },
      });
      expect(response.statusCode).toBe(200);
      expect(runtime.store.getAgent(created.id)!.allowTrailingStop).toBe(true);
    });

    it('new agents default to suggesting', async () => {
      const response = await app.inject({
        method: 'POST', url: '/api/agents', headers: { host: 'localhost:8787', authorization: `Bearer ${token}` },
        payload: { name: 'n', providerId: 'remote', model: 'm', symbol: 'BTCUSDT', timeframe: '15m', strategy: '', accountId, intervalSec: 900 },
      });
      expect(response.json().executionMode).toBe('SIGNAL');
    });
  });
});
