import { describe, expect, it } from 'vitest';
import { DEFAULT_ACCOUNT_CONFIG, DEFAULT_RISK_SETTINGS, type AccountBalance, type AiDecisionData, type Quote } from '@mochiprep/shared';
import { d, evaluateRisk, isMarketable, linearPerp, marketFillPrice } from '../src/index';

const quote: Quote = {
  source: 'REPLAY', symbol: 'BTCUSDT', bid: '99', ask: '101', last: '100', mark: '100',
  markEstimated: false, fundingRate: '0.001', nextFundingTime: null, ts: 1_000,
};

describe('linear perpetual math', () => {
  it('calculates long and short PnL, margin, liquidation, and funding with decimals', () => {
    expect(linearPerp.pnl('LONG', '100', '110', '2').toFixed()).toBe('20');
    expect(linearPerp.pnl('SHORT', '100', '110', '2').toFixed()).toBe('-20');
    expect(linearPerp.initialMargin(d('200'), 4).toFixed()).toBe('50');
    expect(linearPerp.liquidationPrice({ side: 'LONG', entry: '100', quantity: '2', margin: '50', maintenanceRate: '0.005' }).toFixed()).toBe('75.5');
    expect(linearPerp.liquidationPrice({ side: 'SHORT', entry: '100', quantity: '2', margin: '50', maintenanceRate: '0.005' }).toFixed()).toBe('124.5');
    expect(linearPerp.funding('LONG', '100', '2', '0.001').toFixed()).toBe('0.2');
    expect(linearPerp.funding('SHORT', '100', '2', '0.001').toFixed()).toBe('-0.2');
    expect(linearPerp.funding('LONG', '100', '2', '-0.001').toFixed()).toBe('-0.2');
  });

  it('uses the observed touch, adverse slippage, tick rounding, and limit crossing rules', () => {
    const instrument = { symbol: 'BTCUSDT', marketType: 'CRYPTO_PERP' as const, baseCurrency: 'BTC', quoteCurrency: 'USDT', settlementCurrency: 'USDT', contractSize: '1', quantityStep: '0.0001', minQuantity: '0.0001', priceTick: '0.1', maxLeverage: 100, maintenanceMarginRate: '0.005', fundingIntervalHours: 8, venueContractValue: '0.01', venueLotSize: '0.01' };
    expect(marketFillPrice('BUY', '1', quote, { ...DEFAULT_ACCOUNT_CONFIG, slippageBps: '10', impactBpsPerMillion: '0' }, instrument).toFixed()).toBe('101.2');
    expect(marketFillPrice('SELL', '1', quote, { ...DEFAULT_ACCOUNT_CONFIG, slippageBps: '10', impactBpsPerMillion: '0' }, instrument).toFixed()).toBe('98.9');
    expect(isMarketable('BUY', '101', quote)).toBe(true);
    expect(isMarketable('BUY', '100.9', quote)).toBe(false);
    expect(isMarketable('SELL', '99', quote)).toBe(true);
  });
});

const balance: AccountBalance = {
  accountId: 'a', initialBalance: '10000', walletBalance: '10000', availableBalance: '10000',
  usedMargin: '0', orderMargin: '0', equity: '10000', realizedPnl: '0', unrealizedPnl: '0', feesPaid: '0', fundingPaid: '0',
};
const decision: AiDecisionData = {
  symbol: 'BTCUSDT', action: 'LONG', orderType: 'MARKET', entryPrice: null, stopLoss: 90,
  takeProfit: 120, reason: 'test', confidence: 0.8, expiresAt: null,
};
const riskInput = (overrides: Record<string, unknown> = {}) => ({
  decision, symbol: 'BTCUSDT' as const, settings: DEFAULT_RISK_SETTINGS, quote, now: 1_000,
  balance, openPositions: [], dailyPnl: '0', dayStartEquity: '10000', fees: DEFAULT_ACCOUNT_CONFIG,
  allowTrailingStop: false, protection: null,
  ...overrides,
});

describe('risk engine', () => {
  it('reports every applicable passing check and converts risk budget into a bounded quantity', () => {
    const result = evaluateRisk(riskInput());
    expect(result.approved).toBe(true);
    expect(result.checks.map((c) => c.id)).toEqual([
      'symbol', 'expiry', 'freshness', 'daily-loss', 'max-positions', 'no-existing',
      'stop-loss', 'take-profit', 'sizing', 'margin', 'leverage', 'notional', 'stop-before-liq',
    ]);
    expect(Number(result.sized!.quantity)).toBeGreaterThan(0);
    expect(d(result.sized!.notional).lte(DEFAULT_RISK_SETTINGS.maxNotional)).toBe(true);
    expect(d(result.sized!.margin).lte(balance.availableBalance)).toBe(true);
  });

  it.each([
    ['symbol', { decision: { ...decision, symbol: 'ETHUSDT' } }],
    ['expiry', { decision: { ...decision, expiresAt: new Date(500).toISOString() } }],
    ['freshness', { now: 40_000 }],
    ['daily-loss', { dailyPnl: '-250' }],
    ['max-positions', { settings: { ...DEFAULT_RISK_SETTINGS, maxOpenPositions: 1 }, openPositions: [{ symbol: 'ETHUSDT' }] }],
    ['no-existing', { openPositions: [{ symbol: 'BTCUSDT', side: 'LONG', quantity: '1' }] }],
    ['stop-loss', { decision: { ...decision, stopLoss: 102 } }],
    ['stop-loss', { decision: { ...decision, stopLoss: null } }],
    ['take-profit', { decision: { ...decision, takeProfit: 90 } }],
    ['sizing', { balance: { ...balance, equity: '0.01', availableBalance: '0.01' } }],
  ])('rejects a failed %s check', (id, overrides) => {
    const result = evaluateRisk(riskInput(overrides));
    expect(result.approved).toBe(false);
    expect(result.checks.find((c) => c.id === id)?.passed).toBe(false);
  });

  it('sizes from an assumed distance when settings allow a position without a stop', () => {
    const result = evaluateRisk(riskInput({
      settings: { ...DEFAULT_RISK_SETTINGS, requireStopLoss: false },
      decision: { ...decision, stopLoss: null },
    }));
    expect(result.approved).toBe(true);
    expect(Number(result.sized!.quantity)).toBeGreaterThan(0);
  });

  it('rejects a stop beyond the estimated liquidation price', () => {
    const result = evaluateRisk(riskInput({
      settings: { ...DEFAULT_RISK_SETTINGS, maxLeverage: 100 },
      decision: { ...decision, stopLoss: 1 },
    }));
    expect(result.checks.find((c) => c.id === 'stop-before-liq')?.passed).toBe(false);
    expect(result.approved).toBe(false);
  });
  const held = {
    id: 'p', accountId: 'a', symbol: 'BTCUSDT', side: 'LONG', quantity: '1', entryPrice: '100', leverage: 3,
    margin: '33.33', liquidationPrice: '70', status: 'OPEN', realizedPnl: '0', feesPaid: '0', fundingPaid: '0',
    maxQuantity: '1', exitPrice: null, lastFundingTime: 0, openedAt: 0, closedAt: null,
  };
  const protect = (o: Partial<AiDecisionData>): AiDecisionData => ({ ...decision, action: 'PROTECT', stopLoss: 95, ...o });
  const failed = (overrides: Record<string, unknown>) =>
    evaluateRisk(riskInput(overrides)).checks.filter((c) => !c.passed).map((c) => c.id);

  it('with the switch off, keeps an absent trailing stop and refuses any value, null included', () => {
    const base = { openPositions: [held] };
    expect(failed({ ...base, decision: protect({}) })).toEqual([]);
    expect(failed({ ...base, decision: protect({ trailingStop: null }) })).toEqual(['trailing-allowed']);
    expect(failed({ ...base, decision: protect({ trailingStop: { kind: 'RATIO', callback: 0.01, activationPrice: null } }) }))
      .toEqual(['trailing-allowed']);
    expect(failed({ decision: { ...decision, trailingStop: null } })).toEqual(['trailing-allowed']);
  });

  it('refuses a trailing stop on HOLD and CLOSE', () => {
    const trailingStop = { kind: 'RATIO' as const, callback: 0.01, activationPrice: null };
    expect(failed({ allowTrailingStop: true, decision: { ...decision, action: 'HOLD', trailingStop } })).toEqual(['trailing-stop']);
    expect(failed({ allowTrailingStop: true, openPositions: [held], decision: { ...decision, action: 'CLOSE', trailingStop } }))
      .toEqual(['trailing-stop']);
  });

  it('validates trailing ratios against the shared limits', () => {
    const ratio = (callback: number) => failed({ allowTrailingStop: true, decision: { ...decision, trailingStop: { kind: 'RATIO', callback, activationPrice: null } } });
    expect(ratio(0.0009)).toEqual(['trailing-stop']);
    expect(ratio(0.001)).toEqual([]);
    expect(ratio(0.3)).toEqual([]);
    expect(ratio(0.31)).toEqual(['trailing-stop']);
  });

  it('PROTECT approves without sizing and refuses a loosened stop beyond the risk budget', () => {
    const ok = evaluateRisk(riskInput({ openPositions: [held], decision: protect({}) }));
    expect(ok.approved).toBe(true);
    expect(ok.sized).toBeNull();
    // 5 BTC at a last of 100 with a 50 USDT budget allows a stop 10 away and no further.
    const five = [{ ...held, quantity: '5' }];
    expect(failed({ openPositions: five, decision: protect({ stopLoss: 90 }) })).toEqual([]);
    expect(failed({ openPositions: five, decision: protect({ stopLoss: 89.99 }) })).toEqual(['protect-risk']);
    expect(failed({ openPositions: [held], decision: protect({ stopLoss: 65 }) })).toEqual(['stop-before-liq']);
    expect(failed({ openPositions: [held], decision: protect({ stopLoss: null }) })).toEqual(['stop-loss']);
    expect(failed({ decision: protect({}) })).toEqual(['has-position']);
    expect(failed({ openPositions: [held], now: 40_000, decision: protect({}) })).toEqual(['freshness']);
  });

  it('PROTECT measures the risk budget only when the stop is new or looser than the current one', () => {
    // Long 5 at 100 with a stop at 90 and a 50 USDT budget; the last price has risen to 110.
    const five = [{ ...held, quantity: '5' }];
    const up = { ...quote, bid: '109', ask: '111', last: '110', mark: '110' };
    const current = { stopLoss: '90', takeProfit: '120', trailingStop: null };
    const at = (stopLoss: number, protection: unknown = current) =>
      evaluateRisk(riskInput({ openPositions: five, quote: up, protection, decision: protect({ stopLoss, takeProfit: 130 }) })).checks;
    const ids = (checks: ReturnType<typeof at>) => checks.filter((c) => !c.passed).map((c) => c.id);
    // Keeping 90 or tightening to 98 reduces risk, so the budget does not apply.
    expect(ids(at(90))).toEqual([]);
    expect(at(90).some((c) => c.id === 'protect-risk')).toBe(false);
    expect(ids(at(98))).toEqual([]);
    expect(at(98).some((c) => c.id === 'protect-risk')).toBe(false);
    // Loosening is measured from the last price: 100 is 10 away (50 USDT), 99.99 is past it.
    expect(ids(at(100, { ...current, stopLoss: '105' }))).toEqual([]);
    expect(at(100, { ...current, stopLoss: '105' }).some((c) => c.id === 'protect-risk')).toBe(true);
    expect(ids(at(89, current))).toEqual(['protect-risk']);
    // No current stop: always measured.
    expect(ids(at(98, { ...current, stopLoss: null }))).toEqual(['protect-risk']);
    expect(ids(at(98, null))).toEqual(['protect-risk']);
    // A short loosens upwards.
    const short = [{ ...held, side: 'SHORT', quantity: '5', liquidationPrice: '200' }];
    const down = { ...quote, bid: '89', ask: '91', last: '90', mark: '90' };
    const shortAt = (stopLoss: number) =>
      evaluateRisk(riskInput({ openPositions: short, quote: down, protection: { ...current, stopLoss: '110' }, decision: protect({ stopLoss, takeProfit: 80 }) }))
        .checks.filter((c) => !c.passed).map((c) => c.id);
    expect(shortAt(102)).toEqual([]);
    expect(shortAt(110)).toEqual([]);
    expect(shortAt(111)).toEqual(['protect-risk']);
  });

  it('PROTECT skips stop-before-liq when the liquidation price is unknown', () => {
    // OKX sends liqPx empty, or 0, for some positions.
    for (const liquidationPrice of ['', '0']) {
      const checks = evaluateRisk(riskInput({ openPositions: [{ ...held, liquidationPrice }], decision: protect({}) })).checks;
      expect(checks.some((c) => c.id === 'stop-before-liq')).toBe(false);
      expect(checks.filter((c) => !c.passed)).toEqual([]);
    }
  });
});
