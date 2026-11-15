import {
  OkxClient,
  OkxRejection,
  OkxUnknownOutcome,
  sign,
  toAlgoClOrdId,
  toClOrdId,
  toInstId,
  type AlgoOrderBody,
  type PlaceOrderBody,
} from '@mochiprep/exchanges';

/**
 * The signed OKX trading client, for `crates/golden/okx-private.json`: signatures and id hashes,
 * the exact request each endpoint sends (with the clock fixed through the client's `now` hook), and
 * what each endpoint makes of every kind of answer, including no answer at all. Rust replays the
 * same answers through its own client and must produce the same requests, results and error texts.
 */

const CREDENTIALS = { apiKey: 'golden-api-key-0123', secretKey: 'golden-secret-key-4567', passphrase: 'golden-passphrase-89' };
const BASE = 'https://okx.test';
const NOW = Date.UTC(2026, 0, 2, 3, 4, 5, 67);

const SIGN_CASES: Array<[string, string, string, string, string]> = [
  ['test-secret', '2020-12-08T09:08:57.715Z', 'POST', '/api/v5/trade/order', '{"instId":"BTC-USDT-SWAP"}'],
  ['test-secret', '2020-12-08T09:08:57.715Z', 'GET', '/api/v5/account/balance?ccy=USDT', ''],
  ['', '2026-01-02T03:04:05.067Z', 'GET', '/api/v5/account/config', ''],
  ['sëcret-ünicode', '2026-01-02T03:04:05.067Z', 'POST', '/api/v5/trade/cancel-algos', '[{"instId":"ETH-USDT-SWAP","algoId":"1"}]'],
  ['k'.repeat(100), '1999-12-31T23:59:59.999Z', 'POST', '/api/v5/trade/order-algo', '{"note":"é\\n\\u0001"}'],
];

const SYMBOLS = ['BTCUSDT', 'ETHUSDT', 'PEPEUSDT', '1000SATSUSDT', 'USDTUSDT', 'BTCUSDTX', 'USDT', ''];
const IDS = ['', 'agent:agent_123:dec_456', 'ord_1', 'ord_1:sl', 'ord_1:trail', 'manual:ü', 'x'.repeat(300)];
const ISO_TIMES = [0, NOW, 951_782_400_000, 4_102_444_799_999, 1_767_225_600_123];

const PLACE_MARKET: PlaceOrderBody = {
  instId: 'BTC-USDT-SWAP',
  tdMode: 'isolated',
  side: 'buy',
  ordType: 'market',
  sz: '12',
  clOrdId: toClOrdId('agent:agent_1:dec_1'),
  attachAlgoOrds: [
    { attachAlgoClOrdId: toAlgoClOrdId('ord_1:sl'), slTriggerPx: '74250.5', slOrdPx: '-1' },
    { attachAlgoClOrdId: toAlgoClOrdId('ord_1:tp'), tpTriggerPx: '76500', tpOrdPx: '-1' },
    { attachAlgoClOrdId: toAlgoClOrdId('ord_1:trail'), callbackRatio: '0.02', activePx: '75500' },
  ],
};
const PLACE_LIMIT_EXIT: PlaceOrderBody = {
  instId: 'PEPE-USDT-SWAP',
  tdMode: 'isolated',
  side: 'sell',
  ordType: 'limit',
  sz: '0.3',
  clOrdId: toClOrdId('manual:close'),
  px: '0.0000123456',
  reduceOnly: true,
};
const SL_ALGO = {
  instId: 'BTC-USDT-SWAP', tdMode: 'isolated', side: 'sell', algoClOrdId: toAlgoClOrdId('ord_2'), sz: '12', reduceOnly: true,
  ordType: 'conditional', slTriggerPx: '74000', slOrdPx: '-1',
} as const satisfies AlgoOrderBody;
const TP_ALGO = {
  instId: 'ETH-USDT-SWAP', tdMode: 'isolated', side: 'buy', algoClOrdId: toAlgoClOrdId('ord_3'), sz: '4', reduceOnly: true,
  ordType: 'conditional', tpTriggerPx: '3100.25', tpOrdPx: '-1',
} as const satisfies AlgoOrderBody;
const TRAIL_RATIO = {
  instId: 'BTC-USDT-SWAP', tdMode: 'isolated', side: 'sell', algoClOrdId: toAlgoClOrdId('ord_4'), sz: '12', reduceOnly: true,
  ordType: 'move_order_stop', callbackRatio: '0.015', activePx: '76000',
} as const satisfies AlgoOrderBody;
const TRAIL_SPREAD = {
  instId: 'SOL-USDT-SWAP', tdMode: 'isolated', side: 'buy', algoClOrdId: toAlgoClOrdId('ord_5'), sz: '7', reduceOnly: true,
  ordType: 'move_order_stop', callbackSpread: '1.5',
} as const satisfies AlgoOrderBody;
const TRAIL_NEITHER = {
  instId: 'BTC-USDT-SWAP', tdMode: 'isolated', side: 'sell', algoClOrdId: toAlgoClOrdId('ord_6'), sz: '1', reduceOnly: true,
  ordType: 'move_order_stop',
} as const satisfies AlgoOrderBody;
const TRAIL_BOTH = { ...TRAIL_SPREAD, callbackRatio: '0.01' } as const satisfies AlgoOrderBody;
const CANCEL_TWO = [{ instId: 'BTC-USDT-SWAP', algoId: '681' }, { instId: 'ETH-USDT-SWAP', algoId: '682' }];
const CANCEL_ELEVEN = Array.from({ length: 11 }, (_, i) => ({ instId: 'BTC-USDT-SWAP', algoId: String(700 + i) }));

type Call = { name: string; args: unknown[] };
const call = (name: string, ...args: unknown[]): Call => ({ name, args });

/** Every endpoint once, then the argument variants whose request bytes or guards differ. */
const CALLS: Call[] = [
  call('getConfig'),
  call('getContractValue', 'BTCUSDT'),
  call('getUsdtBalance'),
  call('getPositions'),
  call('setLeverage', 'BTC-USDT-SWAP', 3),
  call('placeOrder', PLACE_MARKET),
  call('placeAlgo', SL_ALGO),
  call('cancelAlgos', CANCEL_TWO),
  call('getPendingAlgos', 'conditional'),
  call('getAlgo', toAlgoClOrdId('ord_2')),
  call('cancelOrder', 'BTC-USDT-SWAP', toClOrdId('agent:agent_1:dec_1')),
  call('getOrder', 'BTC-USDT-SWAP', toClOrdId('agent:agent_1:dec_1')),
];
const VARIANTS: Call[] = [
  call('getContractValue', 'PEPEUSDT'),
  call('setLeverage', 'PEPE-USDT-SWAP', 20),
  call('placeOrder', PLACE_LIMIT_EXIT),
  call('placeAlgo', TP_ALGO),
  call('placeAlgo', TRAIL_RATIO),
  call('placeAlgo', TRAIL_SPREAD),
  call('placeAlgo', TRAIL_NEITHER),
  call('placeAlgo', TRAIL_BOTH),
  call('cancelAlgos', []),
  call('cancelAlgos', CANCEL_ELEVEN),
  call('getPendingAlgos', 'move_order_stop'),
  call('getPendingAlgos', 'oco'),
];

const ok = (data: unknown) => JSON.stringify({ code: '0', msg: '', data });

/** A typical successful answer per endpoint, shaped as OKX documents it. */
const OK_REPLY: Record<string, string> = {
  getConfig: ok([{ uid: '42', acctLv: '2', posMode: 'net_mode', perm: 'read_only,trade', autoLoan: false }]),
  getContractValue: ok([{ instId: 'BTC-USDT-SWAP', ctVal: '0.01', lotSz: '0.01', tickSz: '0.1' }]),
  getUsdtBalance: ok([{ totalEq: '1000', details: [{ ccy: 'BTC', eq: '0.1' }, { ccy: 'USDT', eq: '1000.5', availEq: '900', upl: '-2.5' }] }]),
  getPositions: ok([
    { instId: 'BTC-USDT-SWAP', pos: '-2', avgPx: '75000', markPx: '74900', upl: '2', lever: '3', liqPx: '99000', margin: '50', mgnMode: 'isolated' },
    { instId: 'ETH-USDT-SWAP', pos: '0', avgPx: '', markPx: '3000', upl: '0', lever: '5', liqPx: '', margin: '0', mgnMode: 'isolated' },
  ]),
  setLeverage: ok([{ instId: 'BTC-USDT-SWAP', lever: '3', mgnMode: 'isolated', posSide: '' }]),
  placeOrder: ok([{ ordId: '312269865356374016', clOrdId: 'c1', tag: '', sCode: '0', sMsg: '' }]),
  placeAlgo: ok([{ algoId: '681', algoClOrdId: 'a1', sCode: '0', sMsg: '' }]),
  cancelAlgos: ok([{ algoId: '681', sCode: '0', sMsg: '' }, { algoId: '682', sCode: '0', sMsg: '' }]),
  getPendingAlgos: ok([{ algoId: '681', algoClOrdId: 'a1', instId: 'BTC-USDT-SWAP', ordType: 'conditional', state: 'live', side: 'sell', sz: '12', slTriggerPx: '74000' }]),
  getAlgo: ok([{ algoId: '681', algoClOrdId: 'a1', instId: 'BTC-USDT-SWAP', ordType: 'move_order_stop', state: 'effective', side: 'sell', sz: '12', callbackRatio: '0.015', moveTriggerPx: '74100' }]),
  cancelOrder: ok([{ ordId: '312269865356374016', clOrdId: 'c1', sCode: '0', sMsg: '' }]),
  getOrder: ok([{ ordId: '312269865356374016', clOrdId: 'c1', state: 'filled', accFillSz: '12', avgPx: '75000', fee: '-0.05', pnl: '0' }]),
};

/** status, body text; a null body is a fetch that fails without an answer. */
const SHARED_REPLIES: Record<string, [number, string | null]> = {
  empty: [200, ok([])],
  noData: [200, JSON.stringify({ code: '0', msg: '' })],
  nullData: [200, JSON.stringify({ code: '0', msg: '', data: null })],
  refusedWithRow: [200, JSON.stringify({ code: '1', msg: 'Operation failed', data: [{ sCode: '51008', sMsg: 'Insufficient margin' }] })],
  refusedNoRow: [200, JSON.stringify({ code: '50113', msg: 'Invalid Sign', data: [] })],
  refusedEmptyMsg: [200, JSON.stringify({ code: '50000', msg: '', data: [] })],
  refusedEmptyRow: [200, JSON.stringify({ code: '1', msg: 'All operations failed', data: [{ sCode: '', sMsg: '' }] })],
  refusedNoCode: [200, JSON.stringify({ msg: '', data: [] })],
  notFound: [200, JSON.stringify({ code: '51603', msg: 'Order does not exist', data: [] })],
  notFoundOnRow: [200, JSON.stringify({ code: '1', msg: '', data: [{ sCode: '51603', sMsg: 'Order does not exist' }] })],
  rowRefusedInSuccess: [200, ok([{ ordId: '', algoId: '', sCode: '51000', sMsg: 'Parameter error' }])],
  rowWithoutSCode: [200, ok([{ ordId: '9', algoId: '9' }])],
  rowSCodeWithoutMsg: [200, ok([{ sCode: '51000' }])],
  rowEmptySCode: [200, ok([{ ordId: '9', sCode: '', sMsg: '' }])],
  partlyCancelled: [200, ok([{ algoId: '681', sCode: '0', sMsg: '' }, { algoId: '682', sCode: '51400', sMsg: 'Cancellation failed' }])],
  http500Json: [500, ok([])],
  http503Refusal: [503, JSON.stringify({ code: '50001', msg: 'Service temporarily unavailable', data: [] })],
  http502Html: [502, '<html>Bad Gateway</html>'],
  http200Text: [200, 'not json'],
  http200Empty: [200, ''],
  http401Json: [401, JSON.stringify({ code: '50111', msg: 'Invalid OK-ACCESS-KEY', data: [] })],
  http429NoCode: [429, JSON.stringify({ msg: 'Too Many Requests' })],
  jsonNull: [200, 'null'],
  jsonNumber: [200, '5'],
  network: [200, null],
};

type Sent = { url: string; method: string; headers: Array<[string, string]>; body: string | null };

function recordingClient(venue: 'OKX_LIVE' | 'OKX_DEMO', reply: [number, string | null], sent: Sent[]) {
  const fetchImpl = (async (url: string, init: RequestInit) => {
    sent.push({
      url,
      method: String(init.method),
      headers: Object.entries(init.headers as Record<string, string>),
      body: (init.body as string | undefined) ?? null,
    });
    const [status, body] = reply;
    if (body === null) throw new TypeError('fetch failed');
    return new Response(body, { status });
  }) as unknown as typeof fetch;
  return new OkxClient(CREDENTIALS, venue, { fetchImpl, baseUrl: BASE, now: () => NOW });
}

async function outcome(client: OkxClient, { name, args }: Call) {
  const method = (client as unknown as Record<string, (...a: unknown[]) => Promise<unknown>>)[name]!;
  try {
    return { result: (await method.apply(client, args)) ?? null };
  } catch (error) {
    const kind = error instanceof OkxRejection ? 'rejection' : error instanceof OkxUnknownOutcome ? 'unknown' : 'error';
    return { error: { kind, message: (error as Error).message, ...(error instanceof OkxRejection ? { code: error.code } : {}) } };
  }
}

export async function okxPrivateFixture() {
  const requests = [];
  for (const venue of ['OKX_LIVE', 'OKX_DEMO'] as const) {
    for (const c of [...CALLS, ...VARIANTS]) {
      const sent: Sent[] = [];
      const reply: [number, string] = [200, OK_REPLY[c.name]!];
      const result = await outcome(recordingClient(venue, reply, sent), c);
      requests.push({ venue, call: c, reply, sent, outcome: result });
    }
  }
  const outcomes = [];
  for (const c of CALLS) {
    for (const [replyName, reply] of Object.entries(SHARED_REPLIES)) {
      outcomes.push({ call: c, reply: replyName, outcome: await outcome(recordingClient('OKX_LIVE', reply, []), c) });
    }
  }
  return {
    credentials: CREDENTIALS,
    base: BASE,
    now: NOW,
    sign: SIGN_CASES.map(([secretKey, timestamp, method, path, body]) => ({
      secretKey, timestamp, method, path, body, sign: sign(secretKey, timestamp, method, path, body),
    })),
    instIds: SYMBOLS.map((symbol) => ({ symbol, instId: toInstId(symbol) })),
    ids: IDS.map((id) => ({ id, clOrdId: toClOrdId(id), algoClOrdId: toAlgoClOrdId(id) })),
    isoTimes: ISO_TIMES.map((ms) => ({ ms, iso: new Date(ms).toISOString() })),
    replies: SHARED_REPLIES,
    requests,
    outcomes,
  };
}
