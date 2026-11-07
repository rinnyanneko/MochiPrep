import { afterEach, describe, expect, it } from 'vitest';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { DEFAULT_RISK_SETTINGS } from '@mochiprep/shared';
import { openDatabase } from '../src/db/database';
import { Runtime } from '../src/runtime';

describe('file database recovery and secret lifetime', () => {
  const dirs: string[] = [];
  afterEach(() => {
    for (const dir of dirs) rmSync(dir, { recursive: true, force: true });
    dirs.length = 0;
  });

  it('restores paper state and replay cursor, pauses agents, and loses keys', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'mochiprep-recovery-'));
    dirs.push(dir);
    const path = join(dir, 'state.sqlite');
    const secret = 'custom-secret-never-persist-987654321';

    const firstDb = openDatabase(path);
    const first = new Runtime({ db: firstDb, replayMinutes: 5_900 });
    const account = first.engine.createAccount({
      name: 'persistent', source: 'REPLAY',
      config: { slippageBps: '0', impactBpsPerMillion: '0' },
    });
    first.engine.placeOrderSync({
      accountId: account.id, clientOrderId: 'open', symbol: 'BTCUSDT', side: 'BUY', type: 'MARKET',
      quantity: '0.01', leverage: 3,
    });
    const pending = first.engine.placeOrderSync({
      accountId: account.id, clientOrderId: 'pending', symbol: 'ETHUSDT', side: 'BUY', type: 'LIMIT',
      quantity: '0.01', price: '1', leverage: 3,
    }).order;
    first.store.insertProvider({
      id: 'remote', name: 'remote', kind: 'OPENAI', baseUrl: null, defaultModel: 'model',
      inputPricePerMTok: null, outputPricePerMTok: null, createdAt: 1,
    });
    first.setKey('remote', secret);
    const agent = first.createAgent({
      name: 'persistent agent', providerId: 'remote', model: 'model', symbol: 'BTCUSDT', timeframe: '15m',
      strategy: '', accountId: account.id, risk: DEFAULT_RISK_SETTINGS, intervalSec: 900,
    });
    first.runner.start(agent.id);
    await first.stepReplay(3);
    const cursor = first.replay.position;
    await first.stop();
    firstDb.close();

    expect(readFileSync(path).includes(Buffer.from(secret))).toBe(false);

    const secondDb = openDatabase(path);
    const second = new Runtime({ db: secondDb, replayMinutes: 5_900 });
    expect(second.store.getAccount(account.id)?.name).toBe('persistent');
    expect(second.store.getOpenPosition(account.id, 'BTCUSDT')).toBeDefined();
    expect(second.store.getOrder(pending.id)?.status).toBe('OPEN');
    expect(second.replay.position).toBe(cursor);
    expect(second.store.getAgent(agent.id)).toMatchObject({ status: 'PAUSED', running: false, nextRunAt: null });
    expect(second.providerViews().find((p) => p.id === 'remote')?.hasKey).toBe(false);
    await second.stop();
    secondDb.close();
  });
});
