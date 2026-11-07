import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { DEFAULT_SYMBOLS, INSTRUMENTS } from '@mochiprep/shared';

const ROUND_CASES = [
  0.5, 1.5, 2.5, -0.5, -1.5, -2.5, 2.4999999999999996, 0.49999999999999994, -0.49999999999999994,
  1e15 + 0.5, 0, 7.000000000000001,
];
const FIXED_CASES: Array<[number, number]> = [
  [0.125, 2], [0.375, 2], [-0.125, 2], [1.005, 2], [2.5, 0], [-2.5, 0], [68000, 1], [1e-7, 6],
  [0.0000005, 6], [123.456, 3], [0.1 + 0.2, 17], [-0, 2], [99.995, 2], [0.0005, 6], [-0.0005, 6],
  [-1e-7, 6], [12345.6789, 0],
];

// JSON cannot carry -0, so the fixture stores 0 and the freshness test (toEqual) must see the same.
const noNegativeZero = (x: number): number => x + 0;

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
  };
}

/** Entries whose bytes legitimately differ per run (random salts, nonces). The freshness test skips them. */
export const RANDOMIZED = new Set<string>();

const GOLDEN_DIR = join(dirname(fileURLToPath(import.meta.url)), '../../../crates/golden');

async function writeGolden(): Promise<void> {
  mkdirSync(GOLDEN_DIR, { recursive: true });
  for (const [name, value] of Object.entries(await buildGolden())) {
    const bytes = Buffer.isBuffer(value) ? value : JSON.stringify(value, null, 1) + '\n';
    writeFileSync(join(GOLDEN_DIR, name), bytes);
  }
}

// The freshness test imports this file, so only write when it is run as a script.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await writeGolden();
}
