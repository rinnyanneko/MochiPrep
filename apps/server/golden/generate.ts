import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { DEFAULT_SYMBOLS, INSTRUMENTS } from '@mochiprep/shared';

/**
 * Golden fixtures: what the TypeScript backend produces, saved so the Rust port can be tested
 * against it. Each key is a file name under crates/golden; a Buffer is written as raw bytes and
 * anything else as JSON.
 */
export async function buildGolden(): Promise<Record<string, unknown>> {
  return {
    'instruments.json': DEFAULT_SYMBOLS.map((s) => INSTRUMENTS[s]),
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
