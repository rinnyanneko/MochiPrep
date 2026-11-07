import type { AccountBalance, Order, OrderRequest, OrderResult, Position } from '@mochiprep/shared';

/** The seam agents and the UI trade through. Paper and live implement the same contract. */
export interface ExecutionEngine {
  placeOrder(order: OrderRequest): Promise<OrderResult>;
  cancelOrder(orderId: string): Promise<void>;
  getPositions(accountId: string): Promise<Position[]>;
  getOpenOrders(accountId: string): Promise<Order[]>;
  getBalance(accountId: string): Promise<AccountBalance>;
}
