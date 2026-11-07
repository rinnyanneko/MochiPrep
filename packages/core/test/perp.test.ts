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
    const instrument = { symbol: 'BTCUSDT', marketType: 'CRYPTO_PERP' as const, baseCurrency: 'BTC', quoteCurrency: 'USDT', settlementCurrency: 'USDT', contractSize: '1', quantityStep: '0.0001', minQuantity: '0.0001', priceTick: '0.1', maxLeverage: 100, maintenanceMarginRate: '0.005', fundingIntervalHours: 8 };
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
    ['take-profit', { decision: { ...decision, takeProfit: 90 } }],
    ['sizing', { balance: { ...balance, equity: '0.01', availableBalance: '0.01' } }],
  ])('rejects a failed %s check', (id, overrides) => {
    const result = evaluateRisk(riskInput(overrides));
    expect(result.approved).toBe(false);
    expect(result.checks.find((c) => c.id === id)?.passed).toBe(false);
  });

  it('rejects a stop beyond the estimated liquidation price', () => {
    const result = evaluateRisk(riskInput({
      settings: { ...DEFAULT_RISK_SETTINGS, maxLeverage: 100 },
      decision: { ...decision, stopLoss: 1 },
    }));
    expect(result.checks.find((c) => c.id === 'stop-before-liq')?.passed).toBe(false);
    expect(result.approved).toBe(false);
  });
});
