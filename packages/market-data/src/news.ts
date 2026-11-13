import { isIPv4 } from 'node:net';
import type { NewsItem, NewsSource, SymbolId } from '@mochiprep/shared';
import { feedFetch, PUBLIC_HOST_PROBLEM } from './feed-fetch';
import { isPublicAddress } from './public-address';

export const DEFAULT_NEWS_SOURCES: NewsSource[] = [
  { id: 'coindesk', name: 'CoinDesk', url: 'https://www.coindesk.com/arc/outboundfeeds/rss', enabled: true, builtIn: true },
  { id: 'cointelegraph', name: 'Cointelegraph', url: 'https://cointelegraph.com/rss', enabled: true, builtIn: true },
  { id: 'decrypt', name: 'Decrypt', url: 'https://decrypt.co/feed', enabled: true, builtIn: true },
  { id: 'theblock', name: 'The Block', url: 'https://www.theblock.co/rss.xml', enabled: true, builtIn: true },
  { id: 'bitcoinmagazine', name: 'Bitcoin Magazine', url: 'https://bitcoinmagazine.com/feed', enabled: false, builtIn: true },
  { id: 'cryptoslate', name: 'CryptoSlate', url: 'https://cryptoslate.com/feed/', enabled: false, builtIn: true },
];

const REFRESH_MS = 5 * 60_000;
const FETCH_TIMEOUT_MS = 8_000;
const MAX_FEED_BYTES = 3_000_000;
const MAX_TITLE_CHARS = 200;
const MAX_REDIRECTS = 3;
const MAX_AGE_MS = 24 * 3_600_000;
const PER_SOURCE_LIMIT = 3;
export const DEFAULT_NEWS_LIMIT = 8;

const COIN_NAMES: Record<string, string[]> = {
  BTC: ['bitcoin'],
  ETH: ['ethereum', 'ether'],
  SOL: ['solana'],
  DOGE: ['dogecoin'],
  XRP: ['ripple'],
  ADA: ['cardano'],
  BNB: ['binance coin'],
  LTC: ['litecoin'],
  AVAX: ['avalanche'],
  SHIB: ['shiba inu'],
};

/**
 * Headlines reach a model, so they are hostile until proven otherwise. Angle brackets go so a
 * title cannot close the block it sits in, and format characters so nothing can hide in it.
 */
export function sanitizeHeadline(text: string): string {
  return text
    .replace(/[\p{Cc}\p{Cf}<>]/gu, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, MAX_TITLE_CHARS);
}

const ENTITIES: Record<string, string> = { lt: '<', gt: '>', quot: '"', apos: "'", nbsp: ' ', amp: '&' };

function decode(raw: string): string {
  const unwrapped = raw.replace(/<!\[CDATA\[([\s\S]*?)\]\]>/g, '$1');
  return unwrapped.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (match, code: string) => {
    if (code[0] !== '#') return ENTITIES[code.toLowerCase()] ?? match;
    const point = code[1]?.toLowerCase() === 'x' ? parseInt(code.slice(2), 16) : parseInt(code.slice(1), 10);
    return point <= 0x10ffff ? String.fromCodePoint(point) : '';
  });
}

const tag = (block: string, names: string[]) => {
  for (const name of names) {
    const found = new RegExp(`<${name}(?:\\s[^>]*)?>([\\s\\S]*?)</${name}>`, 'i').exec(block);
    if (found) return decode(found[1]!).trim();
  }
  return '';
};

const webUrl = (text: string) => (/^https?:\/\//i.test(text) ? text.slice(0, 500) : '');

/** Reads RSS and Atom. Only headlines are kept: bodies are long, costly, and more to attack with. */
export function parseFeed(xml: string, source: string): NewsItem[] {
  const blocks = xml.match(/<(item|entry)[\s>][\s\S]*?<\/\1>/gi) ?? [];
  return blocks.flatMap((block) => {
    const title = sanitizeHeadline(tag(block, ['title']).replace(/<[^>]*>/g, ' '));
    const publishedAt = Date.parse(tag(block, ['pubDate', 'published', 'updated', 'dc:date']));
    if (!title || Number.isNaN(publishedAt)) return [];
    const link = tag(block, ['link']) || (/<link[^>]*href="([^"]+)"/i.exec(block)?.[1] ?? '');
    return [{ id: `${source}:${title}`, source, title, url: webUrl(decode(link)), publishedAt }];
  });
}

/** Refuses anything but public https, because the backend fetches this URL on the user's machine. */
export function validateFeedUrl(text: string): string | null {
  let url: URL;
  try {
    url = new URL(text);
  } catch {
    return 'Not a valid URL';
  }
  if (url.protocol !== 'https:') return 'Feeds must use https';
  // A DNS root dot must not let local names evade the same checks as their undotted form.
  const host = url.hostname.toLowerCase().replace(/^\[|\]$/g, '').replace(/\.+$/, '');
  const isPrivate = host === 'localhost' || host.endsWith('.local') || host.endsWith('.localhost') || !host.includes('.')
    || /^(127|10|0)\./.test(host) || /^192\.168\./.test(host) || /^169\.254\./.test(host) || /^172\.(1[6-9]|2\d|3[01])\./.test(host)
    || host.includes(':')
    // The URL parser writes every IPv4 form (hex, octal, a bare number) as a dotted quad. An IP
    // literal is never resolved, so this is the only check it gets.
    || (isIPv4(host) && !isPublicAddress(host));
  return isPrivate ? PUBLIC_HOST_PROBLEM : null;
}

export function mentionsCoin(title: string, baseCurrency: string): boolean {
  const ticker = new RegExp(`(^|[^A-Za-z0-9])${baseCurrency}([^A-Za-z0-9]|$)`).test(title);
  const lower = title.toLowerCase();
  return ticker || (COIN_NAMES[baseCurrency] ?? []).some((name) => new RegExp(`\\b${name}\\b`).test(lower));
}

/** Newest first, this coin before general news, with a cap per source for variety. */
export function selectNews(items: NewsItem[], baseCurrency: string, now: number, limit = DEFAULT_NEWS_LIMIT): NewsItem[] {
  const recent = items
    .filter((item) => item.publishedAt <= now && now - item.publishedAt <= MAX_AGE_MS)
    .sort((a, b) => b.publishedAt - a.publishedAt);
  const ranked = [...recent.filter((i) => mentionsCoin(i.title, baseCurrency)), ...recent.filter((i) => !mentionsCoin(i.title, baseCurrency))];
  const perSource = new Map<string, number>();
  const seen = new Set<string>();
  const chosen: NewsItem[] = [];
  for (const item of ranked) {
    const used = perSource.get(item.source) ?? 0;
    const key = item.title.toLowerCase();
    if (used >= PER_SOURCE_LIMIT || seen.has(key)) continue;
    perSource.set(item.source, used + 1);
    seen.add(key);
    chosen.push(item);
    if (chosen.length === limit) break;
  }
  return chosen;
}

export interface NewsResult {
  items: NewsItem[];
  /** Set when a source failed. The items are still usable, just possibly incomplete. */
  error: string | null;
}

/** Fetches feeds on demand and caches them, so analysis never waits on news more than it must. */
export class NewsService {
  private cache: NewsItem[] = [];
  private fetchedAt = 0;
  private lastError: string | null = null;
  private refreshing: Promise<void> | null = null;

  constructor(
    private readonly sources: () => NewsSource[],
    // Connects only to public addresses; see createFeedFetch. Injectable for tests.
    private readonly fetchImpl: typeof fetch = feedFetch,
    private readonly now: () => number = Date.now,
  ) {}

  /** Call after sources change, so the next read does not serve the old set. */
  invalidate() {
    this.fetchedAt = 0;
  }

  /**
   * Follows redirects by hand: each hop must pass the same check as the URL the user entered, and
   * the default fetch checks again at connect time that the name resolves only to public addresses.
   */
  private async fetchFollowing(start: string): Promise<Response> {
    let url = start;
    for (let hop = 0; hop <= MAX_REDIRECTS; hop++) {
      const problem = validateFeedUrl(url);
      if (problem) throw new Error(`${problem}: ${url}`);
      const response = await this.fetchImpl(url, {
        headers: { 'user-agent': 'MochiPrep/0.1 (RSS reader)', accept: 'application/rss+xml, application/atom+xml, application/xml, text/xml' },
        redirect: 'manual',
        signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
      });
      const location = response.headers.get('location');
      if (response.status < 300 || response.status >= 400 || !location) return response;
      url = new URL(location, url).href;
    }
    throw new Error('Too many redirects');
  }

  private async fetchSource(source: NewsSource): Promise<NewsItem[]> {
    const response = await this.fetchFollowing(source.url);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const text = await response.text();
    if (text.length > MAX_FEED_BYTES) throw new Error('feed too large');
    return parseFeed(text, source.name);
  }

  private async refresh() {
    const enabled = this.sources().filter((s) => s.enabled);
    const results = await Promise.allSettled(enabled.map((s) => this.fetchSource(s)));
    const failed = enabled.filter((_, i) => results[i]!.status === 'rejected').map((s) => s.name);
    const items = results.flatMap((r) => (r.status === 'fulfilled' ? r.value : []));
    // A total outage keeps the last good headlines rather than leaving an agent with none.
    if (items.length > 0 || enabled.length === 0) this.cache = items;
    this.lastError = failed.length ? `Could not read: ${failed.join(', ')}` : null;
    this.fetchedAt = this.now();
  }

  async forSymbol(symbol: SymbolId, baseCurrency: string, limit = DEFAULT_NEWS_LIMIT): Promise<NewsResult> {
    if (this.now() - this.fetchedAt >= REFRESH_MS) {
      this.refreshing ??= this.refresh().finally(() => {
        this.refreshing = null;
      });
      await this.refreshing;
    }
    return { items: selectNews(this.cache, baseCurrency, this.now(), limit), error: this.lastError };
  }
}
