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

  it.each([
    ['nested', 'shared-credential-prefix-private-gateway-suffix', ['shared-credential-prefix', 'shared-credential-prefix-private-gateway-suffix'], '[REDACTED]'],
    ['partial overlap', 'left-overlap-right', ['left-overlap', 'overlap-right'], '[REDACTED]'],
    ['repeated overlap', 'left-overlap-right|left-overlap-right', ['left-overlap', 'overlap-right'], '[REDACTED]|[REDACTED]'],
    ['adjacent', 'left-right', ['left-', 'right'], '[REDACTED][REDACTED]'],
    ['separate', 'left gap right', ['left', 'right'], '[REDACTED] gap [REDACTED]'],
    ['Unicode overlap', 'π😀secret-tail', ['π😀secret', 'secret-tail'], '[REDACTED]'],
    ['self overlap and empty secret', 'ababa', ['aba', ''], '[REDACTED]'],
  ])('redacts %s from original spans in either secret order', (_name, text, supplied, expected) => {
    const secrets = supplied as string[];
    expect(redactSecrets(text as string, secrets)).toBe(expected);
    expect(redactSecrets(text as string, [...secrets].reverse())).toBe(expected);
  });
});
