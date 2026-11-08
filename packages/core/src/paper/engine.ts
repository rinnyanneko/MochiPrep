import {
  DEFAULT_ACCOUNT_CONFIG,
  type AccountBalance,
  type DataSource,
  type Fill,
  type LedgerEventType,
  type Order,
  type OrderRequest,
  type OrderResult,
  type PaperAccount,
  type PaperAccountConfig,
  type Position,
  type PositionSide,
  type PositionView,
  type Protection,
  type Quote,
  type SymbolId,
  type TrailingStopSpec,
  type TrailingStopView,
} from '@mochiprep/shared';
import { d, floorToStep, max, min, roundToStep, str, ZERO, type Dec } from '../decimal';
import { getInstrument } from '../instruments';
import type { ExecutionEngine, ProtectionResult } from '../trading/execution';
import {
  isMarketable,
  isRestingLimitFilled,
  isStopTriggered,
  isTakeProfitTriggered,
  marketFillPrice,
} from '../trading/fill-model';
import { linearPerp } from '../trading/perp';
import { advanceExtreme, trailingTrigger, validateTrailing } from '../trading/trailing';
import type { PaperStore } from './store';

/** No fill may be priced off a quote older than this, whatever the caller's own limits are. */
const MAX_FILL_QUOTE_AGE_MS = 60_000;
const money = (v: Dec) => v.toDecimalPlaces(8);

export type EngineEvent =
  | { kind: 'fill'; fill: Fill; order: Order }
  | { kind: 'liquidation'; position: Position }
  | { kind: 'funding'; accountId: string; symbol: SymbolId; amount: string }
  | { kind: 'order'; order: Order };

export interface PaperEngineDeps {
  store: PaperStore;
  getQuote(source: DataSource, symbol: SymbolId): Quote | undefined;
  now(source: DataSource): number;
  newId?(prefix: string): string;
  onEvent?(event: EngineEvent): void;
}

class OrderRejection extends Error {
  constructor(readonly code: string, detail: string) {
    super(`${code}: ${detail}`);
  }
}

const sideOf = (order: { side: 'BUY' | 'SELL' }): PositionSide => (order.side === 'BUY' ? 'LONG' : 'SHORT');

/** The spec a TRAILING_STOP_MARKET order was created from. */
const trailingSpecOf = (order: Order): TrailingStopSpec => ({
  kind: order.trailingKind!,
  callback: order.trailingCallback!,
  activationPrice: order.trailingActivationPrice,
});

const sameTrailing = (a: TrailingStopSpec, b: TrailingStopSpec) =>
  a.kind === b.kind &&
  d(a.callback).eq(b.callback) &&
  (a.activationPrice === null ? b.activationPrice === null : b.activationPrice !== null && d(a.activationPrice).eq(b.activationPrice));

const attachedProtection = (order: Order): Protection => ({
  stopLoss: order.attachedStopLoss,
  takeProfit: order.attachedTakeProfit,
  trailingStop: order.attachedTrailingStop,
});

/** A self-contained perpetuals simulator. See docs/PAPER_ENGINE.md for the exact rules. */
export class PaperExecutionEngine implements ExecutionEngine {
  private readonly store: PaperStore;
  private readonly newId: (prefix: string) => string;
  private queued: EngineEvent[] = [];

  constructor(private readonly deps: PaperEngineDeps) {
    this.store = deps.store;
    this.newId = deps.newId ?? ((prefix) => `${prefix}_${crypto.randomUUID()}`);
  }

  // ---- accounts -------------------------------------------------------------------------

  createAccount(args: {
    name: string;
    source: DataSource;
    initialBalance?: string;
    isDemo?: boolean;
    config?: Partial<PaperAccountConfig>;
  }): PaperAccount {
    const now = this.deps.now(args.source);
    const account: PaperAccount = {
      id: this.newId('acct'),
      name: args.name,
      source: args.source,
      venue: 'PAPER',
      exchangeUid: null,
      liveLimits: null,
      isDemo: args.isDemo ?? false,
      initialBalance: args.initialBalance ?? '10000',
      walletBalance: '0',
      usedMargin: '0',
      realizedPnl: '0',
      feesPaid: '0',
      fundingPaid: '0',
      config: { ...DEFAULT_ACCOUNT_CONFIG, ...args.config },
      createdAt: now,
    };
    this.store.transaction(() => {
      this.store.insertAccount(account);
      this.seedAccount(account, now);
    });
    return this.requireAccount(account.id);
  }

  /** Wipes orders, fills, positions, ledger and equity history, then redeposits the initial balance. */
  resetAccount(accountId: string): PaperAccount {
    const account = this.requireAccount(accountId);
    if (account.venue !== 'PAPER') throw new Error('Exchange accounts cannot be reset');
    const now = this.deps.now(account.source);
    this.store.transaction(() => {
      this.store.deleteAccountData(accountId, true);
      Object.assign(account, { walletBalance: '0', usedMargin: '0', realizedPnl: '0', feesPaid: '0', fundingPaid: '0' });
      this.seedAccount(account, now);
    });
    return this.requireAccount(accountId);
  }

  private seedAccount(account: PaperAccount, now: number) {
    this.post(account, { type: 'DEPOSIT', amount: d(account.initialBalance), memo: 'Initial virtual balance', ts: now });
    this.store.saveAccount(account);
    this.store.upsertEquityPoint({
      accountId: account.id,
      ts: now,
      equity: account.initialBalance,
      wallet: account.initialBalance,
      unrealized: '0',
    });
  }

  private requireAccount(id: string): PaperAccount {
    const account = this.store.getAccount(id);
    if (!account) throw new Error(`Unknown account ${id}`);
    return account;
  }

  // ---- ExecutionEngine ------------------------------------------------------------------

  async placeOrder(request: OrderRequest): Promise<OrderResult> {
    return this.placeOrderSync(request);
  }

  async cancelOrder(orderId: string): Promise<void> {
    this.cancelOrderSync(orderId);
  }

  async getPositions(accountId: string): Promise<PositionView[]> {
    return this.positionViews(accountId);
  }

  async getOpenOrders(accountId: string): Promise<Order[]> {
    return this.store.listOpenOrders(accountId);
  }

  async getBalance(accountId: string): Promise<AccountBalance> {
    return this.balanceOf(accountId);
  }

  async setProtection(
    accountId: string,
    symbol: SymbolId,
    protection: Protection,
    _meta: { clientOrderId: string; agentId: string | null; decisionId: string | null },
  ): Promise<ProtectionResult> {
    return this.setProtectionSync(accountId, symbol, protection);
  }

  async protectionOf(accountId: string, symbol: SymbolId): Promise<Protection> {
    return this.protectionOfSync(accountId, symbol);
  }

  // ---- orders ---------------------------------------------------------------------------

  placeOrderSync(request: OrderRequest): OrderResult {
    const prior = this.store.getOrderByClientId(request.accountId, request.clientOrderId);
    if (prior) return { order: prior, fills: this.store.listFillsByOrder(prior.id), duplicate: true };

    const account = this.requireAccount(request.accountId);
    const now = this.deps.now(account.source);
    try {
      const result = this.store.transaction(() => this.executeRequest(account, request, now));
      this.flush();
      return result;
    } catch (error) {
      this.queued = [];
      if (!(error instanceof OrderRejection)) throw error;
      // The attempt rolled back; only the rejected order itself is recorded.
      const order = this.buildOrder(request, now);
      order.status = 'REJECTED';
      order.rejectReason = error.message;
      this.store.insertOrder(order);
      this.deps.onEvent?.({ kind: 'order', order });
      return { order, fills: [], duplicate: false };
    }
  }

  cancelOrderSync(orderId: string): void {
    const order = this.store.getOrder(orderId);
    if (!order) throw new Error(`Unknown order ${orderId}`);
    if (order.status !== 'OPEN') throw new Error(`Order ${orderId} is ${order.status}`);
    const account = this.requireAccount(order.accountId);
    this.closeOrder(order, 'CANCELED', null, this.deps.now(account.source));
    this.deps.onEvent?.({ kind: 'order', order });
  }

  /** Closes a fraction of a position with a reduce-only market order. */
  closePosition(accountId: string, symbol: SymbolId, fraction: number, clientOrderId: string): OrderResult {
    const position = this.store.getOpenPosition(accountId, symbol);
    if (!position) throw new Error(`No open ${symbol} position`);
    const instrument = getInstrument(symbol);
    const quantity =
      fraction >= 1 ? d(position.quantity) : floorToStep(d(position.quantity).mul(fraction), instrument.quantityStep);
    return this.placeOrderSync({
      accountId,
      clientOrderId,
      symbol,
      side: position.side === 'LONG' ? 'SELL' : 'BUY',
      type: 'MARKET',
      quantity: str(quantity),
      leverage: position.leverage,
      reduceOnly: true,
    });
  }

  /** Replaces the whole protection of an open position. A null leg removes that leg. */
  setProtectionSync(accountId: string, symbol: SymbolId, protection: Protection): ProtectionResult {
    const account = this.requireAccount(accountId);
    const position = this.store.getOpenPosition(accountId, symbol);
    if (!position) throw new Error(`No open ${symbol} position`);
    const quote = this.deps.getQuote(account.source, symbol);
    if (!quote) throw new Error('No market data');
    this.validateProtection(position.side, d(quote.last), protection, getInstrument(symbol).priceTick);
    const before = this.protectionOfSync(accountId, symbol);
    const now = this.deps.now(account.source);
    this.store.transaction(() => {
      this.replaceProtection(position, protection, quote.last, now);
    });
    this.flush();
    return { before, after: this.protectionOfSync(accountId, symbol) };
  }

  /** The stop-loss, take-profit and trailing stop resting for a symbol; null legs are not set. */
  protectionOfSync(accountId: string, symbol: SymbolId): Protection {
    this.requireAccount(accountId);
    const orders = this.store.listOpenOrders(accountId).filter((o) => o.symbol === symbol && o.closePosition);
    const trailing = orders.find((o) => o.type === 'TRAILING_STOP_MARKET');
    return {
      stopLoss: orders.find((o) => o.type === 'STOP_MARKET')?.triggerPrice ?? null,
      takeProfit: orders.find((o) => o.type === 'TAKE_PROFIT_MARKET')?.triggerPrice ?? null,
      trailingStop: trailing ? trailingSpecOf(trailing) : null,
    };
  }

  private buildOrder(request: OrderRequest, now: number): Order {
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
      trailingKind: null,
      trailingCallback: null,
      trailingActivationPrice: null,
      trailingExtreme: null,
      attachedTrailingStop: request.trailingStop ?? null,
      rejectReason: null,
      exchangeOrderId: null,
      agentId: request.agentId ?? null,
      decisionId: request.decisionId ?? null,
      createdAt: now,
      updatedAt: now,
    };
  }

  private executeRequest(account: PaperAccount, request: OrderRequest, now: number): OrderResult {
    const instrument = getInstrument(request.symbol);
    const order = this.buildOrder(request, now);
    const quote = this.deps.getQuote(account.source, request.symbol);
    if (!quote) throw new OrderRejection('NO_MARKET_DATA', `no quote for ${request.symbol}`);
    if (now - quote.ts > MAX_FILL_QUOTE_AGE_MS) {
      throw new OrderRejection('MARKET_DATA_STALE', `quote is ${Math.round((now - quote.ts) / 1000)}s old`);
    }
    if (request.leverage < 1 || request.leverage > instrument.maxLeverage) {
      throw new OrderRejection('INVALID_LEVERAGE', `${request.leverage}x outside 1-${instrument.maxLeverage}x`);
    }

    let quantity = floorToStep(d(request.quantity), instrument.quantityStep);
    const position = this.store.getOpenPosition(account.id, request.symbol);
    if (order.reduceOnly) {
      if (!position || position.side === sideOf(order)) {
        throw new OrderRejection('REDUCE_ONLY_NO_POSITION', 'nothing to reduce');
      }
      if (request.stopLoss || request.takeProfit) {
        throw new OrderRejection('INVALID_PROTECTION', 'reduce-only orders cannot attach SL/TP');
      }
      if (request.trailingStop) {
        throw new OrderRejection('INVALID_PROTECTION', 'reduce-only orders cannot attach a trailing stop');
      }
      quantity = min(quantity, d(position.quantity));
    }
    if (quantity.lt(instrument.minQuantity)) {
      throw new OrderRejection('INVALID_QUANTITY', `minimum is ${instrument.minQuantity}`);
    }
    order.quantity = str(quantity);

    if (request.type === 'LIMIT') {
      if (!request.price) throw new OrderRejection('INVALID_PRICE', 'limit orders need a price');
      const limit = roundToStep(d(request.price), instrument.priceTick);
      if (limit.lte(0)) throw new OrderRejection('INVALID_PRICE', 'price rounds to zero');
      order.price = str(limit);
    }

    const reference = request.type === 'LIMIT' ? d(order.price!) : d(request.side === 'BUY' ? quote.ask : quote.bid);
    if (!order.reduceOnly) {
      this.validateProtection(sideOf(order), reference, attachedProtection(order), instrument.priceTick);
    }

    const crosses = request.type === 'MARKET' || isMarketable(request.side, order.price!, quote);
    if (!crosses) {
      if (!order.reduceOnly) {
        const notional = quantity.mul(order.price!);
        const margin = linearPerp.initialMargin(notional, position?.leverage ?? request.leverage);
        const reserve = money(margin.plus(notional.mul(account.config.makerFeeRate)));
        const available = this.availableOf(account);
        if (reserve.gt(available)) {
          throw new OrderRejection('INSUFFICIENT_MARGIN', `needs ${reserve.toFixed(2)}, available ${available.toFixed(2)}`);
        }
        order.reservedMargin = str(reserve);
      }
      this.store.insertOrder(order);
      this.queued.push({ kind: 'order', order });
      return { order, fills: [], duplicate: false };
    }

    let price = marketFillPrice(request.side, quantity, quote, account.config, instrument);
    if (request.type === 'LIMIT') {
      // Marketable limit: slippage may not push the fill past the limit.
      price = request.side === 'BUY' ? min(price, d(order.price!)) : max(price, d(order.price!));
    }
    this.store.insertOrder(order);
    const fill = this.fillOrder(account, order, quantity, price, 'TAKER', quote.last, now);
    return { order, fills: [fill], duplicate: false };
  }

  private validateProtection(side: PositionSide, reference: Dec, protection: Protection, tick: string) {
    const { stopLoss, takeProfit, trailingStop } = protection;
    if (stopLoss) {
      const ok = side === 'LONG' ? d(stopLoss).lt(reference) : d(stopLoss).gt(reference);
      if (!ok) throw new OrderRejection('INVALID_PROTECTION', `stop ${stopLoss} is on the wrong side of ${reference.toFixed()} for ${side}`);
    }
    if (takeProfit) {
      const ok = side === 'LONG' ? d(takeProfit).gt(reference) : d(takeProfit).lt(reference);
      if (!ok) throw new OrderRejection('INVALID_PROTECTION', `target ${takeProfit} is on the wrong side of ${reference.toFixed()} for ${side}`);
    }
    if (trailingStop) {
      const refused = validateTrailing(side, reference, trailingStop, d(tick));
      if (refused) throw new OrderRejection('INVALID_PROTECTION', refused);
    }
  }

  // ---- fills & accounting ---------------------------------------------------------------

  /**
   * Applies one fill to the position and ledger. Must run inside a transaction. `last` is the
   * last price of the quote the fill came from; a trailing stop attached on fill starts there.
   */
  private fillOrder(
    account: PaperAccount,
    order: Order,
    quantity: Dec,
    price: Dec,
    liquidity: 'MAKER' | 'TAKER',
    last: string,
    ts: number,
  ): Fill {
    const instrument = getInstrument(order.symbol);
    const math = linearPerp;
    const feeRate = liquidity === 'MAKER' ? account.config.makerFeeRate : account.config.takerFeeRate;
    const fee = money(math.notional(quantity, price).mul(feeRate));
    const fillId = this.newId('fill');
    const refs = { symbol: order.symbol, orderId: order.id, fillId, ts };
    const targetSide = sideOf(order);

    let position = this.store.getOpenPosition(account.id, order.symbol);
    let remaining = quantity;
    let realized = ZERO;

    if (position && position.side !== targetSide) {
      const held = d(position.quantity);
      const closeQty = min(remaining, held);
      const fullClose = closeQty.eq(held);
      realized = money(math.pnl(position.side, position.entryPrice, price, closeQty));
      const release = fullClose ? d(position.margin) : money(d(position.margin).mul(closeQty).div(held));
      const closeFee = closeQty.eq(quantity) ? fee : money(fee.mul(closeQty).div(quantity));

      this.post(account, { ...refs, type: 'REALIZED_PNL', amount: realized, positionId: position.id, memo: `Closed ${closeQty.toFixed()} @ ${price.toFixed()}` });
      this.post(account, { ...refs, type: 'MARGIN_RELEASE', marginDelta: release.neg(), positionId: position.id, memo: 'Margin released' });

      position.quantity = str(held.minus(closeQty));
      position.margin = str(d(position.margin).minus(release));
      position.realizedPnl = str(d(position.realizedPnl).plus(realized));
      position.feesPaid = str(d(position.feesPaid).plus(closeFee));
      position.exitPrice = str(price);
      if (fullClose) {
        position.status = 'CLOSED';
        position.closedAt = ts;
        this.store.savePosition(position);
        this.cancelProtection(account.id, order.symbol, order.id, ts);
        position = undefined;
      } else {
        this.refreshLiquidation(position, instrument.maintenanceMarginRate);
        this.store.savePosition(position);
      }
      remaining = remaining.minus(closeQty);
    }

    const opensExposure = remaining.gt(0);
    if (opensExposure) {
      const openFee = remaining.eq(quantity) ? fee : money(fee.mul(remaining).div(quantity));
      const leverage = position?.leverage ?? order.leverage;
      const margin = money(math.initialMargin(math.notional(remaining, price), leverage));
      if (position) {
        const held = d(position.quantity);
        const total = held.plus(remaining);
        position.entryPrice = str(money(held.mul(position.entryPrice).plus(remaining.mul(price)).div(total)));
        position.quantity = str(total);
        position.margin = str(d(position.margin).plus(margin));
        position.feesPaid = str(d(position.feesPaid).plus(openFee));
        position.maxQuantity = str(max(d(position.maxQuantity), total));
        this.refreshLiquidation(position, instrument.maintenanceMarginRate);
        this.store.savePosition(position);
      } else {
        const interval = instrument.fundingIntervalHours * 3_600_000;
        position = {
          id: this.newId('pos'),
          accountId: account.id,
          symbol: order.symbol,
          side: targetSide,
          quantity: str(remaining),
          entryPrice: str(price),
          leverage,
          margin: str(margin),
          liquidationPrice: '0',
          status: 'OPEN',
          realizedPnl: '0',
          feesPaid: str(openFee),
          fundingPaid: '0',
          maxQuantity: str(remaining),
          exitPrice: null,
          lastFundingTime: Math.floor(ts / interval) * interval,
          openedAt: ts,
          closedAt: null,
        };
        this.refreshLiquidation(position, instrument.maintenanceMarginRate);
        this.store.insertPosition(position);
      }
      this.post(account, { ...refs, type: 'MARGIN_LOCK', marginDelta: margin, positionId: position.id, memo: `Isolated margin at ${leverage}x` });
    }

    this.post(account, { ...refs, type: 'TRADE_FEE', amount: fee.neg(), positionId: position?.id ?? null, memo: `${liquidity} fee at ${feeRate}` });

    const fill: Fill = {
      id: fillId,
      orderId: order.id,
      accountId: account.id,
      symbol: order.symbol,
      side: order.side,
      quantity: str(quantity),
      price: str(price),
      fee: str(fee),
      realizedPnl: str(realized),
      liquidity,
      ts,
    };
    this.store.insertFill(fill);

    order.filledQuantity = str(quantity);
    order.avgFillPrice = str(price);
    order.status = 'FILLED';
    order.reservedMargin = '0';
    order.updatedAt = ts;
    this.store.saveOrder(order);
    this.store.saveAccount(account);

    if (opensExposure) {
      const available = this.availableOf(account);
      if (available.lt(0)) {
        throw new OrderRejection('INSUFFICIENT_MARGIN', `short by ${available.neg().toFixed(2)} USDT after fee`);
      }
      if (position && (order.attachedStopLoss || order.attachedTakeProfit || order.attachedTrailingStop)) {
        this.replaceProtection(position, attachedProtection(order), last, ts);
      }
    }
    this.queued.push({ kind: 'fill', fill, order });
    return fill;
  }

  private refreshLiquidation(position: Position, maintenanceRate: string) {
    position.liquidationPrice = str(
      money(
        linearPerp.liquidationPrice({
          side: position.side,
          entry: position.entryPrice,
          quantity: position.quantity,
          margin: position.margin,
          maintenanceRate,
        }),
      ),
    );
  }

  private post(
    account: PaperAccount,
    e: {
      type: LedgerEventType;
      amount?: Dec;
      marginDelta?: Dec;
      memo: string;
      ts: number;
      symbol?: SymbolId | null;
      orderId?: string | null;
      fillId?: string | null;
      positionId?: string | null;
    },
  ) {
    const amount = e.amount ?? ZERO;
    const marginDelta = e.marginDelta ?? ZERO;
    if (amount.isZero() && marginDelta.isZero()) return;
    account.walletBalance = str(d(account.walletBalance).plus(amount));
    account.usedMargin = str(d(account.usedMargin).plus(marginDelta));
    if (e.type === 'REALIZED_PNL' || e.type === 'LIQUIDATION_PENALTY') {
      account.realizedPnl = str(d(account.realizedPnl).plus(amount));
    }
    if (e.type === 'TRADE_FEE') account.feesPaid = str(d(account.feesPaid).minus(amount));
    if (e.type === 'FUNDING') account.fundingPaid = str(d(account.fundingPaid).minus(amount));
    this.store.appendLedger({
      id: this.newId('evt'),
      accountId: account.id,
      type: e.type,
      amount: str(amount),
      marginDelta: str(marginDelta),
      walletAfter: account.walletBalance,
      symbol: e.symbol ?? null,
      orderId: e.orderId ?? null,
      fillId: e.fillId ?? null,
      positionId: e.positionId ?? null,
      memo: e.memo,
      ts: e.ts,
    });
  }

  // ---- protection orders ----------------------------------------------------------------

  /**
   * Cancels every protection order on the position and creates the requested ones. `last` is the
   * last price of the quote in hand: a trailing stop without an activation price starts tracking
   * there. An unchanged trailing stop keeps the extreme it had tracked, so restating it (to change
   * only the stop-loss, say) never loosens its trigger or puts an activated one back to sleep.
   */
  private replaceProtection(position: Position, protection: Protection, last: string, ts: number) {
    const { stopLoss, takeProfit, trailingStop } = protection;
    let carried: string | null | undefined;
    for (const order of this.store.listOpenOrders(position.accountId)) {
      if (order.symbol !== position.symbol || !order.closePosition) continue;
      if (order.type === 'TRAILING_STOP_MARKET' && trailingStop && sameTrailing(trailingSpecOf(order), trailingStop)) {
        carried = order.trailingExtreme;
      }
      this.closeOrder(order, 'CANCELED', null, ts);
    }
    const make = (type: 'STOP_MARKET' | 'TAKE_PROFIT_MARKET' | 'TRAILING_STOP_MARKET', trigger: string | null, extra: Partial<Order> = {}) => {
      const order: Order = {
        ...this.buildOrder(
          {
            accountId: position.accountId,
            clientOrderId: this.newId('protect'),
            symbol: position.symbol,
            side: position.side === 'LONG' ? 'SELL' : 'BUY',
            type: 'MARKET',
            quantity: position.quantity,
            leverage: position.leverage,
            reduceOnly: true,
          },
          ts,
        ),
        type,
        triggerPrice: trigger,
        closePosition: true,
        ...extra,
      };
      this.store.insertOrder(order);
      this.queued.push({ kind: 'order', order });
    };
    if (stopLoss) make('STOP_MARKET', stopLoss);
    if (takeProfit) make('TAKE_PROFIT_MARKET', takeProfit);
    if (trailingStop) {
      const fresh = trailingStop.activationPrice === null ? str(d(last)) : null;
      make('TRAILING_STOP_MARKET', null, {
        trailingKind: trailingStop.kind,
        trailingCallback: trailingStop.callback,
        trailingActivationPrice: trailingStop.activationPrice,
        trailingExtreme: carried === undefined ? fresh : carried,
      });
    }
  }

  /** Flat positions cannot be reduced: clears every reduce-only order on the symbol. */
  private cancelProtection(accountId: string, symbol: SymbolId, exceptOrderId: string | null, ts: number) {
    for (const order of this.store.listOpenOrders(accountId)) {
      if (order.symbol === symbol && order.reduceOnly && order.id !== exceptOrderId) {
        this.closeOrder(order, 'CANCELED', 'Position closed', ts);
      }
    }
  }

  private closeOrder(order: Order, status: 'CANCELED', reason: string | null, ts: number) {
    order.status = status;
    order.reservedMargin = '0';
    order.rejectReason = reason;
    order.updatedAt = ts;
    this.store.saveOrder(order);
  }

  // ---- market events --------------------------------------------------------------------

  /**
   * Advances the simulation by one observed quote. Order of evaluation is fixed and pessimistic:
   * funding, liquidation, stop-loss, trailing stop, take-profit, resting limits. Gaps between
   * quotes are never interpolated, so a stop fills at the first quote past it.
   */
  onQuote(quote: Quote): void {
    const { source, symbol } = quote;
    const instrument = getInstrument(symbol);

    for (const position of this.store.listOpenPositionsForMarket(source, symbol)) {
      this.applyFunding(position, quote, instrument.fundingIntervalHours * 3_600_000);
    }

    for (const position of this.store.listOpenPositionsForMarket(source, symbol)) {
      const liquidated = position.side === 'LONG'
        ? d(quote.mark).lte(position.liquidationPrice)
        : d(quote.mark).gte(position.liquidationPrice);
      if (liquidated) this.liquidate(position, quote);
    }

    const open = this.store.listOpenOrdersForMarket(source, symbol);
    const rank = { STOP_MARKET: 0, TRAILING_STOP_MARKET: 1, TAKE_PROFIT_MARKET: 2, LIMIT: 3, MARKET: 4 } as const;
    open.sort((a, b) => rank[a.type] - rank[b.type] || a.createdAt - b.createdAt);
    for (const candidate of open) {
      // An earlier order in this pass may have closed the position and canceled this one.
      const order = this.store.getOrder(candidate.id);
      if (order?.status === 'OPEN') this.evaluateOrder(order, quote);
    }
    this.flush();
  }

  private applyFunding(position: Position, quote: Quote, interval: number) {
    let next = position.lastFundingTime + interval;
    if (next > quote.ts) return;
    const account = this.requireAccount(position.accountId);
    this.store.transaction(() => {
      while (next <= quote.ts) {
        const payment = quote.fundingRate === null
          ? ZERO
          : money(linearPerp.funding(position.side, quote.mark, position.quantity, quote.fundingRate));
        this.post(account, {
          type: 'FUNDING',
          amount: payment.neg(),
          symbol: position.symbol,
          positionId: position.id,
          memo: `Funding at rate ${quote.fundingRate ?? 'unavailable'}, mark ${quote.mark}`,
          ts: next,
        });
        position.fundingPaid = str(d(position.fundingPaid).plus(payment));
        position.lastFundingTime = next;
        if (!payment.isZero()) {
          this.queued.push({ kind: 'funding', accountId: account.id, symbol: position.symbol, amount: str(payment.neg()) });
        }
        next += interval;
      }
      this.store.savePosition(position);
      this.store.saveAccount(account);
    });
  }

  /**
   * Simplified isolated liquidation: the position is force-closed at its liquidation price and
   * the whole isolated margin is forfeited. Loss is capped at that margin, even through a gap.
   */
  private liquidate(position: Position, quote: Quote) {
    const account = this.requireAccount(position.accountId);
    const instrument = getInstrument(position.symbol);
    const ts = quote.ts;
    const margin = d(position.margin);
    const quantity = d(position.quantity);
    const price = roundToStep(d(position.liquidationPrice), instrument.priceTick);
    const pnl = max(money(linearPerp.pnl(position.side, position.entryPrice, price, quantity)), margin.neg());
    const closing = position.side === 'LONG' ? 'SELL' : 'BUY';

    this.store.transaction(() => {
      const order: Order = {
        ...this.buildOrder(
          {
            accountId: account.id,
            clientOrderId: `liquidation:${position.id}`,
            symbol: position.symbol,
            side: closing,
            type: 'MARKET',
            quantity: position.quantity,
            leverage: position.leverage,
            reduceOnly: true,
          },
          ts,
        ),
        status: 'FILLED',
        filledQuantity: position.quantity,
        avgFillPrice: str(price),
      };
      this.store.insertOrder(order);
      const fill: Fill = {
        id: this.newId('fill'),
        orderId: order.id,
        accountId: account.id,
        symbol: position.symbol,
        side: closing,
        quantity: position.quantity,
        price: str(price),
        fee: '0',
        realizedPnl: str(margin.neg()),
        liquidity: 'LIQUIDATION',
        ts,
      };
      this.store.insertFill(fill);

      const refs = { symbol: position.symbol, orderId: order.id, fillId: fill.id, positionId: position.id, ts };
      this.post(account, { ...refs, type: 'REALIZED_PNL', amount: pnl, memo: `Liquidated @ ${price.toFixed()} (mark ${quote.mark})` });
      this.post(account, {
        ...refs,
        type: 'LIQUIDATION_PENALTY',
        amount: margin.plus(pnl).neg(),
        memo: 'Remaining isolated margin forfeited',
      });
      this.post(account, { ...refs, type: 'MARGIN_RELEASE', marginDelta: margin.neg(), memo: 'Margin released on liquidation' });

      position.status = 'LIQUIDATED';
      position.quantity = '0';
      position.margin = '0';
      position.realizedPnl = str(d(position.realizedPnl).minus(margin));
      position.exitPrice = str(price);
      position.closedAt = ts;
      this.store.savePosition(position);
      this.cancelProtection(account.id, position.symbol, null, ts);
      this.store.saveAccount(account);
      this.queued.push({ kind: 'fill', fill, order });
      this.queued.push({ kind: 'liquidation', position });
    });
  }

  private evaluateOrder(order: Order, quote: Quote) {
    const account = this.requireAccount(order.accountId);
    const instrument = getInstrument(order.symbol);
    const position = this.store.getOpenPosition(order.accountId, order.symbol);

    if (order.type === 'LIMIT') {
      if (!isRestingLimitFilled(order.side, order.price!, quote)) return;
      const quantity = order.reduceOnly ? min(d(order.quantity), d(position?.quantity ?? 0)) : d(order.quantity);
      this.fillResting(account, order, quantity, d(order.price!), 'MAKER', quote);
      return;
    }

    if (!position) {
      this.closeOrder(order, 'CANCELED', 'Position closed', quote.ts);
      return;
    }
    const isLong = position.side === 'LONG';
    let triggered: boolean;
    if (order.type === 'TRAILING_STOP_MARKET') {
      // The extreme moves first, with this quote's last, and is saved on its own so that a
      // restart resumes from it; only then is the trigger it implies compared with last.
      const spec = trailingSpecOf(order);
      const extreme = advanceExtreme(position.side, spec, order.trailingExtreme, d(quote.last));
      if (extreme !== order.trailingExtreme) {
        order.trailingExtreme = extreme;
        this.store.saveOrder(order);
      }
      triggered = extreme !== null
        && isStopTriggered(isLong, trailingTrigger(position.side, spec, d(extreme), d(instrument.priceTick)), quote.last);
    } else {
      triggered = order.type === 'STOP_MARKET'
        ? isStopTriggered(isLong, order.triggerPrice!, quote.last)
        : isTakeProfitTriggered(isLong, order.triggerPrice!, quote.last);
    }
    if (!triggered) return;
    const quantity = d(position.quantity);
    const price = marketFillPrice(order.side, quantity, quote, account.config, instrument);
    this.fillResting(account, order, quantity, price, 'TAKER', quote);
  }

  private fillResting(account: PaperAccount, order: Order, quantity: Dec, price: Dec, liquidity: 'MAKER' | 'TAKER', quote: Quote) {
    const ts = quote.ts;
    try {
      this.store.transaction(() => {
        order.reservedMargin = '0';
        order.quantity = str(quantity);
        this.store.saveOrder(order);
        this.fillOrder(account, order, quantity, price, liquidity, quote.last, ts);
      });
    } catch (error) {
      this.queued = this.queued.filter((e) => !(e.kind === 'fill' && e.order.id === order.id));
      if (!(error instanceof OrderRejection)) throw error;
      const fresh = this.store.getOrder(order.id)!;
      this.closeOrder(fresh, 'CANCELED', error.message, ts);
      this.queued.push({ kind: 'order', order: fresh });
    }
  }

  private flush() {
    const events = this.queued;
    this.queued = [];
    for (const event of events) this.deps.onEvent?.(event);
  }

  // ---- read models ----------------------------------------------------------------------

  private availableOf(account: PaperAccount): Dec {
    const reserved = this.store
      .listOpenOrders(account.id)
      .reduce((sum, o) => sum.plus(o.reservedMargin), ZERO);
    return d(account.walletBalance).minus(account.usedMargin).minus(reserved);
  }

  openOrders(accountId: string): Order[] {
    return this.store.listOpenOrders(accountId);
  }

  balanceOf(accountId: string): AccountBalance {
    const account = this.requireAccount(accountId);
    const orderMargin = this.store.listOpenOrders(accountId).reduce((sum, o) => sum.plus(o.reservedMargin), ZERO);
    let unrealized = ZERO;
    for (const position of this.store.listOpenPositions(accountId)) {
      const quote = this.deps.getQuote(account.source, position.symbol);
      if (quote) unrealized = unrealized.plus(linearPerp.pnl(position.side, position.entryPrice, quote.mark, position.quantity));
    }
    const wallet = d(account.walletBalance);
    return {
      accountId,
      initialBalance: account.initialBalance,
      walletBalance: account.walletBalance,
      availableBalance: str(wallet.minus(account.usedMargin).minus(orderMargin)),
      usedMargin: account.usedMargin,
      orderMargin: str(orderMargin),
      equity: str(money(wallet.plus(unrealized))),
      realizedPnl: account.realizedPnl,
      unrealizedPnl: str(money(unrealized)),
      feesPaid: account.feesPaid,
      fundingPaid: account.fundingPaid,
    };
  }

  positionViews(accountId: string): PositionView[] {
    const account = this.requireAccount(accountId);
    const orders = this.store.listOpenOrders(accountId);
    return this.store.listOpenPositions(accountId).map((position) => {
      const quote = this.deps.getQuote(account.source, position.symbol);
      const protection = (type: Order['type']) =>
        orders.find((o) => o.symbol === position.symbol && o.type === type)?.triggerPrice ?? null;
      const trailing = orders.find((o) => o.symbol === position.symbol && o.type === 'TRAILING_STOP_MARKET');
      const trailingView = (order: Order): TrailingStopView => {
        const spec = trailingSpecOf(order);
        const extreme = order.trailingExtreme;
        const tick = d(getInstrument(position.symbol).priceTick);
        return { ...spec, extreme, triggerPrice: extreme === null ? null : str(trailingTrigger(position.side, spec, d(extreme), tick)) };
      };
      return {
        ...position,
        markPrice: quote?.mark ?? null,
        unrealizedPnl: quote
          ? str(money(linearPerp.pnl(position.side, position.entryPrice, quote.mark, position.quantity)))
          : '0',
        stopLoss: protection('STOP_MARKET'),
        takeProfit: protection('TAKE_PROFIT_MARKET'),
        trailingStop: trailing ? trailingView(trailing) : null,
        externalProtection: [],
      };
    });
  }

  async dailyRisk(accountId: string) {
    return this.dailyRiskSync(accountId);
  }

  /** Loss since 00:00 UTC, from the ledger plus open PnL, for the daily-loss limit. */
  dailyRiskSync(accountId: string): { dailyPnl: string; dayStartEquity: string } {
    const account = this.requireAccount(accountId);
    const now = this.deps.now(account.source);
    const dayStart = Math.floor(now / 86_400_000) * 86_400_000;
    const changed = this.store
      .listLedger(accountId, dayStart)
      .filter((e) => e.type !== 'DEPOSIT')
      .reduce((sum, e) => sum.plus(e.amount), ZERO);
    const balance = this.balanceOf(accountId);
    return {
      dailyPnl: str(changed.plus(balance.unrealizedPnl)),
      dayStartEquity: str(d(account.walletBalance).minus(changed)),
    };
  }

  recordEquity(accountId: string): void {
    const account = this.requireAccount(accountId);
    const balance = this.balanceOf(accountId);
    this.store.upsertEquityPoint({
      accountId,
      ts: this.deps.now(account.source),
      equity: balance.equity,
      wallet: balance.walletBalance,
      unrealized: balance.unrealizedPnl,
    });
  }
}
