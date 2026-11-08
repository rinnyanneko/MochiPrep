import { OkxMarketData, parseOkxCandles, parseOkxInstrument } from '@mochiprep/market-data';
import type { Quote, SymbolId, Timeframe } from '@mochiprep/shared';

/**
 * The public OKX adapter over fixed OKX payloads, for `crates/golden/okx-public.json`: which
 * contracts qualify and their specs, candle parsing, the requests the adapter makes, how socket
 * messages and REST polls merge into quotes, and the error texts of a failed request. Every reply
 * the adapter receives is recorded next to what it made of it, so Rust can serve the same replies.
 */

type Row = Record<string, string>;

/** Real OKX field names and typical values, plus every way a row can fail the eligibility rules. */
const live = (instId: string, ctValCcy: string, fields: Row): Row => ({
  instType: 'SWAP', instId, uly: instId.replace(/-SWAP$/, ''), instFamily: instId.replace(/-SWAP$/, ''),
  settleCcy: 'USDT', ctValCcy, ctType: 'linear', ctMult: '1', state: 'live', ruleType: 'normal',
  instCategory: '1', listTime: '1611916828000', ...fields,
});

const INSTRUMENT_ROWS: Row[] = [
  live('BTC-USDT-SWAP', 'BTC', { ctVal: '0.01', lotSz: '0.01', minSz: '0.01', tickSz: '0.1', lever: '100' }),
  live('ETH-USDT-SWAP', 'ETH', { ctVal: '0.1', lotSz: '0.01', minSz: '0.01', tickSz: '0.01', lever: '100' }),
  live('DOGE-USDT-SWAP', 'DOGE', { ctVal: '1000', lotSz: '0.01', minSz: '0.01', tickSz: '0.00001', lever: '75' }),
  live('PEPE-USDT-SWAP', 'PEPE', { ctVal: '10000000', lotSz: '0.1', minSz: '0.1', tickSz: '0.0000000001', lever: '50' }),
  live('1INCH-USDT-SWAP', '1INCH', { ctVal: '1', lotSz: '1', minSz: '1', tickSz: '0.0001', lever: '' }),
  live('TINY-USDT-SWAP', 'TINY', { ctVal: '0.0001', lotSz: '0.001', minSz: '0.003', tickSz: '0.000001', lever: 'abc' }),
  live('NOLEV-USDT-SWAP', 'NOLEV', { ctVal: '10', lotSz: '1', minSz: '1', tickSz: '0.001' }),
  live('ZERO-USDT-SWAP', 'ZERO', { ctVal: '1', lotSz: '1', minSz: '1', tickSz: '0.01', lever: '0' }),
  live('SPACE-USDT-SWAP', 'SPACE', { ctVal: '1', lotSz: '1', minSz: '1', tickSz: '0.01', lever: ' 20 ' }),
  // Refused: inverse, not live, pre-market rules, a stock, a mismatched contract currency.
  { ...live('BTC-USD-SWAP', 'USD', { ctVal: '100', lotSz: '1', minSz: '1', tickSz: '0.1', lever: '100' }), settleCcy: 'BTC', ctType: 'inverse' },
  live('NEW-USDT-SWAP', 'NEW', { ctVal: '1', lotSz: '1', minSz: '1', tickSz: '0.01', lever: '20', state: 'preopen' }),
  live('PRE-USDT-SWAP', 'PRE', { ctVal: '1', lotSz: '1', minSz: '1', tickSz: '0.01', lever: '5', ruleType: 'pre_market' }),
  live('TSLA-USDT-SWAP', 'TSLA', { ctVal: '0.01', lotSz: '1', minSz: '1', tickSz: '0.01', lever: '10', instCategory: '3' }),
  live('SATS-USDT-SWAP', '1000SATS', { ctVal: '1', lotSz: '1', minSz: '1', tickSz: '0.0000001', lever: '20' }),
  live('XAU-USDT-SWAP', 'XAU', { ctVal: '0.001', lotSz: '1', minSz: '1', tickSz: '0.1', lever: '50', settleCcy: 'USDC' }),
  // Refused: a missing or empty size field, an id that is not a USDT swap.
  live('NOTICK-USDT-SWAP', 'NOTICK', { ctVal: '1', lotSz: '1', minSz: '1', lever: '20' }),
  live('EMPTY-USDT-SWAP', 'EMPTY', { ctVal: '1', lotSz: '', minSz: '1', tickSz: '0.01', lever: '20' }),
  live('btc-USDT-SWAP', 'btc', { ctVal: '1', lotSz: '1', minSz: '1', tickSz: '0.01', lever: '20' }),
  live('BTC-USDT-250627', 'BTC', { ctVal: '0.01', lotSz: '1', minSz: '1', tickSz: '0.1', lever: '100' }),
  live('-USDT-SWAP', '', { ctVal: '1', lotSz: '1', minSz: '1', tickSz: '0.01', lever: '20' }),
  { ctVal: '1', lotSz: '1', minSz: '1', tickSz: '0.01' },
];

/** decimal.js refuses these, which fails the whole list. */
const BAD_DECIMAL_ROWS: Row[] = [
  live('BADVAL-USDT-SWAP', 'BADVAL', { ctVal: 'abc', lotSz: '1', minSz: '1', tickSz: '0.01', lever: '20' }),
  live('BADLOT-USDT-SWAP', 'BADLOT', { ctVal: '1', lotSz: '1,5', minSz: '1', tickSz: '0.01', lever: '20' }),
];

const CANDLE_ROWS: string[][] = [
  ['1767229200000', '67950.1', '68010', '67900.5', '68001.2', '1520.5', '15.205', '1033444.12', '0'],
  ['1767225600000', '67890', '67990.9', '67850', '67950.1', '2210', '22.1', '1501234.5', '1'],
  ['1767222000000', '67800.25', '67900', '67700', '67890', '1800.25', '18.0025', '1222333.4', '1'],
];

const envelope = (data: unknown[]) => ({ code: '0', msg: '', data });

/** A fetch that answers from a URL -> [status, body] table, and records each URL asked for. */
function fakeFetch(replies: Record<string, [number, unknown]>, asked: string[]) {
  return (async (url: string) => {
    asked.push(url);
    const reply = replies[url];
    if (!reply) throw new TypeError('fetch failed');
    const [status, body] = reply;
    return { ok: status >= 200 && status < 300, status, json: async () => body } as Response;
  }) as unknown as typeof fetch;
}

const REST = 'https://okx.test';

function instrumentsCase() {
  return INSTRUMENT_ROWS.map((row) => ({ row, spec: parseOkxInstrument(row) }));
}

async function listCase() {
  const url = `${REST}/api/v5/public/instruments?instType=SWAP`;
  const ok = new OkxMarketData({ restBase: REST, fetchImpl: fakeFetch({ [url]: [200, envelope(INSTRUMENT_ROWS)] }, []) });
  const bad = new OkxMarketData({ restBase: REST, fetchImpl: fakeFetch({ [url]: [200, envelope([...INSTRUMENT_ROWS, ...BAD_DECIMAL_ROWS])] }, []) });
  return {
    reply: envelope(INSTRUMENT_ROWS),
    specs: await ok.listInstruments(),
    badReply: envelope([...INSTRUMENT_ROWS, ...BAD_DECIMAL_ROWS]),
    badError: await bad.listInstruments().then(() => null, (error: Error) => error.message),
  };
}

const CANDLE_REQUESTS: Array<{ symbol: SymbolId; timeframe: Timeframe; limit: number }> = [
  { symbol: 'BTCUSDT', timeframe: '1m', limit: 3 },
  { symbol: 'ETHUSDT', timeframe: '1h', limit: 1000 },
  { symbol: 'DOGEUSDT', timeframe: '4h', limit: 300 },
  { symbol: 'SOLUSDT', timeframe: '15m', limit: 50 },
  { symbol: 'BTCUSDT', timeframe: '5m', limit: 10 },
];

/** Requests with every way one can fail: an OKX error code, an HTTP error, no answer at all. */
async function candlesCase() {
  const asked: string[] = [];
  const url = (instId: string, bar: string, limit: number) =>
    `${REST}/api/v5/market/candles?instId=${instId}&bar=${bar}&limit=${limit}`;
  const replies: Record<string, [number, unknown]> = {
    [url('BTC-USDT-SWAP', '1m', 3)]: [200, envelope(CANDLE_ROWS)],
    [url('ETH-USDT-SWAP', '1H', 300)]: [200, envelope(CANDLE_ROWS.slice(0, 2))],
    [url('DOGE-USDT-SWAP', '4H', 300)]: [200, { code: '51001', msg: 'Instrument ID does not exist', data: [] }],
    [url('SOL-USDT-SWAP', '15m', 50)]: [503, { code: '0', msg: '', data: [] }],
  };
  const feed = new OkxMarketData({ restBase: REST, fetchImpl: fakeFetch(replies, asked) });
  const results = [];
  for (const request of CANDLE_REQUESTS) {
    const before = asked.length;
    const outcome = await feed.getCandles(request.symbol, request.timeframe, request.limit).then(
      (candles) => ({ candles }),
      (error: Error) => ({ error: error.message }),
    );
    results.push({ ...request, url: asked[before] ?? null, ...outcome });
  }
  return { replies, parsed: parseOkxCandles(CANDLE_ROWS), results };
}

const msg = (channel: string, instId: string, row: Row) => JSON.stringify({ arg: { channel, instId }, data: [row] });

const MESSAGES: string[] = [
  msg('mark-price', 'BTC-USDT-SWAP', { instType: 'SWAP', instId: 'BTC-USDT-SWAP', markPx: '67890.5', ts: '1767225600100' }),
  msg('funding-rate', 'BTC-USDT-SWAP', { instType: 'SWAP', instId: 'BTC-USDT-SWAP', fundingRate: '0.0001', fundingTime: '1767254400000', nextFundingTime: '1767283200000' }),
  msg('tickers', 'BTC-USDT-SWAP', { instType: 'SWAP', instId: 'BTC-USDT-SWAP', last: '67890.1', lastSz: '0.1', askPx: '67890.2', askSz: '12', bidPx: '67890', bidSz: '8', ts: '1767225600123' }),
  msg('tickers', 'ETH-USDT-SWAP', { instId: 'ETH-USDT-SWAP', last: '0.15', askPx: '0.2', bidPx: '0.1', ts: '1767225600200' }),
  msg('tickers', 'SOL-USDT-SWAP', { instId: 'SOL-USDT-SWAP', last: '150', askPx: '150.01', bidPx: '149.99' }),
  'pong',
  '{not json',
  msg('tickers', 'XRP-USDT-SWAP', { last: '2', askPx: '2.01', bidPx: '1.99', ts: '1' }),
  JSON.stringify({ event: 'subscribe', arg: { channel: 'tickers', instId: 'BTC-USDT-SWAP' }, connId: 'a1b2' }),
  msg('tickers', 'DOGE-USDT-SWAP', { last: '0.31', askPx: '0.3101', bidPx: '', ts: '1767225600300' }),
  msg('mark-price', 'ETH-USDT-SWAP', {}),
  msg('tickers', 'ETH-USDT-SWAP', { last: '3500.5', askPx: '3500.6', bidPx: '3500.4', ts: '1767225600400' }),
  msg('mark-price', 'SOL-USDT-SWAP', { markPx: '' }),
  msg('tickers', 'SOL-USDT-SWAP', { last: '150', askPx: '150.01', bidPx: '149.99', ts: '1767225600500' }),
  msg('funding-rate', 'DOGE-USDT-SWAP', { fundingRate: '-0.00005' }),
  msg('tickers', 'DOGE-USDT-SWAP', { last: '0.31', askPx: '0.3101', bidPx: '0.3099', ts: '1767225600600' }),
  JSON.stringify({ arg: { channel: 'tickers', instId: 'BTC-USDT-SWAP' }, data: [] }),
  msg('tickers', 'BTC-USDT-SWAP', { last: '67891', askPx: '67891.1', bidPx: '67890.9', ts: '0' }),
  msg('tickers', 'BTC-USDT-SWAP', { last: '67891', askPx: '67891.1', bidPx: '67890.9', ts: '1767225600700' }),
];

const QUOTE_SYMBOLS: SymbolId[] = ['BTCUSDT', 'ETHUSDT', 'SOLUSDT', 'DOGEUSDT', 'XRPUSDT'];

/** Socket messages one by one, with the quote each one emitted, then every symbol's quote. */
function messagesCase() {
  const feed = new OkxMarketData();
  feed.trackSymbol('DOGEUSDT');
  let emitted: Quote | null = null;
  feed.onQuote((quote) => (emitted = quote));
  const steps = MESSAGES.map((raw) => {
    emitted = null;
    feed.handleMessage(raw);
    return { raw, quote: emitted };
  });
  return { tracked: ['DOGEUSDT'], steps, final: QUOTE_SYMBOLS.map((s) => feed.getQuote(s) ?? null) };
}

const restUrls = (instId: string) => ({
  ticker: `${REST}/api/v5/market/ticker?instId=${instId}`,
  mark: `${REST}/api/v5/public/mark-price?instType=SWAP&instId=${instId}`,
  funding: `${REST}/api/v5/public/funding-rate?instId=${instId}`,
});

function restReplies(rows: Record<string, { ticker?: Row[]; mark?: Row[]; funding?: Row[] | 'error' }>) {
  const replies: Record<string, [number, unknown]> = {};
  for (const [instId, r] of Object.entries(rows)) {
    const urls = restUrls(instId);
    replies[urls.ticker] = [200, envelope(r.ticker ?? [])];
    replies[urls.mark] = [200, envelope(r.mark ?? [])];
    replies[urls.funding] = r.funding === 'error'
      ? [200, { code: '50011', msg: 'Too Many Requests', data: [] }]
      : [200, envelope(r.funding ?? [])];
  }
  return replies;
}

/** Two REST polls on one feed: the second sees what the first left behind. */
async function pollsCase() {
  const ticker = (last: string, bid: string, ask: string, ts: string) => [{ instId: 'x', last, bidPx: bid, askPx: ask, ts }];
  const rounds = [
    restReplies({
      'BTC-USDT-SWAP': { ticker: ticker('67890.1', '67890', '67890.2', '1767225600123'), mark: [{ markPx: '67890.5' }], funding: [{ fundingRate: '0.0001', fundingTime: '1767254400000' }] },
      'ETH-USDT-SWAP': { ticker: ticker('3500.5', '3500.4', '3500.6', '1767225600200'), funding: [{ fundingRate: '0.0002', fundingTime: '1767254400000' }] },
      'SOL-USDT-SWAP': { mark: [{ markPx: '150' }], funding: [{ fundingRate: '0.0003', fundingTime: '1767254400000' }] },
      'DOGE-USDT-SWAP': { ticker: ticker('0.31', '0.3099', '0.3101', '1767225600300'), mark: [{ markPx: '0.31' }], funding: 'error' },
    }),
    restReplies({
      'BTC-USDT-SWAP': { ticker: ticker('67900', '67899.9', '67900.1', '1767225605123'), funding: [{ fundingRate: '0.0001', fundingTime: '1767254400000' }] },
      'ETH-USDT-SWAP': { ticker: ticker('3501', '3500.9', '3501.1', '1767225605200'), mark: [{ markPx: '3501' }], funding: [{ fundingRate: '0.0002' }] },
      'SOL-USDT-SWAP': { ticker: ticker('151', '150.99', '151.01', '1767225605300') },
      'DOGE-USDT-SWAP': { ticker: ticker('0.32', '0.3199', '0.3201', '1767225605400'), mark: [{ markPx: '0.32' }], funding: [{ fundingRate: '-0.0001', fundingTime: '1767254400000' }] },
    }),
  ];
  let replies: Record<string, [number, unknown]> = {};
  const feed = new OkxMarketData({ restBase: REST, fetchImpl: fakeFetch(new Proxy({}, { get: (_, url: string) => replies[url] }), []) });
  feed.trackSymbol('DOGEUSDT');
  const emitted: Quote[] = [];
  feed.onQuote((quote) => emitted.push(quote));
  const results = [];
  for (const round of rounds) {
    replies = round;
    emitted.length = 0;
    await (feed as unknown as { pollRest(): Promise<void> }).pollRest();
    results.push({
      replies: round,
      // Symbols poll in parallel, so emission order is not part of the contract.
      emitted: [...emitted].sort((a, b) => a.symbol.localeCompare(b.symbol)),
      quotes: QUOTE_SYMBOLS.map((s) => feed.getQuote(s) ?? null),
    });
  }
  return { tracked: ['DOGEUSDT'], rounds: results };
}

export async function okxPublicFixture() {
  const fixture = {
    restBase: REST,
    instruments: instrumentsCase(),
    list: await listCase(),
    candles: await candlesCase(),
    messages: messagesCase(),
    polls: await pollsCase(),
  };
  // JSON's own view: NaN becomes null, as the API sends it.
  return JSON.parse(JSON.stringify(fixture)) as unknown;
}
