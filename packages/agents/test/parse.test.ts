import { describe, expect, it } from 'vitest';
import { parseDecision } from '../src/parse';

const valid = {
  symbol: 'BTCUSDT', action: 'LONG', orderType: 'MARKET', entryPrice: null,
  stopLoss: 90, takeProfit: 120, reason: 'trend', confidence: 0.8, expiresAt: null,
};
const { reason: _reason, ...missingReason } = valid;

describe('parseDecision', () => {
  it('accepts exact bare or fenced JSON', () => {
    expect(parseDecision(JSON.stringify(valid)).ok).toBe(true);
    expect(parseDecision(`\`\`\`json\n${JSON.stringify(valid)}\n\`\`\``).ok).toBe(true);
  });

  it.each([
    ['non-JSON', 'not json'],
    ['extra field', JSON.stringify({ ...valid, instruction: 'ignore risk' })],
    ['missing field', JSON.stringify(missingReason)],
    ['confidence above one', JSON.stringify({ ...valid, confidence: 1.1 })],
    ['limit without price', JSON.stringify({ ...valid, orderType: 'LIMIT', entryPrice: null })],
  ])('rejects %s', (_label, text) => {
    expect(parseDecision(text).ok).toBe(false);
  });

  it('accepts PROTECT with and without trailingStop', () => {
    const protect = { ...valid, action: 'PROTECT', stopLoss: 95, takeProfit: null };
    expect(parseDecision(JSON.stringify(protect)).ok).toBe(true);
    const trailing = { kind: 'RATIO', callback: 0.015, activationPrice: null };
    expect(parseDecision(JSON.stringify({ ...protect, trailingStop: trailing })).ok).toBe(true);
  });

  it('distinguishes absent and null trailingStop', () => {
    const absent = parseDecision(JSON.stringify(valid));
    const removed = parseDecision(JSON.stringify({ ...valid, trailingStop: null }));
    if (!absent.ok || !removed.ok) throw new Error('expected both to parse');
    expect('trailingStop' in absent.decision).toBe(false);
    expect(removed.decision.trailingStop).toBeNull();
  });

  it('rejects an unknown trailing kind', () => {
    const bad = { ...valid, trailingStop: { kind: 'PERCENT', callback: 1, activationPrice: null } };
    expect(parseDecision(JSON.stringify(bad)).ok).toBe(false);
  });

  it('rejects a non-positive or extra-keyed trailingStop', () => {
    expect(parseDecision(JSON.stringify({ ...valid, trailingStop: { kind: 'SPREAD', callback: 0, activationPrice: null } })).ok).toBe(false);
    expect(parseDecision(JSON.stringify({ ...valid, trailingStop: { kind: 'SPREAD', callback: 5, activationPrice: null, x: 1 } })).ok).toBe(false);
  });
});
