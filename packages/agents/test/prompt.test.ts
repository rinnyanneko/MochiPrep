import { describe, expect, it } from 'vitest';
import type { MarketContext } from '@mochiprep/ai-providers';
import { DEFAULT_RISK_SETTINGS, type PositionView } from '@mochiprep/shared';
import { buildSystemPrompt, buildUserPrompt } from '../src/prompt';

const position = {
  symbol: 'BTCUSDT', side: 'LONG', quantity: '1', entryPrice: '100', liquidationPrice: '50',
  stopLoss: '95', takeProfit: '130',
  trailingStop: { kind: 'RATIO', callback: '0.02', activationPrice: null, extreme: '120', triggerPrice: '117.6' },
  externalProtection: [],
} as unknown as PositionView;

function context(p: PositionView | null): MarketContext {
  return {
    symbol: 'BTCUSDT', timeframe: '1h', now: 0, horizonSec: 3600, candles: [], indicators: {},
    quote: { bid: 1, ask: 2, last: 1.5, mark: 1.5, fundingRate: 0 },
    position: p,
    balance: { equity: '1', availableBalance: '1', unrealizedPnl: '0' },
    risk: DEFAULT_RISK_SETTINGS, strategy: 'x',
  } as unknown as MarketContext;
}

describe('prompts', () => {
  it('system prompt never mentions trailingStop when the switch is off', () => {
    const off = buildSystemPrompt({ allowTrailingStop: false });
    expect(off).not.toContain('trailingStop');
    expect(off).toContain('PROTECT');
    expect(buildSystemPrompt({ allowTrailingStop: true })).toContain('trailingStop');
  });

  it('user prompt shows the current protection and external orders', () => {
    const plain = buildUserPrompt(context(position));
    expect(plain).toContain('"stopLoss":"95"');
    expect(plain).toContain('"takeProfit":"130"');
    expect(plain).toContain('"triggerPrice":"117.6"');
    expect(plain).not.toContain('externalProtection');

    const external = { kind: 'STOP_LOSS', triggerPrice: '90', detail: 'owner algo' } as const;
    const withExternal = buildUserPrompt(context({ ...position, externalProtection: [external] }));
    expect(withExternal).toContain('"externalProtection"');
    expect(withExternal).toContain('owner algo');
    expect(withExternal).toMatch(/MochiPrep will not change them/);
  });
});
