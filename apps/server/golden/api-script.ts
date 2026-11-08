import { TAPE_MINUTES, TAPE_START } from '@mochiprep/market-data';
import { unregisterInstrument, type InstrumentSpecification } from '@mochiprep/shared';
import { buildApp } from '../src/app.js';
import { openDatabase } from '../src/db/database.js';
import { SqliteStore } from '../src/db/store.js';
import { Runtime } from '../src/runtime.js';

/**
 * A scripted session against the real Node app, recorded request by request, so the Rust port
 * (`crates/mochi-server/tests/golden_api.rs`) can replay the same requests and must answer with
 * the same status and the same body bytes.
 *
 * The app is built as main.ts builds it, except that `runtime.start()` is never called: the live
 * OKX feed stays disconnected and nothing touches the network. The tape is short
 * (REPLAY_MINUTES) so the fixture stays quick to make.
 *
 * Two things in a response cannot match across runs, so one normaliser (`ApiNormaliser`, mirrored
 * in the Rust test) rewrites them in the recorded text:
 * - random ids (`<prefix>_<uuid v4>`) become `<prefix>_<n>`, numbered per prefix in the order they
 *   first appear in the session's responses;
 * - wall-clock times, which the engine stamps on LIVE accounts (`OkxMarketData.now()` is
 *   `Date.now()`), become the string "<wall-clock>". A time is wall-clock when it is later than
 *   the end of the longest replay tape; every replay time is on the tape.
 * Requests are written with the `<prefix>_<n>` names and resolved to the real ids before sending.
 */

export const REPLAY_MINUTES = 6500;
export const ID_PREFIXES = ['acct', 'ord', 'fill', 'pos', 'evt', 'protect'] as const;
export const WALL_CLOCK_FIELDS = ['createdAt', 'updatedAt', 'ts'] as const;
/** The end of the longest tape (`ReplayMarketData.endTime` for TAPE_MINUTES). */
export const WALL_CLOCK_AFTER = TAPE_START + (TAPE_MINUTES - 1) * 60_000 + 60_000;
const WALL_CLOCK = '<wall-clock>';

const UUID = '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}';
const PREFIXES = ID_PREFIXES.join('|');
const RANDOM_ID = new RegExp(`\\b(${PREFIXES})_${UUID}\\b`, 'g');
const NAMED_ID = new RegExp(`\\b(${PREFIXES})_\\d+\\b`, 'g');
const TIME_FIELD = new RegExp(`"(${WALL_CLOCK_FIELDS.join('|')})":(\\d+)`, 'g');

export class ApiNormaliser {
  private readonly names = new Map<string, string>();
  private readonly ids = new Map<string, string>();
  private readonly counts = new Map<string, number>();

  /** A response body with every random id and wall-clock time replaced. */
  normalise(text: string): string {
    return text
      .replace(RANDOM_ID, (id: string, prefix: string) => {
        let name = this.names.get(id);
        if (!name) {
          const n = (this.counts.get(prefix) ?? 0) + 1;
          this.counts.set(prefix, n);
          name = `${prefix}_${n}`;
          this.names.set(id, name);
          this.ids.set(name, id);
        }
        return name;
      })
      .replace(TIME_FIELD, (field: string, key: string, value: string) =>
        Number(value) > WALL_CLOCK_AFTER ? `"${key}":"${WALL_CLOCK}"` : field,
      );
  }

  /** A request path or body with every `<prefix>_<n>` seen so far replaced by its real id. */
  resolve(text: string): string {
    return text.replace(NAMED_ID, (name) => this.ids.get(name) ?? name);
  }
}

/** A pair the owner added, already in the database at startup. It is live-only, so the replay refuses it. */
export const ADDED_INSTRUMENT: InstrumentSpecification = {
  symbol: 'DOGEUSDT',
  marketType: 'CRYPTO_PERP',
  baseCurrency: 'DOGE',
  quoteCurrency: 'USDT',
  settlementCurrency: 'USDT',
  contractSize: '1',
  quantityStep: '1',
  minQuantity: '1',
  priceTick: '0.00001',
  maxLeverage: 50,
  maintenanceMarginRate: '0.01',
  fundingIntervalHours: 8,
  venueContractValue: '1000',
  venueLotSize: '0.01',
};

interface Request {
  method: 'GET' | 'POST' | 'PUT' | 'DELETE';
  path: string;
  /** Raw body text; absent sends no body. */
  body?: string;
  /** Instead of `body`: this character repeated `length` times, to keep huge bodies out of the file. */
  repeat?: { char: string; length: number };
  /** Content-type to send with a body. Defaults to application/json; null sends none. */
  contentType?: string | null;
}

interface Step {
  method: string;
  path: string;
  body: string | null;
  repeat: { char: string; length: number } | null;
  /** The body headers the server saw (inject adds content-length), so Rust sends exactly these. */
  headers: Record<string, string>;
  status: number;
  response: string;
}

const HOST = 'localhost:8787';
const json = (value: unknown) => JSON.stringify(value);

export async function apiScriptFixture() {
  const db = openDatabase(':memory:');
  new SqliteStore(db).saveInstrument(ADDED_INSTRUMENT);
  const runtime = new Runtime({ db, replayMinutes: REPLAY_MINUTES });
  const { app, token } = buildApp(runtime, {
    host: '127.0.0.1',
    port: 8787,
    dataDir: '.',
    allowedOrigins: [`http://${HOST}`],
  });
  let seen: Record<string, string> = {};
  app.addHook('onRequest', async (request) => {
    seen = {};
    for (const name of ['content-type', 'content-length', 'transfer-encoding']) {
      const value = request.headers[name];
      if (typeof value === 'string') seen[name] = value;
    }
  });
  await app.ready();

  const normaliser = new ApiNormaliser();
  const steps: Step[] = [];
  /** Sends one request and records it. Returns the normalised body, parsed. */
  const send = async (request: Request): Promise<any> => {
    const body = request.repeat ? request.repeat.char.repeat(request.repeat.length) : request.body;
    const contentType = request.contentType === undefined ? 'application/json' : request.contentType;
    const headers: Record<string, string> = { host: HOST, authorization: `Bearer ${token}` };
    if (body !== undefined && contentType !== null) headers['content-type'] = contentType;
    const response = await app.inject({
      method: request.method,
      url: normaliser.resolve(request.path),
      headers,
      ...(body === undefined ? {} : { payload: normaliser.resolve(body) }),
    });
    const text = normaliser.normalise(response.body);
    steps.push({
      method: request.method,
      path: request.path,
      body: request.repeat ? null : (request.body ?? null),
      repeat: request.repeat ?? null,
      headers: seen,
      status: response.statusCode,
      response: text,
    });
    return JSON.parse(text);
  };
  const get = (path: string) => send({ method: 'GET', path });
  const post = (path: string, value?: unknown) => send({ method: 'POST', path, ...(value === undefined ? {} : { body: json(value) }) });
  const put = (path: string, value: unknown) => send({ method: 'PUT', path, body: json(value) });
  const del = (path: string) => send({ method: 'DELETE', path });

  try {
    // ---- state, instruments, accounts -----------------------------------------------------
    await get('/api/state');
    await get('/api/instruments');
    await get('/api/accounts');
    const main = (await post('/api/accounts', { name: '  Main  ', source: 'REPLAY' })).id as string;
    // Accounts list by (createdAt, id): a step keeps two accounts from tying on a random id.
    await post('/api/replay', { steps: 1 });
    const second = (await post('/api/accounts', { name: 'Second', source: 'REPLAY', initialBalance: '2500.5' })).id as string;
    const live = (await post('/api/accounts', { name: 'Live paper', source: 'LIVE', initialBalance: '1000' })).id as string;
    await post('/api/accounts', {});
    await post('/api/accounts', { name: ' ', source: 'PAPER', initialBalance: '1e3', extra: 1 });
    await post('/api/accounts', { name: 'x'.repeat(61), source: 'LIVE', initialBalance: 10 });
    await post('/api/accounts');
    await send({ method: 'POST', path: '/api/accounts', body: '{"name":' });
    await send({ method: 'POST', path: '/api/accounts', body: '' });
    await send({ method: 'POST', path: '/api/accounts', body: '{"name":"a","source":"REPLAY"}', contentType: 'text/plain' });
    await send({ method: 'POST', path: '/api/accounts', body: '<a/>', contentType: 'application/xml' });
    await send({ method: 'POST', path: '/api/accounts', body: '{"name":"a","source":"REPLAY"}', contentType: null });
    // Fastify refuses an unsupported type before it looks at the size, and never reads a GET body.
    await send({ method: 'POST', path: '/api/accounts', repeat: { char: 'x', length: 300_000 }, contentType: 'application/xml' });
    await send({ method: 'POST', path: '/api/accounts', repeat: { char: ' ', length: 300_000 } });
    await send({ method: 'GET', path: '/api/accounts', body: 'not json' });
    await send({ method: 'GET', path: '/api/accounts', repeat: { char: 'x', length: 300_000 } });
    await get('/api/accounts');

    // ---- market ---------------------------------------------------------------------------
    const quotes = await get('/api/market/quotes?source=REPLAY');
    await get('/api/market/quotes?source=LIVE');
    await get('/api/market/quotes?source=replay');
    await get('/api/market/quotes');
    await get('/api/market/candles?source=REPLAY&symbol=BTCUSDT&timeframe=15m&limit=3');
    await get('/api/market/candles?source=REPLAY&symbol=ETHUSDT&timeframe=4h');
    await get('/api/market/candles?source=REPLAY&symbol=SOLUSDT&timeframe=1m&limit=2&extra=1');
    await get('/api/market/candles?source=REPLAY&symbol=DOGEUSDT&timeframe=1m&limit=2');
    await get('/api/market/candles?source=REPLAY&symbol=XRPUSDT&timeframe=1m');
    await get('/api/market/candles?source=REPLAY&symbol=btc&timeframe=2m&limit=0');
    await get('/api/market/candles?source=LIVE&symbol=BTCUSDT&timeframe=1h&limit=1001');
    await get('/api/market/candles?limit=1.5');
    await get('/api/market/candles');

    // ---- replay ---------------------------------------------------------------------------
    await post('/api/replay', { steps: 5 });
    await post('/api/replay', { speed: 30 });
    await post('/api/replay', { playing: false, speed: 1, steps: 1 });
    await post('/api/replay', {});
    await post('/api/replay');
    await post('/api/replay', { steps: 0, speed: 601, playing: 'yes', extra: true });
    await post('/api/replay', { steps: 1.5 });
    await send({ method: 'POST', path: '/api/replay', body: '[1]' });

    // ---- orders ---------------------------------------------------------------------------
    const last = (symbol: string) => Number((quotes as Array<{ symbol: string; last: string }>).find((q) => q.symbol === symbol)!.last);
    const btc = last('BTCUSDT');
    const eth = last('ETHUSDT');
    const order = (fields: Record<string, unknown>) => ({
      accountId: main, clientOrderId: 'cli-1', symbol: 'BTCUSDT', side: 'BUY', type: 'MARKET', quantity: '0.01', leverage: 5, ...fields,
    });
    const bought = await post('/api/orders', order({
      stopLoss: String(Math.floor(btc * 0.97)),
      takeProfit: String(Math.ceil(btc * 1.03)),
      trailingStop: { kind: 'RATIO', callback: '0.02', activationPrice: null },
    }));
    await post('/api/orders', order({}));
    const resting = await post('/api/orders', order({
      clientOrderId: 'cli-2', symbol: 'ETHUSDT', type: 'LIMIT', quantity: '0.1', price: String(Math.floor(eth * 0.9)), stopLoss: String(Math.floor(eth * 0.8)), leverage: 3,
    }));
    await post('/api/orders', order({ clientOrderId: 'cli-3', symbol: 'SOLUSDT', side: 'SELL', quantity: '2', price: null, stopLoss: null, takeProfit: null, trailingStop: null }));
    await post('/api/orders', order({ clientOrderId: 'cli-4', quantity: '1000' }));
    await post('/api/orders', order({ clientOrderId: 'cli-5', type: 'LIMIT' }));
    await post('/api/orders', order({ clientOrderId: 'cli-6', stopLoss: String(Math.ceil(btc * 1.1)) }));
    await post('/api/orders', order({ clientOrderId: 'cli-7', quantity: '0.00001' }));
    await post('/api/orders', order({ clientOrderId: 'cli-8', accountId: second, symbol: 'ETHUSDT', side: 'SELL', quantity: '0.5', leverage: 10, reduceOnly: true }));
    await post('/api/orders', order({ clientOrderId: 'cli-9', accountId: second, symbol: 'ETHUSDT', side: 'SELL', quantity: '0.5', leverage: 10, trailingStop: { kind: 'SPREAD', callback: '15', activationPrice: String(Math.floor(eth * 0.99)) } }));
    await post('/api/orders', order({ clientOrderId: 'live-1', accountId: live, symbol: 'DOGEUSDT', quantity: '100' }));
    await post('/api/orders', order({ clientOrderId: 'cli-10', symbol: 'DOGEUSDT' }));
    await post('/api/orders', order({ clientOrderId: 'cli-11', symbol: 'XRPUSDT' }));
    await post('/api/orders', order({ accountId: 'acct_nope' }));
    await post('/api/orders', order({ leverage: 0, side: 'HOLD', quantity: '-1', extra: 1 }));
    await post('/api/orders', { accountId: main });
    await post('/api/orders');
    await post('/api/orders', order({ clientOrderId: '', price: '1.', trailingStop: { kind: 'RATIO', callback: '0.01' } }));
    await del(`/api/orders/${resting.order.id}`);
    await del(`/api/orders/${resting.order.id}`);
    await del(`/api/orders/${bought.order.id}`);
    await del('/api/orders/ord_nope');
    await send({ method: 'DELETE', path: '/api/orders/ord_nope', body: '{', contentType: 'application/json' });

    await post('/api/replay', { steps: 40 });

    // ---- positions and protection ---------------------------------------------------------
    await get(`/api/accounts/${main}/positions`);
    await get(`/api/accounts/${second}/positions`);
    await put(`/api/positions/${main}/BTCUSDT/protection`, { stopLoss: String(Math.floor(btc * 0.96)), takeProfit: null });
    await get(`/api/accounts/${main}/positions`);
    await put(`/api/positions/${main}/BTCUSDT/protection`, { stopLoss: null, takeProfit: String(Math.ceil(btc * 1.05)), trailingStop: null });
    await put(`/api/positions/${main}/BTCUSDT/protection`, {
      stopLoss: null, takeProfit: null, trailingStop: { kind: 'SPREAD', callback: '100', activationPrice: String(Math.ceil(btc * 1.2)) },
    });
    await put(`/api/positions/${main}/BTCUSDT/protection`, { stopLoss: String(Math.ceil(btc * 1.5)), takeProfit: null });
    await put(`/api/positions/${main}/ETHUSDT/protection`, { stopLoss: null, takeProfit: null });
    await put(`/api/positions/${main}/btc/protection`, { stopLoss: null, takeProfit: null });
    await put(`/api/positions/acct_nope/BTCUSDT/protection`, { stopLoss: null, takeProfit: null });
    await put(`/api/positions/${main}/BTCUSDT/protection`, {});
    await put(`/api/positions/acct_nope/btc/protection`, { stopLoss: '1', takeProfit: 2, trailingStop: { kind: 'X' } });
    await send({ method: 'PUT', path: `/api/positions/${main}/BTCUSDT/protection` });

    // ---- close ----------------------------------------------------------------------------
    await post(`/api/positions/${main}/BTCUSDT/close`, { fraction: 0.5, clientOrderId: 'close-1' });
    await post(`/api/positions/${main}/BTCUSDT/close`, { fraction: 0.5, clientOrderId: 'close-1' });
    await post(`/api/positions/${main}/BTCUSDT/close`, { fraction: 1, clientOrderId: 'close-2' });
    await post(`/api/positions/${main}/BTCUSDT/close`, { fraction: 1, clientOrderId: 'close-3' });
    await post(`/api/positions/${main}/BTCUSDT/close`, { fraction: 0, clientOrderId: '' });
    await post(`/api/positions/${main}/BTCUSDT/close`, { fraction: 1.5, clientOrderId: 'x'.repeat(129) });
    await post(`/api/positions/${main}/BTCUSDT/close`, { fraction: '1', clientOrderId: 'c' });
    await post(`/api/positions/${main}/BTC/close`, { fraction: 1, clientOrderId: 'c' });
    await post('/api/positions/acct_nope/BTCUSDT/close', { fraction: 1, clientOrderId: 'c' });
    await post(`/api/positions/${main}/BTCUSDT/close`);
    await post(`/api/positions/${second}/ETHUSDT/close`, { fraction: 0.25, clientOrderId: 'close-4' });

    await post('/api/replay', { steps: 30 });

    // ---- history reads --------------------------------------------------------------------
    for (const account of [main, second]) {
      await get(`/api/accounts/${account}/closed-positions`);
      await get(`/api/accounts/${account}/orders`);
      await get(`/api/accounts/${account}/fills`);
      await get(`/api/accounts/${account}/ledger`);
      await get(`/api/accounts/${account}/equity`);
      await get(`/api/accounts/${account}/audit`);
    }
    await get(`/api/accounts/${main}/orders?limit=2`);
    await get(`/api/accounts/${main}/fills?limit=1`);
    await get(`/api/accounts/${main}/ledger?limit=3`);
    await get(`/api/accounts/${main}/equity?limit=2`);
    await get(`/api/accounts/${main}/closed-positions?limit=1&other=x`);
    await get(`/api/accounts/${main}/orders?limit=0`);
    await get(`/api/accounts/${main}/fills?limit=2001`);
    await get(`/api/accounts/${main}/ledger?limit=abc`);
    await get(`/api/accounts/${main}/equity?limit=`);
    await get(`/api/accounts/${main}/orders?limit=2&limit=3`);
    await get(`/api/accounts/${live}/orders`);
    await get(`/api/accounts/${live}/positions`);
    await get(`/api/accounts/${live}/audit`);
    for (const read of ['positions', 'closed-positions', 'orders', 'fills', 'ledger', 'equity', 'audit']) {
      await get(`/api/accounts/acct_nope/${read}`);
    }
    await get('/api/accounts/acct_nope/orders?limit=0');

    // ---- reset and delete -----------------------------------------------------------------
    await post(`/api/accounts/${second}/reset`);
    await get(`/api/accounts/${second}/orders`);
    await get(`/api/accounts/${second}/ledger`);
    await send({ method: 'POST', path: `/api/accounts/${second}/reset`, body: '{', contentType: 'application/json' });
    await send({ method: 'POST', path: `/api/accounts/${second}/reset`, body: 'x', contentType: 'application/xml' });
    await post('/api/accounts/acct_nope/reset');
    await del(`/api/accounts/${second}`);
    await del(`/api/accounts/${second}`);
    await get(`/api/accounts/${second}/orders`);
    await send({ method: 'DELETE', path: `/api/accounts/${live}`, body: 'x', contentType: 'text/csv' });

    // ---- final state ----------------------------------------------------------------------
    await post('/api/replay', { steps: 1440 });
    await post('/api/replay', { steps: 1 });
    await get('/api/accounts');
    await get('/api/market/quotes?source=REPLAY');
    await get('/api/instruments');
    await get('/api/state');

    return {
      about: 'A scripted Node session (apps/server/golden/api-script.ts); Rust replays it in tests/golden_api.rs.',
      replayMinutes: REPLAY_MINUTES,
      addedInstruments: [ADDED_INSTRUMENT],
      normalised: {
        ids: { prefixes: ID_PREFIXES, as: '<prefix>_<n>, numbered per prefix in first-seen order' },
        wallClock: { fields: WALL_CLOCK_FIELDS, after: WALL_CLOCK_AFTER, as: WALL_CLOCK },
      },
      steps,
    };
  } finally {
    await app.close();
    await runtime.stop();
    db.close();
    // INSTRUMENTS is module-global: leave it as the other fixtures expect it.
    unregisterInstrument(ADDED_INSTRUMENT.symbol);
  }
}
