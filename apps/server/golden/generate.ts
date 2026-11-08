import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import {
  auditLedger,
  ceilToStep,
  d,
  evaluateRisk,
  floorToStep,
  foldLedger,
  isMarketable,
  isRestingLimitFilled,
  isStopTriggered,
  isTakeProfitTriggered,
  linearPerp,
  marketFillPrice,
  mathFor,
  maxDrawdownPct,
  roundToStep,
  str,
  winRatePct,
  type Dec,
} from '@mochiprep/core';
import { generateTape, ReplayMarketData } from '@mochiprep/market-data';
import {
  DEFAULT_ACCOUNT_CONFIG,
  DEFAULT_LIVE_LIMITS,
  DEFAULT_RISK_SETTINGS,
  DEFAULT_SYMBOLS,
  INSTRUMENTS,
  TIMEFRAMES,
  type AccountBalance,
  type AiDecisionData,
  type Candle,
  type EquityPoint,
  type Fill,
  type LedgerEvent,
  type Order,
  type OrderRequest,
  type OrderResult,
  type PaperAccount,
  type PaperAccountConfig,
  type Position,
  type PositionSide,
  type PositionView,
  type Quote,
  type RiskCheck,
  type RiskResult,
  type RiskSettings,
  type SizedTrade,
} from '@mochiprep/shared';
import { openDatabase } from '../src/db/database.js';
import { SqliteStore } from '../src/db/store.js';
import { newDataKey, seal, wrapWithPassword } from '../src/vault.js';
import { paperScenariosFixture } from './paper-scenarios.js';

const ROUND_CASES = [
  0.5, 1.5, 2.5, -0.5, -1.5, -2.5, 2.4999999999999996, 0.49999999999999994, -0.49999999999999994,
  1e15 + 0.5, 0, 7.000000000000001,
];
const FIXED_CASES: Array<[number, number]> = [
  [0.125, 2], [0.375, 2], [-0.125, 2], [1.005, 2], [2.5, 0], [-2.5, 0], [68000, 1], [1e-7, 6],
  [0.0000005, 6], [123.456, 3], [0.1 + 0.2, 17], [-0, 2], [99.995, 2], [0.0005, 6], [-0.0005, 6],
  [-1e-7, 6], [12345.6789, 0],
  // Carry paths: rounding up must carry through a run of 9s.
  [0.5, 0], [9.5, 0], [99.5, 0], [9.875, 2], [1.5, 20],
];

const JS_NUMBER_CASES = [
  0, -0, -3, -2.5, 1e-7, 1e-6, 0.0000015, 0.00001234, 1e15, 2 ** 53, 2 ** 53 + 2, 1e17, 1e21, 1.5e21, 1.5e300,
  0.1 + 0.2, 123.456, -1e-7, 1e20, 123456789012345680000, 5e-324,
];

// JSON cannot carry -0, so the fixture stores 0 and the freshness test (toEqual) must see the same.
const noNegativeZero = (x: number): number => x + 0;

/** The schema a fresh database ends up with, so Rust can prove it applies the same migrations. */
function schemaFixture() {
  const db = openDatabase(':memory:');
  try {
    return {
      objects: db
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name")
        .all(),
      migrations: (db.prepare('SELECT name FROM schema_migrations ORDER BY name').all() as { name: string }[]).map(
        (r) => r.name,
      ),
    };
  } finally {
    db.close();
  }
}

/**
 * What Node's SqliteStore writes for the accounting tables, row by row, plus what its readers
 * return. `inputs` are the exact objects inserted (ids and timestamps made distinct so they fit
 * the table constraints), so the Rust tests insert the same data rather than re-deriving it.
 */
function storeRowsFixture() {
  const { samples } = accountingTypesFixture();
  const T0 = 1700000000000;
  const account = samples.paperAccount[0]!; // LIVE source
  const replayAccount = { ...samples.paperAccount[1]!, id: 'acc-2' }; // REPLAY source

  // Two accounts on different sources, so the market joins have something to filter out.
  // Orders share created_at on purpose (ord-2, ord-3, ord-5): ordering then falls to rowid.
  const order = (o: Partial<Order> & Pick<Order, 'id' | 'clientOrderId'>): Order => ({
    ...samples.order[1]!,
    status: 'OPEN',
    ...o,
  });
  const orders: Order[] = [
    { ...samples.order[0]! }, // ord-1, acc-1, BTCUSDT, FILLED
    order({ id: 'ord-2', clientOrderId: 'cli-2', createdAt: T0 + 500, updatedAt: T0 + 1500 }),
    order({ id: 'ord-3', clientOrderId: 'cli-3', createdAt: T0 + 500, updatedAt: T0 + 1600 }),
    order({ id: 'ord-4', clientOrderId: 'cli-4', symbol: 'ETHUSDT', createdAt: T0 + 100 }),
    order({ id: 'ord-5', clientOrderId: 'cli-5', accountId: 'acc-2', createdAt: T0 + 500 }),
    order({ id: 'ord-6', clientOrderId: 'cli-6', status: 'CANCELED', createdAt: T0 + 700 }),
  ];
  // Inserted out of id order, with equal ts, so only the rowid tie-break gives the expected order.
  const fill = (f: Partial<Fill> & Pick<Fill, 'id'>): Fill => ({ ...samples.fill[1]!, ...f });
  const fills: Fill[] = [
    fill({ id: 'fill-1', orderId: 'ord-1', ts: T0 + 2000 }),
    fill({ id: 'fill-2', orderId: 'ord-2', ts: T0 + 2500 }),
    fill({ id: 'fill-4', orderId: 'ord-1', ts: T0 + 2500 }),
    fill({ id: 'fill-3', orderId: 'ord-1', ts: T0 + 2500 }),
    fill({ id: 'fill-5', orderId: 'ord-5', accountId: 'acc-2', ts: T0 + 2600 }),
  ];
  const position = (p: Partial<Position> & Pick<Position, 'id'>): Position => ({
    ...samples.position[1]!,
    ...p,
  });
  const positions: Position[] = [
    { ...samples.position[0]! }, // pos-1, BTCUSDT, LIQUIDATED, closedAt T0+4000
    position({ id: 'pos-2', symbol: 'ETHUSDT' }), // OPEN
    position({ id: 'pos-3', status: 'CLOSED', exitPrice: '68000', closedAt: T0 + 5000 }),
    position({ id: 'pos-4', symbol: 'ETHUSDT', status: 'CLOSED', exitPrice: '3000', closedAt: T0 + 4500 }),
    position({ id: 'pos-5', symbol: 'SOLUSDT', openedAt: T0 - 1000 }), // OPEN, older than pos-2
    position({ id: 'pos-6', accountId: 'acc-2', symbol: 'ETHUSDT' }), // OPEN on the REPLAY account
  ];
  const ledger: Array<Omit<LedgerEvent, 'seq'>> = [0, 1, 2].map((i) => {
    const { seq: _seq, ...event } = samples.ledgerEvent[i]!;
    return { ...event, id: `led-${i + 1}`, ts: event.ts + i };
  });
  const point = (accountId: string, n: number, equity: string): EquityPoint => ({
    ...samples.equityPoint[0]!,
    accountId,
    ts: T0 + 6000 + n * 1000,
    equity,
  });
  const equity: EquityPoint[] = [
    point('acc-1', 0, '10500.5'),
    point('acc-1', 1, '10600.25'),
    point('acc-1', 2, '10400'),
    point('acc-1', 3, '10450.75'),
    point('acc-1', 4, '10700'),
    point('acc-2', 0, '9000'),
  ];
  // Rewrites the first point slot, which covers the ON CONFLICT path.
  const equityUpdate: EquityPoint = { ...equity[0]!, equity: '10501', unrealized: '266.4322' };

  // Edits that include fields the save methods must NOT write (name, price, leverage, openedAt).
  const savedAccount: PaperAccount = {
    ...account,
    name: 'Renamed',
    initialBalance: '1',
    walletBalance: '9000.5',
    usedMargin: '1',
    realizedPnl: '-10',
    feesPaid: '3',
    fundingPaid: '0.5',
  };
  const savedOrder: Order = {
    ...orders[1]!,
    price: '1',
    quantity: '0.25',
    status: 'FILLED',
    filledQuantity: '0.25',
    avgFillPrice: '67100',
    reservedMargin: '12.5',
    rejectReason: 'edited',
    exchangeOrderId: '555',
    updatedAt: T0 + 9000,
  };
  const savedPosition: Position = {
    ...positions[1]!,
    leverage: 9,
    openedAt: 1,
    quantity: '0.2',
    entryPrice: '3100',
    margin: '77.5',
    liquidationPrice: '2500',
    status: 'CLOSED',
    realizedPnl: '42',
    feesPaid: '2',
    fundingPaid: '-1',
    maxQuantity: '0.9',
    exitPrice: '3200',
    lastFundingTime: T0 + 8000,
    closedAt: T0 + 8100,
  };

  const db = openDatabase(':memory:');
  try {
    const store = new SqliteStore(db);
    const tables = ['accounts', 'orders', 'fills', 'positions', 'ledger_events', 'equity_history'];
    const snapshot = () => {
      const rows: Record<string, unknown[]> = {};
      for (const table of tables) rows[table] = db.prepare(`SELECT * FROM ${table} ORDER BY rowid`).all();
      return rows;
    };

    store.insertAccount(account);
    store.insertAccount(replayAccount);
    orders.forEach((o) => store.insertOrder(o));
    fills.forEach((f) => store.insertFill(f));
    positions.forEach((p) => store.insertPosition(p));
    ledger.forEach((e) => store.appendLedger(e));
    equity.forEach((p) => store.upsertEquityPoint(p));
    store.upsertEquityPoint(equityUpdate);
    const rows = snapshot();

    const reads = {
      getAccount: store.getAccount('acc-1'),
      getAccountReplay: store.getAccount('acc-2'),
      listAccounts: store.listAccounts(),
      getOrder: store.getOrder('ord-3'),
      getOrderByClientId: store.getOrderByClientId('acc-1', 'cli-4'),
      listOpenOrders: store.listOpenOrders('acc-1'),
      listOpenOrdersForMarketLive: store.listOpenOrdersForMarket('LIVE', 'BTCUSDT'),
      listOpenOrdersForMarketReplay: store.listOpenOrdersForMarket('REPLAY', 'BTCUSDT'),
      listOrdersLimit3: store.listOrders('acc-1', 3),
      listOrdersAll: store.listOrders('acc-1', 100),
      listFillsByOrder: store.listFillsByOrder('ord-1'),
      listFillsLimit3: store.listFills('acc-1', 3),
      listFillsAll: store.listFills('acc-1', 100),
      getOpenPosition: store.getOpenPosition('acc-1', 'ETHUSDT'),
      listOpenPositions: store.listOpenPositions('acc-1'),
      listOpenPositionsForMarketLive: store.listOpenPositionsForMarket('LIVE', 'ETHUSDT'),
      listOpenPositionsForMarketReplay: store.listOpenPositionsForMarket('REPLAY', 'ETHUSDT'),
      listClosedPositionsLimit2: store.listClosedPositions('acc-1', 2),
      listClosedPositionsAll: store.listClosedPositions('acc-1', 100),
      listLedger: store.listLedger('acc-1'),
      listLedgerSince: store.listLedger('acc-1', ledger[1]!.ts),
      listEquityLimit3: store.listEquity('acc-1', 3),
      listEquityAll: store.listEquity('acc-1', 100),
    };

    store.saveAccount(savedAccount);
    store.saveOrder(savedOrder);
    store.savePosition(savedPosition);
    const afterSaves = {
      rows: snapshot(),
      reads: {
        getAccount: store.getAccount('acc-1'),
        getOrder: store.getOrder('ord-2'),
        getOpenPosition: store.getOpenPosition('acc-1', 'ETHUSDT') ?? null,
        listClosedPositionsAll: store.listClosedPositions('acc-1', 100),
      },
    };

    store.deleteAccountData('acc-2', true);
    const afterKeepingRow = snapshot();
    store.deleteAccountData('acc-1', false);
    const afterDeletingRow = snapshot();

    return {
      inputs: {
        accounts: [account, replayAccount],
        orders,
        fills,
        positions,
        ledger,
        equity,
        equityUpdate,
        saves: { account: savedAccount, order: savedOrder, position: savedPosition },
      },
      rows,
      reads,
      afterSaves,
      afterKeepingRow,
      afterDeletingRow,
    };
  } finally {
    db.close();
  }
}

/** A real file written by the Node backend, so Rust can prove it opens such databases without re-migrating. */
function nodeCreatedDatabase(): Buffer {
  const dir = mkdtempSync(join(tmpdir(), 'mochi-golden-'));
  try {
    const path = join(dir, 'node.sqlite');
    const db = openDatabase(path);
    db.exec(
      `INSERT INTO accounts (id, name, source, initial_balance, wallet_balance, used_margin, realized_pnl, fees_paid, funding_paid, config, created_at)
       VALUES ('acct_golden', 'Golden', 'REPLAY', '10000', '10000', '0', '0', '0', '0', '{}', 1700000000000);
       INSERT INTO providers (id, name, kind, default_model, created_at)
       VALUES ('prov_golden', 'Golden provider', 'mock', 'mock-1', 1700000000000);`,
    );
    // Fold the WAL into the main file so copying that one file captures everything.
    db.exec('PRAGMA wal_checkpoint(TRUNCATE)');
    db.close();
    const copy = join(dir, 'copy.sqlite');
    copyFileSync(path, copy);
    return readFileSync(copy);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

// Decomposed on purpose (a + U+0308, o + U+0308, e + U+0301): the vault must normalize to NFC before scrypt.
// Not a credential: it is published with the fixtures it seals, which hold only fake keys, so the
// Rust tests can derive the same data key. Nothing real is ever sealed with it.
const VAULT_PASSWORD = 'mochi-pa\u0308sswo\u0308rd-e\u0301';
const VAULT_FAST_KDF = { N: 1024, r: 8, p: 1 };

/** Secrets sealed by Node, so Rust can prove it opens what existing installs have stored. */
async function vaultFixture() {
  const passwordNfc = VAULT_PASSWORD.normalize('NFC');
  if (VAULT_PASSWORD === passwordNfc) throw new Error('vault fixture password must not already be NFC');
  const dataKey = newDataKey();
  const sealedCase = (context: string, plaintext: string) => ({ context, plaintext, sealed: seal(dataKey, plaintext, context) });
  return {
    password: VAULT_PASSWORD,
    passwordNfc,
    dataKeyHex: dataKey.toString('hex'),
    wrap: await wrapWithPassword(dataKey, VAULT_PASSWORD, VAULT_FAST_KDF),
    sealed: [
      sealedCase('provider-key:prov_1', 'sk-golden-ai-key'),
      sealedCase('okx-credentials:acct_1', '{"apiKey":"k","secretKey":"s","passphrase":"p","environment":"OKX_DEMO"}'),
    ],
  };
}

/** The replay tape: a readable head per symbol, plus a hash of every column of the full default tape. */
function tapeFixture() {
  const columns = ['open', 'high', 'low', 'close', 'volume'] as const;
  const hash = createHash('sha256');
  const head: Record<string, unknown[]> = {};
  for (const symbol of DEFAULT_SYMBOLS) {
    const tape = generateTape(symbol);
    head[symbol] = Array.from({ length: 64 }, (_, i) => tape.candle(i));
    // Raw float64 bytes, so Rust can hash the same bits without any text formatting in between.
    for (const column of columns) {
      const values = tape[column];
      hash.update(new Uint8Array(values.buffer, values.byteOffset, values.byteLength));
    }
  }
  return { head, sha256: hash.digest('hex') };
}

const HASH_CURSORS = [5760, 9000, 20000, 25919];

/** Every quote of a full replay, hashed as JSON text one line each; Rust must produce the same bytes. */
function stepsHash() {
  const hash = createHash('sha256');
  const feed = new ReplayMarketData();
  let stepCount = 0;
  for (let quotes = feed.step(); quotes.length > 0; quotes = feed.step()) {
    stepCount++;
    for (const quote of quotes) hash.update(JSON.stringify(quote) + '\n');
  }
  return { stepCount, stepsSha256: hash.digest('hex') };
}

/** Raw bytes (int64 time, float64 OHLCV, one closed byte), so no text formatting sits between TS and Rust. */
function candleBytes(candle: Candle): Buffer {
  const bytes = Buffer.alloc(8 * 6 + 1);
  bytes.writeBigInt64LE(BigInt(candle.time), 0);
  bytes.writeDoubleLE(candle.open, 8);
  bytes.writeDoubleLE(candle.high, 16);
  bytes.writeDoubleLE(candle.low, 24);
  bytes.writeDoubleLE(candle.close, 32);
  bytes.writeDoubleLE(candle.volume, 40);
  bytes.writeUInt8(candle.closed ? 1 : 0, 48);
  return bytes;
}

/**
 * Candles for every symbol and timeframe at one cursor, in symbol-then-timeframe order. getCandles
 * reads the cursor synchronously when called, so the requests can start together after one seek.
 */
function candlesAt(feed: ReplayMarketData, cursor: number, limit: number) {
  feed.seek(cursor);
  return Promise.all(DEFAULT_SYMBOLS.flatMap((symbol) => TIMEFRAMES.map((tf) => feed.getCandles(symbol, tf, limit))));
}

async function candlesHash() {
  const hash = createHash('sha256');
  const feed = new ReplayMarketData();
  // Each candlesAt call seeks and starts its requests before the next call runs, so one feed serves every cursor.
  const perCursor = await Promise.all(HASH_CURSORS.map((cursor) => candlesAt(feed, cursor, 300)));
  for (const candle of perCursor.flat(2)) hash.update(candleBytes(candle));
  return { candlesSha256: hash.digest('hex') };
}

/**
 * Hashes of the whole replay output, so a change anywhere in the tape or quote model is caught
 * and not only at the few spots the readable samples cover.
 */
async function fullOutputHashes() {
  return { ...stepsHash(), ...(await candlesHash()) };
}

const SNAPSHOT_SYMBOLS = ['BTCUSDT', 'SOLUSDT'] as const;

/**
 * Readable quotes and candles at one cursor. Everything before the first await runs at call time,
 * so mapping several cursors over one feed still reads each cursor's own state.
 */
async function snapshotAt(feed: ReplayMarketData, cursor: number) {
  feed.seek(cursor);
  const now = feed.now();
  const quotes = DEFAULT_SYMBOLS.map((s) => feed.getQuote(s));
  const pending = SNAPSHOT_SYMBOLS.map((symbol) => Promise.all(TIMEFRAMES.map((tf) => feed.getCandles(symbol, tf, 50))));
  const lists = await Promise.all(pending);
  const candles = Object.fromEntries(
    SNAPSHOT_SYMBOLS.map((symbol, s) => [symbol, Object.fromEntries(TIMEFRAMES.map((tf, t) => [tf, lists[s]![t]]))]),
  );
  return { cursor, now, quotes, candles };
}

/** The replay feed: cursor edges, step quotes, and quotes plus aggregated candles at fixed cursors. */
async function replayFixture() {
  const r = new ReplayMarketData();
  const meta = { len: r.length, firstPlayableTime: r.firstPlayableTime, endTime: r.endTime, nowAtStart: r.now() };
  const fresh = new ReplayMarketData();
  const steps = [fresh.step(), fresh.step(), fresh.step()];
  const snapshots = await Promise.all(HASH_CURSORS.map((cursor) => snapshotAt(r, cursor)));
  const edges: Record<string, number> = {};
  r.seek(-5);
  edges.afterSeekNegative = r.position;
  r.seek(99999);
  edges.afterSeekPastEnd = r.position;
  edges.stepLengthAtEnd = r.step().length;
  r.seek(5760);
  edges.hugeLimitCandles = (await r.getCandles('BTCUSDT', '4h', 100000)).length;
  return { meta, steps, ...(await fullOutputHashes()), snapshots, edges };
}

const DECIMAL_SEED = 0x6d6f6368;

/** Small seeded PRNG so the corpus is identical on every run. */
function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

type DecimalCase = { op: string; a: string | number; b?: string; dp?: number; out: string };

const BINARY_OPS: Record<string, (a: Dec, b: Dec) => Dec> = {
  add: (a, b) => a.add(b),
  sub: (a, b) => a.sub(b),
  mul: (a, b) => a.mul(b),
  div: (a, b) => a.div(b),
  floorToStep: floorToStep,
  ceilToStep: ceilToStep,
  roundToStep: roundToStep,
};
const UNARY_OPS: Record<string, (a: Dec) => Dec> = {
  neg: (a) => a.neg(),
  floor: (a) => a.floor(),
  ceil: (a) => a.ceil(),
  round: (a) => a.round(),
};

/** Runs decimal.js exactly as the accounting code configures it; throws and non-finite results become "error". */
function decimalOut(run: () => Dec): string {
  try {
    const v = run();
    return v.isFinite() ? str(v) : 'error';
  } catch {
    return 'error';
  }
}

/**
 * Expected decimal.js results for the Rust Dec: a seeded random corpus (long operands, so products
 * and quotients overflow 50 digits) plus hand-picked edge cases through every operation.
 */
function decimalFixture(): DecimalCase[] {
  const rand = mulberry32(DECIMAL_SEED);
  const int = (lo: number, hi: number) => lo + Math.floor(rand() * (hi - lo + 1));
  const digits = (n: number) => Array.from({ length: n }, () => int(0, 9)).join('');
  const randomOperand = (): string => {
    const intLen = int(1, 30);
    const head = intLen === 1 ? String(int(0, 9)) : String(int(1, 9)) + digits(intLen - 1);
    const frac = digits(int(0, 30));
    return (rand() < 0.5 ? '-' : '') + head + (frac ? '.' + frac : '');
  };

  const cases: DecimalCase[] = [];
  const binary = (op: string, a: string, b: string) =>
    cases.push({ op, a, b, out: decimalOut(() => BINARY_OPS[op]!(d(a), d(b))) });
  const unary = (op: string, a: string) => cases.push({ op, a, out: decimalOut(() => UNARY_OPS[op]!(d(a))) });
  const withDp = (op: 'toDP' | 'toFixedDp', a: string, dp: number) =>
    cases.push({
      op,
      a,
      dp,
      out: op === 'toDP' ? decimalOut(() => d(a).toDecimalPlaces(dp)) : d(a).toFixed(dp),
    });
  const toFixed = (a: string) => cases.push({ op: 'toFixed', a, out: d(a).toFixed() });
  const cmp = (a: string, b: string) => cases.push({ op: 'cmp', a, b, out: String(d(a).cmp(d(b))) });
  const fromNumber = (x: number) => cases.push({ op: 'fromNumber', a: x, out: str(d(x)) });
  const everyOp = (a: string, b: string) => {
    for (const op of Object.keys(BINARY_OPS)) binary(op, a, b);
    for (const op of Object.keys(UNARY_OPS)) unary(op, a);
    toFixed(a);
    cmp(a, b);
    for (const dp of [0, 2, 8]) withDp('toFixedDp', a, dp);
    for (const dp of [0, 3, 10]) withDp('toDP', a, dp);
  };

  // Random operands: 1-30 integer digits, 0-30 fraction digits, random sign.
  for (let i = 0; i < 450; i++) {
    const a = randomOperand();
    const b = randomOperand();
    const step = (rand() < 0.5 ? '' : '0.') + String(int(1, 9)) + digits(int(0, 6));
    for (const op of ['add', 'sub', 'mul', 'div']) binary(op, a, b);
    for (const op of Object.keys(UNARY_OPS)) unary(op, a);
    toFixed(a);
    cmp(a, b);
    withDp('toDP', a, int(0, 10));
    withDp('toFixedDp', a, int(0, 8));
    for (const op of ['floorToStep', 'ceilToStep', 'roundToStep']) binary(op, a, step);
  }

  // Zero and negative zero through every op, against each other and ordinary values.
  for (const [a, b] of [['0', '0'], ['-0', '0'], ['0', '-0'], ['-0', '-0'], ['0', '5'], ['-0', '5'], ['5', '-0'], ['-3', '0'], ['-0', '-2.5'], ['0.5', '-0.5']]) {
    everyOp(a!, b!);
  }

  // Repeating quotients, and one whose 51st significant digit is exactly a tie.
  for (const [a, b] of [['1', '3'], ['2', '3'], ['-1', '3'], ['1', '-3'], ['1', '7'], ['10', '3'], ['1', '0.3'], ['100', '7']]) {
    binary('div', a!, b!);
  }
  // Exact quotients with 51 significant digits ending in 5: a true tie at the 50th digit, with an even and an odd digit before it.
  binary('div', '1', '2');
  binary('div', '12345678901234567890123456789012345678901234567890', '100');
  binary('div', '123456789012345678901234567890123456789012345678905', '1000');
  binary('div', '123456789012345678901234567890123456789012345678915', '1000');
  binary('div', '-123456789012345678901234567890123456789012345678905', '1000');
  // Just above and below the tie: 51 digits ending 5, then a nonzero tail.
  binary('div', '1234567890123456789012345678901234567890123456789050000000000001', '1000000000000');
  binary('div', '1234567890123456789012345678901234567890123456789049999999999999', '1000000000000');
  binary('div', '1', '1' + '0'.repeat(50));
  binary('mul', '123456789012345678901234567890123456789012345678905', '1');
  binary('add', '12345678901234567890123456789012345678901234567890', '0.5');
  binary('add', '12345678901234567890123456789012345678901234567891', '0.5');
  binary('add', '1e60', '1e-30');
  binary('sub', '1e60', '1e-30');
  binary('add', '1', '1e-60');
  binary('sub', '1', '1e-60');
  binary('sub', '5', '5');
  binary('sub', '-5', '-5');
  binary('add', '-5', '5');

  // Exponent notation and tiny values.
  for (const a of ['1e-7', '5E+3', '0.00000000000000000000000000000001', '1e-40', '-1e-7', '1e40', '1e41', '12e-3']) {
    everyOp(a, '3');
    everyOp('2.5', a);
  }

  // Long inputs are never rounded on construction, only by arithmetic.
  const fifty = '1234567890123456789012345678901234567890.1234567891';
  const sixty = '1234567890123456789012345678901234567890123456789012.12345678';
  for (const a of [fifty, sixty, '-' + sixty]) {
    toFixed(a);
    unary('neg', a);
    binary('add', a, '0');
    binary('mul', a, '1');
    cmp(a, fifty);
  }

  // Ties for round / toDP / toFixedDp.
  for (const a of ['0.5', '1.5', '2.5', '3.5', '-0.5', '-1.5', '-2.5', '-3.5', '0.4999', '-0.4999']) {
    unary('round', a);
    unary('floor', a);
    unary('ceil', a);
    withDp('toDP', a, 0);
    withDp('toFixedDp', a, 0);
  }
  for (const a of ['1.00000000005', '1.00000000015', '-1.00000000005', '0.125', '0.135', '-0.125', '0.005', '-0.005', '-0.0001', '0.0049', '9.995', '99.5', '-99.5']) {
    for (const dp of [0, 2, 3, 10]) {
      withDp('toDP', a, dp);
      withDp('toFixedDp', a, dp);
    }
  }

  // Step helpers, including a zero step (decimal.js yields NaN, which must become an error).
  const stepCases = [
    ['10.37', '0.05'], ['-10.37', '0.05'], ['0.0123', '0.001'], ['7', '0.5'], ['7.25', '0.5'], ['7.75', '0.5'],
    ['5', '0'], ['0', '0.1'], ['-0', '0.1'], ['1234.5678', '10'], ['2.5', '1'], ['3.5', '1'],
  ];
  for (const [a, step] of stepCases) {
    for (const op of ['floorToStep', 'ceilToStep', 'roundToStep']) binary(op, a!, step!);
  }

  // Ordering and equality, including -0 against 0.
  const cmpCases = [['0', '-0'], ['1', '1.0'], ['-1', '-1.00'], ['1e3', '1000'], ['0.1', '0.10'], ['-5', '5'], ['5', '-5']];
  for (const [a, b] of cmpCases) cmp(a!, b!);

  // JS numbers, as new Decimal(x) reads them.
  const numbers = [0.1, 1e-7, 0.0005, 3, 100, 68000, 0, 0.1 + 0.2, 1e21, 1.5e-10, 123456.789, -2.5, 5e-324, 1.7976931348623157e308];
  for (const x of numbers) fromNumber(x);
  return cases;
}

/**
 * Accounting types as TypeScript literals, so tsc checks them against the interfaces. Each type has a
 * fully populated sample and a minimal one (optional fields absent, nullable fields null).
 */
function accountingTypesFixture() {
  const config: PaperAccountConfig = {
    takerFeeRate: '0.0006',
    makerFeeRate: '0.0001',
    slippageBps: '2.5',
    impactBpsPerMillion: '7',
  };
  const risk: RiskSettings = {
    maxLeverage: 5,
    maxRiskPerTradePct: 0.25,
    maxDailyLossPct: 1.5,
    maxOpenPositions: 4,
    requireStopLoss: false,
    maxQuoteAgeSec: 12.5,
    maxNotional: 250000,
  };
  const riskMinimal: RiskSettings = { ...DEFAULT_RISK_SETTINGS };

  const paperAccountFull: PaperAccount = {
    id: 'acc-1',
    name: 'Main',
    source: 'LIVE',
    venue: 'OKX_DEMO',
    exchangeUid: '123456',
    liveLimits: risk,
    isDemo: true,
    initialBalance: '10000',
    walletBalance: '10234.5678',
    usedMargin: '500.25',
    realizedPnl: '234.5678',
    feesPaid: '12.5',
    fundingPaid: '-0.75',
    config,
    createdAt: 1700000000000,
  };
  const paperAccountMinimal: PaperAccount = {
    ...paperAccountFull,
    source: 'REPLAY',
    venue: 'PAPER',
    exchangeUid: null,
    liveLimits: null,
    isDemo: false,
    config: { ...DEFAULT_ACCOUNT_CONFIG },
  };

  const orderFull: Order = {
    id: 'ord-1',
    accountId: 'acc-1',
    clientOrderId: 'cli-1',
    symbol: 'BTCUSDT',
    side: 'BUY',
    type: 'LIMIT',
    quantity: '0.5',
    price: '67000.5',
    triggerPrice: '66000',
    leverage: 3,
    reduceOnly: true,
    closePosition: false,
    status: 'FILLED',
    filledQuantity: '0.5',
    avgFillPrice: '67000.5',
    reservedMargin: '0',
    attachedStopLoss: '65000',
    attachedTakeProfit: '70000',
    trailingKind: 'RATIO',
    trailingCallback: '0.02',
    trailingActivationPrice: '68000',
    trailingExtreme: '69000.5',
    attachedTrailingStop: { kind: 'SPREAD', callback: '150.5', activationPrice: '68500' },
    rejectReason: 'none',
    exchangeOrderId: '987654321',
    agentId: 'agent-1',
    decisionId: 'dec-1',
    createdAt: 1700000000000,
    updatedAt: 1700000001000,
  };
  const orderMinimal: Order = {
    ...orderFull,
    type: 'STOP_MARKET',
    side: 'SELL',
    status: 'REJECTED',
    price: null,
    triggerPrice: null,
    reduceOnly: false,
    closePosition: true,
    avgFillPrice: null,
    attachedStopLoss: null,
    attachedTakeProfit: null,
    trailingKind: null,
    trailingCallback: null,
    trailingActivationPrice: null,
    trailingExtreme: null,
    attachedTrailingStop: null,
    rejectReason: null,
    exchangeOrderId: null,
    agentId: null,
    decisionId: null,
  };

  const fillFull: Fill = {
    id: 'fill-1',
    orderId: 'ord-1',
    accountId: 'acc-1',
    symbol: 'ETHUSDT',
    side: 'SELL',
    quantity: '2',
    price: '3500.25',
    fee: '3.5',
    realizedPnl: '-12.125',
    liquidity: 'MAKER',
    ts: 1700000002000,
  };
  const fillTaker: Fill = { ...fillFull, liquidity: 'TAKER' };
  const fillLiquidation: Fill = { ...fillFull, liquidity: 'LIQUIDATION' };

  const requestFull: OrderRequest = {
    accountId: 'acc-1',
    clientOrderId: 'cli-1',
    symbol: 'SOLUSDT',
    side: 'BUY',
    type: 'LIMIT',
    quantity: '10',
    price: '150.5',
    leverage: 2,
    reduceOnly: true,
    stopLoss: '140',
    takeProfit: '170',
    trailingStop: { kind: 'RATIO', callback: '0.01', activationPrice: null },
    agentId: 'agent-1',
    decisionId: 'dec-1',
  };
  const requestMinimal: OrderRequest = {
    accountId: 'acc-1',
    clientOrderId: 'cli-2',
    symbol: 'SOLUSDT',
    side: 'SELL',
    type: 'MARKET',
    quantity: '1',
    leverage: 1,
  };

  const positionFull: Position = {
    id: 'pos-1',
    accountId: 'acc-1',
    symbol: 'BTCUSDT',
    side: 'LONG',
    quantity: '0.5',
    entryPrice: '67000.5',
    leverage: 3,
    margin: '11166.75',
    liquidationPrice: '45000.1',
    status: 'LIQUIDATED',
    realizedPnl: '-5',
    feesPaid: '20.1',
    fundingPaid: '1.5',
    maxQuantity: '0.75',
    exitPrice: '44999',
    lastFundingTime: 1700000003000,
    openedAt: 1700000000000,
    closedAt: 1700000004000,
  };
  const positionOpen: Position = {
    ...positionFull,
    side: 'SHORT',
    status: 'OPEN',
    exitPrice: null,
    closedAt: null,
  };
  const viewFull: PositionView = {
    ...positionFull,
    markPrice: '67500.5',
    unrealizedPnl: '250',
    stopLoss: '65000',
    takeProfit: '70000',
    trailingStop: { kind: 'RATIO', callback: '0.02', activationPrice: '68000', extreme: '69000.5', triggerPrice: '67620.49' },
    externalProtection: [{ kind: 'TRAILING', triggerPrice: null, detail: 'callback 1%' }],
  };
  const viewMinimal: PositionView = {
    ...positionOpen,
    markPrice: null,
    unrealizedPnl: '0',
    stopLoss: null,
    takeProfit: null,
    trailingStop: null,
    externalProtection: [],
  };

  const resultFull: OrderResult = { order: orderFull, fills: [fillFull, fillTaker], duplicate: true };
  const resultMinimal: OrderResult = { order: orderMinimal, fills: [], duplicate: false };

  const ledgerFull: LedgerEvent = {
    seq: 42,
    id: 'led-1',
    accountId: 'acc-1',
    type: 'LIQUIDATION_PENALTY',
    amount: '-5.5',
    marginDelta: '-100',
    walletAfter: '9894.5',
    symbol: 'BTCUSDT',
    orderId: 'ord-1',
    fillId: 'fill-1',
    positionId: 'pos-1',
    memo: 'penalty',
    ts: 1700000005000,
  };
  const ledgerMinimal: LedgerEvent = {
    ...ledgerFull,
    type: 'DEPOSIT',
    symbol: null,
    orderId: null,
    fillId: null,
    positionId: null,
    memo: '',
  };
  // Every variant of every enum, so a mistyped rename cannot hide.
  const orderTypes: Order['type'][] = ['MARKET', 'LIMIT', 'STOP_MARKET', 'TAKE_PROFIT_MARKET', 'TRAILING_STOP_MARKET'];
  const orderStatuses: Order['status'][] = ['OPEN', 'FILLED', 'CANCELED', 'REJECTED'];
  const orderSides: Order['side'][] = ['BUY', 'SELL'];
  const positionSides: Position['side'][] = ['LONG', 'SHORT'];
  const positionStatuses: Position['status'][] = ['OPEN', 'CLOSED', 'LIQUIDATED'];
  const venues: PaperAccount['venue'][] = ['PAPER', 'OKX_DEMO', 'OKX_LIVE'];
  const sources: PaperAccount['source'][] = ['REPLAY', 'LIVE'];
  const actions: AiDecisionData['action'][] = ['LONG', 'SHORT', 'CLOSE', 'HOLD'];
  const entryTypes: AiDecisionData['orderType'][] = ['MARKET', 'LIMIT'];
  const ledgerTypes: LedgerEvent['type'][] = [
    'DEPOSIT', 'TRADE_FEE', 'REALIZED_PNL', 'FUNDING', 'LIQUIDATION_PENALTY', 'MARGIN_LOCK', 'MARGIN_RELEASE',
  ];

  const equity: EquityPoint = {
    accountId: 'acc-1',
    ts: 1700000006000,
    equity: '10500.5',
    wallet: '10234.5678',
    unrealized: '265.9322',
  };
  const balance: AccountBalance = {
    accountId: 'acc-1',
    initialBalance: '10000',
    walletBalance: '10234.5678',
    availableBalance: '9000',
    usedMargin: '500.25',
    orderMargin: '100',
    equity: '10500.5',
    realizedPnl: '234.5678',
    unrealizedPnl: '265.9322',
    feesPaid: '12.5',
    fundingPaid: '-0.75',
  };

  const decisionFull: AiDecisionData = {
    symbol: 'BTCUSDT',
    action: 'LONG',
    orderType: 'LIMIT',
    entryPrice: 67000.5,
    stopLoss: 65000,
    takeProfit: 70000,
    reason: 'Breakout above resistance',
    confidence: 0.72,
    expiresAt: '2026-01-01T00:00:00.000Z',
  };
  const decisionMinimal: AiDecisionData = {
    symbol: 'ETHUSDT',
    action: 'HOLD',
    orderType: 'MARKET',
    entryPrice: null,
    stopLoss: null,
    takeProfit: null,
    reason: '',
    confidence: 1,
    expiresAt: null,
  };

  const check: RiskCheck = { id: 'leverage', label: 'Leverage', passed: false, detail: '5x exceeds 3x' };
  const sized: SizedTrade = {
    quantity: '0.5',
    leverage: 3,
    notional: '33500.25',
    margin: '11166.75',
    riskAmount: '1000',
    referencePrice: '67000.5',
    estimatedLiquidationPrice: '45000.1',
  };
  const riskResultFull: RiskResult = {
    approved: false,
    checks: [check, { ...check, id: 'stop', passed: true }],
    rejectionReasons: ['Leverage too high'],
    sized,
  };
  const riskResultMinimal: RiskResult = { approved: true, checks: [], rejectionReasons: [], sized: null };

  return {
    defaults: {
      accountConfig: DEFAULT_ACCOUNT_CONFIG,
      riskSettings: DEFAULT_RISK_SETTINGS,
      liveLimits: DEFAULT_LIVE_LIMITS,
    },
    samples: {
      paperAccount: [
        paperAccountFull,
        paperAccountMinimal,
        ...venues.map((venue) => ({ ...paperAccountFull, venue })),
        ...sources.map((source) => ({ ...paperAccountFull, source })),
      ],
      paperAccountConfig: [config, { ...DEFAULT_ACCOUNT_CONFIG }],
      order: [
        orderFull,
        orderMinimal,
        ...orderTypes.map((type) => ({ ...orderFull, type })),
        ...orderStatuses.map((status) => ({ ...orderFull, status })),
        ...orderSides.map((side) => ({ ...orderFull, side })),
      ],
      orderRequest: [
        requestFull,
        requestMinimal,
        ...entryTypes.map((type) => ({ ...requestFull, type })),
        ...orderSides.map((side) => ({ ...requestFull, side })),
      ],
      orderResult: [resultFull, resultMinimal],
      fill: [fillFull, fillTaker, fillLiquidation],
      position: [
        positionFull,
        positionOpen,
        ...positionSides.map((side) => ({ ...positionFull, side })),
        ...positionStatuses.map((status) => ({ ...positionFull, status })),
      ],
      positionView: [viewFull, viewMinimal],
      ledgerEvent: [ledgerFull, ledgerMinimal, ...ledgerTypes.map((type) => ({ ...ledgerFull, type }))],
      equityPoint: [equity],
      accountBalance: [balance],
      riskSettings: [risk, riskMinimal],
      aiDecisionData: [
        decisionFull,
        decisionMinimal,
        ...actions.map((action) => ({ ...decisionFull, action })),
        ...entryTypes.map((orderType) => ({ ...decisionFull, orderType })),
      ],
      riskCheck: [check],
      riskResult: [riskResultFull, riskResultMinimal],
      sizedTrade: [sized],
    },
    configJsonText: JSON.stringify(DEFAULT_ACCOUNT_CONFIG),
    riskJsonText: JSON.stringify(DEFAULT_RISK_SETTINGS),
  };
}

const MATH_SEED = 0x6d617468;
const MATH_CASES = 150;

/**
 * Pure trading math: every call the Rust port makes, with the TypeScript answer. The first entries of
 * each list are assertions from packages/core/test/perp.test.ts; the rest are seeded random inputs
 * built from the default instruments and the default account config.
 */
function coreMathFixture() {
  const rand = mulberry32(MATH_SEED);
  const pick = <T>(items: readonly T[]): T => items[Math.floor(rand() * items.length)]!;
  const num = (lo: number, hi: number, dp: number) => (lo + rand() * (hi - lo)).toFixed(dp);
  const sides: PositionSide[] = ['LONG', 'SHORT'];
  // INSTRUMENTS is open-ended (index lookups may miss), so resolve explicitly rather than assert at each use.
  const instrumentOf = (symbol: string) => {
    const spec = INSTRUMENTS[symbol];
    if (!spec) throw new Error(`No instrument specification for ${symbol}`);
    return spec;
  };
  const instruments = DEFAULT_SYMBOLS.map(instrumentOf);
  const rest = (hand: number) => MATH_CASES - hand;

  const notional = [{ quantity: '2', price: '100', out: str(linearPerp.notional('2', '100')) }];
  for (let i = 0; i < rest(1); i++) {
    const quantity = num(0, 50, pick([0, 2, 4, 8]));
    const price = num(0.01, 100000, pick([0, 2, 4, 8]));
    notional.push({ quantity, price, out: str(linearPerp.notional(quantity, price)) });
  }

  const pnlInputs: Array<[PositionSide, string, string, string]> = [
    ['LONG', '100', '110', '2'],
    ['SHORT', '100', '110', '2'],
  ];
  for (let i = 0; i < rest(2); i++) {
    pnlInputs.push([pick(sides), num(1, 90000, 2), num(1, 90000, 2), num(0.0001, 20, 4)]);
  }
  const pnl = pnlInputs.map(([side, entry, exit, quantity]) => ({
    side, entry, exit, quantity, out: str(linearPerp.pnl(side, entry, exit, quantity)),
  }));

  const marginInputs: Array<[string, number]> = [['200', 4]];
  for (let i = 0; i < rest(1); i++) {
    marginInputs.push([num(1, 1_000_000, pick([0, 2, 6])), 1 + Math.floor(rand() * 125)]);
  }
  const initialMargin = marginInputs.map(([n, leverage]) => ({
    notional: n, leverage, out: str(linearPerp.initialMargin(d(n), leverage)),
  }));

  const liqInputs = [
    { side: 'LONG' as PositionSide, entry: '100', quantity: '2', margin: '50', maintenanceRate: '0.005' },
    { side: 'SHORT' as PositionSide, entry: '100', quantity: '2', margin: '50', maintenanceRate: '0.005' },
  ];
  for (let i = 0; i < rest(2); i++) {
    const instrument = pick(instruments);
    const entry = num(1, 90000, 2);
    const quantity = num(0.001, 20, 4);
    const leverage = 1 + Math.floor(rand() * instrument.maxLeverage);
    // Margin near entry*quantity/leverage, so some positions are already past liquidation (price clamps to 0).
    const margin = d(entry).mul(quantity).div(leverage).mul(num(0.2, 3, 3)).toFixed(8);
    liqInputs.push({ side: pick(sides), entry, quantity, margin, maintenanceRate: instrument.maintenanceMarginRate });
  }
  const liquidationPrice = liqInputs.map((input) => ({ ...input, out: str(linearPerp.liquidationPrice(input)) }));

  const fundingInputs: Array<[PositionSide, string, string, string]> = [
    ['LONG', '100', '2', '0.001'],
    ['SHORT', '100', '2', '0.001'],
    ['LONG', '100', '2', '-0.001'],
  ];
  for (let i = 0; i < rest(3); i++) {
    fundingInputs.push([pick(sides), num(1, 90000, 2), num(0.001, 20, 4), num(-0.003, 0.003, 6)]);
  }
  const funding = fundingInputs.map(([side, markPrice, quantity, rate]) => ({
    side, markPrice, quantity, rate, out: str(linearPerp.funding(side, markPrice, quantity, rate)),
  }));

  const mathForCases = [...instruments, { ...instrumentOf(DEFAULT_SYMBOLS[0]!), marketType: 'SPOT' as never }].map((instrument) => {
    try {
      mathFor(instrument);
      return { marketType: instrument.marketType as string, error: null as string | null };
    } catch (e) {
      return { marketType: instrument.marketType as string, error: (e as Error).message };
    }
  });

  // Fill model.
  const hand: PaperAccountConfig = { ...DEFAULT_ACCOUNT_CONFIG, slippageBps: '10', impactBpsPerMillion: '0' };
  const handQuote: Quote = {
    source: 'REPLAY', symbol: 'BTCUSDT', bid: '99', ask: '101', last: '100', mark: '100',
    markEstimated: false, fundingRate: '0.001', nextFundingTime: null, ts: 1_000,
  };
  const handInstrument = { ...instrumentOf('BTCUSDT'), priceTick: '0.1' };
  const randomQuote = (instrument: (typeof instruments)[number]): Quote => {
    const tick = d(instrument.priceTick);
    const mid = d(num(1, 90000, 2)).div(tick).floor().mul(tick);
    const half = tick.mul(1 + Math.floor(rand() * 6));
    const bid = mid.minus(half);
    return {
      source: 'REPLAY', symbol: instrument.symbol, bid: str(bid.lt(tick) ? tick : bid), ask: str(mid.plus(half)),
      last: str(mid), mark: str(mid), markEstimated: false, fundingRate: null, nextFundingTime: null, ts: 1_000,
    };
  };

  const marketFillInputs: Array<{
    side: 'BUY' | 'SELL'; quantity: string; quote: Quote; config: PaperAccountConfig; instrument: typeof handInstrument;
  }> = [
    { side: 'BUY', quantity: '1', quote: handQuote, config: hand, instrument: handInstrument },
    { side: 'SELL', quantity: '1', quote: handQuote, config: hand, instrument: handInstrument },
  ];
  for (let i = 0; i < rest(2); i++) {
    const instrument = pick(instruments);
    marketFillInputs.push({
      side: pick(['BUY', 'SELL'] as const),
      quantity: num(0.0001, 500, 4),
      quote: randomQuote(instrument),
      config: {
        ...DEFAULT_ACCOUNT_CONFIG,
        slippageBps: pick([DEFAULT_ACCOUNT_CONFIG.slippageBps, '0', '10', num(0, 50, 2)]),
        impactBpsPerMillion: pick([DEFAULT_ACCOUNT_CONFIG.impactBpsPerMillion, '0', num(0, 100, 3)]),
      },
      instrument: instrument as typeof handInstrument,
    });
  }
  const marketFill = marketFillInputs.map((input) => ({
    ...input,
    out: str(marketFillPrice(input.side, input.quantity, input.quote, input.config, input.instrument)),
  }));

  // Limits sit on, just inside and just outside the touch, so the comparison direction is exercised.
  const limitInputs: Array<{ side: 'BUY' | 'SELL'; limit: string; quote: Quote }> = [
    { side: 'BUY', limit: '101', quote: handQuote },
    { side: 'BUY', limit: '100.9', quote: handQuote },
    { side: 'SELL', limit: '99', quote: handQuote },
  ];
  for (let i = 0; i < rest(3); i++) {
    const quote = randomQuote(pick(instruments));
    const side = pick(['BUY', 'SELL'] as const);
    const touch = d(side === 'BUY' ? quote.ask : quote.bid);
    const limit = i % 3 === 0 ? touch : touch.plus(num(-5, 5, 2));
    limitInputs.push({ side, limit: str(limit), quote });
  }
  const marketable = limitInputs.map((input) => ({ ...input, out: isMarketable(input.side, input.limit, input.quote) }));
  const restingLimit = limitInputs.map((input) => ({
    ...input, out: isRestingLimitFilled(input.side, input.limit, input.quote),
  }));

  const triggerInputs: Array<{ positionIsLong: boolean; trigger: string; last: string }> = [];
  for (let i = 0; i < MATH_CASES; i++) {
    const trigger = num(1, 90000, 2);
    // Every third case lands exactly on the trigger: both comparisons are inclusive.
    const last = i % 3 === 0 ? trigger : num(1, 90000, 2);
    triggerInputs.push({ positionIsLong: rand() < 0.5, trigger, last });
  }
  const stop = triggerInputs.map((i) => ({ ...i, out: isStopTriggered(i.positionIsLong, i.trigger, i.last) }));
  const takeProfit = triggerInputs.map((i) => ({ ...i, out: isTakeProfitTriggered(i.positionIsLong, i.trigger, i.last) }));

  return {
    perp: { notional, pnl, initialMargin, liquidationPrice, funding, mathFor: mathForCases },
    fill: { marketFill, marketable, restingLimit, stop, takeProfit },
    ledger: ledgerStreams(),
    performance: performanceCases(rand),
  };
}

/** Event streams with the fold and audit output TypeScript gives. */
function ledgerStreams() {
  const base = { accountId: 'acct', symbol: null, orderId: null, fillId: null, positionId: null };
  const steps: Array<[LedgerEvent['type'], string, string]> = [
    ['DEPOSIT', '10000', '0'],
    ['MARGIN_LOCK', '0', '250.5'],
    ['TRADE_FEE', '-1.2345', '0'],
    ['FUNDING', '-0.0173', '0'],
    ['REALIZED_PNL', '84.37', '0'],
    ['MARGIN_RELEASE', '0', '-250.5'],
    ['FUNDING', '0.0042', '0'],
    ['LIQUIDATION_PENALTY', '-12.5', '0'],
  ];
  let running = d(0);
  const events: LedgerEvent[] = steps.map(([type, amount, marginDelta], i) => {
    running = running.plus(amount);
    return {
      ...base, seq: i + 1, id: `evt-${i + 1}`, type, amount, marginDelta, walletAfter: str(running),
      memo: `step ${i + 1}`, ts: 1_000 + i,
    };
  });
  const totals = foldLedger(events);
  const account = (over: Partial<PaperAccount>): PaperAccount => ({
    id: 'acct', name: 'Golden', source: 'REPLAY', venue: 'PAPER', exchangeUid: null, liveLimits: null, isDemo: false,
    initialBalance: '10000', walletBalance: totals.wallet, usedMargin: totals.usedMargin,
    realizedPnl: totals.realizedPnl, feesPaid: totals.feesPaid, fundingPaid: totals.fundingPaid,
    config: DEFAULT_ACCOUNT_CONFIG, createdAt: 1_000, ...over,
  });
  const brokenEvents = events.map((e) => (e.seq === 3 ? { ...e, walletAfter: str(d(e.walletAfter).plus('0.5')) } : e));
  const streams: Array<[string, PaperAccount, LedgerEvent[]]> = [
    ['consistent', account({}), events],
    ['wallet-off-by-1e-8', account({ walletBalance: str(d(totals.wallet).plus('0.00000001')) }), events],
    ['wallet-after-broken-at-seq-3', account({}), brokenEvents],
    // Same values, different spelling: the audit compares numbers, not text.
    [
      'unnormalised-stored-strings',
      account({ walletBalance: totals.wallet + '000', usedMargin: '0.0', feesPaid: '+' + totals.feesPaid }),
      events,
    ],
    ['empty', account({ walletBalance: '0', usedMargin: '0', realizedPnl: '0', feesPaid: '0', fundingPaid: '0' }), []],
  ];
  return streams.map(([name, acct, evts]) => ({
    name, account: acct, events: evts, fold: foldLedger(evts), audit: auditLedger(acct, evts),
  }));
}

function performanceCases(rand: () => number) {
  const equities: number[][] = [
    [], [100], [0], [0, 0, 0], [0, 100, 50], [-5, 10, 5], [100, 90, 95, 80, 120, 60], [100, 100, 100],
    [10000, 10100.5, 9950.25, 10200, 9800.75], [1, 2, 3, 4], [5, 4, 3, 2, 1],
  ];
  for (let i = 0; i < 100; i++) {
    const length = 1 + Math.floor(rand() * 40);
    let value = 10000;
    const series: number[] = [];
    for (let j = 0; j < length; j++) {
      value = Math.max(0, value * (1 + (rand() - 0.5) * 0.1));
      series.push(Number(value.toFixed(2)));
    }
    equities.push(series);
  }
  const pnls: number[][] = [[], [0], [1], [-1], [1, -1], [0, 0, 1], [5.5, -2.25, 0, 3, -8], [-1, -2, -3]];
  for (let i = 0; i < 100; i++) {
    const length = 1 + Math.floor(rand() * 30);
    pnls.push(Array.from({ length }, () => (rand() < 0.1 ? 0 : Number(((rand() - 0.45) * 200).toFixed(2)))));
  }
  return {
    maxDrawdown: equities.map((equity) => ({ equity, out: noNegativeZero(maxDrawdownPct(equity)) })),
    winRate: pnls.map((closedNetPnl) => ({ closedNetPnl, out: winRatePct(closedNetPnl) })),
  };
}

/**
 * Risk engine cases: `{ input, output }` where output is what `evaluateRisk` returns. Hand-picked
 * cases cover every check id passing and failing, then a seeded sweep checks the sizing arithmetic.
 */
function riskFixture() {
  const base: Quote = {
    source: 'REPLAY', symbol: 'BTCUSDT', bid: '99', ask: '101', last: '100', mark: '100',
    markEstimated: false, fundingRate: '0.001', nextFundingTime: null, ts: 1_000,
  };
  const balance: AccountBalance = {
    accountId: 'a', initialBalance: '10000', walletBalance: '10000', availableBalance: '10000',
    usedMargin: '0', orderMargin: '0', equity: '10000', realizedPnl: '0', unrealizedPnl: '0', feesPaid: '0', fundingPaid: '0',
  };
  const decision: AiDecisionData = {
    symbol: 'BTCUSDT', action: 'LONG', orderType: 'MARKET', entryPrice: null, stopLoss: 90,
    takeProfit: 120, reason: 'test', confidence: 0.8, expiresAt: null,
  };
  // The TS test passes partial positions; Rust deserializes the full type, so fill in the rest.
  const position = (symbol: string, side: PositionSide = 'LONG', quantity = '1'): Position => ({
    id: `p-${symbol}`, accountId: 'a', symbol, side, quantity, entryPrice: '100', leverage: 3,
    margin: '33.33', liquidationPrice: '70', status: 'OPEN', realizedPnl: '0', feesPaid: '0',
    fundingPaid: '0', maxQuantity: quantity, exitPrice: null, lastFundingTime: 0, openedAt: 0, closedAt: null,
  });
  const make = (overrides: Record<string, unknown> = {}) => ({
    decision, symbol: 'BTCUSDT', settings: DEFAULT_RISK_SETTINGS, quote: base as Quote | null, now: 1_000,
    balance, openPositions: [] as Position[], dailyPnl: '0', dayStartEquity: '10000', fees: DEFAULT_ACCOUNT_CONFIG,
    ...overrides,
  });
  const settings = (o: Partial<RiskSettings>): RiskSettings => ({ ...DEFAULT_RISK_SETTINGS, ...o });
  const dec = (o: Partial<AiDecisionData>): AiDecisionData => ({ ...decision, ...o });
  const quoteFor = (symbol: string, bid: string, ask: string, ts = 1_000): Quote => ({ ...base, symbol, bid, ask, last: bid, mark: bid, ts });

  const inputs: Array<ReturnType<typeof make>> = [
    // The risk section of packages/core/test/perp.test.ts, in order.
    make(),
    make({ decision: dec({ symbol: 'ETHUSDT' }) }),
    make({ decision: dec({ expiresAt: new Date(500).toISOString() }) }),
    make({ now: 40_000 }),
    make({ dailyPnl: '-250' }),
    make({ settings: settings({ maxOpenPositions: 1 }), openPositions: [position('ETHUSDT')] }),
    make({ openPositions: [position('BTCUSDT')] }),
    make({ decision: dec({ stopLoss: 102 }) }),
    make({ decision: dec({ stopLoss: null }) }),
    make({ decision: dec({ takeProfit: 90 }) }),
    make({ balance: { ...balance, equity: '0.01', availableBalance: '0.01' } }),
    make({ settings: settings({ requireStopLoss: false }), decision: dec({ stopLoss: null }) }),
    make({ settings: settings({ maxLeverage: 100 }), decision: dec({ stopLoss: 1 }) }),
    // Pass and fail around each limit.
    make({ decision: dec({ expiresAt: new Date(5_000).toISOString() }) }),
    make({ decision: dec({ expiresAt: '1970-01-01T00:00:02.123+02:00' }) }),
    make({ decision: dec({ expiresAt: '1970-01-01T00:00:05.9876Z' }) }),
    make({ decision: dec({ expiresAt: new Date(1_000).toISOString() }) }),
    make({ decision: dec({ expiresAt: '2030-12-31T23:59:59-05:30' }) }),
    make({ now: 31_000 }),
    make({ now: 31_001 }),
    make({ now: 500 }),
    make({ quote: null }),
    make({ quote: null, decision: dec({ action: 'HOLD' }) }),
    make({ dailyPnl: '-200' }),
    make({ dailyPnl: '-199.999' }),
    make({ dailyPnl: '55.555' }),
    make({ dayStartEquity: '0', dailyPnl: '0' }),
    make({ settings: settings({ maxOpenPositions: 1 }), openPositions: [position('BTCUSDT', 'SHORT', '0.5')] }),
    make({ settings: settings({ maxOpenPositions: 0 }) }),
    make({ settings: settings({ maxOpenPositions: 2.5 }), openPositions: [position('ETHUSDT'), position('SOLUSDT')] }),
    make({ openPositions: [position('ETHUSDT')] }),
    make({ openPositions: [position('BTCUSDT', 'SHORT', '0.25')] }),
    // Actions.
    make({ decision: dec({ action: 'HOLD' }) }),
    make({ decision: dec({ action: 'HOLD', symbol: 'ETHUSDT' }) }),
    make({ decision: dec({ action: 'CLOSE' }), openPositions: [position('BTCUSDT')] }),
    make({ decision: dec({ action: 'CLOSE' }), openPositions: [position('BTCUSDT', 'SHORT', '2.5')] }),
    make({ decision: dec({ action: 'CLOSE' }) }),
    make({ decision: dec({ action: 'CLOSE' }), openPositions: [position('ETHUSDT')] }),
    make({ decision: dec({ action: 'CLOSE', symbol: 'ETHUSDT' }), openPositions: [position('BTCUSDT')] }),
    make({ decision: dec({ action: 'CLOSE', expiresAt: new Date(500).toISOString() }), openPositions: [position('BTCUSDT')] }),
    // Stop loss and take profit on both sides, with and without a stop.
    make({ decision: dec({ action: 'SHORT', stopLoss: 110, takeProfit: 80 }) }),
    make({ decision: dec({ action: 'SHORT', stopLoss: 90, takeProfit: 80 }) }),
    make({ decision: dec({ action: 'SHORT', stopLoss: 110, takeProfit: 99 }) }),
    make({ decision: dec({ action: 'SHORT', stopLoss: 110, takeProfit: null }) }),
    make({ decision: dec({ takeProfit: null }) }),
    make({ decision: dec({ takeProfit: 101 }) }),
    make({ decision: dec({ takeProfit: 101.0001 }) }),
    make({ decision: dec({ stopLoss: 101 }) }),
    make({ decision: dec({ stopLoss: 100.9 }) }),
    make({ settings: settings({ requireStopLoss: false }), decision: dec({ action: 'SHORT', stopLoss: null, takeProfit: 80 }) }),
    make({ settings: settings({ requireStopLoss: false }), decision: dec({ stopLoss: null, takeProfit: 90 }) }),
    make({ settings: settings({ requireStopLoss: false }), decision: dec({ stopLoss: 102 }) }),
    // Limit entries.
    make({ decision: dec({ orderType: 'LIMIT', entryPrice: 95 }) }),
    make({ decision: dec({ orderType: 'LIMIT', entryPrice: 95.25, stopLoss: 94.5, takeProfit: 99 }) }),
    make({ decision: dec({ orderType: 'LIMIT', entryPrice: null }) }),
    make({ decision: dec({ orderType: 'MARKET', entryPrice: 95 }) }),
    make({ decision: dec({ action: 'SHORT', orderType: 'LIMIT', entryPrice: 105, stopLoss: 108, takeProfit: 98 }) }),
    make({ decision: dec({ orderType: 'LIMIT', entryPrice: 95, stopLoss: 96 }) }),
    make({ decision: dec({ orderType: 'LIMIT', entryPrice: 0.1 + 0.2, stopLoss: 0.1, takeProfit: 1 }) }),
    // Sizing, leverage and notional limits.
    make({ settings: settings({ maxLeverage: 2.5 }) }),
    make({ settings: settings({ maxLeverage: 0.5 }) }),
    make({ settings: settings({ maxLeverage: 1 }) }),
    make({ settings: settings({ maxLeverage: 1000 }) }),
    make({ settings: settings({ maxLeverage: 100 }) }),
    make({ settings: settings({ maxLeverage: 100 }), decision: dec({ action: 'SHORT', stopLoss: 400, takeProfit: 50 }) }),
    make({ settings: settings({ maxLeverage: 100 }), decision: dec({ action: 'SHORT', stopLoss: 100000, takeProfit: 50 }) }),
    make({ settings: settings({ maxNotional: 500 }) }),
    make({ settings: settings({ maxNotional: 50 }) }),
    make({ settings: settings({ maxNotional: 0.1 }) }),
    make({ settings: settings({ maxRiskPerTradePct: 0.1 }) }),
    make({ settings: settings({ maxRiskPerTradePct: 50 }) }),
    make({ settings: settings({ maxRiskPerTradePct: 0 }) }),
    make({ balance: { ...balance, availableBalance: '120' } }),
    make({ balance: { ...balance, availableBalance: '0' } }),
    make({ balance: { ...balance, availableBalance: '-5' } }),
    make({ balance: { ...balance, equity: '123456.789', availableBalance: '98765.4321' } }),
    make({ fees: { ...DEFAULT_ACCOUNT_CONFIG, takerFeeRate: '0.01', slippageBps: '50' } }),
    make({ fees: { ...DEFAULT_ACCOUNT_CONFIG, takerFeeRate: '0', slippageBps: '0' } }),
    // Other instruments, including a symbol the agent does not trade.
    make({
      symbol: 'ETHUSDT', decision: dec({ symbol: 'ETHUSDT', stopLoss: 3400, takeProfit: 3700 }),
      quote: quoteFor('ETHUSDT', '3499.9', '3500.1'),
    }),
    make({
      symbol: 'SOLUSDT', decision: dec({ symbol: 'SOLUSDT', action: 'SHORT', stopLoss: 160, takeProfit: 140 }),
      quote: quoteFor('SOLUSDT', '149.97', '150.03'),
    }),
    make({ symbol: 'ETHUSDT' }),
    make({ symbol: 'BTCUSDT', decision: dec({ symbol: 'SOLUSDT' }) }),
    make({ quote: quoteFor('BTCUSDT', '67234.5', '67234.6'), decision: dec({ stopLoss: 66000, takeProfit: 70000 }) }),
    make({ quote: quoteFor('BTCUSDT', '99', '101', 2_500) }),
    make({ quote: quoteFor('BTCUSDT', '99', '101', 29_999), now: 59_999 }),
  ];

  // Seeded sweep: random sides, prices, balances and settings so the 50-digit sizing is compared too.
  const rand = mulberry32(0x72697368);
  const pick = <T>(items: readonly T[]): T => items[Math.floor(rand() * items.length)]!;
  const between = (lo: number, hi: number, dp: number) => Number((lo + rand() * (hi - lo)).toFixed(dp));
  for (let i = 0; i < 200; i++) {
    const symbol = pick(DEFAULT_SYMBOLS);
    const mid = symbol === 'BTCUSDT' ? between(20000, 90000, 2) : symbol === 'ETHUSDT' ? between(1000, 5000, 2) : between(20, 300, 3);
    const spread = mid * 0.0002;
    const bid = (mid - spread).toFixed(4);
    const ask = (mid + spread).toFixed(4);
    const action = pick(['LONG', 'SHORT'] as const);
    const limit = rand() < 0.3;
    const entry = limit ? Number((mid * (1 + (rand() - 0.5) * 0.02)).toFixed(2)) : null;
    const ref = entry ?? Number(action === 'LONG' ? ask : bid);
    const sign = action === 'LONG' ? -1 : 1;
    // Some stops land on the wrong side so the validity check fails too.
    const stopSign = rand() < 0.1 ? -sign : sign;
    const stop = rand() < 0.1 ? null : Number((ref * (1 + stopSign * between(0.002, 0.15, 4))).toFixed(2));
    const tp = rand() < 0.3 ? null : Number((ref * (1 - sign * between(-0.01, 0.2, 4))).toFixed(2));
    const equity = between(50, 500000, 2);
    inputs.push(make({
      symbol,
      decision: dec({
        symbol, action, orderType: limit ? 'LIMIT' : 'MARKET', entryPrice: entry, stopLoss: stop, takeProfit: tp,
        expiresAt: rand() < 0.2 ? new Date(between(0, 3000, 0)).toISOString() : null,
      }),
      settings: settings({
        maxLeverage: pick([1, 2, 3, 3.7, 5, 10, 25, 75, 125, 400]),
        maxRiskPerTradePct: pick([0.1, 0.25, 0.5, 1, 2, 5]),
        maxDailyLossPct: pick([1, 2, 5]),
        maxOpenPositions: pick([1, 2, 3]),
        requireStopLoss: rand() < 0.8,
        maxQuoteAgeSec: pick([5, 30, 60]),
        maxNotional: pick([100, 1000, 25000, 100000, 1e6]),
      }),
      quote: rand() < 0.03 ? null : quoteFor(symbol, bid, ask, 1_000 - Math.floor(rand() * 8) * 1_000),
      now: 1_000 + Math.floor(rand() * 40) * 1_000,
      balance: { ...balance, equity: String(equity), availableBalance: String(Number((equity * between(0.2, 1, 3)).toFixed(2))) },
      openPositions: rand() < 0.15 ? [position(pick(DEFAULT_SYMBOLS), pick(['LONG', 'SHORT'] as const))] : [],
      dailyPnl: String(between(-1500, 800, 2)),
      dayStartEquity: String(equity),
      fees: { ...DEFAULT_ACCOUNT_CONFIG, takerFeeRate: pick(['0.0005', '0.001', '0.0002']), slippageBps: pick(['1', '5', '12.5']) },
    }));
  }

  return inputs.map((input) => ({ input, output: evaluateRisk(input as Parameters<typeof evaluateRisk>[0]) }));
}

/**
 * Golden fixtures: what the TypeScript backend produces, saved so the Rust port can be tested
 * against it. Each key is a file name under crates/golden; a Buffer is written as raw bytes and
 * anything else as JSON.
 */
export async function buildGolden(): Promise<Record<string, unknown>> {
  return {
    'instruments.json': DEFAULT_SYMBOLS.map((s) => INSTRUMENTS[s]),
    'jsnumber.json': JS_NUMBER_CASES.map((x) => [noNegativeZero(x), JSON.stringify(x)]),
    'jsnum.json': {
      round: ROUND_CASES.map((x) => [noNegativeZero(x), noNegativeZero(Math.round(x))]),
      toFixed: FIXED_CASES.map(([x, d]) => [noNegativeZero(x), d, x.toFixed(d)]),
    },
    'accounting-types.json': accountingTypesFixture(),
    'decimal.json': decimalFixture(),
    'core-math.json': coreMathFixture(),
    'risk.json': riskFixture(),
    'tape.json': tapeFixture(),
    'replay.json': await replayFixture(),
    'schema.json': schemaFixture(),
    'store-rows.json': storeRowsFixture(),
    'paper-scenarios.json': paperScenariosFixture(),
    'node-created.sqlite': nodeCreatedDatabase(),
    'vault.json': await vaultFixture(),
  };
}

/** Entries whose bytes legitimately differ per run (random salts, nonces). The freshness test skips them. */
export const RANDOMIZED = new Set<string>(['node-created.sqlite', 'vault.json']);

const GOLDEN_DIR = join(dirname(fileURLToPath(import.meta.url)), '../../../crates/golden');

async function writeGolden(all: boolean): Promise<void> {
  mkdirSync(GOLDEN_DIR, { recursive: true });
  for (const [name, value] of Object.entries(await buildGolden())) {
    // Randomized files differ on every run, so rewriting them only churns git. `--all` forces it.
    if (!all && RANDOMIZED.has(name) && existsSync(join(GOLDEN_DIR, name))) continue;
    const bytes = Buffer.isBuffer(value) ? value : JSON.stringify(value, null, 1) + '\n';
    writeFileSync(join(GOLDEN_DIR, name), bytes);
  }
}

// The freshness test imports this file, so only write when it is run as a script.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await writeGolden(process.argv.includes('--all'));
}
