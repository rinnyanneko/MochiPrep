import { describe, expect, it } from 'vitest';
import { DEFAULT_RISK_SETTINGS, type AiDecisionData } from '@mochiprep/shared';
import { mockProvider, MOCK_MODELS, type MarketContext } from '../src/index';

const START = Date.UTC(2026, 0, 5);

const context = (now: number, overrides: Partial<MarketContext> = {}) =>
  ({
    symbol: 'BTCUSDT',
    timeframe: '15m',
    now,
    horizonSec: 900,
    candles: [],
    indicators: { ema20: 110, ema50: 100, rsi14: 55, macd: 2, macdSignal: 1, macdHistogram: 1, atr14: 100 },
    quote: { last: '1000.0' },
    position: null,
    risk: DEFAULT_RISK_SETTINGS,
    strategy: '',
    ...overrides,
  }) as unknown as MarketContext;

const decide = async (model: string, ctx: MarketContext) =>
  JSON.parse((await mockProvider.complete({ model, system: '', user: '', context: ctx })).text) as AiDecisionData;

describe('mock provider', () => {
  it('repeats its decision for the same input and labels it as a demonstration', async () => {
    const first = await decide(MOCK_MODELS.momentum, context(START));
    expect(await decide(MOCK_MODELS.momentum, context(START))).toEqual(first);
    expect(first.action).toBe('LONG');
    expect(first.reason).toContain('[DEMO');
  });

  it('cycles the reckless violations on a schedule that divides evenly by three', async () => {
    const kinds = new Set<string>();
    for (let cycle = 0; cycle < 6; cycle++) {
      const decision = await decide(MOCK_MODELS.reckless, context(START + cycle * 900_000));
      const isSafe = decision.action === 'LONG' ? decision.stopLoss! < 1000 : decision.stopLoss! > 1000;
      kinds.add(decision.stopLoss === null ? 'no-stop' : isSafe ? 'valid' : 'wrong-side');
    }
    expect([...kinds].sort()).toEqual(['no-stop', 'valid', 'wrong-side']);
  });
});
