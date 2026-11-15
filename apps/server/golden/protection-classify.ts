import { classifyProtectionChange } from '@mochiprep/core';
import type { PositionSide, Protection, TrailingStopSpec } from '@mochiprep/shared';

const none: Protection = { stopLoss: null, takeProfit: null, trailingStop: null };
const p = (patch: Partial<Protection>): Protection => ({ ...none, ...patch });
const ratio = (callback: string, activationPrice: string | null = null): TrailingStopSpec => ({ kind: 'RATIO', callback, activationPrice });
const spread = (callback: string, activationPrice: string | null = null): TrailingStopSpec => ({ kind: 'SPREAD', callback, activationPrice });

type Case = [label: string, side: PositionSide, before: Protection, after: Protection];

/** All 27 cases of apps/server/test/protection.test.ts, in its order, then edge cases. */
const CASES: Case[] = [
  ['nothing changes', 'LONG', p({ stopLoss: '95', takeProfit: '110', trailingStop: ratio('0.01') }), p({ stopLoss: '95', takeProfit: '110', trailingStop: ratio('0.01') })],
  ['the same stop written differently', 'LONG', p({ stopLoss: '95' }), p({ stopLoss: '95.00' })],
  ['no protection at all, unchanged', 'SHORT', none, none],
  ['a stop added to a long', 'LONG', none, p({ stopLoss: '95' })],
  ['a stop added to a short', 'SHORT', none, p({ stopLoss: '105' })],
  ['a long stop moved up', 'LONG', p({ stopLoss: '95' }), p({ stopLoss: '97' })],
  ['a short stop moved down', 'SHORT', p({ stopLoss: '105' }), p({ stopLoss: '103' })],
  ['a long stop moved down', 'LONG', p({ stopLoss: '95' }), p({ stopLoss: '93' })],
  ['a short stop moved up', 'SHORT', p({ stopLoss: '105' }), p({ stopLoss: '107' })],
  ['a stop removed', 'LONG', p({ stopLoss: '95' }), none],
  ['a target added', 'LONG', none, p({ takeProfit: '110' })],
  ['a target moved closer', 'LONG', p({ takeProfit: '110' }), p({ takeProfit: '105' })],
  ['a target removed', 'SHORT', p({ takeProfit: '90' }), none],
  ['a trailing stop added', 'LONG', none, p({ trailingStop: ratio('0.01') })],
  ['a trailing stop with activation added', 'SHORT', none, p({ trailingStop: spread('2', '95') })],
  ['a ratio callback made smaller', 'LONG', p({ trailingStop: ratio('0.02') }), p({ trailingStop: ratio('0.01') })],
  ['a spread callback made smaller with the same activation', 'SHORT', p({ trailingStop: spread('3', '95') }), p({ trailingStop: spread('2', '95') })],
  ['a callback made larger', 'LONG', p({ trailingStop: ratio('0.01') }), p({ trailingStop: ratio('0.02') })],
  ['the kind changed', 'LONG', p({ trailingStop: ratio('0.01') }), p({ trailingStop: spread('0.5') })],
  ['the activation changed', 'LONG', p({ trailingStop: ratio('0.02', '110') }), p({ trailingStop: ratio('0.01', '111') })],
  ['an activation added', 'LONG', p({ trailingStop: ratio('0.02') }), p({ trailingStop: ratio('0.01', '110') })],
  ['an activation dropped', 'LONG', p({ trailingStop: ratio('0.02', '110') }), p({ trailingStop: ratio('0.01') })],
  ['a trailing stop removed', 'SHORT', p({ trailingStop: ratio('0.01') }), none],
  ['a tighter stop with a target added', 'LONG', p({ stopLoss: '95' }), p({ stopLoss: '97', takeProfit: '110' })],
  ['a tighter stop and a tighter trail', 'LONG', p({ stopLoss: '95', trailingStop: ratio('0.02') }), p({ stopLoss: '97', trailingStop: ratio('0.01') })],
  ['a tighter stop with the trail removed', 'LONG', p({ stopLoss: '95', trailingStop: ratio('0.02') }), p({ stopLoss: '97' })],
  ['a tighter stop with an unchanged target', 'SHORT', p({ stopLoss: '105', takeProfit: '90' }), p({ stopLoss: '104', takeProfit: '90' })],
  // Edge cases the Rust port must read the same way.
  ['a long stop moved to the same price written differently', 'LONG', p({ stopLoss: '95.5' }), p({ stopLoss: '95.50000' })],
  ['a short stop added', 'SHORT', none, p({ stopLoss: '0.0000090123' })],
  ['a long stop moved up by one tiny tick', 'LONG', p({ stopLoss: '0.000009' }), p({ stopLoss: '0.0000090001' })],
  ['a short stop moved down by one tiny tick', 'SHORT', p({ stopLoss: '0.0000091' }), p({ stopLoss: '0.00000909' })],
  ['a long stop moved up past the old target', 'LONG', p({ stopLoss: '95', takeProfit: '96' }), p({ stopLoss: '97', takeProfit: '96' })],
  ['the same target written differently', 'LONG', p({ takeProfit: '110' }), p({ takeProfit: '110.0' })],
  ['a target moved further', 'SHORT', p({ takeProfit: '90' }), p({ takeProfit: '80' })],
  ['the same trail written differently', 'LONG', p({ trailingStop: ratio('0.01', '110') }), p({ trailingStop: ratio('0.010', '110.00') })],
  ['a ratio callback made smaller, activation written differently', 'LONG', p({ trailingStop: ratio('0.02', '110') }), p({ trailingStop: ratio('0.01', '110.0') })],
  ['a spread callback unchanged with the activation removed', 'SHORT', p({ trailingStop: spread('2', '95') }), p({ trailingStop: spread('2') })],
  ['a spread callback made smaller without activation', 'LONG', p({ trailingStop: spread('500') }), p({ trailingStop: spread('499.5') })],
  ['the kind changed with a smaller callback', 'SHORT', p({ trailingStop: spread('2') }), p({ trailingStop: ratio('0.01') })],
  ['a stop swapped for a trailing stop', 'LONG', p({ stopLoss: '74000' }), p({ trailingStop: ratio('0.01') })],
  ['a trailing stop added beside a kept stop', 'LONG', p({ stopLoss: '74000' }), p({ stopLoss: '74000', trailingStop: spread('500', '76000') })],
  ['everything added at once', 'SHORT', none, p({ stopLoss: '105', takeProfit: '90', trailingStop: ratio('0.01') })],
  ['everything removed at once', 'LONG', p({ stopLoss: '95', takeProfit: '110', trailingStop: ratio('0.01') }), none],
  ['a looser stop and a tighter trail', 'SHORT', p({ stopLoss: '105', trailingStop: ratio('0.02') }), p({ stopLoss: '106', trailingStop: ratio('0.01') })],
];

export function protectionClassifyFixture() {
  return CASES.map(([label, side, before, after]) => ({ label, side, before, after, expected: classifyProtectionChange(side, before, after) }));
}
