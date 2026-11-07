import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { generateTape, ReplayMarketData } from '@mochiprep/market-data';
import { DEFAULT_SYMBOLS, INSTRUMENTS, TIMEFRAMES, type Candle } from '@mochiprep/shared';
import { openDatabase } from '../src/db/database.js';
import { newDataKey, seal, wrapWithPassword } from '../src/vault.js';

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

/**
 * Golden fixtures: what the TypeScript backend produces, saved so the Rust port can be tested
 * against it. Each key is a file name under crates/golden; a Buffer is written as raw bytes and
 * anything else as JSON.
 */
export async function buildGolden(): Promise<Record<string, unknown>> {
  return {
    'instruments.json': DEFAULT_SYMBOLS.map((s) => INSTRUMENTS[s]),
    'jsnum.json': {
      round: ROUND_CASES.map((x) => [noNegativeZero(x), noNegativeZero(Math.round(x))]),
      toFixed: FIXED_CASES.map(([x, d]) => [noNegativeZero(x), d, x.toFixed(d)]),
    },
    'tape.json': tapeFixture(),
    'replay.json': await replayFixture(),
    'schema.json': schemaFixture(),
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
