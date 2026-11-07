import {
  DEFAULT_SYMBOLS,
  INSTRUMENTS,
  TIMEFRAME_MS,
  type Candle,
  type FeedStatus,
  type Quote,
  type SymbolId,
  type Timeframe,
} from '@mochiprep/shared';
import type { MarketDataProvider } from './provider';
import { generateTape, WARMUP_MINUTES } from './synthetic';

const FUNDING_MS = 8 * 3_600_000;
const SPREAD_FRACTION = 0.00005;

/**
 * Offline deterministic feed. Time only moves when `step()` is called, so a caller can hold the
 * tape still while slow AI calls finish, and two runs of the same tape see identical quotes.
 * One quote per 1m bar, taken at the close: wicks are never treated as tradable prices.
 */
export class ReplayMarketData implements MarketDataProvider {
  readonly source = 'REPLAY' as const;
  readonly name = 'Synthetic replay';
  private readonly tape: Record<string, Candle[]>;
  private cursor = WARMUP_MINUTES;
  private readonly quoteListeners = new Set<(q: Quote) => void>();

  constructor(minutes?: number) {
    this.tape = Object.fromEntries(DEFAULT_SYMBOLS.map((s) => [s, generateTape(s, minutes)]));
  }

  start() {}
  stop() {}

  get length() {
    return this.tape.BTCUSDT!.length;
  }
  get position() {
    return this.cursor;
  }
  get atEnd() {
    return this.cursor >= this.length - 1;
  }
  get firstPlayableTime() {
    return this.tape.BTCUSDT![WARMUP_MINUTES]!.time + 60_000;
  }
  get endTime() {
    return this.tape.BTCUSDT![this.length - 1]!.time + 60_000;
  }

  seek(index: number) {
    this.cursor = Math.min(Math.max(index, WARMUP_MINUTES), this.length - 1);
  }

  /** Advances one minute and publishes one quote per symbol. Returns them, empty at tape end. */
  step(): Quote[] {
    if (this.atEnd) return [];
    this.cursor++;
    const quotes = DEFAULT_SYMBOLS.map((symbol) => this.getQuote(symbol)!);
    for (const quote of quotes) for (const listener of this.quoteListeners) listener(quote);
    return quotes;
  }

  now() {
    return this.tape.BTCUSDT![this.cursor]!.time + 60_000;
  }

  /** Undefined for any pair outside the tape: custom pairs only exist on live data. */
  getQuote(symbol: SymbolId): Quote | undefined {
    const tape = this.tape[symbol];
    if (!tape) return undefined;
    const candle = tape[this.cursor]!;
    const instrument = INSTRUMENTS[symbol]!;
    const tick = Number(instrument.priceTick);
    const decimals = instrument.priceTick.split('.')[1]?.length ?? 0;
    const half = Math.max(tick, Math.round((candle.close * SPREAD_FRACTION) / tick) * tick);
    const ts = candle.time + 60_000;
    return {
      source: 'REPLAY',
      symbol,
      bid: (candle.close - half).toFixed(decimals),
      ask: (candle.close + half).toFixed(decimals),
      last: candle.close.toFixed(decimals),
      mark: candle.close.toFixed(decimals),
      markEstimated: false,
      fundingRate: this.fundingRate(symbol),
      nextFundingTime: (Math.floor(ts / FUNDING_MS) + 1) * FUNDING_MS,
      ts,
    };
  }

  /** Follows the tape's own 8h return, like a perp premium would, clamped to +-0.05%. */
  private fundingRate(symbol: SymbolId): string {
    const back = Math.max(0, this.cursor - 480);
    const tape = this.tape[symbol]!;
    const change = (tape[this.cursor]!.close - tape[back]!.close) / tape[back]!.close;
    const rate = Math.max(-0.0005, Math.min(0.0005, 0.0001 + change * 0.01));
    return rate.toFixed(6);
  }

  async getCandles(symbol: SymbolId, timeframe: Timeframe, limit: number): Promise<Candle[]> {
    const tape = this.tape[symbol];
    if (!tape) return [];
    const size = TIMEFRAME_MS[timeframe];
    const nowMs = this.now();
    const out: Candle[] = [];
    let i = this.cursor;
    while (i >= 0 && out.length < limit) {
      const bucket = Math.floor(tape[i]!.time / size) * size;
      const bar: Candle = { ...tape[i]!, time: bucket, closed: bucket + size <= nowMs };
      i--;
      while (i >= 0 && tape[i]!.time >= bucket) {
        const c = tape[i]!;
        bar.open = c.open;
        bar.high = Math.max(bar.high, c.high);
        bar.low = Math.min(bar.low, c.low);
        bar.volume += c.volume;
        i--;
      }
      bar.volume = Number(bar.volume.toFixed(3));
      out.push(bar);
    }
    return out.reverse();
  }

  status(): FeedStatus {
    return { source: 'REPLAY', provider: this.name, state: 'OFFLINE_REPLAY', lastMessageAt: this.now() };
  }

  onQuote(listener: (quote: Quote) => void) {
    this.quoteListeners.add(listener);
    return () => void this.quoteListeners.delete(listener);
  }

  onStatus() {
    return () => {};
  }
}
