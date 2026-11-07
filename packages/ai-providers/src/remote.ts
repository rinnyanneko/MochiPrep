import type { ProviderConfig } from '@mochiprep/shared';
import { ProviderError, type AiProvider, type AiRequest, type AiResponse } from './types';
import { redactSecrets } from './redact';

const TIMEOUT_MS = 60_000;

async function post(
  url: string,
  headers: Record<string, string>,
  body: unknown,
  request: AiRequest,
  apiKey: string,
  fetchImpl: typeof fetch,
): Promise<any> {
  const timeout = AbortSignal.timeout(TIMEOUT_MS);
  const signal = request.signal ? AbortSignal.any([request.signal, timeout]) : timeout;
  let response: Response;
  try {
    response = await fetchImpl(url, {
      method: 'POST',
      headers: { 'content-type': 'application/json', ...headers },
      body: JSON.stringify(body),
      signal,
    });
  } catch (error) {
    if (signal.aborted) throw new ProviderError('TIMEOUT', 'The model did not answer in time');
    throw new ProviderError('NETWORK', redactSecrets(`Could not reach the provider: ${(error as Error).message}`, [apiKey]));
  }
  if (!response.ok) {
    const text = redactSecrets((await response.text().catch(() => '')).slice(0, 400), [apiKey]);
    const code = response.status === 401 || response.status === 403 ? 'AUTH' : response.status === 429 ? 'RATE_LIMIT' : 'HTTP';
    throw new ProviderError(code, `Provider returned HTTP ${response.status}: ${text}`);
  }
  try {
    return await response.json();
  } catch {
    throw new ProviderError('BAD_RESPONSE', 'Provider returned a body that is not JSON');
  }
}

const trimBase = (url: string) => url.replace(/\/+$/, '');

export function createRemoteProvider(config: ProviderConfig, apiKey: string, fetchImpl: typeof fetch = fetch): AiProvider {
  switch (config.kind) {
    case 'OPENAI':
    case 'OPENAI_COMPATIBLE': {
      const base = trimBase(config.baseUrl ?? 'https://api.openai.com/v1');
      return {
        kind: config.kind,
        async complete(request): Promise<AiResponse> {
          const json = await post(
            `${base}/chat/completions`,
            { authorization: `Bearer ${apiKey}` },
            {
              model: request.model,
              messages: [
                { role: 'system', content: request.system },
                { role: 'user', content: request.user },
              ],
              // Not every compatible server implements this, and the prompt demands JSON anyway.
              ...(config.kind === 'OPENAI' ? { response_format: { type: 'json_object' } } : {}),
            },
            request,
            apiKey,
            fetchImpl,
          );
          const text = json?.choices?.[0]?.message?.content;
          if (typeof text !== 'string') throw new ProviderError('BAD_RESPONSE', 'No message content in the response');
          return { text, inputTokens: json.usage?.prompt_tokens ?? null, outputTokens: json.usage?.completion_tokens ?? null };
        },
      };
    }
    case 'ANTHROPIC': {
      const base = trimBase(config.baseUrl ?? 'https://api.anthropic.com');
      return {
        kind: 'ANTHROPIC',
        async complete(request): Promise<AiResponse> {
          const json = await post(
            `${base}/v1/messages`,
            { 'x-api-key': apiKey, 'anthropic-version': '2023-06-01' },
            {
              model: request.model,
              max_tokens: 2048,
              system: request.system,
              messages: [{ role: 'user', content: request.user }],
            },
            request,
            apiKey,
            fetchImpl,
          );
          const text = (json?.content ?? [])
            .filter((block: { type: string }) => block.type === 'text')
            .map((block: { text: string }) => block.text)
            .join('');
          if (!text) throw new ProviderError('BAD_RESPONSE', 'No text block in the response');
          return { text, inputTokens: json.usage?.input_tokens ?? null, outputTokens: json.usage?.output_tokens ?? null };
        },
      };
    }
    case 'GEMINI': {
      const base = trimBase(config.baseUrl ?? 'https://generativelanguage.googleapis.com');
      return {
        kind: 'GEMINI',
        async complete(request): Promise<AiResponse> {
          const json = await post(
            `${base}/v1beta/models/${encodeURIComponent(request.model)}:generateContent`,
            { 'x-goog-api-key': apiKey },
            {
              systemInstruction: { parts: [{ text: request.system }] },
              contents: [{ role: 'user', parts: [{ text: request.user }] }],
              generationConfig: { responseMimeType: 'application/json' },
            },
            request,
            apiKey,
            fetchImpl,
          );
          const text = (json?.candidates?.[0]?.content?.parts ?? [])
            .map((part: { text?: string }) => part.text ?? '')
            .join('');
          if (!text) throw new ProviderError('BAD_RESPONSE', 'No candidate text in the response');
          return {
            text,
            inputTokens: json.usageMetadata?.promptTokenCount ?? null,
            outputTokens: json.usageMetadata?.candidatesTokenCount ?? null,
          };
        },
      };
    }
    case 'MOCK':
      throw new Error('Use mockProvider for MOCK');
  }
}
