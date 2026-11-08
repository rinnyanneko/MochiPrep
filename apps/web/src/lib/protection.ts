import type { ExternalProtection } from '@mochiprep/shared';

const KIND_LABEL: Record<ExternalProtection['kind'], string> = { STOP_LOSS: 'SL', TAKE_PROFIT: 'TP', TRAILING: 'trail' };

/** The engine prefixes this onto the detail of MochiPrep's own algo whose cancel has not gone through yet. */
export const CANCEL_PENDING_PREFIX = 'ours, cancel pending:';

/**
 * Badge text for a protection order MochiPrep does not currently manage. The owner's own OKX orders are
 * External. Our own leftover algo is not: MochiPrep is still trying to cancel it, so calling it External
 * would wrongly say it is never touched.
 */
export function externalBadgeLabel(e: ExternalProtection, shownPrice: string | null): string {
  const head = e.detail.startsWith(CANCEL_PENDING_PREFIX) ? 'Cancel pending' : 'External';
  return `${head} ${KIND_LABEL[e.kind]}${shownPrice ? ` ${shownPrice}` : ''}`;
}
