import type {
  AccountBalance,
  AiDecisionData,
  PaperAccountConfig,
  Position,
  PositionSide,
  Quote,
  RiskCheck,
  RiskResult,
  RiskSettings,
  SizedTrade,
  SymbolId,
} from '@mochiprep/shared';
import { d, floorToStep, min, str, type Dec } from '../decimal';
import { getInstrument } from '../instruments';
import { linearPerp } from '../trading/perp';

export interface RiskInput {
  decision: AiDecisionData;
  /** The symbol this agent is allowed to trade. */
  symbol: SymbolId;
  settings: RiskSettings;
  quote: Quote | null;
  now: number;
  balance: AccountBalance;
  openPositions: Position[];
  /** Realized + unrealized since 00:00 UTC, and the equity base it is measured against. */
  dailyPnl: string;
  dayStartEquity: string;
  fees: PaperAccountConfig;
}

/** Keeps a little available balance free for the entry fee and adverse slippage. */
const MARGIN_SAFETY = '0.98';
/** Sizing distance when settings allow a position without a stop. */
const ASSUMED_STOP_FRACTION = '0.02';

/**
 * Pure function: same input, same verdict. It sees AI output only as untrusted data, and
 * nothing the model says can change a limit. Every check is reported, passed or not.
 */
export function evaluateRisk(input: RiskInput): RiskResult {
  const { decision, settings, quote, now, balance, openPositions } = input;
  const checks: RiskCheck[] = [];
  const check = (id: string, label: string, passed: boolean, detail: string) => {
    checks.push({ id, label, passed, detail });
    return passed;
  };
  const finish = (sized: SizedTrade | null = null): RiskResult => {
    const failed = checks.filter((c) => !c.passed);
    return {
      approved: failed.length === 0,
      checks,
      rejectionReasons: failed.map((c) => `${c.label}: ${c.detail}`),
      sized: failed.length === 0 ? sized : null,
    };
  };

  check(
    'symbol',
    'Symbol allowed',
    decision.symbol === input.symbol,
    decision.symbol === input.symbol
      ? `${input.symbol} matches the agent`
      : `agent trades ${input.symbol}, AI asked for ${decision.symbol}`,
  );

  if (decision.action === 'HOLD') return finish();

  const expiry = decision.expiresAt ? Date.parse(decision.expiresAt) : null;
  check(
    'expiry',
    'Decision not expired',
    expiry === null || expiry > now,
    expiry === null ? 'no expiry given' : `expires ${new Date(expiry).toISOString()}`,
  );

  const existing = openPositions.find((p) => p.symbol === input.symbol);
  if (decision.action === 'CLOSE') {
    check('has-position', 'Position to close', !!existing, existing ? `${existing.side} ${existing.quantity}` : `no open ${input.symbol} position`);
    return finish();
  }

  const quoteAgeSec = quote ? (now - quote.ts) / 1000 : Infinity;
  const fresh = check(
    'freshness',
    'Market data fresh',
    quoteAgeSec <= settings.maxQuoteAgeSec,
    quote ? `quote age ${quoteAgeSec.toFixed(1)}s, limit ${settings.maxQuoteAgeSec}s` : 'no quote available',
  );

  const dayBase = d(input.dayStartEquity);
  const lossLimit = dayBase.mul(settings.maxDailyLossPct).div(100);
  check(
    'daily-loss',
    'Daily loss limit',
    d(input.dailyPnl).gt(lossLimit.neg()),
    `today ${d(input.dailyPnl).toFixed(2)} USDT, limit -${lossLimit.toFixed(2)} USDT`,
  );

  check(
    'max-positions',
    'Open position limit',
    openPositions.length < settings.maxOpenPositions,
    `${openPositions.length} open, limit ${settings.maxOpenPositions}`,
  );

  check(
    'no-existing',
    'No existing position',
    !existing,
    existing ? `already ${existing.side}; CLOSE first (no pyramiding or flipping)` : 'flat on this symbol',
  );

  if (!fresh || !quote) return finish();

  const side: PositionSide = decision.action === 'LONG' ? 'LONG' : 'SHORT';
  const reference = decision.orderType === 'LIMIT' && decision.entryPrice !== null
    ? d(decision.entryPrice)
    : d(side === 'LONG' ? quote.ask : quote.bid);

  const stopValid = stopOnCorrectSide(side, reference, decision.stopLoss);
  if (decision.stopLoss === null) {
    check('stop-loss', 'Stop loss present', !settings.requireStopLoss, settings.requireStopLoss ? 'new positions must carry a stop loss' : 'not required by settings');
  } else {
    check('stop-loss', 'Stop loss valid', stopValid, `stop ${decision.stopLoss} vs entry ${reference.toFixed()} for ${side}`);
  }
  if (decision.takeProfit !== null) {
    const tp = d(decision.takeProfit);
    check(
      'take-profit',
      'Take profit valid',
      side === 'LONG' ? tp.gt(reference) : tp.lt(reference),
      `target ${decision.takeProfit} vs entry ${reference.toFixed()} for ${side}`,
    );
  }

  if (checks.some((c) => !c.passed)) return finish();

  const instrument = getInstrument(input.symbol);
  const leverage = Math.max(1, Math.floor(Math.min(settings.maxLeverage, instrument.maxLeverage)));
  const equity = d(balance.equity);
  const riskBudget = equity.mul(settings.maxRiskPerTradePct).div(100);

  // Round-trip taker fees plus slippage count against the budget, not just the stop distance.
  const costPerUnit = reference.mul(
    d(input.fees.takerFeeRate).mul(2).plus(d(input.fees.slippageBps).mul(2).div(10_000)),
  );
  const stopDistance = decision.stopLoss === null
    ? reference.mul(ASSUMED_STOP_FRACTION)
    : reference.minus(decision.stopLoss).abs();
  const riskQty = riskBudget.div(stopDistance.plus(costPerUnit));
  const marginQty = d(balance.availableBalance).mul(MARGIN_SAFETY).mul(leverage).div(reference);
  const notionalQty = d(settings.maxNotional).div(reference);
  const leverageQty = equity.mul(leverage).div(reference);
  const quantity = floorToStep(min(min(riskQty, marginQty), min(notionalQty, leverageQty)), instrument.quantityStep);

  const notional = quantity.mul(reference);
  const margin = notional.div(leverage);
  if (
    !check(
      'sizing',
      'Position sized',
      quantity.gte(instrument.minQuantity),
      `risk ${riskBudget.toFixed(2)} USDT over ${stopDistance.toFixed()} stop distance gives ${quantity.toFixed()} ${instrument.baseCurrency} (min ${instrument.minQuantity})`,
    )
  ) {
    return finish();
  }
  check('margin', 'Sufficient margin', margin.lte(balance.availableBalance), `needs ${margin.toFixed(2)} USDT, available ${d(balance.availableBalance).toFixed(2)} USDT`);
  check('leverage', 'Leverage limit', leverage <= settings.maxLeverage, `${leverage}x, limit ${settings.maxLeverage}x`);
  check('notional', 'Notional limit', notional.lte(settings.maxNotional), `${notional.toFixed(2)} USDT, limit ${settings.maxNotional} USDT`);

  const liquidation = linearPerp.liquidationPrice({
    side,
    entry: reference,
    quantity,
    margin,
    maintenanceRate: instrument.maintenanceMarginRate,
  });
  if (decision.stopLoss !== null) {
    const stopBeforeLiq = side === 'LONG' ? d(decision.stopLoss).gt(liquidation) : d(decision.stopLoss).lt(liquidation);
    check('stop-before-liq', 'Stop triggers before liquidation', stopBeforeLiq, `stop ${decision.stopLoss}, estimated liquidation ${liquidation.toFixed(2)}`);
  }

  return finish({
    quantity: str(quantity),
    leverage,
    notional: str(notional),
    margin: str(margin),
    riskAmount: str(riskBudget),
    referencePrice: str(reference),
    estimatedLiquidationPrice: str(liquidation),
  });
}

function stopOnCorrectSide(side: PositionSide, reference: Dec, stop: number | null): boolean {
  if (stop === null) return false;
  return side === 'LONG' ? d(stop).lt(reference) : d(stop).gt(reference);
}
