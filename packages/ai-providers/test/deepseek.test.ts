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
    const provider = createRemoteProvider({ ...config, extraHeaders: { 'x-org': 'gateway-secret-value' } }, 'key', fetchImpl as unknown as typeof fetch, [0]);
    await expect(provider.complete(request)).rejects.toThrow(/\[REDACTED\]/);
    await expect(provider.complete(request)).rejects.not.toThrow(/gateway-secret-value/);
  });
});

describe('retries', () => {
  const ok = () => new Response(JSON.stringify({ choices: [{ message: { content: '{"ok":true}' } }] }));
  const status = (code: number) => new Response('{"error":"busy"}', { status: code });
  const provider = (fetchImpl: unknown) => createRemoteProvider(config, 'key', fetchImpl as typeof fetch, [0, 0]);

  it.each([500, 502, 503, 429])('recovers from a passing HTTP %i', async (code) => {
    const fetchImpl = vi.fn<() => Promise<Response>>().mockResolvedValueOnce(status(code)).mockResolvedValueOnce(ok());
    expect((await provider(fetchImpl).complete(request)).text).toBe('{"ok":true}');
    expect(fetchImpl).toHaveBeenCalledTimes(2);
  });

  it('recovers from a dropped connection', async () => {
    const fetchImpl = vi.fn<() => Promise<Response>>().mockRejectedValueOnce(new TypeError('socket hang up')).mockResolvedValueOnce(ok());
    await expect(provider(fetchImpl).complete(request)).resolves.toBeDefined();
  });

  it('gives up after the last delay and says how often it tried', async () => {
    const fetchImpl = vi.fn(async () => status(500));
    await expect(provider(fetchImpl).complete(request)).rejects.toThrow('after 3 attempts');
    expect(fetchImpl).toHaveBeenCalledTimes(3);
  });

  it.each([[400, 'HTTP'], [401, 'AUTH'], [403, 'AUTH'], [404, 'HTTP']])('does not retry HTTP %i, which would fail the same way', async (code, errorCode) => {
    const fetchImpl = vi.fn(async () => status(code as number));
    await expect(provider(fetchImpl).complete(request)).rejects.toMatchObject({ code: errorCode });
    expect(fetchImpl).toHaveBeenCalledTimes(1);
  });

  it('does not retry a reply it could not read', async () => {
    const fetchImpl = vi.fn(async () => new Response('not json'));
    await expect(provider(fetchImpl).complete(request)).rejects.toMatchObject({ code: 'BAD_RESPONSE' });
    expect(fetchImpl).toHaveBeenCalledTimes(1);
  });

  it('keeps an auth failure critical after a retried server error', async () => {
    const fetchImpl = vi.fn<() => Promise<Response>>().mockResolvedValueOnce(status(500)).mockResolvedValueOnce(status(401));
    const error = await provider(fetchImpl).complete(request).catch((e) => e as ProviderError);
    expect((error as ProviderError).critical).toBe(true);
  });

  it('retries on every provider type', async () => {
    const body = { content: [{ type: 'text', text: 'hi' }] };
    const fetchImpl = vi.fn<() => Promise<Response>>().mockResolvedValueOnce(status(500)).mockResolvedValueOnce(new Response(JSON.stringify(body)));
    await createRemoteProvider({ ...config, kind: 'ANTHROPIC' }, 'key', fetchImpl as unknown as typeof fetch, [0]).complete(request);
    expect(fetchImpl).toHaveBeenCalledTimes(2);
  });
});
