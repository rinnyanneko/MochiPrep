import { describe, expect, it } from 'vitest';
import { arenaView, compareArenaRows } from '../src/lib/arena';

const row = (name: string, returnPct: number | null, pnl: string | null) => ({ name, returnPct, pnl });

describe('compareArenaRows', () => {
  it('ranks returns first, then exchange profit, then rows with neither', () => {
    const rows = [
      row('shared', null, null),
      row('okx loss', null, '-2'),
      row('paper low', -1, '-10'),
      row('okx gain', null, '3.5'),
      row('paper high', 4, '40'),
      row('empty paper', null, '0'),
    ];
    // An empty paper account has a null return too and ranks among the profits.
    expect([...rows].sort(compareArenaRows).map((r) => r.name)).toEqual([
      'paper high',
      'paper low',
      'okx gain',
      'empty paper',
      'okx loss',
      'shared',
    ]);
  });

  it.each([Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY])(
    'treats a non-finite return (%s) as unavailable and ranks by P&L',
    (returnPct) => {
      const rows = [row('unknown', returnPct, null), row('profit', returnPct, '12'), row('paper', -99, '-999')];
      expect(rows.sort(compareArenaRows).map((r) => r.name)).toEqual(['paper', 'profit', 'unknown']);
    },
  );

  it.each(['NaN', 'Infinity', '-Infinity', 'not a decimal', null])(
    'places unavailable P&L (%s) after a known loss in either argument order',
    (pnl) => {
      const unavailable = row('unavailable', null, pnl);
      const loss = row('loss', null, '-100');
      expect(compareArenaRows(unavailable, loss)).toBeGreaterThan(0);
      expect(compareArenaRows(loss, unavailable)).toBeLessThan(0);
      expect(compareArenaRows(unavailable, row('shared', null, null))).toBe(0);
    },
  );

  it('orders decimal profits numerically, including zero and small losses', () => {
    const rows = [row('two', null, '2'), row('loss', null, '-0.0001'), row('ten', null, '10'), row('zero', null, '0')];
    expect(rows.sort(compareArenaRows).map((r) => r.name)).toEqual(['ten', 'two', 'zero', 'loss']);
  });

  it('preserves creation order for equal returns regardless of P&L', () => {
    const first = row('first', 2, '-100');
    const second = row('second', 2, '100');
    const rows = [first, second, row('third', 2, null)];
    expect([...rows].sort(compareArenaRows)).toEqual(rows);
    expect(compareArenaRows(first, second)).toBe(0);
  });

  it('preserves creation order for equal decimal P&L and unavailable metrics', () => {
    const first = row('first', null, '1.00');
    const second = row('second', null, '1');
    const rows = [first, second, row('third', null, null), row('fourth', null, null)];
    expect([...rows].sort(compareArenaRows)).toEqual(rows);
    expect(compareArenaRows(first, second)).toBe(0);
  });
});

describe('arenaView', () => {
  it('puts every exchange row under OKX and paper rows under their market', () => {
    expect(arenaView({ source: 'LIVE', venue: 'OKX_LIVE' })).toBe('OKX');
    expect(arenaView({ source: 'LIVE', venue: 'OKX_DEMO' })).toBe('OKX');
    expect(arenaView({ source: 'LIVE', venue: 'PAPER' })).toBe('LIVE');
    expect(arenaView({ source: 'REPLAY', venue: 'PAPER' })).toBe('REPLAY');
  });
});
