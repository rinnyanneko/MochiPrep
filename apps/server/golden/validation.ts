import fastify, { type InjectOptions } from 'fastify';
import {
  AddInstrumentSchema,
  ClosePositionSchema,
  CreateAccountSchema,
  NewsSourceInputSchema,
  NewsSourceToggleSchema,
  PlaceOrderSchema,
  ProtectionSchema,
  ReplayControlSchema,
  SymbolSchema,
  type ServerEvent,
} from '@mochiprep/shared';
import { ZodError, type ZodType } from 'zod';
import { CandleQuery, DecisionQuery, formatZodError, LimitQuery, NewsQuery, SourceQuery } from '../src/app.js';

/**
 * What the Node server answers for request bodies, query strings and path params, so the Rust
 * port (`crates/mochi-server/tests/golden_validation.rs`) can match it byte for byte. Every case
 * goes through a small Fastify app with buildApp's options and error handler, so the recorded
 * answer includes Fastify's own body parsing as well as the Zod schemas.
 */

const BODY_SCHEMAS: Record<string, ZodType> = {
  CreateAccount: CreateAccountSchema,
  PlaceOrder: PlaceOrderSchema,
  ClosePosition: ClosePositionSchema,
  Protection: ProtectionSchema,
  AddInstrument: AddInstrumentSchema,
  NewsSourceInput: NewsSourceInputSchema,
  NewsSourceToggle: NewsSourceToggleSchema,
  ReplayControl: ReplayControlSchema,
};

const QUERY_SCHEMAS: Record<string, ZodType> = {
  CandleQuery,
  SourceQuery,
  LimitQuery,
  DecisionQuery,
  NewsQuery,
};

const decimal41 = '1'.repeat(41);
const decimal40 = '1'.repeat(40);
const trailing = { kind: 'RATIO', callback: '0.01', activationPrice: null };
const order = {
  accountId: 'acc-1',
  clientOrderId: 'cli-1',
  symbol: 'BTCUSDT',
  side: 'BUY',
  type: 'MARKET',
  quantity: '0.01',
  leverage: 5,
};
const without = (value: Record<string, unknown>, key: string) => {
  const copy = { ...value };
  delete copy[key];
  return copy;
};

/** Inputs per body schema. Each is sent as JSON; strings are sent as written, to keep key order. */
const BODY_CASES: Record<string, unknown[]> = {
  CreateAccount: [
    { name: 'Main', source: 'REPLAY' },
    { name: '  Padded  ', source: 'LIVE', initialBalance: '2500.50' },
    { name: '   ', source: 'REPLAY' },
    { name: '', source: 'REPLAY' },
    { name: 'x'.repeat(60), source: 'REPLAY' },
    { name: 'x'.repeat(61), source: 'REPLAY' },
    // Length counts UTF-16 units: 30 emoji are 60 units, 31 are 62.
    { name: '😀'.repeat(30), source: 'REPLAY' },
    { name: '😀'.repeat(31), source: 'REPLAY' },
    // JS trim also removes no-break and ideographic spaces and the BOM.
    { name: ' 　﻿Main ', source: 'REPLAY' },
    { source: 'REPLAY' },
    { name: 'Main', source: 'PAPER' },
    { name: 'Main', source: 'REPLAY', initialBalance: null },
    { name: 'Main', source: 'REPLAY', initialBalance: 10000 },
    { name: 'Main', source: 'REPLAY', initialBalance: '-5' },
    { name: 'Main', source: 'REPLAY', initialBalance: '1.' },
    { name: 'Main', source: 'REPLAY', initialBalance: decimal40 },
    { name: 'Main', source: 'REPLAY', initialBalance: decimal41 },
    { name: 'Main', source: 'REPLAY', initialBalance: 'x'.repeat(41) },
    { name: 'Main', source: 'REPLAY', isDemo: true },
    '{"zeta":1,"name":"Main","alpha":2,"source":"REPLAY","10":3,"2":4}',
    { name: 7, source: null, initialBalance: [] },
    // Zod still runs length checks after a type error when the value has a .length: arrays
    // report items, other objects report "unknown", and the length is compared as JS would.
    { name: Array(61).fill(1), source: 'REPLAY', initialBalance: Array(41).fill('1') },
    { name: [], source: 'REPLAY', initialBalance: { length: 41 } },
    { name: { length: 'x' }, source: 'REPLAY', initialBalance: { length: null } },
    { name: { length: [61] }, source: 'REPLAY', initialBalance: { length: '0x29' } },
    { name: { length: true }, source: 'REPLAY', initialBalance: { length: [[40]] } },
    {},
    [],
    null,
    'Main',
    42,
  ],
  PlaceOrder: [
    order,
    { ...order, type: 'LIMIT', price: '68000.5', reduceOnly: false, stopLoss: '67000', takeProfit: '70000', trailingStop: trailing },
    { ...order, price: null, stopLoss: null, takeProfit: null, trailingStop: null },
    { ...order, reduceOnly: null },
    { ...order, trailingStop: { kind: 'SPREAD', callback: '50', activationPrice: '69000' } },
    { ...order, trailingStop: { kind: 'RATIO', callback: '0.01' } },
    { ...order, trailingStop: { ...trailing, extra: 1 } },
    { ...order, trailingStop: { kind: 'PERCENT', callback: 'x', activationPrice: 5 } },
    { ...order, trailingStop: 'none' },
    { ...order, leverage: 0 },
    { ...order, leverage: 101 },
    { ...order, leverage: 1 },
    { ...order, leverage: 100 },
    { ...order, leverage: 2.5 },
    { ...order, leverage: '5' },
    { ...order, leverage: 1e16 },
    { ...order, leverage: -0 },
    '{"accountId":"acc-1","clientOrderId":"cli-1","symbol":"BTCUSDT","side":"BUY","type":"MARKET","quantity":"0.01","leverage":5.0}',
    { ...order, quantity: '1e5' },
    { ...order, quantity: '.5' },
    { ...order, quantity: ' 1' },
    { ...order, quantity: '١٢' },
    { ...order, quantity: '1\n' },
    { ...order, price: decimal41 },
    { ...order, symbol: 'btcusdt' },
    { ...order, symbol: 'USDT' },
    { ...order, symbol: 'USDTUSDT' },
    { ...order, symbol: `${'A'.repeat(24)}USDT` },
    { ...order, symbol: `${'A'.repeat(25)}USDT` },
    { ...order, symbol: 'BTC-USDT' },
    { ...order, clientOrderId: '' },
    { ...order, clientOrderId: 'c'.repeat(128) },
    { ...order, clientOrderId: 'c'.repeat(129) },
    { ...order, accountId: [], clientOrderId: Array(200).fill(0), symbol: Array(3), quantity: { length: 50 }, price: Array(41).fill(null) },
    { ...order, side: 'buy', type: 'STOP_MARKET' },
    { ...order, agentId: 'agent-1' },
    { ...order, agentId: 'agent-1', decisionId: 'dec-1' },
    without(order, 'quantity'),
    without(without(order, 'accountId'), 'leverage'),
    '{"accountId":"acc-1","accountId":"acc-2","clientOrderId":"cli-1","symbol":"BTCUSDT","side":"BUY","type":"MARKET","quantity":"0.01","leverage":5}',
  ],
  ClosePosition: [
    { fraction: 0.5, clientOrderId: 'close-1' },
    { fraction: 1, clientOrderId: 'close-1' },
    { fraction: 0, clientOrderId: 'close-1' },
    { fraction: 1.0000001, clientOrderId: 'close-1' },
    { fraction: -0.5, clientOrderId: 'close-1' },
    { fraction: 1e-300, clientOrderId: 'close-1' },
    { fraction: '0.5', clientOrderId: 'close-1' },
    { fraction: null, clientOrderId: 'close-1' },
    { fraction: 0.5 },
    { fraction: 0.5, clientOrderId: '' },
    { fraction: 0.5, clientOrderId: 'c'.repeat(129) },
    { fraction: 0.5, clientOrderId: 'close-1', symbol: 'BTCUSDT' },
    { clientOrderId: 7 },
    {},
    [0.5],
  ],
  Protection: [
    { stopLoss: '67000', takeProfit: '70000' },
    { stopLoss: null, takeProfit: null },
    { stopLoss: null, takeProfit: null, trailingStop: null },
    { stopLoss: null, takeProfit: null, trailingStop: trailing },
    { stopLoss: '67000', takeProfit: null, trailingStop: { kind: 'SPREAD', callback: '25.5', activationPrice: '69000' } },
    { takeProfit: null },
    { stopLoss: null },
    {},
    { stopLoss: 67000, takeProfit: '70000' },
    { stopLoss: '-1', takeProfit: decimal41 },
    { stopLoss: null, takeProfit: null, trailingStop: {} },
    { stopLoss: null, takeProfit: null, trailingStop: { ...trailing, callback: null } },
    { stopLoss: null, takeProfit: null, trailingStop: [] },
    { stopLoss: null, takeProfit: null, pending: true },
    { stopLoss: null, takeProfit: null, b: 1, a: 2 },
  ],
  AddInstrument: [
    { symbol: 'DOGEUSDT' },
    { symbol: '1000PEPEUSDT' },
    { symbol: 'dogeusdt' },
    { symbol: 'DOGEUSDC' },
    { symbol: 'DOGE-USDT-SWAP' },
    { symbol: '' },
    { symbol: 'DOGEUSDT ' },
    { symbol: `${'Z'.repeat(24)}USDT` },
    { symbol: `${'Z'.repeat(25)}USDT` },
    { symbol: null },
    { symbol: ['DOGEUSDT'] },
    { symbol: 'DOGEUSDT', venue: 'OKX' },
    {},
    'DOGEUSDT',
    true,
  ],
  NewsSourceInput: [
    { name: 'CoinDesk', url: 'https://www.coindesk.com/arc/outboundfeeds/rss/' },
    { name: '  Padded  ', url: '  https://example.com/feed.xml  ' },
    { name: 'Tabs', url: 'https://exa\tmple.com/fe\ned' },
    { name: 'Plain', url: 'http://example.com' },
    { name: 'Mail', url: 'mailto:someone@example.com' },
    { name: 'Odd', url: 'http:example.com' },
    { name: 'Rel', url: '/feed.xml' },
    { name: 'Words', url: 'not a url' },
    { name: 'Empty', url: '' },
    { name: 'Long', url: `https://example.com/${'a'.repeat(280)}` },
    { name: 'TooLong', url: `https://example.com/${'a'.repeat(282)}` },
    { name: 'Both', url: `not a url ${'a'.repeat(300)}` },
    { name: 'x'.repeat(40), url: 'https://example.com' },
    { name: 'x'.repeat(41), url: 'https://example.com' },
    { name: ' ', url: 'https://example.com' },
    { name: 'IPv6', url: 'http://[::1]:8080/feed' },
    { name: 'BadPort', url: 'http://example.com:99999/' },
    { url: 'https://example.com' },
    { name: 'Extra', url: 'https://example.com', enabled: true },
    { name: null, url: 5 },
    { name: { length: 0 }, url: Array(301).fill('a') },
  ],
  NewsSourceToggle: [
    { enabled: true },
    { enabled: false },
    { enabled: 'true' },
    { enabled: 1 },
    { enabled: null },
    {},
    { enabled: true, id: 'news_1' },
    { enabled: [] },
    { enabled: {} },
    null,
    [],
    'enabled',
    0,
    false,
    { Enabled: true },
  ],
  ReplayControl: [
    {},
    { playing: true },
    { playing: false, speed: 20, steps: 5 },
    { speed: 1 },
    { speed: 600 },
    { speed: 0 },
    { speed: 601 },
    { speed: 2.5 },
    { speed: null },
    { steps: 1440 },
    { steps: 1441 },
    { steps: 0 },
    { steps: '5' },
    { playing: 'yes' },
    { playing: null, speed: null, steps: null },
    { playing: true, reset: true },
    { speed: 1e300, steps: -1e300 },
  ],
};

/** Raw query strings, as the client sends them after `?`. */
const QUERY_CASES: Record<string, string[]> = {
  CandleQuery: [
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m',
    'source=LIVE&symbol=ETHUSDT&timeframe=4h&limit=5',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=0',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=1',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=1000',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=1001',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=5.5',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=abc',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=%205%20',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=0x10',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=1e2',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=Infinity',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=1e400',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=1e16',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=5&limit=6',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=+7',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&limit=%2B7',
    'source=REPLAY&symbol=BTCUSDT&timeframe=2m',
    'source=replay&symbol=btcusdt&timeframe=1m',
    'symbol=BTCUSDT',
    '',
    'source=REPLAY&symbol=BTCUSDT&timeframe=1m&extra=1',
    'source=REPLAY&source=LIVE&symbol=BTCUSDT&timeframe=1m',
    'source=REPLAY&symbol=BTC%55SDT&timeframe=1m',
    'source=REPLAY&symbol=%E2%82%ACUSDT&timeframe=1m',
    'source=REPLAY&symbol=%ZZUSDT&timeframe=1m',
    'source=REPLAY&symbol=%C3USDT&timeframe=1m',
  ],
  SourceQuery: ['source=REPLAY', 'source=LIVE', 'source=', 'source', '', 'source=PAPER', 'source=LIVE&x=1', 'Source=LIVE'],
  LimitQuery: [
    '',
    'limit=5',
    'limit=5.5',
    'limit=abc',
    'limit=0',
    'limit=1',
    'limit=2000',
    'limit=2001',
    'limit=-0',
    'limit=0b11',
    'limit=0o17',
    'limit=.5e1',
    'limit=1.',
    'limit=1_000',
    'limit=%09%0A12%0D',
    'limit',
    'limit=5&limit=',
    'limit=3&other=x',
  ],
  DecisionQuery: ['', 'limit=10', 'agentId=agent-1', 'agentId=', 'agentId=a&agentId=b', 'agentId=a+b%20c&limit=20', 'limit=0&agentId=x'],
  NewsQuery: ['symbol=BTCUSDT', 'symbol=btcusdt', '', 'symbol=', 'symbol=BTCUSDT&symbol=ETHUSDT', 'symbol=DOGE%55SDT'],
};

/** Decoded path params for `/api/instruments/:symbol` and `/api/positions/:accountId/:symbol/...`. */
const SYMBOL_PARAMS = ['BTCUSDT', '1000PEPEUSDT', 'btcusdt', 'BTC USDT', 'BTCUSDC', 'USDT', `${'A'.repeat(25)}USDT`, 'ÉUSDT'];

interface ParserCase {
  method: 'POST' | 'PUT' | 'DELETE';
  /** A body schema name, or 'none' for a route that never reads its body (e.g. /api/demo/start). */
  route: string;
  headers: Record<string, string>;
  body?: string;
}

const okBody = '{"enabled":true}';
/** How Fastify's body parser treats content types and bodies before any schema runs. */
const PARSER_CASES: ParserCase[] = [
  { method: 'POST', route: 'none', headers: {} },
  { method: 'POST', route: 'none', headers: { 'content-length': '0' } },
  { method: 'POST', route: 'ReplayControl', headers: {} },
  { method: 'PUT', route: 'NewsSourceToggle', headers: {} },
  { method: 'DELETE', route: 'none', headers: {} },
  { method: 'POST', route: 'none', headers: { 'content-type': 'application/json' }, body: '' },
  { method: 'DELETE', route: 'none', headers: { 'content-type': 'application/json' }, body: '' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '{"enabled":' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '   ' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: "{'enabled':true}" },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '{"enabled":true,}' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: `${okBody} x` },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: ` \r\n\t${okBody}\n` },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: `﻿${okBody}` },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: 'null' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '"text"' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '[true]' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '{"enabled":true,"__proto__":{"x":1}}' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '{"enabled":true,"\\u005f_proto__":1}' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '{"a":[{"__proto__":null}]}' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '{"enabled":true,"constructor":{"prototype":{}}}' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '{"enabled":true,"constructor":{"name":"x"}}' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '{"enabled":true,"constructor":[1]}' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: '{"enabled":true,"note":"\\"__proto__\\":"}' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json; charset=utf-8' }, body: okBody },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'Application/JSON' }, body: okBody },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json;charset="utf-8"' }, body: okBody },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json ; x=é' }, body: okBody },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json/x' }, body: okBody },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/jsonx' }, body: okBody },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/vnd.api+json' }, body: okBody },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'application/x-www-form-urlencoded' }, body: 'enabled=true' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'multipart/form-data; boundary=x' }, body: '--x--' },
  { method: 'POST', route: 'none', headers: { 'content-type': 'application/xml' }, body: '<a/>' },
  { method: 'POST', route: 'none', headers: { 'content-type': 'application/xml' }, body: '' },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'json' }, body: okBody },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': '' }, body: okBody },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': '; charset=utf-8' }, body: okBody },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'text/plain' }, body: okBody },
  { method: 'POST', route: 'NewsSourceToggle', headers: { 'content-type': 'text/plain; charset=utf-8' }, body: '' },
  { method: 'POST', route: 'none', headers: { 'content-type': 'text/plain' }, body: 'hello' },
  { method: 'POST', route: 'none', headers: {}, body: okBody },
  { method: 'POST', route: 'NewsSourceToggle', headers: {}, body: okBody },
  { method: 'PUT', route: 'NewsSourceToggle', headers: { 'content-type': 'application/json' }, body: okBody },
  { method: 'DELETE', route: 'none', headers: { 'content-type': 'application/json' }, body: '{"x":1}' },
  { method: 'DELETE', route: 'none', headers: { 'content-type': 'text/csv' }, body: 'a,b' },
];

/** JSON.stringify output of each event type, which the SSE stream sends after `data: `. */
const EVENTS: ServerEvent[] = [
  { type: 'heartbeat' },
  {
    type: 'quote',
    quote: {
      source: 'LIVE', symbol: 'BTCUSDT', bid: '68000.1', ask: '68000.2', last: '68000.1', mark: '68000.15',
      markEstimated: false, fundingRate: '0.0001', nextFundingTime: 1767254400000, ts: 1767225600123,
    },
  },
  {
    type: 'quote',
    quote: {
      source: 'REPLAY', symbol: 'ETHUSDT', bid: '3500', ask: '3500.5', last: '3500.2', mark: '3500.25',
      markEstimated: true, fundingRate: null, nextFundingTime: null, ts: 1767225600000,
    },
  },
  { type: 'feed', status: { source: 'REPLAY', provider: 'Deterministic replay', state: 'OFFLINE_REPLAY', lastMessageAt: 1767571200000 } },
  { type: 'feed', status: { source: 'LIVE', provider: 'OKX public', state: 'DEGRADED', lastMessageAt: null, detail: 'WebSocket silent; polling REST' } },
  { type: 'feed', status: { source: 'LIVE', provider: 'OKX public', state: 'CONNECTED', lastMessageAt: 1767225600999 } },
  { type: 'feed', status: { source: 'LIVE', provider: 'OKX public', state: 'CONNECTING', lastMessageAt: null } },
  { type: 'feed', status: { source: 'LIVE', provider: 'OKX public', state: 'DISCONNECTED', lastMessageAt: 5, detail: 'closed' } },
  {
    type: 'replay',
    state: { loaded: true, playing: false, speed: 20, now: 1767571200000, startTime: 1767571200000, endTime: 1768780740000, atEnd: false, progress: 0 },
  },
  {
    type: 'replay',
    state: { loaded: true, playing: true, speed: 600, now: 1768000000000, startTime: 1767571200000, endTime: 1768780740000, atEnd: false, progress: (1768000000000 - 1767571200000) / (1768780740000 - 1767571200000) },
  },
  {
    type: 'replay',
    state: { loaded: true, playing: false, speed: 1, now: 1768780740000, startTime: 1767571200000, endTime: 1768780740000, atEnd: true, progress: 1 },
  },
  { type: 'invalidate', topics: ['accounts', 'orders', 'positions', 'agents', 'decisions', 'providers', 'arena', 'exchange', 'instruments', 'news', 'vault'] },
  { type: 'invalidate', topics: [] },
  { type: 'toast', level: 'info', message: 'Momentum suggests LONG BTCUSDT' },
  { type: 'toast', level: 'warn', message: 'Quote is stale' },
  { type: 'toast', level: 'error', message: 'Main: LONG BTCUSDT was liquidated (simulated) "quoted" é \n' },
];

interface Answer {
  status: number;
  response: unknown;
}

/** The body headers the last request reached the server with (inject adds content-length). */
let seen: Record<string, string> = {};

/** A Fastify app with buildApp's options and the same error handler, minus the runtime. */
function miniApp() {
  const app = fastify({ logger: false, bodyLimit: 256 * 1024 });
  app.addHook('onRequest', async (request) => {
    seen = {};
    for (const name of ['content-type', 'content-length', 'transfer-encoding']) {
      const value = request.headers[name];
      if (typeof value === 'string') seen[name] = value;
    }
  });
  app.setErrorHandler((error: Error & { statusCode?: number }, _request, reply) => {
    if (error instanceof ZodError) return reply.code(400).send({ error: formatZodError(error) });
    return reply.code(error.statusCode ?? 400).send({ error: error.message });
  });
  const body = async (request: { params: unknown; body: unknown }) => {
    const { schema } = request.params as { schema: string };
    if (schema === 'none') return { ok: true };
    return BODY_SCHEMAS[schema]!.parse(request.body);
  };
  app.post('/body/:schema', body);
  app.put('/body/:schema', body);
  app.delete('/body/:schema', body);
  app.get('/query/:schema', async (request) => QUERY_SCHEMAS[(request.params as { schema: string }).schema]!.parse(request.query));
  // Wrapped in an object: Fastify would send a bare string as text/plain.
  app.get('/symbol/:symbol', async (request) => ({ symbol: SymbolSchema.parse((request.params as { symbol: string }).symbol) }));
  return app;
}

type App = ReturnType<typeof miniApp>;

async function answer(app: App, options: InjectOptions): Promise<Answer> {
  const response = await app.inject(options);
  return { status: response.statusCode, response: response.json() };
}

export async function validationFixture() {
  const app = miniApp();
  await app.ready();
  try {
    const bodies: Array<{ schema: string; body: string } & Answer> = [];
    for (const [schema, inputs] of Object.entries(BODY_CASES)) {
      for (const input of inputs) {
        const body = typeof input === 'string' && /^[[{]/.test(input) ? input : JSON.stringify(input);
        const result = await answer(app, {
          method: 'POST',
          url: `/body/${schema}`,
          headers: { 'content-type': 'application/json' },
          payload: body,
        });
        bodies.push({ schema, body, ...result });
      }
    }

    const parser: Array<ParserCase & Answer> = [];
    for (const testCase of PARSER_CASES) {
      const result = await answer(app, {
        method: testCase.method,
        url: `/body/${testCase.route}`,
        headers: testCase.headers,
        ...(testCase.body === undefined ? {} : { payload: testCase.body }),
      });
      // Record the headers as the server saw them, so Rust replays exactly those.
      parser.push({ ...testCase, headers: seen, ...result });
    }

    const queries: Array<{ schema: string; query: string } & Answer> = [];
    for (const [schema, list] of Object.entries(QUERY_CASES)) {
      for (const query of list) {
        queries.push({ schema, query, ...(await answer(app, { method: 'GET', url: `/query/${schema}?${query}` })) });
      }
    }

    const symbols: Array<{ param: string } & Answer> = [];
    for (const param of SYMBOL_PARAMS) {
      symbols.push({ param, ...(await answer(app, { method: 'GET', url: `/symbol/${encodeURIComponent(param)}` })) });
    }

    return { bodies, parser, queries, symbols, events: EVENTS.map((event) => JSON.stringify(event)) };
  } finally {
    await app.close();
  }
}
