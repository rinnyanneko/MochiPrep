import { describe, expect, it } from 'vitest';
import { sign, toClOrdId } from '../src/index';

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
