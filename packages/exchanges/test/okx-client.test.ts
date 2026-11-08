import { describe, expect, it } from 'vitest';
import { OkxClient, OkxRejection, OkxUnknownOutcome, sign, toAlgoClOrdId, toClOrdId, type AlgoOrderBody } from '../src/index';

// Expected values come from openssl, not from this codebase, so a wrong prehash cannot agree with itself.
describe('OKX request signing', () => {
  const timestamp = '2020-12-08T09:08:57.715Z';

  it('signs timestamp + method + path + body', () => {
    expect(sign('test-secret', timestamp, 'POST', '/api/v5/trade/order', '{"instId":"BTC-USDT-SWAP"}')).toBe('pTcfOssGhm9sSvJrX7EenxdYrhHiqBkSG5mJc6vZoQg=');
  });

  it('keeps the query string in the signed path and signs no body for GET', () => {
    expect(sign('test-secret', timestamp, 'GET', '/api/v5/account/balance?ccy=USDT', '')).toBe('Yt2R9ATAX6Jz6+ud53rCYenVK09+CEpafaWGadqhybE=');
  });
});

describe('client order ids', () => {
  it('meets the OKX format and is stable for a retry', () => {
    const id = 'agent:agent_123:dec_456';
    expect(toClOrdId(id)).toMatch(/^[a-f0-9]{32}$/);
    expect(toClOrdId(id)).toBe(toClOrdId(id));
    expect(toClOrdId(id)).not.toBe(toClOrdId(`${id}x`));
  });
});

describe('algo orders', () => {
  const credentials = { apiKey: 'key', secretKey: 'test-secret', passphrase: 'pass' };
  const trailing: AlgoOrderBody = {
    instId: 'BTC-USDT-SWAP',
    tdMode: 'isolated',
    side: 'sell',
    ordType: 'move_order_stop',
    algoClOrdId: toAlgoClOrdId('order_1'),
    sz: '3',
    reduceOnly: true,
    callbackRatio: '0.02',
    activePx: '105',
  };

  function fake(reply: unknown, venue: 'OKX_LIVE' | 'OKX_DEMO' = 'OKX_LIVE') {
    const calls: { url: string; init: RequestInit }[] = [];
    const fetchImpl = (async (url: string, init: RequestInit) => {
      calls.push({ url, init });
      return new Response(JSON.stringify(reply), { status: 200 });
    }) as unknown as typeof fetch;
    return { calls, client: new OkxClient(credentials, venue, { fetchImpl, baseUrl: 'https://okx.test' }) };
  }
  const ok = (data: unknown[]) => ({ code: '0', msg: '', data });

  it('derives an algoClOrdId that cannot collide with the clOrdId', () => {
    expect(toAlgoClOrdId('order_1')).toMatch(/^[a-f0-9]{32}$/);
    expect(toAlgoClOrdId('order_1')).toBe(toAlgoClOrdId('order_1'));
    expect(toAlgoClOrdId('order_1')).not.toBe(toClOrdId('order_1'));
  });

  it('placeAlgo signs and posts a move_order_stop body', async () => {
    const { calls, client } = fake(ok([{ algoId: 'A1', sCode: '0', sMsg: '' }]));
    await expect(client.placeAlgo(trailing)).resolves.toBe('A1');
    const [call] = calls;
    expect(call!.url).toBe('https://okx.test/api/v5/trade/order-algo');
    expect(call!.init.method).toBe('POST');
    expect(JSON.parse(call!.init.body as string)).toEqual(trailing);
    const headers = call!.init.headers as Record<string, string>;
    const expected = sign('test-secret', headers['OK-ACCESS-TIMESTAMP']!, 'POST', '/api/v5/trade/order-algo', call!.init.body as string);
    expect(headers['OK-ACCESS-SIGN']).toBe(expected);
  });

  it('placeAlgo maps an sCode rejection', async () => {
    const { client } = fake(ok([{ algoId: '', sCode: '51000', sMsg: 'Parameter error' }]));
    await expect(client.placeAlgo(trailing)).rejects.toMatchObject({ code: '51000' });
    await expect(client.placeAlgo(trailing)).rejects.toBeInstanceOf(OkxRejection);
  });

  it('placeAlgo with no row is an unknown outcome', async () => {
    const { client } = fake(ok([]));
    await expect(client.placeAlgo(trailing)).rejects.toBeInstanceOf(OkxUnknownOutcome);
  });

  it('cancelAlgos refuses more than 10 items before fetching', async () => {
    const { calls, client } = fake(ok([]));
    const items = Array.from({ length: 11 }, (_, i) => ({ instId: 'BTC-USDT-SWAP', algoId: String(i) }));
    await expect(client.cancelAlgos(items)).rejects.toThrow(/10/);
    expect(calls).toHaveLength(0);
  });

  it('cancelAlgos posts the array body', async () => {
    const { calls, client } = fake(ok([{ algoId: '1', sCode: '0', sMsg: '' }]));
    await client.cancelAlgos([{ instId: 'BTC-USDT-SWAP', algoId: '1' }]);
    expect(calls[0]!.url).toBe('https://okx.test/api/v5/trade/cancel-algos');
    expect(JSON.parse(calls[0]!.init.body as string)).toEqual([{ instId: 'BTC-USDT-SWAP', algoId: '1' }]);
  });

  it('cancelAlgos surfaces a per-row refusal', async () => {
    const { client } = fake(ok([{ algoId: '1', sCode: '51000', sMsg: 'nope' }]));
    await expect(client.cancelAlgos([{ instId: 'BTC-USDT-SWAP', algoId: '1' }])).rejects.toBeInstanceOf(OkxRejection);
  });

  it('getPendingAlgos queries by ordType for SWAP', async () => {
    const { calls, client } = fake(ok([{ algoId: 'A1' }]));
    await expect(client.getPendingAlgos('move_order_stop')).resolves.toHaveLength(1);
    expect(calls[0]!.url).toBe('https://okx.test/api/v5/trade/orders-algo-pending?ordType=move_order_stop&instType=SWAP');
  });

  it('getAlgo returns null only for an empty data array', async () => {
    const { calls, client } = fake(ok([]));
    await expect(client.getAlgo('abc')).resolves.toBeNull();
    expect(calls[0]!.url).toBe('https://okx.test/api/v5/trade/order-algo?algoClOrdId=abc');
    const found = fake(ok([{ algoId: 'A1', algoClOrdId: 'abc' }]));
    await expect(found.client.getAlgo('abc')).resolves.toMatchObject({ algoId: 'A1' });
    // Even the code the order endpoint uses for "not found" must not become absent here.
    const refused = fake({ code: '51603', msg: 'Order does not exist', data: [] });
    await expect(refused.client.getAlgo('abc')).rejects.toBeInstanceOf(OkxRejection);
  });

  it('demo sends x-simulated-trading on algo calls', async () => {
    const demo = fake(ok([{ algoId: 'A1', sCode: '0', sMsg: '' }]), 'OKX_DEMO');
    await demo.client.placeAlgo(trailing);
    expect((demo.calls[0]!.init.headers as Record<string, string>)['x-simulated-trading']).toBe('1');
    const live = fake(ok([{ algoId: 'A1', sCode: '0', sMsg: '' }]));
    await live.client.placeAlgo(trailing);
    expect((live.calls[0]!.init.headers as Record<string, string>)['x-simulated-trading']).toBeUndefined();
  });
});
