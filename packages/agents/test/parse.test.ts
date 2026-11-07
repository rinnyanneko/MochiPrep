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
});
