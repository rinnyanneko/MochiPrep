import type {
  AccountBalance,
  Candle,
  Position,
  ProviderKind,
  Quote,
  RiskSettings,
  SymbolId,
  Timeframe,
  IndicatorSnapshot,
  NewsItem,
} from '@mochiprep/shared';

/** Everything an analysis may see. Deliberately excludes keys, ids and exchange credentials. */
export interface MarketContext {
  symbol: SymbolId;
  timeframe: Timeframe;
  now: number;
  /** How long a decision may stay valid, usually the agent's interval. */
  horizonSec: number;
  candles: Candle[];
  indicators: IndicatorSnapshot;
  quote: Quote;
  position: Position | null;
  balance: AccountBalance;
  risk: RiskSettings;
  strategy: string;
  /** Third-party headlines, when the agent reads news. Untrusted data. */
  news?: NewsItem[];
}

export interface AiRequest {
  model: string;
  system: string;
  user: string;
  context: MarketContext;
  signal?: AbortSignal;
}

export interface AiResponse {
  text: string;
  inputTokens: number | null;
  outputTokens: number | null;
}

export interface AiProvider {
  readonly kind: ProviderKind;
  complete(request: AiRequest): Promise<AiResponse>;
}

export type ProviderErrorCode = 'NO_KEY' | 'AUTH' | 'RATE_LIMIT' | 'TIMEOUT' | 'NETWORK' | 'SERVER' | 'HTTP' | 'BAD_RESPONSE';

export class ProviderError extends Error {
  constructor(readonly code: ProviderErrorCode, message: string) {
    super(message);
  }
  /** Retrying cannot help: the agent should pause instead of burning cycles. */
  get critical() {
    return this.code === 'NO_KEY' || this.code === 'AUTH';
  }
}
