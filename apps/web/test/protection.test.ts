import { describe, expect, it } from 'vitest';
import type { ExternalProtection } from '@mochiprep/shared';
import { externalBadgeLabel, trailingText } from '../src/lib/protection';

const order = (kind: ExternalProtection['kind'], detail: string): ExternalProtection => ({ kind, triggerPrice: '74000', detail });

describe('externalBadgeLabel', () => {
  it('calls the owner\'s own OKX orders External', () => {
    expect(externalBadgeLabel(order('STOP_LOSS', 'SL trigger 74000'), '74,000')).toBe('External SL 74,000');
    expect(externalBadgeLabel(order('TRAILING', 'trail'), null)).toBe('External trail');
  });

  it('does not call our own algo with a pending cancel External', () => {
    expect(externalBadgeLabel(order('TAKE_PROFIT', 'ours, cancel pending: TP trigger 74000'), '74,000')).toBe('Cancel pending TP 74,000');
    expect(externalBadgeLabel(order('TRAILING', 'ours, cancel pending: trail'), null)).toBe('Cancel pending trail');
  });
});

describe('trailingText', () => {
  it('describes a suggested or stored trailing stop', () => {
    expect(trailingText({ kind: 'RATIO', callback: 0.015, activationPrice: null })).toBe('1.5% callback');
    expect(trailingText({ kind: 'SPREAD', callback: '150.5', activationPrice: '68000' })).toBe('150.5 USDT callback, from 68000');
    expect(trailingText(null)).toBe('none');
  });
});
