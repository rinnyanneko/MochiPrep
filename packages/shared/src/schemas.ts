import { z } from 'zod';
import { TIMEFRAMES, DEFAULT_LIVE_LIMITS, DEFAULT_RISK_SETTINGS } from './domain';

const positive = z.number().finite().positive();

/**
 * The only shape of AI output the system accepts. Strict: unknown keys are rejected, because a
 * model adding fields means it has not followed the contract.
 */
export const AiDecisionSchema = z
  .strictObject({
    symbol: z.string().min(1).max(32),
    action: z.enum(['LONG', 'SHORT', 'CLOSE', 'HOLD']),
    orderType: z.enum(['MARKET', 'LIMIT']),
    entryPrice: positive.nullable(),
    stopLoss: positive.nullable(),
    takeProfit: positive.nullable(),
    reason: z.string().min(1).max(4000),
    confidence: z.number().min(0).max(1),
    expiresAt: z.iso.datetime({ offset: true }).nullable(),
  })
  .superRefine((value, ctx) => {
    if (value.orderType === 'LIMIT' && value.entryPrice === null && (value.action === 'LONG' || value.action === 'SHORT')) {
      ctx.addIssue({ code: 'custom', path: ['entryPrice'], message: 'LIMIT orders need entryPrice' });
    }
  });

const decimalString = z.string().regex(/^\d+(\.\d+)?$/, 'expected a positive decimal string').max(40);
/** Shape only. Whether a pair is actually tradable is checked against the instrument registry. */
export const SymbolSchema = z.string().regex(/^[A-Z0-9]{1,24}USDT$/, 'expected a pair like DOGEUSDT');
const symbol = SymbolSchema;

export const RiskSettingsSchema = z.strictObject({
  maxLeverage: z.number().min(1).max(100),
  maxRiskPerTradePct: z.number().gt(0).max(100),
  maxDailyLossPct: z.number().gt(0).max(100),
  maxOpenPositions: z.number().int().min(1).max(50),
  requireStopLoss: z.boolean(),
  maxQuoteAgeSec: z.number().min(1).max(3600),
  maxNotional: z.number().positive(),
});

export const CreateAccountSchema = z.strictObject({
  name: z.string().trim().min(1).max(60),
  source: z.enum(['REPLAY', 'LIVE']),
  initialBalance: decimalString.default('10000'),
});

export const PlaceOrderSchema = z.strictObject({
  accountId: z.string().min(1),
  clientOrderId: z.string().min(1).max(128),
  symbol,
  side: z.enum(['BUY', 'SELL']),
  type: z.enum(['MARKET', 'LIMIT']),
  quantity: decimalString,
  price: decimalString.nullish(),
  leverage: z.number().int().min(1).max(100),
  reduceOnly: z.boolean().optional(),
  stopLoss: decimalString.nullish(),
  takeProfit: decimalString.nullish(),
});

export const ProtectionSchema = z.strictObject({
  stopLoss: decimalString.nullable(),
  takeProfit: decimalString.nullable(),
});

export const ClosePositionSchema = z.strictObject({
  fraction: z.number().gt(0).max(1),
  clientOrderId: z.string().min(1).max(128),
});

export const ProviderInputSchema = z.strictObject({
  name: z.string().trim().min(1).max(60),
  kind: z.enum(['OPENAI_COMPATIBLE', 'OPENAI', 'ANTHROPIC', 'GEMINI', 'DEEPSEEK']),
  baseUrl: z.url().max(300).nullable(),
  defaultModel: z.string().trim().min(1).max(120),
  inputPricePerMTok: z.number().min(0).nullable(),
  outputPricePerMTok: z.number().min(0).nullable(),
});

export const ProviderKeySchema = z.strictObject({ apiKey: z.string().trim().min(1).max(1000) });

export const AgentInputSchema = z.strictObject({
  name: z.string().trim().min(1).max(60),
  providerId: z.string().min(1),
  model: z.string().trim().min(1).max(120),
  symbol,
  timeframe: z.enum(TIMEFRAMES),
  strategy: z.string().max(8000),
  accountId: z.string().min(1),
  risk: RiskSettingsSchema.default(DEFAULT_RISK_SETTINGS),
  intervalSec: z.number().int().min(10).max(86_400),
});

const secret = z.string().trim().min(1).max(200);

export const OkxCredentialsSchema = z.strictObject({ apiKey: secret, secretKey: secret, passphrase: secret });

export const OkxConnectSchema = OkxCredentialsSchema.extend({
  name: z.string().trim().min(1).max(60),
  environment: z.enum(['OKX_DEMO', 'OKX_LIVE']),
  limits: RiskSettingsSchema.default(DEFAULT_LIVE_LIMITS),
});

export const ArmSchema = z.strictObject({ confirmation: z.string().max(100) });

export const AddInstrumentSchema = z.strictObject({ symbol: SymbolSchema });

export const ReplayControlSchema = z.strictObject({
  playing: z.boolean().optional(),
  speed: z.number().int().min(1).max(600).optional(),
  steps: z.number().int().min(1).max(1440).optional(),
});
