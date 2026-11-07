import { afterEach } from 'vitest';

const LOOPBACK = new Set(['127.0.0.1', 'localhost', '[::1]']);
const realFetch = globalThis.fetch;
const blocked: string[] = [];

/**
 * Tests must run offline. Provider code turns a fetch failure into an AI_ERROR record, so the
 * throw alone could pass unnoticed: attempts are collected and fail the test afterwards.
 */
globalThis.fetch = (async (input: Parameters<typeof fetch>[0], init?: RequestInit) => {
  const url = new URL(input instanceof Request ? input.url : String(input));
  if (LOOPBACK.has(url.hostname)) return realFetch(input, init);
  blocked.push(url.href);
  throw new Error(`Tests must not use the network: ${url.href}`);
}) as typeof fetch;

afterEach(() => {
  const attempts = blocked.splice(0);
  if (attempts.length) throw new Error(`Test tried to reach the network: ${attempts.join(', ')}`);
});
