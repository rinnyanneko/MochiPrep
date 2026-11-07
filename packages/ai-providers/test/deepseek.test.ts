import { describe, expect, it, vi } from 'vitest';
import type { ProviderConfig } from '@mochiprep/shared';
import { createRemoteProvider, ProviderError, type AiRequest } from '../src/index';

const config: ProviderConfig = {
  id: 'd', name: 'DeepSeek', kind: 'DEEPSEEK', baseUrl: null, defaultModel: 'deepseek-flash',
  inputPricePerMTok: null, outputPricePerMTok: null, createdAt: 1,
};
const request = { model: 'deepseek-flash', system: 'json only', user: 'go', context: undefined as never } satisfies AiRequest;
const reply = (content: string) =>
  vi.fn(async () => new Response(JSON.stringify({ choices: [{ message: { content } }], usage: { prompt_tokens: 7, completion_tokens: 3 } })));

describe('DeepSeek provider', () => {
  it('calls the official endpoint in JSON mode and reports usage', async () => {
    const fetchImpl = reply('{"ok":true}');
    const response = await createRemoteProvider(config, 'key', fetchImpl as unknown as typeof fetch).complete(request);
    const [url, init] = fetchImpl.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe('https://api.deepseek.com/chat/completions');
    expect(JSON.parse(init.body as string)).toMatchObject({ model: 'deepseek-flash', response_format: { type: 'json_object' } });
    expect((init.headers as Record<string, string>).authorization).toBe('Bearer key');
    expect(response).toEqual({ text: '{"ok":true}', inputTokens: 7, outputTokens: 3 });
  });

  it('treats the blank content its JSON mode can return as a failed response', async () => {
    const provider = createRemoteProvider(config, 'key', reply('   \n') as unknown as typeof fetch);
    await expect(provider.complete(request)).rejects.toBeInstanceOf(ProviderError);
  });
});
