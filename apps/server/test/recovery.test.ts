import { afterEach, describe, expect, it } from 'vitest';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AiProvider } from '@mochiprep/ai-providers';
import { PaperExecutionEngine } from '@mochiprep/core';
import { DEFAULT_RISK_SETTINGS, type Quote } from '@mochiprep/shared';
import { openDatabase } from '../src/db/database';
import { SqliteStore } from '../src/db/store';
import { Runtime } from '../src/runtime';

const holdProvider: AiProvider = {
  kind: 'OPENAI',
  complete: async () => ({
    text: JSON.stringify({
      symbol: 'BTCUSDT', action: 'HOLD', orderType: 'MARKET', entryPrice: null, stopLoss: null,
      takeProfit: null, reason: 'wait', confidence: 0.5, expiresAt: null,
    }),
    inputTokens: null,
    outputTokens: null,
  }),
};

describe('file database recovery', () => {
  const dirs: string[] = [];
  afterEach(() => {
    for (const dir of dirs) rmSync(dir, { recursive: true, force: true });
    dirs.length = 0;
  });

  it('restores paper state, replay cursor and AI keys, and pauses agents', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'mochiprep-recovery-'));
    dirs.push(dir);
    const path = join(dir, 'state.sqlite');
    const secret = 'custom-ai-key-987654321';

    const firstDb = openDatabase(path);
    const first = new Runtime({ db: firstDb, replayMinutes: 5_900, providerFactory: () => holdProvider });
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
    // Still armed here, so only the restart itself can explain the paused state asserted below.
    expect(first.store.getAgent(agent.id)).toMatchObject({ status: 'WAITING', running: true });
    const cursor = first.replay.position;
    await first.stop();
    firstDb.close();

    const secondDb = openDatabase(path);
    const second = new Runtime({ db: secondDb, replayMinutes: 5_900, providerFactory: () => holdProvider });
    expect(second.store.getAccount(account.id)?.name).toBe('persistent');
    expect(second.store.getOpenPosition(account.id, 'BTCUSDT')).toBeDefined();
    expect(second.store.getOrder(pending.id)?.status).toBe('OPEN');
    expect(second.replay.position).toBe(cursor);
    expect(second.store.getAgent(agent.id)).toMatchObject({
      status: 'PAUSED', running: false, nextRunAt: null, statusDetail: 'Paused because the backend restarted',
    });
    expect(second.providerViews().find((p) => p.id === 'remote')?.hasKey).toBe(true);
    expect(second.providerFor(second.store.getProvider('remote')!)).toBe(holdProvider);
    await second.stop();
    secondDb.close();
  });

  it('trailing extreme survives a restart', () => {
    const dir = mkdtempSync(join(tmpdir(), 'mochiprep-trailing-'));
    dirs.push(dir);
    const path = join(dir, 'state.sqlite');
    let quote: Quote = { source: 'REPLAY', symbol: 'BTCUSDT', bid: '99.9', ask: '100.1', last: '100', mark: '100', markEstimated: false, fundingRate: '0', nextFundingTime: null, ts: 1_000 };
    const at = (ts: number, last: string): Quote => ({ ...quote, bid: last, ask: last, last, mark: last, ts });
    const open = (file: string) => {
      const db = openDatabase(file);
      const store = new SqliteStore(db);
      return { db, store, engine: new PaperExecutionEngine({ store, getQuote: () => quote, now: () => quote.ts }) };
    };

    const first = open(path);
    const account = first.engine.createAccount({ name: 'trail', source: 'REPLAY', config: { slippageBps: '0', impactBpsPerMillion: '0' } });
    first.engine.placeOrderSync({
      accountId: account.id, clientOrderId: 'open', symbol: 'BTCUSDT', side: 'BUY', type: 'MARKET',
      quantity: '0.01', leverage: 3, trailingStop: { kind: 'RATIO', callback: '0.01', activationPrice: null },
    });
    quote = at(2_000, '110');
    first.engine.onQuote(quote);
    expect(first.engine.positionViews(account.id)[0]!.trailingStop).toMatchObject({ extreme: '110', triggerPrice: '108.9' });
    first.db.close();

    // A fresh engine on the same file must resume from 110, not restart at the next last.
    const second = open(path);
    quote = at(3_000, '109');
    second.engine.onQuote(quote);
    expect(second.engine.positionViews(account.id)[0]!.trailingStop).toMatchObject({ extreme: '110', triggerPrice: '108.9' });
    quote = at(4_000, '108.89');
    second.engine.onQuote(quote);
    expect(second.engine.positionViews(account.id)).toEqual([]);
    // Filled at that quote's bid, floored to BTCUSDT's 0.1 tick.
    expect(second.store.listFills(account.id, 10)[0]).toMatchObject({ side: 'SELL', price: '108.8', liquidity: 'TAKER' });
    second.db.close();
  });
});
