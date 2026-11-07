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
  const json = (await response.json().catch(() => ({}))) as T & { error?: string };
  if (!response.ok) throw new Error(json.error ?? `Request failed (${response.status})`);
  return json;
}

export const api = {
  get: <T>(path: string) => request<T>('GET', path),
  post: <T>(path: string, body?: unknown) => request<T>('POST', path, body),
  put: <T>(path: string, body?: unknown) => request<T>('PUT', path, body),
  del: <T>(path: string) => request<T>('DELETE', path),
};
