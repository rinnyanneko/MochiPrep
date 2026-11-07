import type { LedgerEvent, PaperAccount } from '@mochiprep/shared';
import { d, str, ZERO } from '../decimal';

export interface LedgerTotals {
  wallet: string;
  usedMargin: string;
  realizedPnl: string;
  feesPaid: string;
  fundingPaid: string;
}

/** Rebuilds account totals purely from the event stream. */
export function foldLedger(events: LedgerEvent[]): LedgerTotals {
  let wallet = ZERO;
  let usedMargin = ZERO;
  let realized = ZERO;
  let fees = ZERO;
  let funding = ZERO;
  for (const e of events) {
    wallet = wallet.plus(e.amount);
    usedMargin = usedMargin.plus(e.marginDelta);
    if (e.type === 'REALIZED_PNL' || e.type === 'LIQUIDATION_PENALTY') realized = realized.plus(e.amount);
    if (e.type === 'TRADE_FEE') fees = fees.minus(e.amount);
    if (e.type === 'FUNDING') funding = funding.minus(e.amount);
  }
  return {
    wallet: str(wallet),
    usedMargin: str(usedMargin),
    realizedPnl: str(realized),
    feesPaid: str(fees),
    fundingPaid: str(funding),
  };
}

/** Returns discrepancies between stored totals and the ledger; empty means consistent. */
export function auditLedger(account: PaperAccount, events: LedgerEvent[]): string[] {
  const totals = foldLedger(events);
  const issues: string[] = [];
  const compare = (label: string, stored: string, derived: string) => {
    if (!d(stored).eq(derived)) issues.push(`${label}: stored ${stored}, ledger ${derived}`);
  };
  compare('walletBalance', account.walletBalance, totals.wallet);
  compare('usedMargin', account.usedMargin, totals.usedMargin);
  compare('realizedPnl', account.realizedPnl, totals.realizedPnl);
  compare('feesPaid', account.feesPaid, totals.feesPaid);
  compare('fundingPaid', account.fundingPaid, totals.fundingPaid);
  let running = ZERO;
  for (const e of events) {
    running = running.plus(e.amount);
    if (!running.eq(e.walletAfter)) {
      issues.push(`event ${e.seq}: walletAfter ${e.walletAfter}, running ${str(running)}`);
      break;
    }
  }
  return issues;
}
