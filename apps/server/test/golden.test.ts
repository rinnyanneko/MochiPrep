import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { RANDOMIZED, buildGolden } from '../golden/generate.js';
import { open, unwrapWithPassword, type PasswordWrap } from '../src/vault.js';

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

  it('Node opens what Rust sealed', async () => {
    const rustVault = JSON.parse(readFileSync(join(GOLDEN_DIR, 'rust-vault.json'), 'utf8')) as {
      password: string;
      passwordNfc: string;
      dataKeyHex: string;
      wrap: PasswordWrap;
      sealed: Array<{ context: string; plaintext: string; sealed: string }>;
    };
    expect(rustVault.password).not.toBe(rustVault.passwordNfc);
    // Node normalizes the decomposed form itself, so both spellings must unwrap.
    const key = await unwrapWithPassword(rustVault.wrap, rustVault.passwordNfc);
    expect(key.toString('hex')).toBe(rustVault.dataKeyHex);
    const keyFromNfd = await unwrapWithPassword(rustVault.wrap, rustVault.password);
    expect(keyFromNfd.toString('hex')).toBe(rustVault.dataKeyHex);
    for (const entry of rustVault.sealed) {
      expect(open(key, entry.sealed, entry.context)).toBe(entry.plaintext);
    }
  });
});
