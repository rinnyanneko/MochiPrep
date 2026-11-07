/** Largest peak-to-trough fall, in percent. */
export function maxDrawdownPct(equity: number[]): number {
  let peak = -Infinity;
  let worst = 0;
  for (const e of equity) {
    if (e > peak) peak = e;
    if (peak > 0) worst = Math.max(worst, ((peak - e) / peak) * 100);
  }
  return worst;
}

/** Win rate over closed positions. Null when none closed yet, so 0% is never implied. */
export function winRatePct(closedNetPnl: number[]): number | null {
  if (closedNetPnl.length === 0) return null;
  return (closedNetPnl.filter((p) => p > 0).length / closedNetPnl.length) * 100;
}
