import type { ExecutionEngine } from '@mochiprep/core';
import type { Order, Position, SymbolId } from '@mochiprep/shared';

/**
 * Live connectivity seam. Nothing implements these yet, and MochiPrep cannot place real orders.
 * OKX is the first planned adapter; see docs/ROADMAP.md for what it must satisfy.
 */
export interface ExchangeCredentials {
  apiKey: string;
  apiSecret: string;
  passphrase?: string;
}

export interface ExchangeAdapter {
  readonly id: string;
  /** Must fail if the key can withdraw. */
  verifyPermissions(credentials: ExchangeCredentials): Promise<{ canTrade: boolean; canWithdraw: boolean }>;
  submitOrder(order: Order): Promise<{ exchangeOrderId: string }>;
  cancelOrder(exchangeOrderId: string, symbol: SymbolId): Promise<void>;
  /** Source of truth after a timeout: the order may or may not have reached the exchange. */
  findOrderByClientId(clientOrderId: string, symbol: SymbolId): Promise<Order | null>;
  fetchPositions(): Promise<Position[]>;
}

/** Same contract as paper trading, plus reconciliation of local state against the exchange. */
export interface LiveExecutionEngine extends ExecutionEngine {
  readonly adapter: ExchangeAdapter;
  reconcile(accountId: string): Promise<void>;
}
