import { CANCEL_PENDING_PREFIX, type AiTrailingStop, type ExternalProtection, type TrailingStopSpec } from '@mochiprep/shared';

const KIND_LABEL: Record<ExternalProtection['kind'], string> = { STOP_LOSS: 'SL', TAKE_PROFIT: 'TP', TRAILING: 'trail' };

export { CANCEL_PENDING_PREFIX };

/**
 * Badge text for a protection order MochiPrep does not currently manage. The owner's own OKX orders are
 * External. Our own leftover algo is not: MochiPrep is still trying to cancel it, so calling it External
 * would wrongly say it is never touched.
 */
export function externalBadgeLabel(e: ExternalProtection, shownPrice: string | null): string {
  const head = e.detail.startsWith(CANCEL_PENDING_PREFIX) ? 'Cancel pending' : 'External';
  return `${head} ${KIND_LABEL[e.kind]}${shownPrice ? ` ${shownPrice}` : ''}`;
}

// Display only: the ratio is shown as a percent, trimmed so 0.015 reads 1.5% rather than 1.5000000000000002%.
export const trailingText = (t: TrailingStopSpec | AiTrailingStop | null) =>
  !t ? 'none' : `${t.kind === 'RATIO' ? `${Number((Number(t.callback) * 100).toPrecision(10))}%` : `${t.callback} USDT`} callback${t.activationPrice === null ? '' : `, from ${t.activationPrice}`}`;
