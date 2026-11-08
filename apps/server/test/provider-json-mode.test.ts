import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import type { DatabaseSync } from 'node:sqlite';
import { createRemoteProvider } from '@mochiprep/ai-providers';
import { buildSystemPrompt } from '@mochiprep/agents';
import type { ProviderKind } from '@mochiprep/shared';
import { buildApp } from '../src/app';
import { openDatabase } from '../src/db/database';
import { Runtime } from '../src/runtime';

/** Applies the real rule from OpenAI and DeepSeek: JSON mode needs the word json in the messages. */
const strictJsonMode = (async (_url: string, init: RequestInit) => {
  const body = JSON.parse(init.body as string);
  const text = JSON.stringify(body.messages).toLowerCase();
  if (body.response_format?.type === 'json_object' && !text.includes('json')) {
    return new Response(JSON.stringify({ error: { message: "'messages' must contain the word 'json' in some form" } }), { status: 400 });
  }
  return new Response(JSON.stringify({ choices: [{ message: { content: '{"ok":true}' } }] }));
}) as unknown as typeof fetch;

describe('JSON mode prompts', () => {
  let db: DatabaseSync;
  let runtime: Runtime;
  let app: ReturnType<typeof buildApp>['app'];
  let token: string;

  beforeEach(() => {
    db = openDatabase(':memory:');
    runtime = new Runtime({
      db,
      replayMinutes: 5_800,
      providerFactory: (config, key) => createRemoteProvider(config, key, strictJsonMode),
    });
    ({ app, token } = buildApp(runtime, { host: '127.0.0.1', port: 8787, dataDir: '.', allowedOrigins: ['http://localhost:8787'] }));
  });

  afterEach(async () => {
    await app.close();
    await runtime.stop();
    db.close();
  });

  it.each<ProviderKind>(['OPENAI', 'DEEPSEEK'])('passes the Settings connection test on %s', async (kind) => {
    runtime.store.insertProvider({ id: 'p', name: 'p', kind, baseUrl: null, defaultModel: 'm', inputPricePerMTok: null, outputPricePerMTok: null, createdAt: 1 });
    runtime.setKey('p', 'test-key');
    const response = await app.inject({
      method: 'POST',
      url: '/api/providers/p/test',
      headers: { host: 'localhost:8787', authorization: `Bearer ${token}` },
      payload: {},
    });
    expect(response.json()).toMatchObject({ ok: true });
  });

  it('keeps the word json in the agent prompt', () => {
    expect(buildSystemPrompt({ allowTrailingStop: false }).toLowerCase()).toContain('json');
  });
});
