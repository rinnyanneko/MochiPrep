import type {
  DataSource,
  EquityPoint,
  Fill,
  LedgerEvent,
  Order,
  PaperAccount,
  Position,
  SymbolId,
} from '@mochiprep/shared';

/**
 * Persistence the paper engine needs. Synchronous on purpose: every financial change happens
 * inside one `transaction`, with no await point where another fill could interleave.
 */
export interface PaperStore {
  /** Runs fn atomically; any throw rolls back everything fn wrote. */
  transaction<T>(fn: () => T): T;

  insertAccount(account: PaperAccount): void;
  saveAccount(account: PaperAccount): void;
  getAccount(id: string): PaperAccount | undefined;
  listAccounts(): PaperAccount[];
  deleteAccountData(id: string, keepAccountRow: boolean): void;

  insertOrder(order: Order): void;
  saveOrder(order: Order): void;
  getOrder(id: string): Order | undefined;
  getOrderByClientId(accountId: string, clientOrderId: string): Order | undefined;
  listOpenOrders(accountId: string): Order[];
  listOpenOrdersForMarket(source: DataSource, symbol: SymbolId): Order[];
  listOrders(accountId: string, limit: number): Order[];

  insertFill(fill: Fill): void;
  listFillsByOrder(orderId: string): Fill[];
  listFills(accountId: string, limit: number): Fill[];

  insertPosition(position: Position): void;
  savePosition(position: Position): void;
  getOpenPosition(accountId: string, symbol: SymbolId): Position | undefined;
  listOpenPositions(accountId: string): Position[];
  listOpenPositionsForMarket(source: DataSource, symbol: SymbolId): Position[];
  listClosedPositions(accountId: string, limit: number): Position[];

  appendLedger(event: Omit<LedgerEvent, 'seq'>): void;
  listLedger(accountId: string, sinceTs?: number): LedgerEvent[];

  upsertEquityPoint(point: EquityPoint): void;
  listEquity(accountId: string, limit: number): EquityPoint[];
}
