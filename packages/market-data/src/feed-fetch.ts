import { lookup as dnsLookup, type LookupAddress } from 'node:dns';
import type { ClientRequest, IncomingMessage, RequestOptions } from 'node:http';
import { request as httpsRequest } from 'node:https';
import type { LookupFunction } from 'node:net';
import { isPublicAddress } from './public-address';

/** What `validateFeedUrl` says of a private host; a resolve-time refusal reads the same. */
export const PUBLIC_HOST_PROBLEM = 'Feeds must be on a public host';

/**
 * Every decoded UTF-16 unit takes at most 3 bytes of UTF-8, plus a 3-byte BOM, so a body longer
 * than this is certainly over the 3 000 000-unit feed cap and need not be read to the end. The
 * Rust port stops at the same byte count.
 */
const MAX_BODY_BYTES = 3 * 3_000_000 + 6;
const NULL_BODY_STATUSES = new Set([101, 103, 204, 205, 304]);

export type LookupAll = (hostname: string) => Promise<LookupAddress[]>;
export type FeedRequest = (url: URL, options: RequestOptions) => ClientRequest;

class NonPublicAddressError extends Error {}

const systemLookupAll: LookupAll = (hostname) =>
  new Promise((resolve, reject) => {
    dnsLookup(hostname, { all: true }, (error, addresses) => (error ? reject(error) : resolve(addresses)));
  });

/**
 * A `lookup` for net.connect that refuses the whole name if any address it resolves to is not
 * public, and otherwise hands back only addresses it checked. The socket connects to exactly
 * what this returns, with no second lookup, so a DNS answer that changes between the check and
 * the connection (rebinding) cannot reach a private address.
 */
function publicOnlyLookup(lookupAll: LookupAll): LookupFunction {
  return (hostname, options, callback) => {
    lookupAll(hostname).then(
      (found) => {
        const addresses = options.family === 4 || options.family === 6 ? found.filter((a) => a.family === options.family) : found;
        if (found.some((a) => !isPublicAddress(a.address))) {
          callback(new NonPublicAddressError(PUBLIC_HOST_PROBLEM), '', 0);
        } else if (addresses.length === 0) {
          callback(Object.assign(new Error(`getaddrinfo ENOTFOUND ${hostname}`), { code: 'ENOTFOUND' }), '', 0);
        } else if (options.all) {
          callback(null, addresses);
        } else {
          callback(null, addresses[0]!.address, addresses[0]!.family);
        }
      },
      (error: NodeJS.ErrnoException) => callback(error, '', 0),
    );
  };
}

function responseFrom(message: IncomingMessage, body: Buffer | null): Response {
  const headers = new Headers();
  for (const [name, value] of Object.entries(message.headers)) {
    for (const one of Array.isArray(value) ? value : value === undefined ? [] : [value]) headers.append(name, one);
  }
  const status = message.statusCode ?? 0;
  return new Response(NULL_BODY_STATUSES.has(status) ? null : body, { status, headers });
}

/**
 * The news fetcher's `fetch`: one https GET over `node:https` whose connection may only reach
 * a public address (see `publicOnlyLookup`). An IP-literal host never goes through the lookup;
 * `validateFeedUrl`, which runs on every hop before this, refuses every non-public literal.
 * Redirects are handed back, never followed, because the caller checks each hop. A refused
 * address rejects with `Feeds must be on a public host: <url>`; other failures with the
 * `TypeError('fetch failed')` the global fetch gives, and an abort with the signal's reason.
 */
export function createFeedFetch(deps: { lookupAll?: LookupAll; request?: FeedRequest } = {}): typeof fetch {
  const lookup = publicOnlyLookup(deps.lookupAll ?? systemLookupAll);
  const request = deps.request ?? ((url: URL, options: RequestOptions) => httpsRequest(url, options));
  const feedFetch = (input: string | URL | Request, init: RequestInit = {}): Promise<Response> =>
    new Promise((resolve, reject) => {
      const text = input instanceof Request ? input.url : String(input);
      const signal = init.signal ?? undefined;
      if (signal?.aborted) {
        reject(signal.reason);
        return;
      }
      let url: URL;
      try {
        url = new URL(text);
      } catch (cause) {
        reject(new TypeError('fetch failed', { cause }));
        return;
      }
      let req: ClientRequest | null = null;
      const fail = (error: unknown) => {
        signal?.removeEventListener('abort', onAbort);
        req?.destroy();
        reject(error);
      };
      const onAbort = () => fail(signal!.reason);
      signal?.addEventListener('abort', onAbort, { once: true });
      req = request(url, {
        method: 'GET',
        headers: Object.fromEntries(new Headers(init.headers).entries()),
        lookup,
        // A fresh connection for every request, so one checked for another name is never reused.
        agent: false,
      });
      req.on('error', (error) => fail(error instanceof NonPublicAddressError ? new Error(`${PUBLIC_HOST_PROBLEM}: ${text}`) : new TypeError('fetch failed', { cause: error })));
      req.on('response', (message: IncomingMessage) => {
        const status = message.statusCode ?? 0;
        if (status < 200 || status > 599) {
          fail(new TypeError('fetch failed'));
          return;
        }
        // Only a 2xx body is ever read; a redirect or error status is judged by its status.
        if (status >= 300) {
          message.resume();
          signal?.removeEventListener('abort', onAbort);
          resolve(responseFrom(message, null));
          return;
        }
        const chunks: Buffer[] = [];
        let size = 0;
        message.on('data', (chunk: Buffer) => {
          size += chunk.length;
          if (size > MAX_BODY_BYTES) {
            message.destroy();
            fail(new Error('feed too large'));
            return;
          }
          chunks.push(chunk);
        });
        message.on('error', (error) => fail(new TypeError('fetch failed', { cause: error })));
        message.on('end', () => {
          if (size > MAX_BODY_BYTES) return;
          signal?.removeEventListener('abort', onAbort);
          try {
            resolve(responseFrom(message, Buffer.concat(chunks)));
          } catch (cause) {
            reject(new TypeError('fetch failed', { cause }));
          }
        });
      });
      req.end();
    });
  return feedFetch as typeof fetch;
}

/** The default `NewsService` fetch, over the system resolver. */
export const feedFetch = createFeedFetch();
