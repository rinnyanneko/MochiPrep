import { createHash, createHmac } from 'node:crypto';
import type { AccountVenue, SymbolId } from '@mochiprep/shared';

const REST: Record<Exclude<AccountVenue, 'PAPER'>, string> = {
  OKX_LIVE: 'https://www.okx.com',
  OKX_DEMO: 'https://openapi.okx.com',
};
const TIMEOUT_MS = 8_000;
/** Community-documented meaning: order does not exist. Anything else is treated as unknown. */
const ORDER_NOT_FOUND = '51603';

export interface OkxCredentials {
  apiKey: string;
  secretKey: string;
  passphrase: string;
}

/** OKX answered and refused. Safe to treat as "did not happen". */
export class OkxRejection extends Error {
  constructor(readonly code: string, message: string) {
    super(`OKX ${code}: ${message}`);
  }
}

/** No usable answer. The request may or may not have taken effect, so never blindly retry a write. */
export class OkxUnknownOutcome extends Error {}

export const toInstId = (symbol: SymbolId) => `${symbol.replace(/USDT$/, '')}-USDT-SWAP`;

/** clOrdId allows only 32 alphanumerics. Hashing keeps it deterministic for any local key. */
export const toClOrdId = (clientOrderId: string) => createHash('sha256').update(clientOrderId).digest('hex').slice(0, 32);

export function sign(secretKey: string, timestamp: string, method: string, requestPath: string, body: string) {
  return createHmac('sha256', secretKey).update(`${timestamp}${method}${requestPath}${body}`).digest('base64');
}

export interface OkxOrder {
  ordId: string;
  clOrdId: string;
  state: string;
  accFillSz: string;
  avgPx: string;
  fee: string;
  pnl: string;
}

export interface OkxPosition {
  instId: string;
  pos: string;
  avgPx: string;
  markPx: string;
  upl: string;
  lever: string;
  liqPx: string;
  margin: string;
  mgnMode: string;
}

export interface PlaceOrderBody {
  instId: string;
  tdMode: 'isolated';
  side: 'buy' | 'sell';
  ordType: 'market' | 'limit';
  sz: string;
  clOrdId: string;
  px?: string;
  reduceOnly?: boolean;
  attachAlgoOrds?: Record<string, string>[];
}

type Row = Record<string, string>;

export class OkxClient {
  private readonly base: string;
  private readonly fetchImpl: typeof fetch;

  constructor(
    private readonly credentials: OkxCredentials,
    private readonly venue: Exclude<AccountVenue, 'PAPER'>,
    options: { fetchImpl?: typeof fetch; baseUrl?: string } = {},
  ) {
    this.base = options.baseUrl ?? REST[venue];
    this.fetchImpl = options.fetchImpl ?? fetch;
  }

  private async request<T>(method: 'GET' | 'POST', path: string, payload?: unknown): Promise<T[]> {
    const body = payload === undefined ? '' : JSON.stringify(payload);
    const timestamp = new Date().toISOString();
    let response: Response;
    try {
      response = await this.fetchImpl(`${this.base}${path}`, {
        method,
        headers: {
          'content-type': 'application/json',
          'OK-ACCESS-KEY': this.credentials.apiKey,
          'OK-ACCESS-SIGN': sign(this.credentials.secretKey, timestamp, method, path, body),
          'OK-ACCESS-TIMESTAMP': timestamp,
          'OK-ACCESS-PASSPHRASE': this.credentials.passphrase,
          ...(this.venue === 'OKX_DEMO' ? { 'x-simulated-trading': '1' } : {}),
        },
        ...(body ? { body } : {}),
        signal: AbortSignal.timeout(TIMEOUT_MS),
      });
    } catch (error) {
      throw new OkxUnknownOutcome(`No answer from OKX: ${(error as Error).message}`);
    }
    let json: { code?: string; msg?: string; data?: T[] };
    try {
      json = (await response.json()) as typeof json;
    } catch {
      throw new OkxUnknownOutcome(`OKX returned HTTP ${response.status} without JSON`);
    }
    if (response.status >= 500) throw new OkxUnknownOutcome(`OKX returned HTTP ${response.status}`);
    if (json.code === '0') return json.data ?? [];
    // On batch-style writes the useful reason sits on the first row.
    const row = json.data?.[0] as { sCode?: string; sMsg?: string } | undefined;
    throw new OkxRejection(row?.sCode || json.code || String(response.status), row?.sMsg || json.msg || 'Request refused');
  }

  async getConfig() {
    const [config] = await this.request<Row>('GET', '/api/v5/account/config');
    if (!config) throw new OkxRejection('config', 'No account configuration returned');
    return config;
  }

  async getContractValue(symbol: SymbolId) {
    const [instrument] = await this.request<Row>('GET', `/api/v5/public/instruments?instType=SWAP&instId=${toInstId(symbol)}`);
    return { ctVal: instrument?.ctVal ?? '', lotSz: instrument?.lotSz ?? '' };
  }

  async getUsdtBalance() {
    const [account] = await this.request<{ details?: Row[] }>('GET', '/api/v5/account/balance?ccy=USDT');
    return account?.details?.find((d) => d.ccy === 'USDT') ?? null;
  }

  getPositions() {
    return this.request<OkxPosition>('GET', '/api/v5/account/positions?instType=SWAP');
  }

  async setLeverage(instId: string, lever: number) {
    await this.request('POST', '/api/v5/account/set-leverage', { instId, lever: String(lever), mgnMode: 'isolated' });
  }

  async placeOrder(body: PlaceOrderBody) {
    const [row] = await this.request<Row>('POST', '/api/v5/trade/order', body);
    if (!row) throw new OkxUnknownOutcome('OKX acknowledged the order without a result row');
    if (row.sCode !== '0') throw new OkxRejection(row.sCode ?? 'order', row.sMsg ?? 'Order refused');
    return row.ordId ?? '';
  }

  async cancelOrder(instId: string, clOrdId: string) {
    await this.request('POST', '/api/v5/trade/cancel-order', { instId, clOrdId });
  }

  /** Null only when OKX positively says the order does not exist. */
  async getOrder(instId: string, clOrdId: string): Promise<OkxOrder | null> {
    try {
      const [order] = await this.request<OkxOrder>('GET', `/api/v5/trade/order?instId=${instId}&clOrdId=${clOrdId}`);
      return order ?? null;
    } catch (error) {
      if (error instanceof OkxRejection && error.code === ORDER_NOT_FOUND) return null;
      throw error;
    }
  }
}
