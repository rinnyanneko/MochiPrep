import { afterEach, describe, expect, it } from 'vitest';
import type { DatabaseSync } from 'node:sqlite';
import { openDatabase } from '../src/db/database';
import { Runtime } from '../src/runtime';
import { createRemoteProvider } from '@mochiprep/ai-providers';

describe('Demo reset determinism', () => {
  let db: DatabaseSync | undefined;
  let runtime: Runtime | undefined;
  afterEach(async () => {
    if (runtime) await runtime.stop();
    db?.close();
  });

  it('replays equal account values and economic fill sequences after reset', async () => {
    db = openDatabase(':memory:');
    runtime = new Runtime({ db, replayMinutes: 6_500 });
    runtime.startDemo();

    const run = async () => {
      for (const agent of runtime!.store.listAgents().filter((a) => a.isDemo)) runtime!.runner.start(agent.id);
      await runtime!.stepReplay(600);
      return runtime!.store.listAccounts().filter((a) => a.isDemo).sort((a, b) => a.name.localeCompare(b.name)).map((account) => ({
        name: account.name,
        balance: runtime!.engine.balanceOf(account.id),
        fills: runtime!.store.listFills(account.id, 10_000).reverse().map(({ id: _id, orderId: _orderId, accountId: _accountId, ...economic }) => economic),
      })).map(({ name, balance, fills }) => ({
        name,
        balance: {
          initialBalance: balance.initialBalance, walletBalance: balance.walletBalance,
          availableBalance: balance.availableBalance, usedMargin: balance.usedMargin, orderMargin: balance.orderMargin,
          equity: balance.equity, realizedPnl: balance.realizedPnl, unrealizedPnl: balance.unrealizedPnl,
          feesPaid: balance.feesPaid, fundingPaid: balance.fundingPaid,
        },
        fills,
      }));
    };

    const first = await run();
    expect(first.some((account) => account.fills.length > 0)).toBe(true);
    await runtime.resetDemo();
    const second = await run();
    expect(second).toEqual(first);
  });

  it.each(['reset', 'exit'] as const)('%s waits for queued batches before changing demo ownership', async (action) => {
    let enter!: () => void;
    let release!: () => void;
    const entered = new Promise<void>((done) => { enter = done; });
    const held = new Promise<void>((done) => { release = done; });
    let calls = 0;
    db = openDatabase(':memory:');
    runtime = new Runtime({ db, replayMinutes: 6_500, providerFactory: (config, key) => createRemoteProvider(config, key, async () => {
      if (calls++ === 0) { enter(); await held; }
      return new Response(JSON.stringify({ choices: [{ message: { content: JSON.stringify({
        symbol: 'BTCUSDT', action: 'HOLD', orderType: 'MARKET', entryPrice: null, stopLoss: null,
        takeProfit: null, reason: 'held', confidence: 0.8, expiresAt: null,
      }) } }] }));
    }) });
    runtime.store.insertProvider({ id: 'remote', name: 'test', kind: 'OPENAI_COMPATIBLE', baseUrl: 'https://local-transport.test', defaultModel: 'm', inputPricePerMTok: null, outputPricePerMTok: null, createdAt: 1 });
    runtime.setKey('remote', 'local-transport-key');
    runtime.startDemo();
    const start = runtime.replay.position;
    const agent = runtime.store.listAgents()[0]!;
    runtime.store.saveAgent({ ...agent, providerId: 'remote', intervalSec: 10 });
    runtime.runner.start(agent.id);
    const first = runtime.stepReplay(1);
    await entered;
    const queued = runtime.stepReplay(10);
    const changing = action === 'reset' ? runtime.resetDemo() : runtime.exitDemo();
    release();
    await Promise.all([first, queued, changing]);
    expect(calls).toBe(11);
    expect(runtime.replay.position).toBe(action === 'reset' ? start : start + 11);
    expect(runtime.store.listDecisions({ limit: 100 })).toEqual([]);
    expect(runtime.store.listAgents().some((a) => a.id === agent.id)).toBe(false);
  });
});
