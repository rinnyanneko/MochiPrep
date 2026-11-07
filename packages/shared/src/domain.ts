/**
 * Shared domain vocabulary. All money/price/quantity values that feed accounting are decimal
 * strings. Candle OHLCV stays numeric because it only feeds charts and indicators, never the ledger.
 */

/** Only CRYPTO_PERP has a calculation model today; the others mark the extension point. */
export type MarketType = 'CRYPTO_PERP' | 'CRYPTO_SPOT' | 'FOREX' | 'CFD';

export interface InstrumentSpecification {
  symbol: string;
  marketType: MarketType;
  baseCurrency: string;
  quoteCurrency: string;
  settlementCurrency: string;
  /** Base units per contract. Simulated quantities are in base units, so this is 1 here. */
  contractSize: string;
  quantityStep: string;
  minQuantity: string;
  priceTick: string;
  maxLeverage: number;
  maintenanceMarginRate: string;
  fundingIntervalHours: number;
  /** Base units per OKX contract. Orders are sent in contracts, so this must match OKX exactly. */
  venueContractValue: string;
  /** Smallest contract increment on OKX. */
  venueLotSize: string;
}

/** Always available, and the only pairs on the replay tape. */
export const DEFAULT_SYMBOLS = ['BTCUSDT', 'ETHUSDT', 'SOLUSDT'] as const;
export type DefaultSymbol = (typeof DEFAULT_SYMBOLS)[number];
/** A registered instrument symbol, such as DOGEUSDT. Users can add pairs, so this is open. */
export type SymbolId = string;

const perp = (
  base: string,
  quantityStep: string,
  priceTick: string,
  venueContractValue: string,
): InstrumentSpecification => ({
  symbol: `${base}USDT`,
  marketType: 'CRYPTO_PERP',
  baseCurrency: base,
  quoteCurrency: 'USDT',
  settlementCurrency: 'USDT',
  contractSize: '1',
  quantityStep,
  minQuantity: quantityStep,
  priceTick,
  maxLeverage: 100,
  maintenanceMarginRate: '0.005',
  fundingIntervalHours: 8,
  venueContractValue,
  venueLotSize: '0.01',
});

/** Steps mirror OKX USDT swaps (ctVal x lotSz): 0.01 x 0.01 BTC, 0.1 x 0.01 ETH, 1 x 0.01 SOL. */
export const INSTRUMENTS: Record<SymbolId, InstrumentSpecification> = {
  BTCUSDT: perp('BTC', '0.0001', '0.1', '0.01'),
  ETHUSDT: perp('ETH', '0.001', '0.01', '0.1'),
  SOLUSDT: perp('SOL', '0.01', '0.01', '1'),
};

export const isDefaultSymbol = (symbol: SymbolId) => (DEFAULT_SYMBOLS as readonly string[]).includes(symbol);

export const listSymbols = (): SymbolId[] => Object.keys(INSTRUMENTS);

export function registerInstrument(spec: InstrumentSpecification) {
  INSTRUMENTS[spec.symbol] = spec;
}

export function unregisterInstrument(symbol: SymbolId) {
  if (!isDefaultSymbol(symbol)) delete INSTRUMENTS[symbol];
}

/** REPLAY = deterministic synthetic tape, LIVE = real public market data. */
export type DataSource = 'REPLAY' | 'LIVE';

export const TIMEFRAMES = ['1m', '5m', '15m', '1h', '4h'] as const;
export type Timeframe = (typeof TIMEFRAMES)[number];
export const TIMEFRAME_MS: Record<Timeframe, number> = {
  '1m': 60_000,
  '5m': 300_000,
  '15m': 900_000,
  '1h': 3_600_000,
  '4h': 14_400_000,
};

export interface Candle {
  /** Bar open time, ms since epoch. */
  time: number;
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
  /** False for the bar still forming. Agents only ever see closed bars. */
  closed: boolean;
}

export interface Quote {
  source: DataSource;
  symbol: SymbolId;
  bid: string;
  ask: string;
  last: string;
  mark: string;
  /** True when the venue gave no mark price and `mark` falls back to the bid/ask mid. */
  markEstimated: boolean;
  fundingRate: string | null;
  nextFundingTime: number | null;
  /** Market timestamp of this quote, ms. */
  ts: number;
}

export type ConnectionState = 'CONNECTED' | 'CONNECTING' | 'DEGRADED' | 'DISCONNECTED' | 'OFFLINE_REPLAY';

export interface FeedStatus {
  source: DataSource;
  provider: string;
  state: ConnectionState;
  lastMessageAt: number | null;
  detail?: string;
}

export type OrderSide = 'BUY' | 'SELL';
export type OrderType = 'MARKET' | 'LIMIT' | 'STOP_MARKET' | 'TAKE_PROFIT_MARKET';
export type OrderStatus = 'OPEN' | 'FILLED' | 'CANCELED' | 'REJECTED';
export type PositionSide = 'LONG' | 'SHORT';

export interface OrderRequest {
  accountId: string;
  /** Idempotency key, unique per account. Replays return the original order. */
  clientOrderId: string;
  symbol: SymbolId;
  side: OrderSide;
  type: 'MARKET' | 'LIMIT';
  quantity: string;
  price?: string | null;
  leverage: number;
  reduceOnly?: boolean;
  stopLoss?: string | null;
  takeProfit?: string | null;
  agentId?: string | null;
  decisionId?: string | null;
}

export interface Order {
  id: string;
  accountId: string;
  clientOrderId: string;
  symbol: SymbolId;
  side: OrderSide;
  type: OrderType;
  quantity: string;
  /** Limit price for LIMIT orders. */
  price: string | null;
  /** Trigger for STOP_MARKET / TAKE_PROFIT_MARKET. */
  triggerPrice: string | null;
  leverage: number;
  reduceOnly: boolean;
  /** SL/TP orders always close whatever is left of the position when they trigger. */
  closePosition: boolean;
  status: OrderStatus;
  filledQuantity: string;
  avgFillPrice: string | null;
  /** Margin and fee held back while a resting opening order waits. */
  reservedMargin: string;
  attachedStopLoss: string | null;
  attachedTakeProfit: string | null;
  rejectReason: string | null;
  /** OKX ordId once the exchange has acknowledged the order. */
  exchangeOrderId: string | null;
  agentId: string | null;
  decisionId: string | null;
  createdAt: number;
  updatedAt: number;
}

export interface OrderResult {
  order: Order;
  fills: Fill[];
  /** True when the idempotency key matched an earlier order and nothing was executed. */
  duplicate: boolean;
}

export interface Fill {
  id: string;
  orderId: string;
  accountId: string;
  symbol: SymbolId;
  side: OrderSide;
  quantity: string;
  price: string;
  fee: string;
  realizedPnl: string;
  liquidity: 'MAKER' | 'TAKER' | 'LIQUIDATION';
  ts: number;
}

export interface Position {
  id: string;
  accountId: string;
  symbol: SymbolId;
  side: PositionSide;
  quantity: string;
  entryPrice: string;
  leverage: number;
  /** Isolated margin currently committed. */
  margin: string;
  liquidationPrice: string;
  status: 'OPEN' | 'CLOSED' | 'LIQUIDATED';
  realizedPnl: string;
  feesPaid: string;
  /** Positive = paid, negative = received. */
  fundingPaid: string;
  maxQuantity: string;
  exitPrice: string | null;
  lastFundingTime: number;
  openedAt: number;
  closedAt: number | null;
}

export interface PositionView extends Position {
  markPrice: string | null;
  unrealizedPnl: string;
  stopLoss: string | null;
  takeProfit: string | null;
}

export interface PaperAccountConfig {
  takerFeeRate: string;
  makerFeeRate: string;
  /** Fixed slippage on market fills, basis points. */
  slippageBps: string;
  /** Extra slippage, basis points per 1,000,000 USDT of notional. */
  impactBpsPerMillion: string;
}

export const DEFAULT_ACCOUNT_CONFIG: PaperAccountConfig = {
  takerFeeRate: '0.0005',
  makerFeeRate: '0.0002',
  slippageBps: '1',
  impactBpsPerMillion: '5',
};

/** PAPER is simulated locally. OKX_DEMO uses OKX virtual funds. OKX_LIVE is real money. */
export type AccountVenue = 'PAPER' | 'OKX_DEMO' | 'OKX_LIVE';

export interface PaperAccount {
  id: string;
  name: string;
  source: DataSource;
  venue: AccountVenue;
  exchangeUid: string | null;
  /** Hard ceilings on every order for an exchange account, whoever places it. */
  liveLimits: RiskSettings | null;
  isDemo: boolean;
  initialBalance: string;
  walletBalance: string;
  usedMargin: string;
  realizedPnl: string;
  feesPaid: string;
  /** Positive = paid, negative = received. */
  fundingPaid: string;
  config: PaperAccountConfig;
  createdAt: number;
}

export interface AccountBalance {
  accountId: string;
  initialBalance: string;
  walletBalance: string;
  availableBalance: string;
  usedMargin: string;
  orderMargin: string;
  equity: string;
  realizedPnl: string;
  unrealizedPnl: string;
  feesPaid: string;
  fundingPaid: string;
}

export type LedgerEventType =
  | 'DEPOSIT'
  | 'TRADE_FEE'
  | 'REALIZED_PNL'
  | 'FUNDING'
  | 'LIQUIDATION_PENALTY'
  | 'MARGIN_LOCK'
  | 'MARGIN_RELEASE';

export interface LedgerEvent {
  seq: number;
  id: string;
  accountId: string;
  type: LedgerEventType;
  /** Signed change to wallet balance. */
  amount: string;
  /** Signed change to used margin. */
  marginDelta: string;
  walletAfter: string;
  symbol: SymbolId | null;
  orderId: string | null;
  fillId: string | null;
  positionId: string | null;
  memo: string;
  ts: number;
}

export interface EquityPoint {
  accountId: string;
  ts: number;
  equity: string;
  wallet: string;
  unrealized: string;
}

export interface RiskSettings {
  maxLeverage: number;
  maxRiskPerTradePct: number;
  maxDailyLossPct: number;
  maxOpenPositions: number;
  requireStopLoss: boolean;
  /** Market data older than this blocks new positions. */
  maxQuoteAgeSec: number;
  maxNotional: number;
}

export const DEFAULT_RISK_SETTINGS: RiskSettings = {
  maxLeverage: 3,
  maxRiskPerTradePct: 0.5,
  maxDailyLossPct: 2,
  maxOpenPositions: 2,
  requireStopLoss: true,
  maxQuoteAgeSec: 30,
  maxNotional: 100_000,
};

export interface RiskCheck {
  id: string;
  label: string;
  passed: boolean;
  detail: string;
}

export interface SizedTrade {
  quantity: string;
  leverage: number;
  notional: string;
  margin: string;
  riskAmount: string;
  referencePrice: string;
  estimatedLiquidationPrice: string;
}

export interface RiskResult {
  approved: boolean;
  checks: RiskCheck[];
  rejectionReasons: string[];
  sized: SizedTrade | null;
}

export const AGENT_STATUSES = ['IDLE', 'ANALYZING', 'WAITING', 'EXECUTING', 'PAUSED', 'ERROR'] as const;
export type AgentStatus = (typeof AGENT_STATUSES)[number];

export interface Agent {
  id: string;
  name: string;
  providerId: string;
  model: string;
  symbol: SymbolId;
  timeframe: Timeframe;
  strategy: string;
  accountId: string;
  risk: RiskSettings;
  intervalSec: number;
  status: AgentStatus;
  /** True while the schedule is armed. Always false after a backend restart. */
  running: boolean;
  nextRunAt: number | null;
  lastRunAt: number | null;
  statusDetail: string | null;
  consecutiveErrors: number;
  isDemo: boolean;
  createdAt: number;
}

export type ProviderKind = 'MOCK' | 'OPENAI_COMPATIBLE' | 'OPENAI' | 'ANTHROPIC' | 'GEMINI' | 'DEEPSEEK';

export interface ProviderConfig {
  id: string;
  name: string;
  kind: ProviderKind;
  baseUrl: string | null;
  defaultModel: string;
  /** User-entered list prices for cost estimates; null = unknown, so no estimate is shown. */
  inputPricePerMTok: number | null;
  outputPricePerMTok: number | null;
  createdAt: number;
}

export interface ProviderView extends ProviderConfig {
  /** Whether a key is saved. The key itself is never returned. */
  hasKey: boolean;
  requiresKey: boolean;
}

export type DecisionOutcome =
  | 'HOLD'
  | 'ANALYSIS_ONLY'
  | 'EXECUTED'
  | 'ORDER_PLACED'
  | 'ORDER_REJECTED'
  | 'RISK_REJECTED'
  | 'INVALID_OUTPUT'
  | 'AI_ERROR'
  | 'DATA_ERROR';

export interface AiDecisionData {
  symbol: string;
  action: 'LONG' | 'SHORT' | 'CLOSE' | 'HOLD';
  orderType: 'MARKET' | 'LIMIT';
  entryPrice: number | null;
  stopLoss: number | null;
  takeProfit: number | null;
  reason: string;
  confidence: number;
  expiresAt: string | null;
}

export interface DecisionRecord {
  id: string;
  agentId: string;
  agentName: string;
  accountId: string;
  source: DataSource;
  venue: AccountVenue;
  ts: number;
  trigger: 'SCHEDULE' | 'RUN_ONCE' | 'ANALYZE';
  providerKind: ProviderKind | null;
  model: string;
  symbol: SymbolId;
  timeframe: Timeframe;
  snapshot: Quote | null;
  indicators: Record<string, number | null> | null;
  decision: AiDecisionData | null;
  rawResponse: string | null;
  validationError: string | null;
  risk: RiskResult | null;
  outcome: DecisionOutcome;
  outcomeDetail: string | null;
  orderId: string | null;
  fillIds: string[];
  latencyMs: number | null;
  inputTokens: number | null;
  outputTokens: number | null;
  estimatedCost: string | null;
  isMock: boolean;
}

export interface ArenaRow {
  agentId: string;
  agentName: string;
  accountId: string;
  source: DataSource;
  venue: AccountVenue;
  isMock: boolean;
  providerName: string;
  model: string;
  symbol: SymbolId;
  status: AgentStatus;
  initialBalance: string;
  equity: string;
  returnPct: number;
  realizedPnl: string;
  maxDrawdownPct: number;
  winRatePct: number | null;
  tradeCount: number;
  decisionCount: number;
  estimatedAiCost: string | null;
  curve: { ts: number; equity: number }[];
}

export interface ExchangeStatus {
  accountId: string;
  name: string;
  venue: AccountVenue;
  /** Credentials are in memory for this process. False after every restart. */
  connected: boolean;
  /** Real orders allowed. Always true for OKX_DEMO once connected. */
  armed: boolean;
  /** Last sync with the exchange succeeded recently enough to trade on. */
  healthy: boolean;
  lastSyncAt: number | null;
  error: string | null;
  limits: RiskSettings;
}

/** Typed by the user to arm real-money orders. */
export const ARM_PHRASE = 'TRADE REAL MONEY';

/** Starting ceilings for exchange accounts: deliberately far below the paper defaults. */
export const DEFAULT_LIVE_LIMITS: RiskSettings = {
  maxLeverage: 3,
  maxRiskPerTradePct: 0.5,
  maxDailyLossPct: 2,
  maxOpenPositions: 2,
  requireStopLoss: true,
  maxQuoteAgeSec: 10,
  maxNotional: 100,
};

export interface ReplayState {
  loaded: boolean;
  playing: boolean;
  speed: number;
  now: number | null;
  startTime: number | null;
  endTime: number | null;
  atEnd: boolean;
  progress: number;
}

export type ServerEvent =
  | { type: 'heartbeat' }
  | { type: 'quote'; quote: Quote }
  | { type: 'feed'; status: FeedStatus }
  | { type: 'replay'; state: ReplayState }
  | { type: 'invalidate'; topics: Topic[] }
  | { type: 'toast'; level: 'info' | 'warn' | 'error'; message: string };

export type Topic = 'accounts' | 'orders' | 'positions' | 'agents' | 'decisions' | 'providers' | 'arena' | 'exchange' | 'instruments';
