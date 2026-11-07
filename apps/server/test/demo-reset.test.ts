import { afterEach, describe, expect, it } from 'vitest';
import type { DatabaseSync } from 'node:sqlite';
import { openDatabase } from '../src/db/database';
import { Runtime } from '../src/runtime';

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
});
