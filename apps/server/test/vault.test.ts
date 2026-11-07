import { describe, expect, it } from 'vitest';
import { newDataKey, open, seal, unwrapWithPassword, wrapWithPassword, WrongPasswordError } from '../src/vault';

const FAST = { N: 2 ** 10, r: 8, p: 1 };

describe('vault crypto', () => {
  it('round-trips a secret and hides the plaintext', () => {
    const key = newDataKey();
    const sealed = seal(key, 'sk-secret-value', 'provider:a');
    expect(sealed).not.toContain('sk-secret-value');
    expect(open(key, sealed, 'provider:a')).toBe('sk-secret-value');
  });

  it('uses a fresh IV, so equal secrets do not look equal', () => {
    const key = newDataKey();
    expect(seal(key, 'same', 'c')).not.toBe(seal(key, 'same', 'c'));
  });

  it('refuses a ciphertext moved to another row, a wrong key, or tampering', () => {
    const key = newDataKey();
    const sealed = seal(key, 'secret', 'provider:a');
    expect(() => open(key, sealed, 'provider:b')).toThrow();
    expect(() => open(newDataKey(), sealed, 'provider:a')).toThrow();
    const parts = sealed.split('.');
    parts[3] = Buffer.from('tampered').toString('base64');
    expect(() => open(key, parts.join('.'), 'provider:a')).toThrow();
  });

  it('unwraps the data key only with the right password', async () => {
    const key = newDataKey();
    const wrap = await wrapWithPassword(key, 'correct horse battery', FAST);
    expect(JSON.stringify(wrap)).not.toContain('correct horse');
    expect((await unwrapWithPassword(wrap, 'correct horse battery')).equals(key)).toBe(true);
    await expect(unwrapWithPassword(wrap, 'wrong password')).rejects.toBeInstanceOf(WrongPasswordError);
  });

  it('salts each wrap, so the same password gives different records', async () => {
    const key = newDataKey();
    const a = await wrapWithPassword(key, 'pw-1234567890', FAST);
    const b = await wrapWithPassword(key, 'pw-1234567890', FAST);
    expect(a.salt).not.toBe(b.salt);
    expect(a.wrappedKey).not.toBe(b.wrappedKey);
  });
});
