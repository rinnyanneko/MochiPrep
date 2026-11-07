import { createCipheriv, createDecipheriv, randomBytes, scrypt } from 'node:crypto';

/**
 * Envelope encryption for secrets at rest. One random data key seals every secret; the master
 * password only wraps that data key. Changing the password rewraps 32 bytes instead of every
 * row, and another unlock method can later wrap the same data key without re-encrypting.
 */

export interface KdfParams {
  N: number;
  r: number;
  p: number;
}

/** About 128 MiB and a few hundred ms per guess: slow enough to make offline guessing expensive. */
export const DEFAULT_KDF: KdfParams = { N: 2 ** 17, r: 8, p: 1 };

export interface PasswordWrap {
  salt: string;
  kdf: KdfParams;
  wrappedKey: string;
}

export class WrongPasswordError extends Error {
  constructor() {
    super('Wrong master password');
  }
}

const VERSION = 'v1';
const WRAP_CONTEXT = 'mochiprep:vault-key';

function derive(password: string, salt: Buffer, kdf: KdfParams): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    // Async scrypt runs on the libuv pool, so a slow unlock does not freeze market feeds.
    scrypt(password.normalize('NFC'), salt, 32, { ...kdf, maxmem: 256 * kdf.N * kdf.r }, (error, key) => (error ? reject(error) : resolve(key)));
  });
}

/**
 * AES-256-GCM. `context` is authenticated but not stored: a ciphertext copied into another row
 * (say, one provider's key onto another provider) fails to open instead of being misused.
 */
export function seal(key: Buffer, plaintext: string, context: string): string {
  const iv = randomBytes(12);
  const cipher = createCipheriv('aes-256-gcm', key, iv);
  cipher.setAAD(Buffer.from(context));
  const body = Buffer.concat([cipher.update(plaintext, 'utf8'), cipher.final()]);
  return [VERSION, iv.toString('base64'), cipher.getAuthTag().toString('base64'), body.toString('base64')].join('.');
}

export function open(key: Buffer, sealed: string, context: string): string {
  const [version, iv, tag, body] = sealed.split('.');
  if (version !== VERSION || !iv || !tag || body === undefined) throw new Error('Unrecognized sealed value');
  const decipher = createDecipheriv('aes-256-gcm', key, Buffer.from(iv, 'base64'));
  decipher.setAAD(Buffer.from(context));
  decipher.setAuthTag(Buffer.from(tag, 'base64'));
  return Buffer.concat([decipher.update(Buffer.from(body, 'base64')), decipher.final()]).toString('utf8');
}

export async function wrapWithPassword(dataKey: Buffer, password: string, kdf: KdfParams = DEFAULT_KDF): Promise<PasswordWrap> {
  const salt = randomBytes(16);
  const kek = await derive(password, salt, kdf);
  return { salt: salt.toString('base64'), kdf, wrappedKey: seal(kek, dataKey.toString('base64'), WRAP_CONTEXT) };
}

/** The GCM tag doubles as the password check, so nothing about the password is stored. */
export async function unwrapWithPassword(wrap: PasswordWrap, password: string): Promise<Buffer> {
  const kek = await derive(password, Buffer.from(wrap.salt, 'base64'), wrap.kdf);
  try {
    return Buffer.from(open(kek, wrap.wrappedKey, WRAP_CONTEXT), 'base64');
  } catch {
    throw new WrongPasswordError();
  }
}

export function newDataKey(): Buffer {
  return randomBytes(32);
}
