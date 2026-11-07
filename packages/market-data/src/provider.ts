import type { Candle, DataSource, FeedStatus, InstrumentSpecification, Quote, SymbolId, Timeframe } from '@mochiprep/shared';

/** Normalized market data, whatever the venue. Adding an exchange means implementing this. */
export interface MarketDataProvider {
  readonly source: DataSource;
  readonly name: string;
  start(): void;
  stop(): void;
  /** Oldest first. The last bar may be forming (`closed: false`). */
  getCandles(symbol: SymbolId, timeframe: Timeframe, limit: number): Promise<Candle[]>;
  getQuote(symbol: SymbolId): Quote | undefined;
  /** This feed's clock: wall time when live, tape time in replay. */
  now(): number;
  status(): FeedStatus;
  onQuote(listener: (quote: Quote) => void): () => void;
  onStatus(listener: (status: FeedStatus) => void): () => void;
  /** Tradable pairs this venue lists, for feeds that can add pairs at runtime. */
  listInstruments?(fresh?: boolean): Promise<InstrumentSpecification[]>;
  trackSymbol?(symbol: SymbolId): void;
  untrackSymbol?(symbol: SymbolId): void;
}
