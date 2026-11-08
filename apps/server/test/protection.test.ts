import { describe, expect, it } from 'vitest';
import { classifyProtectionChange } from '@mochiprep/core';
import type { PositionSide, Protection, TrailingStopSpec } from '@mochiprep/shared';

const none: Protection = { stopLoss: null, takeProfit: null, trailingStop: null };
const p = (patch: Partial<Protection>): Protection => ({ ...none, ...patch });
const ratio = (callback: string, activationPrice: string | null = null): TrailingStopSpec => ({ kind: 'RATIO', callback, activationPrice });
const spread = (callback: string, activationPrice: string | null = null): TrailingStopSpec => ({ kind: 'SPREAD', callback, activationPrice });

type Case = [label: string, side: PositionSide, before: Protection, after: Protection, expected: 'REDUCES_RISK' | 'NEEDS_ARMED'];

const cases: Case[] = [
  // Unchanged legs.
  ['nothing changes', 'LONG', p({ stopLoss: '95', takeProfit: '110', trailingStop: ratio('0.01') }), p({ stopLoss: '95', takeProfit: '110', trailingStop: ratio('0.01') }), 'REDUCES_RISK'],
  ['the same stop written differently', 'LONG', p({ stopLoss: '95' }), p({ stopLoss: '95.00' }), 'REDUCES_RISK'],
  ['no protection at all, unchanged', 'SHORT', none, none, 'REDUCES_RISK'],

  // Stop-loss.
  ['a stop added to a long', 'LONG', none, p({ stopLoss: '95' }), 'REDUCES_RISK'],
  ['a stop added to a short', 'SHORT', none, p({ stopLoss: '105' }), 'REDUCES_RISK'],
  ['a long stop moved up', 'LONG', p({ stopLoss: '95' }), p({ stopLoss: '97' }), 'REDUCES_RISK'],
  ['a short stop moved down', 'SHORT', p({ stopLoss: '105' }), p({ stopLoss: '103' }), 'REDUCES_RISK'],
  ['a long stop moved down', 'LONG', p({ stopLoss: '95' }), p({ stopLoss: '93' }), 'NEEDS_ARMED'],
  ['a short stop moved up', 'SHORT', p({ stopLoss: '105' }), p({ stopLoss: '107' }), 'NEEDS_ARMED'],
  ['a stop removed', 'LONG', p({ stopLoss: '95' }), none, 'NEEDS_ARMED'],

  // Take-profit: every change needs arming.
  ['a target added', 'LONG', none, p({ takeProfit: '110' }), 'NEEDS_ARMED'],
  ['a target moved closer', 'LONG', p({ takeProfit: '110' }), p({ takeProfit: '105' }), 'NEEDS_ARMED'],
  ['a target removed', 'SHORT', p({ takeProfit: '90' }), none, 'NEEDS_ARMED'],

  // Trailing stop.
  ['a trailing stop added', 'LONG', none, p({ trailingStop: ratio('0.01') }), 'REDUCES_RISK'],
  ['a trailing stop with activation added', 'SHORT', none, p({ trailingStop: spread('2', '95') }), 'REDUCES_RISK'],
  ['a ratio callback made smaller', 'LONG', p({ trailingStop: ratio('0.02') }), p({ trailingStop: ratio('0.01') }), 'REDUCES_RISK'],
  ['a spread callback made smaller with the same activation', 'SHORT', p({ trailingStop: spread('3', '95') }), p({ trailingStop: spread('2', '95') }), 'REDUCES_RISK'],
  ['a callback made larger', 'LONG', p({ trailingStop: ratio('0.01') }), p({ trailingStop: ratio('0.02') }), 'NEEDS_ARMED'],
  ['the kind changed', 'LONG', p({ trailingStop: ratio('0.01') }), p({ trailingStop: spread('0.5') }), 'NEEDS_ARMED'],
  ['the activation changed', 'LONG', p({ trailingStop: ratio('0.02', '110') }), p({ trailingStop: ratio('0.01', '111') }), 'NEEDS_ARMED'],
  ['an activation added', 'LONG', p({ trailingStop: ratio('0.02') }), p({ trailingStop: ratio('0.01', '110') }), 'NEEDS_ARMED'],
  ['an activation dropped', 'LONG', p({ trailingStop: ratio('0.02', '110') }), p({ trailingStop: ratio('0.01') }), 'NEEDS_ARMED'],
  ['a trailing stop removed', 'SHORT', p({ trailingStop: ratio('0.01') }), none, 'NEEDS_ARMED'],

  // Mixed: one leg that needs arming is enough.
  ['a tighter stop with a target added', 'LONG', p({ stopLoss: '95' }), p({ stopLoss: '97', takeProfit: '110' }), 'NEEDS_ARMED'],
  ['a tighter stop and a tighter trail', 'LONG', p({ stopLoss: '95', trailingStop: ratio('0.02') }), p({ stopLoss: '97', trailingStop: ratio('0.01') }), 'REDUCES_RISK'],
  ['a tighter stop with the trail removed', 'LONG', p({ stopLoss: '95', trailingStop: ratio('0.02') }), p({ stopLoss: '97' }), 'NEEDS_ARMED'],
  ['a tighter stop with an unchanged target', 'SHORT', p({ stopLoss: '105', takeProfit: '90' }), p({ stopLoss: '104', takeProfit: '90' }), 'REDUCES_RISK'],
];

describe('classifyProtectionChange', () => {
  it.each(cases)('%s on a %s', (_label, side, before, after, expected) => {
    expect(classifyProtectionChange(side, before, after)).toBe(expected);
  });
});
