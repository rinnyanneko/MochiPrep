import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import type { DatabaseSync } from 'node:sqlite';
import { ProviderInputSchema } from '@mochiprep/shared';
import { buildApp } from '../src/app';
import { openDatabase } from '../src/db/database';
import { Runtime } from '../src/runtime';

const input = (extraHeaders?: Record<string, string>) => ({
  name: 'Gateway', kind: 'OPENAI_COMPATIBLE', baseUrl: 'https://gateway.test/v1', defaultModel: 'm',
  inputPricePerMTok: null, outputPricePerMTok: null, ...(extraHeaders ? { extraHeaders } : {}),
});

describe('ProviderInputSchema headers', () => {
  it('stores names in lower case, so a casing variant cannot duplicate a header', () => {
    expect(ProviderInputSchema.parse(input({ 'X-Title': 'mochi' })).extraHeaders).toEqual({ 'x-title': 'mochi' });
  });

  it.each(['Authorization', 'x-api-key', 'X-Goog-Api-Key', 'content-type', 'Host', 'Content-Length', 'anthropic-version'])('refuses to override %s', (name) => {
    expect(ProviderInputSchema.safeParse(input({ [name]: 'x' })).success).toBe(false);
  });

  it('refuses header injection and bad names', () => {
    expect(ProviderInputSchema.safeParse(input({ 'x-a': 'ok\r\nevil: 1' })).success).toBe(false);
    expect(ProviderInputSchema.safeParse(input({ 'bad name': 'x' })).success).toBe(false);
  });
});

describe('provider header API', () => {
  let db: DatabaseSync;
  let runtime: Runtime;
  let app: ReturnType<typeof buildApp>['app'];
  let token: string;
  const SECRET = 'gateway-secret-value-123';

  const call = (method: 'GET' | 'POST' | 'PUT', url: string, payload?: object) =>
    app.inject({ method, url, headers: { host: 'localhost:8787', authorization: `Bearer ${token}` }, ...(payload ? { payload } : {}) });

  beforeEach(() => {
    db = openDatabase(':memory:');
    runtime = new Runtime({ db, replayMinutes: 5_800 });
    ({ app, token } = buildApp(runtime, { host: '127.0.0.1', port: 8787, dataDir: '.', allowedOrigins: ['http://localhost:8787'] }));
  });

  afterEach(async () => {
    await app.close();
    await runtime.stop();
    db.close();
  });

  it('never returns header values, only their names', async () => {
    const created = await call('POST', '/api/providers', input({ 'X-Org': SECRET }));
    expect(created.body).not.toContain(SECRET);
    expect(created.json().headerNames).toEqual(['x-org']);
    const updated = await call('PUT', `/api/providers/${created.json().id}`, input({ 'X-Org': SECRET }));
    expect(updated.body).not.toContain(SECRET);
    expect((await call('GET', '/api/providers')).body).not.toContain(SECRET);
  });

  it('keeps saved headers when an edit omits them, and replaces them when given', async () => {
    const { id } = (await call('POST', '/api/providers', input({ 'X-Org': SECRET }))).json();
    await call('PUT', `/api/providers/${id}`, input());
    expect(runtime.store.getProvider(id)!.extraHeaders).toEqual({ 'x-org': SECRET });
    await call('PUT', `/api/providers/${id}`, input({ 'X-Other': 'b' }));
    expect(runtime.store.getProvider(id)!.extraHeaders).toEqual({ 'x-other': 'b' });
  });

  it('redacts saved header values from messages', async () => {
    await call('POST', '/api/providers', input({ 'X-Org': SECRET }));
    expect(runtime.redact(`failed with ${SECRET}`)).not.toContain(SECRET);
  });

  it('accepts the OpenCode provider type', async () => {
    const response = await call('POST', '/api/providers', { ...input(), kind: 'OPENCODE', baseUrl: null });
    expect(response.json().kind).toBe('OPENCODE');
  });
});
