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

export const TrailingStopSchema = z.strictObject({
  kind: z.enum(['RATIO', 'SPREAD']),
  callback: decimalString,
  activationPrice: decimalString.nullable(),
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
  trailingStop: TrailingStopSchema.nullish(),
});

export const ProtectionSchema = z.strictObject({
  stopLoss: decimalString.nullable(),
  takeProfit: decimalString.nullable(),
  trailingStop: TrailingStopSchema.nullable(),
});

export const ClosePositionSchema = z.strictObject({
  fraction: z.number().gt(0).max(1),
  clientOrderId: z.string().min(1).max(128),
});

/** Each provider sets these itself. A duplicate would break auth or the request framing. */
const RESERVED_HEADERS = ['authorization', 'x-api-key', 'x-goog-api-key', 'anthropic-version', 'content-type', 'content-length', 'host', 'connection', 'transfer-encoding'];

const ExtraHeadersSchema = z
  .record(z.string().regex(/^[A-Za-z0-9!#$%&'*+.^_`|~-]{1,100}$/, 'invalid header name'), z.string().min(1).max(2000).regex(/^[^\r\n]*$/, 'header values cannot contain line breaks'))
  .superRefine((headers, ctx) => {
    const names = Object.keys(headers);
    if (names.length > 20) ctx.addIssue({ code: 'custom', message: 'at most 20 headers' });
    for (const name of names) {
      if (RESERVED_HEADERS.includes(name.toLowerCase())) ctx.addIssue({ code: 'custom', message: `${name} is set by the provider and cannot be overridden` });
    }
  })
  .transform((headers) => Object.fromEntries(Object.entries(headers).map(([name, value]) => [name.toLowerCase(), value])));

export const ProviderInputSchema = z.strictObject({
  name: z.string().trim().min(1).max(60),
  kind: z.enum(['OPENAI_COMPATIBLE', 'OPENAI', 'ANTHROPIC', 'GEMINI', 'DEEPSEEK', 'OPENCODE']),
  /** Omit to keep the saved headers: the API never returns their values to send back. */
  extraHeaders: ExtraHeadersSchema.optional(),
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
  executionMode: z.enum(['AUTO', 'SIGNAL']).default('SIGNAL'),
  useNews: z.boolean().default(false),
  allowTrailingStop: z.boolean().default(false),
});

export const NewsSourceInputSchema = z.strictObject({
  name: z.string().trim().min(1).max(40),
  url: z.url().max(300),
});

export const NewsSourceToggleSchema = z.strictObject({ enabled: z.boolean() });

const secret = z.string().trim().min(1).max(200);

export const OkxCredentialsSchema = z.strictObject({ apiKey: secret, secretKey: secret, passphrase: secret });

export const OkxConnectSchema = OkxCredentialsSchema.extend({
  name: z.string().trim().min(1).max(60),
  environment: z.enum(['OKX_DEMO', 'OKX_LIVE']),
  limits: RiskSettingsSchema.default(DEFAULT_LIVE_LIMITS),
  /** Save the credentials sealed by the master password. Needs an unlocked vault. */
  remember: z.boolean().default(false),
});

const masterPassword = z.string().min(10, 'Use at least 10 characters').max(200);

export const VaultPasswordSchema = z.strictObject({ password: z.string().min(1).max(200) });
export const VaultCreateSchema = z.strictObject({ password: masterPassword });
export const VaultChangeSchema = z.strictObject({ current: z.string().min(1).max(200), next: masterPassword });
export const VaultResetSchema = z.strictObject({ confirmation: z.string().max(100) });

export const ArmSchema = z.strictObject({ confirmation: z.string().max(100) });

export const AddInstrumentSchema = z.strictObject({ symbol: SymbolSchema });

export const ReplayControlSchema = z.strictObject({
  playing: z.boolean().optional(),
  speed: z.number().int().min(1).max(600).optional(),
  steps: z.number().int().min(1).max(1440).optional(),
});
