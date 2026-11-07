import type { AiDecisionData } from '@mochiprep/shared';
import type { AiProvider, MarketContext } from './types';

export const MOCK_MODELS = {
  momentum: 'mock-momentum-v1',
  reversion: 'mock-reversion-v1',
  reckless: 'mock-reckless-v1',
} as const;

const LABEL = '[DEMO: deterministic mock, not a real model]';
const fmt = (v: number | null, digits = 2) => (v === null ? 'n/a' : v.toFixed(digits));

function base(context: MarketContext): Pick<AiDecisionData, 'symbol' | 'orderType' | 'entryPrice' | 'expiresAt'> {
  return {
    symbol: context.symbol,
    orderType: 'MARKET',
    entryPrice: null,
    expiresAt: new Date(context.now + context.horizonSec * 1000).toISOString(),
  };
}

const hold = (context: MarketContext, why: string): AiDecisionData => ({
  ...base(context),
  action: 'HOLD',
  stopLoss: null,
  takeProfit: null,
  reason: `${LABEL} ${why}`,
  confidence: 0.3,
});

const close = (context: MarketContext, why: string): AiDecisionData => ({
  ...base(context),
  action: 'CLOSE',
  stopLoss: null,
  takeProfit: null,
  reason: `${LABEL} ${why}`,
  confidence: 0.55,
});

const round = (context: MarketContext, value: number) => {
  const decimals = context.quote.last.split('.')[1]?.length ?? 2;
  return Number(value.toFixed(decimals));
};

function enter(context: MarketContext, side: 'LONG' | 'SHORT', stopAtr: number, targetAtr: number, why: string, confidence: number): AiDecisionData {
  const price = Number(context.quote.last);
  const atr = context.indicators.atr14 ?? price * 0.005;
  const sign = side === 'LONG' ? 1 : -1;
  return {
    ...base(context),
    action: side,
    stopLoss: round(context, price - sign * stopAtr * atr),
    takeProfit: round(context, price + sign * targetAtr * atr),
    reason: `${LABEL} ${why}`,
    confidence,
  };
}

function momentum(c: MarketContext): AiDecisionData {
  const { ema20, ema50, rsi14, macdHistogram } = c.indicators;
  if (ema20 === null || ema50 === null || rsi14 === null || macdHistogram === null) return hold(c, 'Not enough history for EMA, RSI and MACD yet.');
  const trend = `EMA20 ${fmt(ema20)} vs EMA50 ${fmt(ema50)}, RSI ${fmt(rsi14, 1)}, MACD histogram ${fmt(macdHistogram)}.`;
  if (c.position) {
    const against = c.position.side === 'LONG' ? ema20 < ema50 : ema20 > ema50;
    return against ? close(c, `Trend flipped against the ${c.position.side}: ${trend}`) : hold(c, `Trend still supports the ${c.position.side}: ${trend}`);
  }
  if (ema20 > ema50 && macdHistogram > 0 && rsi14 < 70) return enter(c, 'LONG', 2, 4, `Uptrend with rising momentum: ${trend}`, 0.62);
  if (ema20 < ema50 && macdHistogram < 0 && rsi14 > 30) return enter(c, 'SHORT', 2, 4, `Downtrend with falling momentum: ${trend}`, 0.62);
  return hold(c, `Trend and momentum disagree: ${trend}`);
}

function reversion(c: MarketContext): AiDecisionData {
  const { rsi14 } = c.indicators;
  if (rsi14 === null) return hold(c, 'Not enough history for RSI yet.');
  const state = `RSI ${fmt(rsi14, 1)}.`;
  if (c.position) {
    const done = c.position.side === 'LONG' ? rsi14 >= 50 : rsi14 <= 50;
    return done ? close(c, `RSI back at the midline, reversion done: ${state}`) : hold(c, `Waiting for RSI to reach 50: ${state}`);
  }
  if (rsi14 < 35) return enter(c, 'LONG', 2.5, 3, `Oversold, fading the move: ${state}`, 0.55);
  if (rsi14 > 65) return enter(c, 'SHORT', 2.5, 3, `Overbought, fading the move: ${state}`, 0.55);
  return hold(c, `RSI is in the neutral zone: ${state}`);
}

/** Misbehaves on purpose so the demo shows the risk engine saying no. */
function reckless(c: MarketContext): AiDecisionData {
  // Counted in cycles, not minutes: a 15m schedule would otherwise always land on one branch.
  const cycle = Math.floor(c.now / (c.horizonSec * 1000));
  const side = cycle % 2 === 0 ? 'LONG' : 'SHORT';
  if (c.position) return cycle % 4 === 0 ? close(c, 'Bored of this position.') : hold(c, 'Letting it ride.');
  const entry = enter(c, side, 1.5, 3, 'Feeling lucky, trading every bar.', 0.95);
  switch (cycle % 3) {
    case 0:
      return { ...entry, stopLoss: null, reason: `${LABEL} Stops are for the timid. No stop loss on this one.` };
    case 1: {
      const wrong = Number(c.quote.last) * (side === 'LONG' ? 1.01 : 0.99);
      return { ...entry, stopLoss: round(c, wrong), reason: `${LABEL} Stop deliberately on the wrong side of entry.` };
    }
    default:
      return entry;
  }
}

/** Offline, keyless, repeatable. Answers through the same JSON path as a real model. */
export const mockProvider: AiProvider = {
  kind: 'MOCK',
  async complete(request) {
    const strategy = request.model === MOCK_MODELS.reversion ? reversion : request.model === MOCK_MODELS.reckless ? reckless : momentum;
    return { text: JSON.stringify(strategy(request.context)), inputTokens: 0, outputTokens: 0 };
  },
};
