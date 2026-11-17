import { describe, expect, it } from 'vitest';
import { compareArenaRows } from '../src/lib/arena';

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
});
