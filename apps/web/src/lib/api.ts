/** A failed API answer. `code` is set only for failures the UI must tell apart. */
export class ApiRequestError extends Error {
  constructor(message: string, readonly status: number, readonly code?: string) {
    super(message);
    this.name = 'ApiRequestError';
  }
}

/** The server refused an order before anything reached OKX. */
export const isNotDispatched = (error: unknown): boolean =>
  error instanceof ApiRequestError && error.status === 409 && error.code === 'NOT_DISPATCHED';

let token: string | null = null;

async function session(): Promise<string> {
  const response = await fetch('/api/session');
  if (!response.ok) throw new Error('Backend is not reachable');
  token = ((await response.json()) as { token: string }).token;
  return token;
}

export async function getToken(): Promise<string> {
  return token ?? session();
}

/** The token changes on every backend restart, so a dropped stream must fetch a new one. */
export function forgetToken() {
  token = null;
}

async function request<T>(method: string, path: string, body?: unknown, retried = false): Promise<T> {
  const auth = await getToken();
  const response = await fetch(`/api${path}`, {
    method,
    headers: {
      authorization: `Bearer ${auth}`,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  if (response.status === 401 && !retried) {
    token = null;
    return request(method, path, body, true);
  }
  const json = (await response.json().catch(() => ({}))) as T & { error?: string; code?: unknown };
  if (!response.ok) {
    throw new ApiRequestError(json.error ?? `Request failed (${response.status})`, response.status, typeof json.code === 'string' ? json.code : undefined);
  }
  return json;
}

export const api = {
  get: <T>(path: string) => request<T>('GET', path),
  post: <T>(path: string, body?: unknown) => request<T>('POST', path, body),
  put: <T>(path: string, body?: unknown) => request<T>('PUT', path, body),
  del: <T>(path: string) => request<T>('DELETE', path),
};
