import { d, floorToStep, min, ZERO, type ExecutionEngine } from '@mochiprep/core';
import {
  INSTRUMENTS,
  SYMBOLS,
  type AccountBalance,
  type AccountVenue,
  type EquityPoint,
  type Fill,
  type Order,
  type OrderRequest,
  type OrderResult,
  type PaperAccount,
  type PositionView,
  type Quote,
  type RiskSettings,
  type SymbolId,
} from '@mochiprep/shared';
import { OkxRejection, toClOrdId, toInstId, type OkxClient, type OkxPosition, type PlaceOrderBody } from './okx-client';

/** Base units per contract, verified against OKX on connect. */
export const CONTRACT_VALUE: Record<SymbolId, string> = { BTCUSDT: '0.01', ETHUSDT: '0.1', SOLUSDT: '1' };
export const LOT_SIZE = '0.01';
const MAX_SNAPSHOT_AGE_MS = 30_000;
const NEVER_ARRIVED_AFTER_MS = 60_000;
const EQUITY_EVERY_MS = 60_000;

/** The slice of persistence the live engine needs. Orders and fills share the paper tables. */
export interface LiveStore {
  getAccount(id: string): PaperAccount | undefined;
  insertOrder(order: Order): void;
  saveOrder(order: Order): void;
  getOrder(id: string): Order | undefined;
  getOrderByClientId(accountId: string, clientOrderId: string): Order | undefined;
  listOpenOrders(accountId: string): Order[];
  listOrders(accountId: string, limit: number): Order[];
  insertFill(fill: Fill): void;
  listFillsByOrder(orderId: string): Fill[];
  listFills(accountId: string, limit: number): Fill[];
  upsertEquityPoint(point: EquityPoint): void;
  listEquity(accountId: string, limit: number): EquityPoint[];
}

export interface OkxEngineDeps {
  accountId: string;
  venue: Exclude<AccountVenue, 'PAPER'>;
  client: OkxClient;
  store: LiveStore;
  getQuote(symbol: SymbolId): Quote | undefined;
  now(): number;
  isArmed(): boolean;
  limits(): RiskSettings;
  newId?(prefix: string): string;
}

const fromInstId = (instId: string) => SYMBOLS.find((s) => toInstId(s) === instId);

/**
 * Trades through OKX. The exchange is the source of truth for balances and positions; local rows
 * record what MochiPrep asked for, so nothing here pretends to be a ledger of the real account.
 */
export class OkxExecutionEngine implements ExecutionEngine {
  private readonly newId: (prefix: string) => string;
  private balance: AccountBalance | null = null;
  private positions: PositionView[] = [];
  private lastSyncAt: number | null = null;
  private lastEquityAt = 0;
  private syncError: string | null = null;
  private queue: Promise<unknown> = Promise.resolve();

  constructor(private readonly deps: OkxEngineDeps) {
    this.newId = deps.newId ?? ((prefix) => `${prefix}_${crypto.randomUUID()}`);
  }

  get lastSync() {
    return this.lastSyncAt;
  }
  get error() {
    return this.syncError;
  }
  get healthy() {
    return this.syncError === null && this.lastSyncAt !== null && this.deps.now() - this.lastSyncAt <= MAX_SNAPSHOT_AGE_MS;
  }

  /** One order at a time, so two callers can never both pass the same exposure check. */
  private serialized<T>(task: () => Promise<T>): Promise<T> {
    const run = this.queue.then(task, task);
    this.queue = run.catch(() => undefined);
    return run;
  }

  async getBalance(): Promise<AccountBalance> {
    if (!this.balance) throw new Error('No balance from OKX yet');
    return this.balance;
  }

  /** Last synced balance, for read models that must not fail while disconnected. */
  cachedBalance() {
    return this.balance;
  }

  async getPositions(): Promise<PositionView[]> {
    return this.positions;
  }

  async getOpenOrders(accountId: string): Promise<Order[]> {
    return this.deps.store.listOpenOrders(accountId);
  }

  async dailyRisk(accountId: string) {
    const balance = await this.getBalance();
    const dayStart = Math.floor(this.deps.now() / 86_400_000) * 86_400_000;
    const first = this.deps.store.listEquity(accountId, 2000).find((p) => p.ts >= dayStart);
    const base = first?.equity ?? balance.equity;
    return { dailyPnl: d(balance.equity).minus(base).toFixed(), dayStartEquity: base };
  }

  placeOrder(request: OrderRequest): Promise<OrderResult> {
    return this.serialized(() => this.submit(request));
  }

  private buildOrder(request: OrderRequest): Order {
    const now = this.deps.now();
    return {
      id: this.newId('ord'),
      accountId: request.accountId,
      clientOrderId: request.clientOrderId,
      symbol: request.symbol,
      side: request.side,
      type: request.type,
      quantity: request.quantity,
      price: request.price ?? null,
      triggerPrice: null,
      leverage: request.leverage,
      reduceOnly: request.reduceOnly ?? false,
      closePosition: false,
      status: 'OPEN',
      filledQuantity: '0',
      avgFillPrice: null,
      reservedMargin: '0',
      attachedStopLoss: request.stopLoss ?? null,
      attachedTakeProfit: request.takeProfit ?? null,
      rejectReason: null,
      exchangeOrderId: null,
      agentId: request.agentId ?? null,
      decisionId: request.decisionId ?? null,
      createdAt: now,
      updatedAt: now,
    };
  }

  private async submit(request: OrderRequest): Promise<OrderResult> {
    const { store } = this.deps;
    const prior = store.getOrderByClientId(request.accountId, request.clientOrderId);
    if (prior) return { order: prior, fills: store.listFillsByOrder(prior.id), duplicate: true };

    const order = this.buildOrder(request);
    const position = this.positions.find((p) => p.symbol === request.symbol);
    if (order.reduceOnly && position) order.quantity = min(d(order.quantity), d(position.quantity)).toFixed();

    const refusal = this.preTradeRefusal(order, position);
    const contracts = this.toContracts(order);
    if (refusal || contracts.lte(0)) {
      return this.reject(order, refusal ?? 'INVALID_QUANTITY: below one lot');
    }
    order.quantity = contracts.mul(CONTRACT_VALUE[order.symbol]).toFixed();

    // Written before anything is sent, so a crash can never leave an order we do not know about.
    store.insertOrder(order);

    const instId = toInstId(order.symbol);
    const clOrdId = toClOrdId(order.clientOrderId);
    try {
      if (!order.reduceOnly) await this.deps.client.setLeverage(instId, order.leverage);
      order.exchangeOrderId = await this.deps.client.placeOrder(this.toBody(order, instId, clOrdId, contracts.toFixed()));
      store.saveOrder(order);
    } catch (error) {
      if (error instanceof OkxRejection) return this.finishRejected(order, error.message);
      // Unknown outcome: the order stays OPEN and is resolved by lookup. It is never resent.
      order.rejectReason = `Awaiting confirmation: ${(error as Error).message}`;
      store.saveOrder(order);
    }

    await this.syncOrder(order).catch(() => undefined);
    await this.sync().catch(() => undefined);
    return { order: store.getOrder(order.id)!, fills: store.listFillsByOrder(order.id), duplicate: false };
  }

  private toContracts(order: Order) {
    return floorToStep(d(order.quantity).div(CONTRACT_VALUE[order.symbol]), LOT_SIZE);
  }

  private toBody(order: Order, instId: string, clOrdId: string, sz: string): PlaceOrderBody {
    const algo: Record<string, string> = {};
    if (order.attachedStopLoss) Object.assign(algo, { slTriggerPx: order.attachedStopLoss, slOrdPx: '-1' });
    if (order.attachedTakeProfit) Object.assign(algo, { tpTriggerPx: order.attachedTakeProfit, tpOrdPx: '-1' });
    return {
      instId,
      tdMode: 'isolated',
      side: order.side === 'BUY' ? 'buy' : 'sell',
      ordType: order.type === 'LIMIT' ? 'limit' : 'market',
      sz,
      clOrdId,
      ...(order.type === 'LIMIT' ? { px: order.price! } : {}),
      ...(order.reduceOnly ? { reduceOnly: true } : {}),
      ...(Object.keys(algo).length ? { attachAlgoOrds: [algo] } : {}),
    };
  }

  private reject(order: Order, reason: string): OrderResult {
    order.status = 'REJECTED';
    order.rejectReason = reason;
    this.deps.store.insertOrder(order);
    return { order, fills: [], duplicate: false };
  }

  private finishRejected(order: Order, reason: string): OrderResult {
    order.status = 'REJECTED';
    order.rejectReason = reason;
    order.updatedAt = this.deps.now();
    this.deps.store.saveOrder(order);
    return { order, fills: [], duplicate: false };
  }

  /** The last line of defence: every order passes here, from the UI or from an agent. */
  private preTradeRefusal(order: Order, position: PositionView | undefined): string | null {
    if (order.reduceOnly) {
      if (!position || (position.side === 'LONG') === (order.side === 'BUY')) return 'REDUCE_ONLY_NO_POSITION: nothing to reduce';
      return null;
    }
    const limits = this.deps.limits();
    if (!this.deps.isArmed()) return 'NOT_ARMED: real-money orders are switched off for this account';
    if (!this.healthy || !this.balance) return `EXCHANGE_NOT_SYNCED: ${this.syncError ?? 'no recent sync with OKX'}`;

    const quote = this.deps.getQuote(order.symbol);
    if (!quote) return 'NO_MARKET_DATA: no quote';
    const age = (this.deps.now() - quote.ts) / 1000;
    if (age > limits.maxQuoteAgeSec) return `MARKET_DATA_STALE: quote is ${age.toFixed(0)}s old`;

    if (order.leverage > limits.maxLeverage) return `LIMIT_LEVERAGE: ${order.leverage}x exceeds ${limits.maxLeverage}x`;
    if (position) return `LIMIT_EXISTING_POSITION: already ${position.side} ${order.symbol}`;
    if (this.positions.length >= limits.maxOpenPositions) return `LIMIT_POSITIONS: ${this.positions.length} open, limit ${limits.maxOpenPositions}`;

    const reference = d(order.type === 'LIMIT' ? order.price! : order.side === 'BUY' ? quote.ask : quote.bid);
    const stop = order.attachedStopLoss;
    if (!stop && limits.requireStopLoss) return 'LIMIT_STOP_REQUIRED: new positions must carry a stop loss';
    if (stop && (order.side === 'BUY' ? d(stop).gte(reference) : d(stop).lte(reference))) {
      return `LIMIT_STOP_INVALID: stop ${stop} is on the wrong side of ${reference.toFixed()}`;
    }

    const notional = d(order.quantity).mul(reference);
    if (notional.gt(limits.maxNotional)) return `LIMIT_NOTIONAL: ${notional.toFixed(2)} USDT exceeds ${limits.maxNotional} USDT`;
    const margin = notional.div(order.leverage);
    if (margin.gt(this.balance.availableBalance)) return `INSUFFICIENT_MARGIN: needs ${margin.toFixed(2)} USDT`;

    const equity = d(this.balance.equity);
    const dayStart = Math.floor(this.deps.now() / 86_400_000) * 86_400_000;
    const base = d(this.deps.store.listEquity(order.accountId, 2000).find((p) => p.ts >= dayStart)?.equity ?? equity);
    if (equity.minus(base).lte(base.mul(limits.maxDailyLossPct).div(-100))) return `LIMIT_DAILY_LOSS: down ${base.minus(equity).toFixed(2)} USDT today`;
    return null;
  }

  cancelOrder(orderId: string): Promise<void> {
    return this.serialized(async () => {
      const order = this.deps.store.getOrder(orderId);
      if (!order) throw new Error(`Unknown order ${orderId}`);
      if (order.status !== 'OPEN') throw new Error(`Order ${orderId} is ${order.status}`);
      await this.deps.client.cancelOrder(toInstId(order.symbol), toClOrdId(order.clientOrderId));
      await this.syncOrder(order);
    });
  }

  /** Brings one local order in line with OKX. */
  private async syncOrder(order: Order) {
    const { store, client, now } = this.deps;
    const remote = await client.getOrder(toInstId(order.symbol), toClOrdId(order.clientOrderId));
    const at = now();
    if (!remote) {
      if (at - order.createdAt >= NEVER_ARRIVED_AFTER_MS) {
        order.status = 'CANCELED';
        order.rejectReason = 'OKX has no record of this order';
        order.updatedAt = at;
        store.saveOrder(order);
      }
      return;
    }
    order.exchangeOrderId = remote.ordId;
    order.rejectReason = null;
    const filled = d(remote.accFillSz || 0).mul(CONTRACT_VALUE[order.symbol]);
    order.filledQuantity = filled.toFixed();
    if (remote.avgPx) order.avgFillPrice = remote.avgPx;
    order.updatedAt = at;
    if (remote.state === 'filled') order.status = 'FILLED';
    else if (remote.state === 'canceled' || remote.state === 'mmp_canceled') order.status = 'CANCELED';
    store.saveOrder(order);

    if (order.status !== 'OPEN' && filled.gt(0) && store.listFillsByOrder(order.id).length === 0) {
      store.insertFill({
        id: `fill_okx_${remote.ordId}`,
        orderId: order.id,
        accountId: order.accountId,
        symbol: order.symbol,
        side: order.side,
        quantity: filled.toFixed(),
        price: remote.avgPx,
        fee: d(remote.fee || 0).abs().toFixed(),
        realizedPnl: remote.pnl || '0',
        liquidity: 'TAKER',
        ts: at,
      });
    }
  }

  /** Full reconciliation with the exchange. Throws, and marks the account unhealthy, on failure. */
  async sync(): Promise<void> {
    const { client, store, accountId, now } = this.deps;
    try {
      const [usdt, remote] = await Promise.all([client.getUsdtBalance(), client.getPositions()]);
      for (const order of store.listOpenOrders(accountId)) await this.syncOrder(order);
      this.positions = remote.filter((p) => Number(p.pos) !== 0).flatMap((p) => this.toView(p));
      this.balance = this.toBalance(usdt);
      this.lastSyncAt = now();
      this.syncError = null;
      if (this.lastSyncAt - this.lastEquityAt >= EQUITY_EVERY_MS) {
        this.lastEquityAt = this.lastSyncAt;
        store.upsertEquityPoint({
          accountId,
          ts: this.lastSyncAt,
          equity: this.balance.equity,
          wallet: this.balance.walletBalance,
          unrealized: this.balance.unrealizedPnl,
        });
      }
    } catch (error) {
      this.syncError = (error as Error).message;
      throw error;
    }
  }

  private toBalance(usdt: Record<string, string> | null): AccountBalance {
    const { store, accountId } = this.deps;
    const equity = d(usdt?.eq || 0);
    const unrealized = d(usdt?.upl || 0);
    const available = d(usdt?.availEq || usdt?.availBal || 0);
    const fills = store.listFills(accountId, 5000);
    return {
      accountId,
      initialBalance: store.getAccount(accountId)?.initialBalance ?? '0',
      walletBalance: equity.minus(unrealized).toFixed(),
      availableBalance: available.toFixed(),
      usedMargin: equity.minus(available).toFixed(),
      orderMargin: '0',
      equity: equity.toFixed(),
      realizedPnl: fills.reduce((sum, f) => sum.plus(f.realizedPnl), ZERO).toFixed(),
      unrealizedPnl: unrealized.toFixed(),
      feesPaid: fills.reduce((sum, f) => sum.plus(f.fee), ZERO).toFixed(),
      fundingPaid: '0',
    };
  }

  private toView(p: OkxPosition): PositionView[] {
    const symbol = fromInstId(p.instId);
    if (!symbol) return [];
    const { store, accountId, now } = this.deps;
    const quantity = d(p.pos).abs().mul(CONTRACT_VALUE[symbol]).toFixed();
    const entry = store.listOrders(accountId, 200).find((o) => o.symbol === symbol && !o.reduceOnly && o.status === 'FILLED');
    return [
      {
        id: `okx:${p.instId}`,
        accountId,
        symbol,
        side: Number(p.pos) > 0 ? 'LONG' : 'SHORT',
        quantity,
        entryPrice: p.avgPx,
        leverage: Number(p.lever) || 1,
        margin: p.margin || '0',
        liquidationPrice: p.liqPx || '0',
        status: 'OPEN',
        realizedPnl: '0',
        feesPaid: '0',
        fundingPaid: '0',
        maxQuantity: quantity,
        exitPrice: null,
        lastFundingTime: now(),
        openedAt: entry?.createdAt ?? now(),
        closedAt: null,
        markPrice: p.markPx || null,
        unrealizedPnl: p.upl || '0',
        stopLoss: entry?.attachedStopLoss ?? null,
        takeProfit: entry?.attachedTakeProfit ?? null,
      },
    ];
  }

  /** Reduce-only market close of a fraction. Works even when disarmed: exits must always be possible. */
  closePosition(accountId: string, symbol: SymbolId, fraction: number, clientOrderId: string) {
    const position = this.positions.find((p) => p.symbol === symbol);
    if (!position) throw new Error(`No open ${symbol} position`);
    const quantity = fraction >= 1 ? d(position.quantity) : floorToStep(d(position.quantity).mul(fraction), INSTRUMENTS[symbol].quantityStep);
    return this.placeOrder({
      accountId,
      clientOrderId,
      symbol,
      side: position.side === 'LONG' ? 'SELL' : 'BUY',
      type: 'MARKET',
      quantity: quantity.toFixed(),
      leverage: position.leverage,
      reduceOnly: true,
    });
  }
}

