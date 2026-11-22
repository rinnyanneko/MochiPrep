import type { ArenaRow } from '@mochiprep/shared';

type Ranked = Pick<ArenaRow, 'returnPct' | 'pnl'>;

const known = (n: number | null): n is number => n !== null && Number.isFinite(n);

/**
 * Best first. A return is only comparable with another return, so rows that have one come first;
 * exchange rows, which have none, follow by USDT profit; rows with neither come last.
 */
export function compareArenaRows(a: Ranked, b: Ranked): number {
  const ra = known(a.returnPct) ? a.returnPct : null;
  const rb = known(b.returnPct) ? b.returnPct : null;
  if (ra !== null || rb !== null) {
    if (ra === null) return 1;
    if (rb === null) return -1;
    return rb - ra;
  }
  const pa = a.pnl === null ? Number.NaN : Number(a.pnl);
  const pb = b.pnl === null ? Number.NaN : Number(b.pnl);
  if (!known(pa)) return known(pb) ? 1 : 0;
  if (!known(pb)) return -1;
  return pb - pa;
}
