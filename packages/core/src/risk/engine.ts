import type {
  AccountBalance,
  AiDecisionData,
  AiTrailingStop,
  PaperAccountConfig,
  Position,
  PositionSide,
  Protection,
  Quote,
  RiskCheck,
  RiskResult,
  RiskSettings,
  SizedTrade,
  SymbolId,
  TrailingStopSpec,
} from '@mochiprep/shared';
import { d, floorToStep, min, str, type Dec } from '../decimal';
import { getInstrument } from '../instruments';
import { linearPerp } from '../trading/perp';
import { validateTrailing } from '../trading/trailing';

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
  /** The agent's switch: when off, any `trailingStop` in the decision, null included, is refused. */
  allowTrailingStop: boolean;
  /**
   * The current protection of this symbol's open position, or null without one. PROTECT states
   * the full target, and a kept trailing stop was validated when it was set; only the current stop
   * loss is read, to tell a looser stop (budget applies) from a kept or tighter one.
   */
  protection: Protection | null;
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

  // Before HOLD returns, so a HOLD that carries a trailing stop is still checked.
  if (decision.trailingStop !== undefined) {
    check(
      'trailing-allowed',
      'Trailing stops allowed',
      input.allowTrailingStop,
      input.allowTrailingStop ? 'trailing stops are on for this agent' : 'trailing stops are off for this agent',
    );
  }
  if ((decision.action === 'HOLD' || decision.action === 'CLOSE') && decision.trailingStop != null) {
    check('trailing-stop', 'Trailing stop valid', false, 'only LONG, SHORT and PROTECT take a trailing stop');
  }

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
  if (decision.action === 'PROTECT') return protect(input, existing, check, finish);

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
  if (decision.trailingStop) checkTrailing(check, side, reference, decision.trailingStop, input.symbol);

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

type Check = (id: string, label: string, passed: boolean, detail: string) => boolean;

/**
 * PROTECT changes the stops of an open position and nothing else: no sizing, no leverage. The
 * decision is the full target, so a null stop loss means removing it. Checks are measured from
 * the last price, the price the position would be closed at if a stop fired now.
 */
function protect(
  input: RiskInput,
  existing: Position | undefined,
  check: Check,
  finish: (sized?: SizedTrade | null) => RiskResult,
): RiskResult {
  const { decision, settings, quote, now } = input;
  check('has-position', 'Position to protect', !!existing, existing ? `${existing.side} ${existing.quantity}` : `no open ${input.symbol} position`);
  if (!existing) return finish();

  const quoteAgeSec = quote ? (now - quote.ts) / 1000 : Infinity;
  const fresh = check(
    'freshness',
    'Market data fresh',
    quoteAgeSec <= settings.maxQuoteAgeSec,
    quote ? `quote age ${quoteAgeSec.toFixed(1)}s, limit ${settings.maxQuoteAgeSec}s` : 'no quote available',
  );
  if (!fresh || !quote) return finish();

  const side = existing.side;
  const last = d(quote.last);
  if (decision.stopLoss === null) {
    check('stop-loss', 'Stop loss present', !settings.requireStopLoss, settings.requireStopLoss ? 'open positions must keep a stop loss' : 'not required by settings');
  } else {
    check('stop-loss', 'Stop loss valid', stopOnCorrectSide(side, last, decision.stopLoss), `stop ${decision.stopLoss} vs last ${last.toFixed()} for ${side}`);
  }
  if (decision.takeProfit !== null) {
    const tp = d(decision.takeProfit);
    check(
      'take-profit',
      'Take profit valid',
      side === 'LONG' ? tp.gt(last) : tp.lt(last),
      `target ${decision.takeProfit} vs last ${last.toFixed()} for ${side}`,
    );
  }
  if (decision.trailingStop) checkTrailing(check, side, last, decision.trailingStop, input.symbol);

  if (decision.stopLoss !== null) {
    const stop = d(decision.stopLoss);
    // OKX sends liqPx empty, or 0, for some positions: the liquidation price is then unknown and
    // there is nothing to compare against.
    if (liquidationKnown(existing.liquidationPrice)) {
      const liquidation = d(existing.liquidationPrice);
      check(
        'stop-before-liq',
        'Stop triggers before liquidation',
        side === 'LONG' ? stop.gt(liquidation) : stop.lt(liquidation),
        `stop ${decision.stopLoss}, liquidation ${liquidation.toFixed(2)}`,
      );
    }
    // Keeping or tightening the stop only reduces risk, so a trade in profit can always keep its
    // stop. A new or looser stop must fit the budget: the loss if it fired from here, on the size held.
    const current = input.protection?.stopLoss ?? null;
    const looser = current === null || (side === 'LONG' ? stop.lt(d(current)) : stop.gt(d(current)));
    if (looser) {
      const risk = d(existing.quantity).mul(last.minus(stop).abs());
      const budget = d(input.balance.equity).mul(settings.maxRiskPerTradePct).div(100);
      check('protect-risk', 'Risk per trade', risk.lte(budget), `risk ${risk.toFixed(2)} USDT, limit ${budget.toFixed(2)} USDT`);
    }
  }

  return finish();
}

/** The model writes numbers; the trailing rule reads decimal strings, as the engine stores them. */
function trailingSpec(ai: AiTrailingStop): TrailingStopSpec {
  return {
    kind: ai.kind,
    callback: str(d(ai.callback)),
    activationPrice: ai.activationPrice === null ? null : str(d(ai.activationPrice)),
  };
}

function checkTrailing(check: Check, side: PositionSide, reference: Dec, ai: AiTrailingStop, symbol: SymbolId): void {
  const spec = trailingSpec(ai);
  const refusal = validateTrailing(side, reference, spec, d(getInstrument(symbol).priceTick));
  const activation = spec.activationPrice === null ? 'no activation price' : `activation ${spec.activationPrice}`;
  check('trailing-stop', 'Trailing stop valid', refusal === null, refusal ?? `${spec.kind} ${spec.callback}, ${activation} for ${side}`);
}

function liquidationKnown(price: string): boolean {
  return price.trim() !== '' && !d(price).isZero();
}

function stopOnCorrectSide(side: PositionSide, reference: Dec, stop: number | null): boolean {
  if (stop === null) return false;
  return side === 'LONG' ? d(stop).lt(reference) : d(stop).gt(reference);
}
