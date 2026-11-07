import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { RANDOMIZED, buildGolden } from '../golden/generate.js';

const GOLDEN_DIR = join(dirname(fileURLToPath(import.meta.url)), '../../../crates/golden');

describe('golden fixtures', () => {
  it('are up to date with the TypeScript backend', async () => {
    for (const [name, fresh] of Object.entries(await buildGolden())) {
      if (RANDOMIZED.has(name)) continue;
      const committed = readFileSync(join(GOLDEN_DIR, name));
      const message = `crates/golden/${name} is stale: run pnpm golden`;
      if (Buffer.isBuffer(fresh)) {
        expect(committed.equals(fresh), message).toBe(true);
      } else {
        expect(JSON.parse(committed.toString('utf8')), message).toEqual(fresh);
      }
    }
  });
});
