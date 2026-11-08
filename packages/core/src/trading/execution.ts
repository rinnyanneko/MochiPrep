import type { AccountBalance, Order, OrderRequest, OrderResult, PositionView, Protection, SymbolId } from '@mochiprep/shared';

/** The protection of a position just before and just after a change. */
export interface ProtectionResult {
  before: Protection;
  after: Protection;
}

/** The seam agents and the UI trade through. Paper and live implement the same contract. */
export interface ExecutionEngine {
  placeOrder(order: OrderRequest): Promise<OrderResult>;
  cancelOrder(orderId: string): Promise<void>;
  closePosition(accountId: string, symbol: SymbolId, fraction: number, clientOrderId: string): OrderResult | Promise<OrderResult>;
  getPositions(accountId: string): Promise<PositionView[]>;
  getOpenOrders(accountId: string): Promise<Order[]>;
  getBalance(accountId: string): Promise<AccountBalance>;
  /** Realized and unrealized change since 00:00 UTC, for the daily-loss limit. */
  dailyRisk(accountId: string): Promise<{ dailyPnl: string; dayStartEquity: string }>;
  /** Replaces the whole protection of an open position. A null leg removes that leg. */
  setProtection(
    accountId: string,
    symbol: SymbolId,
    protection: Protection,
    meta: { clientOrderId: string; agentId: string | null; decisionId: string | null },
  ): Promise<ProtectionResult>;
  /** The protection currently in force for a position, as the runner and risk engine see it. */
  protectionOf(accountId: string, symbol: SymbolId): Promise<Protection>;
}
