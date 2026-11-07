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

describe('OpenCode provider', () => {
  const opencode: ProviderConfig = { ...config, id: 'o', name: 'OpenCode', kind: 'OPENCODE', defaultModel: 'deepseek-v4.1-flash' };

  it('calls the Chat Completions endpoint without JSON mode', async () => {
    const fetchImpl = reply('{"ok":true}');
    await createRemoteProvider(opencode, 'key', fetchImpl as unknown as typeof fetch).complete(request);
    const [url, init] = fetchImpl.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe('https://opencode.ai/inference/openai/v1/chat/completions');
    expect((init.headers as Record<string, string>).authorization).toBe('Bearer key');
    expect(JSON.parse(init.body as string).response_format).toBeUndefined();
  });

  it('lets a custom base URL replace the default', async () => {
    const fetchImpl = reply('{"ok":true}');
    await createRemoteProvider({ ...opencode, baseUrl: 'https://gateway.test/v1/' }, 'key', fetchImpl as unknown as typeof fetch).complete(request);
    expect((fetchImpl.mock.calls[0] as unknown as [string])[0]).toBe('https://gateway.test/v1/chat/completions');
  });
});

describe('extra headers', () => {
  const kinds: [ProviderConfig['kind'], string][] = [['OPENAI_COMPATIBLE', 'https://x.test/v1'], ['ANTHROPIC', 'https://x.test'], ['GEMINI', 'https://x.test']];

  it.each(kinds)('reach the provider on %s', async (kind, baseUrl) => {
    const body = kind === 'ANTHROPIC' ? { content: [{ type: 'text', text: 'hi' }] } : kind === 'GEMINI' ? { candidates: [{ content: { parts: [{ text: 'hi' }] } }] } : { choices: [{ message: { content: 'hi' } }] };
    const fetchImpl = vi.fn(async () => new Response(JSON.stringify(body)));
    const provider = createRemoteProvider({ ...config, kind, baseUrl, extraHeaders: { 'x-title': 'mochi' } }, 'key', fetchImpl as unknown as typeof fetch);
    await provider.complete(request);
    expect(((fetchImpl.mock.calls[0] as unknown as [string, RequestInit])[1].headers as Record<string, string>)['x-title']).toBe('mochi');
  });

  it('are redacted from provider errors', async () => {
    const fetchImpl = vi.fn(async () => new Response('rejected header gateway-secret-value', { status: 500 }));
    const provider = createRemoteProvider({ ...config, extraHeaders: { 'x-org': 'gateway-secret-value' } }, 'key', fetchImpl as unknown as typeof fetch);
    await expect(provider.complete(request)).rejects.toThrow(/\[REDACTED\]/);
    await expect(provider.complete(request)).rejects.not.toThrow(/gateway-secret-value/);
  });
});
