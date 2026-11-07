import type { AccountBalance, Order, OrderRequest, OrderResult, PositionView, SymbolId } from '@mochiprep/shared';

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
}
