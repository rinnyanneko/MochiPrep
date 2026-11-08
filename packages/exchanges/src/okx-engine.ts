import {
  classifyProtectionChange,
  d,
  floorToStep,
  getInstrument,
  min,
  ProtectionChangeError,
  roundToStep,
  sameTrailing,
  validateTrailing,
  ZERO,
  type Dec,
  type ExecutionEngine,
  type ProtectionResult,
} from '@mochiprep/core';
import {
  listSymbols,
  type AccountBalance,
  type AccountVenue,
  type EquityPoint,
  type ExternalProtection,
  type Fill,
  type Order,
  type OrderRequest,
  type OrderResult,
  type PaperAccount,
  type PositionSide,
  type PositionView,
  type Protection,
  type Quote,
  type RiskSettings,
  type SymbolId,
  type TrailingStopSpec,
} from '@mochiprep/shared';
import {
  OkxRejection,
  OkxUnknownOutcome,
  toAlgoClOrdId,
  toClOrdId,
  toInstId,
  type AlgoOrderBody,
  type AttachAlgoOrd,
  type OkxAlgo,
  type OkxClient,
  type OkxPosition,
  type PlaceOrderBody,
} from './okx-client';

const MAX_SNAPSHOT_AGE_MS = 30_000;
const NEVER_ARRIVED_AFTER_MS = 60_000;
const EQUITY_EVERY_MS = 60_000;
/** How far back sync looks for our own algo ids. Older ones read as external, which is the safe side. */
const RECENT_ORDERS = 500;
/** How long a position must stay missing before our protection on it is cancelled. */
const POSITION_MISSING_MS = 10_000;
const ALGO_TYPES = ['conditional', 'oco', 'move_order_stop'] as const;
/** Algo states that can still trigger. Anything else, including states OKX may add later, cannot. */
const ALIVE_STATES = new Set(['live', 'partially_effective']);
const REPLACED = 'Replaced';
const REMOVED = 'Removed';

type Leg = 'sl' | 'tp' | 'trail';
const LEGS: Leg[] = ['sl', 'tp', 'trail'];
const LEG_TYPE = { sl: 'STOP_MARKET', tp: 'TAKE_PROFIT_MARKET', trail: 'TRAILING_STOP_MARKET' } as const;
const legOf = (order: Order): Leg | null =>
  !order.closePosition ? null : order.type === 'STOP_MARKET' ? 'sl' : order.type === 'TAKE_PROFIT_MARKET' ? 'tp' : order.type === 'TRAILING_STOP_MARKET' ? 'trail' : null;

const specOf = (row: Order): TrailingStopSpec => ({ kind: row.trailingKind!, callback: row.trailingCallback!, activationPrice: row.trailingActivationPrice });
const legValue = (protection: Protection, leg: Leg) =>
  leg === 'sl' ? protection.stopLoss : leg === 'tp' ? protection.takeProfit : protection.trailingStop;
const sameLeg = (leg: Leg, a: Protection, b: Protection) => {
  if (leg === 'trail') return sameTrailing(a.trailingStop, b.trailingStop);
  const [x, y] = [legValue(a, leg) as string | null, legValue(b, leg) as string | null];
  return x === null || y === null ? x === y : d(x).eq(y);
};
const trailingFields = (spec: TrailingStopSpec) => ({
  ...(spec.kind === 'RATIO' ? { callbackRatio: spec.callback } : { callbackSpread: spec.callback }),
  ...(spec.activationPrice ? { activePx: spec.activationPrice } : {}),
});

/** Exchange text never reaches the model: only fields that parse as plain numbers are kept. */
const NUMERIC = /^\d+(\.\d+)?$/;
const num = (value: string | undefined) => (value && NUMERIC.test(value) ? d(value).toFixed() : null);

function externalOf(algo: OkxAlgo): ExternalProtection[] {
  if (algo.ordType === 'move_order_stop') {
    const ratio = num(algo.callbackRatio);
    const spread = num(algo.callbackSpread);
    const from = num(algo.activePx);
    const distance = ratio ? `${d(ratio).mul(100).toFixed()}%` : (spread ?? 'unknown');
    return [{ kind: 'TRAILING', triggerPrice: num(algo.moveTriggerPx), detail: `trailing ${distance}${from ? ` from ${from}` : ''}` }];
  }
  const found: ExternalProtection[] = [];
  const leg = (kind: 'STOP_LOSS' | 'TAKE_PROFIT', label: string, raw: string | undefined) => {
    if (!raw) return;
    const price = num(raw);
    found.push({ kind, triggerPrice: price, detail: `${label} trigger ${price ?? 'unknown'}` });
  };
  leg('STOP_LOSS', 'SL', algo.slTriggerPx);
  leg('TAKE_PROFIT', 'TP', algo.tpTriggerPx);
  return found;
}

/** The protection rows in force for one symbol, read back from OKX on the last sync. */
interface SymbolProtection {
  rows: Partial<Record<Leg, Order>>;
  trailTrigger: string | null;
  external: ExternalProtection[];
}

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

const fromInstId = (instId: string) => listSymbols().find((s) => toInstId(s) === instId);

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
  private protection = new Map<SymbolId, SymbolProtection>();
  /** The last failed algo cancel, until a sync finishes with every cancel accepted. */
  private cancelError: string | null = null;
  /** Per instId: consecutive syncs that found our protection but no position, and the first such sync. */
  private missingPosition = new Map<string, { count: number; since: number }>();
  private queue: Promise<unknown> = Promise.resolve();

  constructor(private readonly deps: OkxEngineDeps) {
    this.newId = deps.newId ?? ((prefix) => `${prefix}_${crypto.randomUUID()}`);
  }

  get lastSync() {
    return this.lastSyncAt;
  }
  get error() {
    return this.syncError ?? this.cancelError;
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

  /** Our protection that OKX listed as pending on the last sync. External algos are not included. */
  async protectionOf(_accountId: string, symbol: SymbolId): Promise<Protection> {
    return this.effectiveOf(symbol);
  }

  private effectiveOf(symbol: SymbolId): Protection {
    const rows = this.protection.get(symbol)?.rows ?? {};
    return {
      stopLoss: rows.sl?.triggerPrice ?? null,
      takeProfit: rows.tp?.triggerPrice ?? null,
      trailingStop: rows.trail ? specOf(rows.trail) : null,
    };
  }

  setProtection(
    accountId: string,
    symbol: SymbolId,
    protection: Protection,
    meta: { clientOrderId: string; agentId: string | null; decisionId: string | null },
  ): Promise<ProtectionResult> {
    return this.serialized(() => this.applyProtection(accountId, symbol, protection, meta));
  }

  /**
   * Every changed leg is placed as a new algo first, and old or removed algos are cancelled only once
   * OKX has accepted all of them, so the position is never left without a stop. Two reduce-only algos
   * may briefly coexist; sync keeps the newest. A refused or unanswered placement stops here and
   * cancels nothing.
   */
  private async applyProtection(
    accountId: string,
    symbol: SymbolId,
    requested: Protection,
    meta: { agentId: string | null; decisionId: string | null },
  ): Promise<ProtectionResult> {
    const { store, client } = this.deps;
    const position = this.positions.find((p) => p.symbol === symbol);
    if (!position) throw new Error(`No open ${symbol} position`);
    const target = this.alignProtection(symbol, requested);
    const before = this.effectiveOf(symbol);
    const refusal = this.protectionRefusal(position, before, target);
    if (refusal) throw new Error(refusal);

    const changed = LEGS.filter((leg) => !sameLeg(leg, before, target));
    const old = this.protection.get(symbol)?.rows ?? {};
    // Every placement first. Nothing is cancelled unless all of them were accepted, so swapping a
    // stop for a trailing stop can never leave the position with neither.
    for (const leg of changed.filter((l) => legValue(target, l) !== null)) {
      const row = this.protectionRow(this.newId('ord'), { ...position, accountId, ...meta }, leg, target);
      // Written before anything is sent, so an unanswered request can be looked up by algoClOrdId.
      store.insertOrder(row);
      try {
        row.exchangeOrderId = await client.placeAlgo(this.algoBody(row));
        row.updatedAt = this.deps.now();
        store.saveOrder(row);
      } catch (error) {
        if (!(error instanceof OkxUnknownOutcome)) {
          row.status = 'REJECTED';
          row.rejectReason = (error as Error).message;
          row.updatedAt = this.deps.now();
          store.saveOrder(row);
          // Legs placed before this one stay; sync retires the algos they superseded.
          await this.sync().catch(() => undefined);
          throw new ProtectionChangeError((error as Error).message, this.effectiveOf(symbol));
        }
        // Unknown outcome: never placed again. A sync looks it up by algoClOrdId; if OKX shows it
        // live, the placement is known to have succeeded and the change carries on.
        row.rejectReason = `Awaiting confirmation: ${error.message}`;
        store.saveOrder(row);
        await this.sync().catch(() => undefined);
        const resolved = store.getOrder(row.id);
        if (resolved?.status === 'OPEN' && resolved.exchangeOrderId) continue;
        // Still unknown: the old algos stay until sync resolves it.
        return { before, after: this.effectiveOf(symbol), pending: true };
      }
    }
    for (const leg of changed) {
      // A sync above may already have retired a superseded row; it is not cancelled twice.
      const row = old[leg] && store.getOrder(old[leg].id);
      if (row && row.status === 'OPEN') await this.cancelRows([row], legValue(target, leg) === null ? REMOVED : REPLACED);
    }

    await this.sync().catch(() => undefined);
    return { before, after: this.effectiveOf(symbol), pending: false };
  }

  /** The gate for protection changes. A change that only lowers risk needs no arming, like an exit. */
  private protectionRefusal(position: PositionView, before: Protection, target: Protection): string | null {
    const { symbol, side } = position;
    const unresolved = this.deps.store.listOpenOrders(position.accountId).some((o) => o.symbol === symbol && legOf(o) && !o.exchangeOrderId);
    if (unresolved) return 'PROTECTION_PENDING: an earlier protection change is awaiting confirmation from OKX';
    if (!this.deps.isArmed() && classifyProtectionChange(side, before, target) === 'NEEDS_ARMED') {
      return 'NOT_ARMED: this change increases risk';
    }
    if (!this.healthy || !this.balance) return `EXCHANGE_NOT_SYNCED: ${this.syncError ?? 'no recent sync with OKX'}`;
    const limits = this.deps.limits();
    const quote = this.deps.getQuote(symbol);
    if (!quote) return 'NO_MARKET_DATA: no quote';
    const age = (this.deps.now() - quote.ts) / 1000;
    if (age > limits.maxQuoteAgeSec) return `MARKET_DATA_STALE: quote is ${age.toFixed(0)}s old`;

    if (limits.requireStopLoss && before.stopLoss !== null && target.stopLoss === null) {
      return 'LIMIT_STOP_REQUIRED: positions must keep a stop loss';
    }
    const last = d(quote.last);
    const wrongSide = (price: string, below: boolean) => (below ? d(price).gte(last) : d(price).lte(last));
    const long = side === 'LONG';
    if (target.stopLoss && wrongSide(target.stopLoss, long)) {
      return `INVALID_PROTECTION: stop ${target.stopLoss} is on the wrong side of ${last.toFixed()} for ${side}`;
    }
    if (target.takeProfit && wrongSide(target.takeProfit, !long)) {
      return `INVALID_PROTECTION: target ${target.takeProfit} is on the wrong side of ${last.toFixed()} for ${side}`;
    }
    // A kept trailing stop is not re-checked: its activation may rightly be behind the market by now.
    if (target.trailingStop && !sameTrailing(before.trailingStop, target.trailingStop)) {
      const refused = validateTrailing(side, last, target.trailingStop, d(getInstrument(symbol).priceTick));
      if (refused) return `INVALID_PROTECTION: ${refused}`;
    }
    return null;
  }

  private alignProtection(symbol: SymbolId, protection: Protection): Protection {
    const align = this.aligner(symbol);
    return { stopLoss: align(protection.stopLoss), takeProfit: align(protection.takeProfit), trailingStop: this.alignTrailing(symbol, protection.trailingStop) };
  }

  private aligner(symbol: SymbolId) {
    const { priceTick } = getInstrument(symbol);
    return (price: string | null) => (price ? roundToStep(d(price), priceTick).toFixed() : null);
  }

  private alignTrailing(symbol: SymbolId, spec: TrailingStopSpec | null): TrailingStopSpec | null {
    if (!spec) return null;
    const align = this.aligner(symbol);
    return { ...spec, callback: spec.kind === 'SPREAD' ? align(spec.callback)! : spec.callback, activationPrice: align(spec.activationPrice) };
  }

  /** A local row for one protective algo. Its id is also its clientOrderId and the algoClOrdId source. */
  private protectionRow(
    id: string,
    source: { accountId: string; symbol: SymbolId; side: PositionSide; quantity: string; leverage: number; agentId: string | null; decisionId: string | null },
    leg: Leg,
    protection: Protection,
  ): Order {
    const now = this.deps.now();
    const trail = leg === 'trail' ? protection.trailingStop : null;
    return {
      id,
      accountId: source.accountId,
      clientOrderId: id,
      symbol: source.symbol,
      side: source.side === 'LONG' ? 'SELL' : 'BUY',
      type: LEG_TYPE[leg],
      quantity: source.quantity,
      price: null,
      triggerPrice: leg === 'sl' ? protection.stopLoss : leg === 'tp' ? protection.takeProfit : null,
      leverage: source.leverage,
      reduceOnly: true,
      closePosition: true,
      status: 'OPEN',
      filledQuantity: '0',
      avgFillPrice: null,
      reservedMargin: '0',
      attachedStopLoss: null,
      attachedTakeProfit: null,
      trailingKind: trail?.kind ?? null,
      trailingCallback: trail?.callback ?? null,
      trailingActivationPrice: trail?.activationPrice ?? null,
      trailingExtreme: null,
      attachedTrailingStop: null,
      rejectReason: null,
      exchangeOrderId: null,
      agentId: source.agentId,
      decisionId: source.decisionId,
      createdAt: now,
      updatedAt: now,
    };
  }

  /** Net mode is required at connect, so no posSide, exactly as entry orders are sent. */
  private algoBody(row: Order): AlgoOrderBody {
    const base = {
      instId: toInstId(row.symbol),
      tdMode: 'isolated',
      side: row.side === 'BUY' ? 'buy' : 'sell',
      algoClOrdId: toAlgoClOrdId(row.id),
      sz: this.toContracts(row).toFixed(),
      reduceOnly: true,
    } as const;
    if (row.type === 'STOP_MARKET') return { ...base, ordType: 'conditional', slTriggerPx: row.triggerPrice!, slOrdPx: '-1' };
    if (row.type === 'TAKE_PROFIT_MARKET') return { ...base, ordType: 'conditional', tpTriggerPx: row.triggerPrice!, tpOrdPx: '-1' };
    return { ...base, ordType: 'move_order_stop', ...trailingFields(specOf(row)) };
  }

  /**
   * Cancels the algos behind these rows and marks the rows closed. The cancel answer is not trusted
   * either way: the next sync sees whether each algo is still pending and cancels it again if so.
   * `reason` null keeps a row's existing reason.
   */
  private async cancelRows(rows: Order[], reason: string | null): Promise<string | null> {
    const { client, store, now } = this.deps;
    const sent = rows.filter((row) => row.exchangeOrderId);
    let failure: string | null = null;
    for (let i = 0; i < sent.length; i += 10) {
      const batch = sent.slice(i, i + 10).map((row) => ({ instId: toInstId(row.symbol), algoId: row.exchangeOrderId! }));
      try {
        await client.cancelAlgos(batch);
      } catch (error) {
        failure = `Could not cancel an OKX algo order: ${(error as Error).message}`;
      }
    }
    for (const row of rows) {
      if (row.status === 'OPEN') row.status = 'CANCELED';
      if (reason) row.rejectReason = reason;
      row.updatedAt = now();
      store.saveOrder(row);
    }
    // Shown with the account status (redacted there). It does not make the account unhealthy:
    // the algo is only an extra reduce-only order, and sync keeps retrying the cancel.
    if (failure) this.cancelError = failure;
    return failure;
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

  private async submit(request: OrderRequest): Promise<OrderResult> {
    const { store } = this.deps;
    const prior = store.getOrderByClientId(request.accountId, request.clientOrderId);
    if (prior) return { order: prior, fills: store.listFillsByOrder(prior.id), duplicate: true };

    const order = this.buildOrder(request);
    this.alignToTick(order);
    const position = this.positions.find((p) => p.symbol === request.symbol);
    if (order.reduceOnly && position) order.quantity = min(d(order.quantity), d(position.quantity)).toFixed();

    const refusal = this.preTradeRefusal(order, position);
    const contracts = this.toContracts(order);
    if (refusal || contracts.lte(0)) {
      return this.reject(order, refusal ?? 'INVALID_QUANTITY: below one lot');
    }
    order.quantity = contracts.mul(getInstrument(order.symbol).venueContractValue).toFixed();

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
    const { venueContractValue, venueLotSize } = getInstrument(order.symbol);
    return floorToStep(d(order.quantity).div(venueContractValue), venueLotSize);
  }

  /** OKX refuses prices off the tick, and models rarely produce them on it. */
  private alignToTick(order: Order) {
    const align = this.aligner(order.symbol);
    order.price = align(order.price);
    order.attachedStopLoss = align(order.attachedStopLoss);
    order.attachedTakeProfit = align(order.attachedTakeProfit);
    order.attachedTrailingStop = this.alignTrailing(order.symbol, order.attachedTrailingStop);
  }

  /**
   * One attachAlgoOrds element per leg, each tagged with the id of the local row that will track it
   * once the entry fills (`<order id>:sl`, `:tp`, `:trail`), so every attached algo can be found by
   * its algoClOrdId. Whether OKX keeps separate SL and TP elements apart is UNCONFIRMED (Demo check).
   */
  private toBody(order: Order, instId: string, clOrdId: string, sz: string): PlaceOrderBody {
    const algo: AttachAlgoOrd[] = [];
    if (!order.reduceOnly) {
      const tag = (leg: Leg) => toAlgoClOrdId(`${order.id}:${leg}`);
      if (order.attachedStopLoss) algo.push({ attachAlgoClOrdId: tag('sl'), slTriggerPx: order.attachedStopLoss, slOrdPx: '-1' });
      if (order.attachedTakeProfit) algo.push({ attachAlgoClOrdId: tag('tp'), tpTriggerPx: order.attachedTakeProfit, tpOrdPx: '-1' });
      if (order.attachedTrailingStop) algo.push({ attachAlgoClOrdId: tag('trail'), ...trailingFields(order.attachedTrailingStop) });
    }
    return {
      instId,
      tdMode: 'isolated',
      side: order.side === 'BUY' ? 'buy' : 'sell',
      ordType: order.type === 'LIMIT' ? 'limit' : 'market',
      sz,
      clOrdId,
      ...(order.type === 'LIMIT' ? { px: order.price! } : {}),
      ...(order.reduceOnly ? { reduceOnly: true } : {}),
      ...(algo.length ? { attachAlgoOrds: algo } : {}),
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
    const trailing = order.attachedTrailingStop;
    if (trailing) {
      const refused = validateTrailing(order.side === 'BUY' ? 'LONG' : 'SHORT', reference, trailing, d(getInstrument(order.symbol).priceTick));
      if (refused) return `INVALID_PROTECTION: ${refused}`;
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
      // A protective algo is not an order on OKX, and removing one may need arming: it goes through setProtection.
      if (legOf(order)) throw new Error('Change protection on OKX through the protection settings');
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
    const filled = d(remote.accFillSz || 0).mul(getInstrument(order.symbol).venueContractValue);
    order.filledQuantity = filled.toFixed();
    if (remote.avgPx) order.avgFillPrice = remote.avgPx;
    order.updatedAt = at;
    if (remote.state === 'filled') order.status = 'FILLED';
    else if (remote.state === 'canceled' || remote.state === 'mmp_canceled') order.status = 'CANCELED';
    store.saveOrder(order);
    if (order.status !== 'OPEN' && filled.gt(0) && !order.reduceOnly) this.recordAttached(order, filled);

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

  /**
   * Rows for what the entry attached, created once it has filled (wholly, or partly before a
   * cancel). Before that no rows exist, so cleanup can never touch a resting entry's protection.
   * The ids are fixed by toBody's attachAlgoClOrdId tags.
   */
  private recordAttached(entry: Order, filled: Dec) {
    const { store } = this.deps;
    const protection: Protection = { stopLoss: entry.attachedStopLoss, takeProfit: entry.attachedTakeProfit, trailingStop: entry.attachedTrailingStop };
    const source = { ...entry, side: (entry.side === 'BUY' ? 'LONG' : 'SHORT') as PositionSide, quantity: filled.toFixed() };
    for (const leg of LEGS) {
      const id = `${entry.id}:${leg}`;
      if (legValue(protection, leg) === null || store.getOrder(id)) continue;
      store.insertOrder(this.protectionRow(id, source, leg, protection));
    }
  }

  /** Full reconciliation with the exchange. Throws, and marks the account unhealthy, on failure. */
  async sync(): Promise<void> {
    const { client, store, accountId, now } = this.deps;
    try {
      // Orders first, so an entry seen filling here has its protection rows before algos are matched.
      for (const order of store.listOpenOrders(accountId)) if (!legOf(order)) await this.syncOrder(order);
      const [usdt, remote, ...pending] = await Promise.all([
        client.getUsdtBalance(),
        client.getPositions(),
        ...ALGO_TYPES.map((type) => client.getPendingAlgos(type)),
      ]);
      const open = remote.filter((p) => Number(p.pos) !== 0);
      const held = new Set(open.map((p) => p.instId));
      this.protection = await this.reconcileProtection(pending.flat(), held);
      this.positions = open.flatMap((p) => this.toView(p));
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

  /**
   * Matches pending algos to our protection rows by algoClOrdId. OKX is the truth: a row counts only
   * once its algo is seen live, and an algo we did not place is reported as external and never touched.
   * Per (symbol, leg) the newest live row is in force and older ones are cancelled; rows on a symbol
   * whose position is missing on two syncs in a row, over at least 10s, are cancelled. An algo still pending after a
   * cancel attempt stays on the view, marked, and the cancel is retried each sync.
   */
  private async reconcileProtection(algos: OkxAlgo[], held: Set<string>): Promise<Map<SymbolId, SymbolProtection>> {
    const { store, accountId, now } = this.deps;
    const at = now();
    const open = store.listOpenOrders(accountId).filter((o) => legOf(o));
    const ours = new Map<string, Order>();
    for (const row of [...store.listOrders(accountId, RECENT_ORDERS), ...open]) if (legOf(row)) ours.set(toAlgoClOrdId(row.id), row);

    const result = new Map<SymbolId, SymbolProtection>();
    const entry = (symbol: SymbolId) => {
      if (!result.has(symbol)) result.set(symbol, { rows: {}, trailTrigger: null, external: [] });
      return result.get(symbol)!;
    };
    const alive = new Map<string, { row: Order; algo: OkxAlgo }>();
    const strays: { row: Order; algo: OkxAlgo }[] = [];
    const confirm = (row: Order, algo: OkxAlgo) => {
      if (row.exchangeOrderId !== algo.algoId || row.rejectReason) {
        row.exchangeOrderId = algo.algoId;
        row.rejectReason = null;
        row.updatedAt = at;
        store.saveOrder(row);
      }
      alive.set(row.id, { row, algo });
    };

    for (const algo of algos) {
      const row = algo.algoClOrdId ? ours.get(algo.algoClOrdId) : undefined;
      if (!row) {
        const symbol = fromInstId(algo.instId);
        if (symbol) entry(symbol).external.push(...externalOf(algo));
      } else if (row.status === 'OPEN') {
        confirm(row, algo);
      } else {
        strays.push({ row, algo });
      }
    }

    // Our open rows OKX did not list: lagging, finished, or never arrived.
    for (const row of open) {
      if (alive.has(row.id)) continue;
      const remote = await this.lookupAlgo(row);
      if (remote && ALIVE_STATES.has(remote.state)) confirm(row, remote);
      else if (remote || row.exchangeOrderId) this.closeRow(row, 'Gone from OKX');
      else if (at - row.createdAt >= NEVER_ARRIVED_AFTER_MS) this.closeRow(row, 'OKX has no record of this algo');
    }

    const groups = new Map<string, Order[]>();
    const rank = new Map(open.map((row, i) => [row.id, i]));
    for (const { row } of [...alive.values()].sort((a, b) => rank.get(a.row.id)! - rank.get(b.row.id)!)) {
      const key = `${row.symbol}:${legOf(row)}`;
      groups.set(key, [...(groups.get(key) ?? []), row]);
    }

    // Compared by instId, which every row maps to, so a symbol missing from the registry still
    // counts as held. A position must be missing on two syncs in a row, and for at least
    // POSITION_MISSING_MS since the first miss, before its rows are cancelled: syncs can be
    // milliseconds apart, and the position list may lag behind a fill.
    const isHeld = (row: Order) => held.has(toInstId(row.symbol));
    const missing = new Map<string, { count: number; since: number }>();
    for (const row of [...[...alive.values()].map((a) => a.row), ...strays.map((s) => s.row)]) {
      const instId = toInstId(row.symbol);
      if (held.has(instId) || missing.has(instId)) continue;
      const prior = this.missingPosition.get(instId);
      missing.set(instId, { count: (prior?.count ?? 0) + 1, since: prior?.since ?? at });
    }
    this.missingPosition = missing;
    const positionGone = (row: Order) => {
      const miss = missing.get(toInstId(row.symbol));
      return !!miss && miss.count >= 2 && at - miss.since >= POSITION_MISSING_MS;
    };

    const cancel: { row: Order; algo: OkxAlgo; reason: string | null }[] = [];
    // A closed row whose algo is still live: cancel it again, unless it is the only stop for that leg.
    for (const { row, algo } of strays) {
      row.exchangeOrderId = algo.algoId;
      const key = `${row.symbol}:${legOf(row)}`;
      if (!isHeld(row)) {
        if (positionGone(row)) cancel.push({ row, algo, reason: null });
      } else if (row.rejectReason === REMOVED || groups.has(key)) {
        cancel.push({ row, algo, reason: null });
      } else {
        row.status = 'OPEN';
        row.rejectReason = null;
        row.updatedAt = at;
        store.saveOrder(row);
        alive.set(row.id, { row, algo });
        groups.set(key, [row]);
      }
    }

    for (const rows of groups.values()) {
      const first = rows[0]!;
      if (!isHeld(first)) {
        if (positionGone(first)) for (const row of rows) cancel.push({ row, algo: alive.get(row.id)!.algo, reason: 'Position closed' });
        continue;
      }
      const effective = rows.at(-1)!;
      for (const row of rows.slice(0, -1)) cancel.push({ row, algo: alive.get(row.id)!.algo, reason: REPLACED });
      const leg = legOf(effective)!;
      entry(effective.symbol).rows[leg] = effective;
      if (leg === 'trail') entry(effective.symbol).trailTrigger = num(alive.get(effective.id)!.algo.moveTriggerPx);
    }

    // Still live on OKX as of this sync, so still shown, marked, until a later sync no longer lists it.
    for (const { row, algo } of cancel) {
      if (!isHeld(row)) continue;
      entry(row.symbol).external.push(...externalOf(algo).map((e) => ({ ...e, detail: `ours, cancel pending: ${e.detail}` })));
    }
    let failure: string | null = null;
    for (const reason of new Set(cancel.map((c) => c.reason))) {
      failure = (await this.cancelRows(cancel.filter((c) => c.reason === reason).map((c) => c.row), reason)) ?? failure;
    }
    this.cancelError = failure;
    return result;
  }

  /**
   * Called only for a row absent from the pending lists, so a refused lookup is read as "not live":
   * the pending list is the evidence, and the lookup can only show the algo is still alive.
   */
  private async lookupAlgo(row: Order): Promise<OkxAlgo | null> {
    try {
      return await this.deps.client.getAlgo(toAlgoClOrdId(row.id));
    } catch (error) {
      if (error instanceof OkxRejection) return null;
      throw error;
    }
  }

  private closeRow(row: Order, reason: string) {
    row.status = 'CANCELED';
    row.rejectReason = reason;
    row.updatedAt = this.deps.now();
    this.deps.store.saveOrder(row);
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
    const quantity = d(p.pos).abs().mul(getInstrument(symbol).venueContractValue).toFixed();
    const entry = store.listOrders(accountId, 200).find((o) => o.symbol === symbol && !o.reduceOnly && o.status === 'FILLED');
    const protection = this.effectiveOf(symbol);
    const { trailTrigger, external } = this.protection.get(symbol) ?? { trailTrigger: null, external: [] };
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
        stopLoss: protection.stopLoss,
        takeProfit: protection.takeProfit,
        // OKX does not report the tracked extreme, only the current trigger.
        trailingStop: protection.trailingStop ? { ...protection.trailingStop, extreme: null, triggerPrice: trailTrigger } : null,
        externalProtection: external,
      },
    ];
  }

  /** Reduce-only market close of a fraction. Works even when disarmed: exits must always be possible. */
  closePosition(accountId: string, symbol: SymbolId, fraction: number, clientOrderId: string) {
    const position = this.positions.find((p) => p.symbol === symbol);
    if (!position) throw new Error(`No open ${symbol} position`);
    const quantity = fraction >= 1 ? d(position.quantity) : floorToStep(d(position.quantity).mul(fraction), getInstrument(symbol).quantityStep);
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

