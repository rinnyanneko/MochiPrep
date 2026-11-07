import { describe, expect, it } from 'vitest';
import { redactSecrets } from '../src/redact';

describe('redactSecrets', () => {
  it('removes supplied and key-shaped secrets from errors and raw responses', () => {
    const secret = 'custom-secret-value-123';
    const input = `failed ${secret}; Authorization: Bearer sk-proj_abcdefghijk; x-api-key='AIzaABCDEFGHIJKLMNOPQRST'`;
    const output = redactSecrets(input, [secret]);
    expect(output).not.toContain(secret);
    expect(output).not.toContain('sk-proj_abcdefghijk');
    expect(output).not.toContain('AIzaABCDEFGHIJKLMNOPQRST');
    expect(output.match(/\[REDACTED\]/g)?.length).toBeGreaterThanOrEqual(3);
  });
});
