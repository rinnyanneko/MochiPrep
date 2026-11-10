import { d, evaluateRisk, ProtectionChangeError, type ExecutionEngine } from '@mochiprep/core';
import { ProviderError, redactSecrets, type AiProvider, type MarketContext } from '@mochiprep/ai-providers';
import type { MarketDataProvider } from '@mochiprep/market-data';
import {
  indicatorSnapshot,
  TIMEFRAME_MS,
  type Agent,
  type AiDecisionData,
  type AiTrailingStop,
  type DataSource,
  type DecisionOutcome,
  type DecisionRecord,
  type NewsItem,
  type OrderRequest,
  type PaperAccount,
  type Protection,
  type ProviderConfig,
  type Quote,
  type RiskSettings,
  type Topic,
  type TrailingStopSpec,
} from '@mochiprep/shared';
import { parseDecision } from './parse';
import { buildUserPrompt, buildSystemPrompt } from './prompt';

export const MAX_CONSECUTIVE_ERRORS = 3;
const MIN_CANDLES = 30;
const RAW_RESPONSE_LIMIT = 8_000;

export interface AgentStore {
  insertAgent(agent: Agent): void;
  saveAgent(agent: Agent): void;
  getAgent(id: string): Agent | undefined;
  listAgents(): Agent[];
  insertDecision(record: DecisionRecord): void;
  saveDecision(record: DecisionRecord): void;
  getDecision(id: string): DecisionRecord | undefined;
  listPendingSignals(): DecisionRecord[];
}

export interface AgentRunnerDeps {
  store: AgentStore;
  /** Paper or exchange engine for an account. Throws if the exchange is not connected. */
  execution(accountId: string): ExecutionEngine;
  market(source: DataSource): MarketDataProvider;
  account(id: string): PaperAccount | undefined;
  /** Throws ProviderError('NO_KEY') when the key is not loaded. */
  resolveProvider(agent: Agent): { provider: AiProvider; config: ProviderConfig };
  secrets(): Iterable<string>;
  onChange(topics: Topic[]): void;
  onNotice?(level: 'warn' | 'error', message: string): void;
  /** Headlines for an agent's pair. Failure must surface as `error`, never as a throw. */
  news?(agent: Agent): Promise<{ items: NewsItem[]; error: string | null }>;
  /** A new suggestion is waiting for the user. */
  onSignal?(record: DecisionRecord): void;
  newId?(): string;
}

export type CycleTrigger = DecisionRecord['trigger'];

/** Runs the analysis loop. Only this class turns AI output into orders, and only via the risk engine. */
export class AgentRunner {
  private readonly inFlight = new Set<string>();
  private readonly newId: () => string;

  constructor(private readonly deps: AgentRunnerDeps) {
    this.newId = deps.newId ?? (() => `dec_${crypto.randomUUID()}`);
  }

  private source(agent: Agent): DataSource {
    const account = this.deps.account(agent.accountId);
    if (!account) throw new Error(`Agent ${agent.id} has no account`);
    return account.source;
  }

  private requireAgent(id: string): Agent {
    const agent = this.deps.store.getAgent(id);
    if (!agent) throw new Error(`Unknown agent ${id}`);
    return agent;
  }

  private update(agent: Agent, patch: Partial<Agent>) {
    Object.assign(agent, patch);
    this.deps.store.saveAgent(agent);
    this.deps.onChange(['agents']);
  }

  // ---- lifecycle ------------------------------------------------------------------------

  start(id: string) {
    const agent = this.requireAgent(id);
    const now = this.deps.market(this.source(agent)).now();
    this.update(agent, { running: true, status: 'WAITING', nextRunAt: now, statusDetail: null, consecutiveErrors: 0 });
  }

  pause(id: string, detail: string | null = 'Paused by user') {
    const agent = this.requireAgent(id);
    this.update(agent, { running: false, status: 'PAUSED', nextRunAt: null, statusDetail: detail });
  }

  resume(id: string) {
    this.start(id);
  }

  stop(id: string) {
    const agent = this.requireAgent(id);
    this.update(agent, { running: false, status: 'IDLE', nextRunAt: null, statusDetail: null });
  }

  stopAll(): number {
    let stopped = 0;
    for (const agent of this.deps.store.listAgents()) {
      if (agent.running || agent.status !== 'IDLE') {
        this.update(agent, { running: false, status: 'IDLE', nextRunAt: null, statusDetail: 'Stopped by Stop All' });
        stopped++;
      }
    }
    return stopped;
  }

  /** Agents never survive the process: nothing trades on restart until the user starts it. */
  disarmAfterRestart() {
    for (const agent of this.deps.store.listAgents()) {
      if (agent.running || agent.status === 'ANALYZING' || agent.status === 'EXECUTING') {
        this.update(agent, {
          running: false,
          status: 'PAUSED',
          nextRunAt: null,
          statusDetail: 'Paused because the backend restarted',
        });
      }
    }
  }

  isBusy(id: string) {
    return this.inFlight.has(id);
  }

  /** Runs every due agent on this feed, and resolves once they have all finished. */
  async tick(source: DataSource): Promise<void> {
    this.expireSignals(source);
    const now = this.deps.market(source).now();
    const due = this.deps.store
      .listAgents()
      .filter((a) => a.running && a.nextRunAt !== null && a.nextRunAt <= now && this.source(a) === source);
    await Promise.all(due.map((a) => this.runCycle(a.id, 'SCHEDULE')));
  }

  /** Pauses the agents trading an account, for example after a liquidation. */
  pauseAccountAgents(accountId: string, detail: string) {
    for (const agent of this.deps.store.listAgents()) {
      if (agent.accountId === accountId && agent.running) this.pause(agent.id, detail);
    }
  }

  // ---- the cycle ------------------------------------------------------------------------

  /** One pass of the loop. Returns null when a cycle for this agent is already in progress. */
  async runCycle(agentId: string, trigger: CycleTrigger): Promise<DecisionRecord | null> {
    if (this.inFlight.has(agentId)) return null;
    this.inFlight.add(agentId);
    const agent = this.requireAgent(agentId);
    try {
      const source = this.source(agent);
      const market = this.deps.market(source);
      this.update(agent, { status: 'ANALYZING', statusDetail: null });
      const record = sanitizeRecord(await this.analyzeAndAct(agent, market, source, trigger), this.deps.secrets());
      if (record.outcome === 'SIGNAL') this.retireSignals((s) => s.agentId === agentId, 'Replaced by a newer suggestion');
      this.deps.store.insertDecision(record);
      this.finishCycle(agent, market, record);
      this.deps.onChange(['decisions', 'accounts', 'orders', 'positions', 'arena']);
      if (record.outcome === 'SIGNAL') this.deps.onSignal?.(record);
      return record;
    } finally {
      this.inFlight.delete(agentId);
    }
  }

  // ---- suggestions ----------------------------------------------------------------------

  private retireSignals(match: (signal: DecisionRecord) => boolean, detail: string) {
    for (const signal of this.deps.store.listPendingSignals()) {
      if (match(signal)) this.deps.store.saveDecision(settle(signal, 'SIGNAL_EXPIRED', detail));
    }
    this.deps.onChange(['decisions']);
  }

  /** Suggestions go stale on the feed's own clock, so a paused tape does not age them. */
  expireSignals(source: DataSource) {
    const now = this.deps.market(source).now();
    const due = this.deps.store.listPendingSignals().filter((s) => s.source === source && (s.signalExpiresAt ?? 0) <= now);
    if (due.length === 0) return;
    const ids = new Set(due.map((s) => s.id));
    this.retireSignals((s) => ids.has(s.id), 'Expired before it was acted on');
  }

  /** Stop All must leave nothing to act on by accident. */
  retireAllSignals() {
    this.retireSignals(() => true, 'Cleared by Stop All');
  }

  private requirePendingSignal(id: string): DecisionRecord {
    const signal = this.deps.store.getDecision(id);
    if (!signal) throw new Error('Unknown suggestion');
    if (signal.outcome !== 'SIGNAL') throw new Error('This suggestion is no longer pending');
    return signal;
  }

  dismissSignal(id: string): DecisionRecord {
    const signal = settle(this.requirePendingSignal(id), 'SIGNAL_DISMISSED', 'Dismissed by user');
    this.deps.store.saveDecision(signal);
    this.deps.onChange(['decisions']);
    return signal;
  }

  /**
   * Acts on a suggestion the user accepted. Nothing from the original analysis is trusted except
   * the idea itself: the risk engine runs again on current prices and balances, and resizes it.
   */
  async executeSignal(id: string): Promise<DecisionRecord> {
    const signal = this.requirePendingSignal(id);
    const agent = this.requireAgent(signal.agentId);
    if (this.inFlight.has(agent.id)) throw new Error('This agent is busy. Try again in a moment.');
    this.inFlight.add(agent.id);
    try {
      const account = this.deps.account(agent.accountId)!;
      const market = this.deps.market(signal.source);
      if ((signal.signalExpiresAt ?? 0) <= market.now()) {
        settle(signal, 'SIGNAL_EXPIRED', 'Expired before it was acted on');
      } else {
        const engine = this.deps.execution(agent.accountId);
        const protection = await this.currentProtection(agent, engine);
        // A PROTECT was judged against the stops it saw; applied over newer ones it could undo them.
        const moved =
          signal.decision!.action === 'PROTECT' &&
          !(protection && signal.protectionBefore && sameProtection(protection, signal.protectionBefore));
        if (moved) {
          settle(signal, 'SIGNAL_EXPIRED', 'Protection changed since this suggestion');
        } else {
          signal.risk = await this.assess(agent, account, signal.decision!, engine, market, market.getQuote(agent.symbol) ?? null, protection);
          if (!signal.risk.approved) settle(signal, 'RISK_REJECTED', signal.risk.rejectionReasons.join(' | '));
          else await this.execute(agent, signal.decision!, signal, engine);
        }
      }
      signal.signalExpiresAt = null;
      this.deps.store.saveDecision(signal);
      this.deps.onChange(['decisions', 'accounts', 'orders', 'positions', 'arena']);
      return signal;
    } finally {
      this.inFlight.delete(agent.id);
    }
  }

  /** The protection in force for the agent's symbol, or null without an open position. */
  private async currentProtection(agent: Agent, engine: ExecutionEngine): Promise<Protection | null> {
    const open = (await engine.getPositions(agent.accountId)).some((p) => p.symbol === agent.symbol);
    return open ? engine.protectionOf(agent.accountId, agent.symbol) : null;
  }

  private async assess(
    agent: Agent,
    account: PaperAccount,
    decision: AiDecisionData,
    engine: ExecutionEngine,
    market: MarketDataProvider,
    quote: Quote | null,
    protection: Protection | null,
  ) {
    const daily = await engine.dailyRisk(agent.accountId);
    return evaluateRisk({
      decision,
      symbol: agent.symbol,
      settings: effectiveRisk(agent.risk, account.liveLimits),
      quote,
      now: market.now(),
      balance: await engine.getBalance(agent.accountId),
      openPositions: await engine.getPositions(agent.accountId),
      dailyPnl: daily.dailyPnl,
      dayStartEquity: daily.dayStartEquity,
      fees: account.config,
      allowTrailingStop: agent.allowTrailingStop,
      protection,
    });
  }

  private async execute(agent: Agent, decision: AiDecisionData, record: DecisionRecord, engine: ExecutionEngine) {
    if (decision.action === 'PROTECT') return this.protect(agent, decision, record, engine);
    let result;
    try {
      result = await engine.placeOrder(await this.toOrder(agent, decision, record, engine));
    } catch (error) {
      return settle(record, 'ORDER_REJECTED', redactSecrets((error as Error).message, this.deps.secrets()));
    }
    record.orderId = result.order.id;
    record.fillIds = result.fills.map((f) => f.id);
    switch (result.order.status) {
      case 'FILLED':
        return settle(record, 'EXECUTED', `Filled ${result.order.quantity} @ ${result.order.avgFillPrice}`);
      case 'OPEN':
        return settle(record, 'ORDER_PLACED', `Resting ${result.order.side} limit @ ${result.order.price}`);
      default:
        return settle(record, 'ORDER_REJECTED', result.order.rejectReason);
    }
  }

  /** Applies an approved PROTECT, merged onto the snapshot it was judged against. */
  private async protect(agent: Agent, decision: AiDecisionData, record: DecisionRecord, engine: ExecutionEngine) {
    const target = targetProtection(decision, record.protectionBefore!);
    let pending: boolean;
    let unchanged: boolean;
    try {
      const result = await engine.setProtection(agent.accountId, agent.symbol, target, {
        clientOrderId: `protect:${record.id}`,
        agentId: agent.id,
        decisionId: record.id,
      });
      record.protectionBefore = result.before;
      record.protectionAfter = result.after;
      pending = result.pending;
      unchanged = sameProtection(result.before, result.after);
    } catch (error) {
      // Refused part-way: legs already placed stay, so History shows what is in force now.
      if (error instanceof ProtectionChangeError) record.protectionAfter = error.after;
      return settle(record, 'ORDER_REJECTED', redactSecrets((error as Error).message, this.deps.secrets()));
    }
    if (pending) return settle(record, 'ORDER_PLACED', 'Protection change sent to OKX, awaiting confirmation');
    // The engine aligns prices to the tick, so a target that differed only off the tick changes nothing.
    if (unchanged) return settle(record, 'HOLD', 'Protection already as requested');
    return settle(record, 'EXECUTED', 'Protection updated');
  }

  private finishCycle(agent: Agent, market: MarketDataProvider, record: DecisionRecord) {
    const latest = this.requireAgent(agent.id);
    const failed = record.outcome === 'AI_ERROR' || record.outcome === 'INVALID_OUTPUT';
    const errors = failed ? latest.consecutiveErrors + 1 : 0;
    const now = market.now();
    const patch: Partial<Agent> = { lastRunAt: now, consecutiveErrors: errors };
    if (latest.running) patch.nextRunAt = now + latest.intervalSec * 1000;

    const critical = record.outcomeDetail?.startsWith('[critical]') ?? false;
    if (failed && (critical || errors >= MAX_CONSECUTIVE_ERRORS)) {
      const detail = critical ? (record.outcomeDetail ?? '').replace('[critical] ', '') : `${errors} failed cycles in a row`;
      Object.assign(patch, { running: false, status: 'PAUSED', nextRunAt: null, statusDetail: `Auto-paused: ${detail}` });
      this.deps.onNotice?.('error', `${agent.name} auto-paused: ${detail}`);
    } else if (failed) {
      Object.assign(patch, { status: 'ERROR', statusDetail: record.outcomeDetail });
    } else if (latest.status === 'ANALYZING' || latest.status === 'EXECUTING') {
      Object.assign(patch, { status: latest.running ? 'WAITING' : 'IDLE', statusDetail: null });
    }
    this.update(latest, patch);
  }

  private async analyzeAndAct(
    agent: Agent,
    market: MarketDataProvider,
    source: DataSource,
    trigger: CycleTrigger,
  ): Promise<DecisionRecord> {
    const now = market.now();
    const account = this.deps.account(agent.accountId)!;
    const record: DecisionRecord = {
      id: this.newId(),
      agentId: agent.id,
      agentName: agent.name,
      accountId: agent.accountId,
      source,
      venue: account.venue,
      ts: now,
      trigger,
      providerKind: null,
      model: agent.model,
      symbol: agent.symbol,
      timeframe: agent.timeframe,
      snapshot: null,
      indicators: null,
      decision: null,
      rawResponse: null,
      validationError: null,
      risk: null,
      outcome: 'DATA_ERROR',
      outcomeDetail: null,
      news: null,
      newsError: null,
      signalExpiresAt: null,
      orderId: null,
      fillIds: [],
      latencyMs: null,
      inputTokens: null,
      outputTokens: null,
      estimatedCost: null,
      isMock: false,
    };
    const conclude = (outcome: DecisionOutcome, detail: string | null = null) => settle(record, outcome, detail);

    let engine: ExecutionEngine;
    try {
      engine = this.deps.execution(agent.accountId);
    } catch (error) {
      return conclude('DATA_ERROR', (error as Error).message);
    }

    // 1-3. Market data, freshness, indicators.
    const quote = market.getQuote(agent.symbol);
    if (!quote) return conclude('DATA_ERROR', 'No quote available yet');
    record.snapshot = quote;

    let closed;
    try {
      const candles = await market.getCandles(agent.symbol, agent.timeframe, 200);
      closed = candles.filter((c) => c.closed);
    } catch (error) {
      return conclude('DATA_ERROR', `Could not load candles: ${(error as Error).message}`);
    }
    if (closed.length < MIN_CANDLES) return conclude('DATA_ERROR', `Only ${closed.length} closed candles, need ${MIN_CANDLES}`);

    // 4-5. Account state. Unfilled entries from the last cycle expire here.
    let balance;
    let positions;
    try {
      await this.cancelStaleEntries(agent, engine);
      balance = await engine.getBalance(agent.accountId);
      positions = await engine.getPositions(agent.accountId);
    } catch (error) {
      return conclude('DATA_ERROR', `Could not read the account: ${(error as Error).message}`);
    }
    const position = positions.find((p) => p.symbol === agent.symbol) ?? null;

    const stale = isStale(quote, closed[closed.length - 1]!.time, TIMEFRAME_MS[agent.timeframe], now, agent.risk.maxQuoteAgeSec);
    if (stale && !position) return conclude('DATA_ERROR', stale);

    const indicators = indicatorSnapshot(closed);
    record.indicators = indicators;

    // News is optional context: losing it must never stop an analysis.
    if (agent.useNews && source === 'LIVE' && this.deps.news) {
      const result = await this.deps.news(agent).catch((error: Error) => ({ items: [], error: error.message }));
      record.news = result.items;
      record.newsError = result.error;
    }

    // 6. Ask the model.
    const context: MarketContext = {
      symbol: agent.symbol,
      timeframe: agent.timeframe,
      now,
      horizonSec: agent.intervalSec,
      candles: closed,
      indicators,
      quote,
      position,
      balance,
      risk: agent.risk,
      strategy: agent.strategy,
      ...(record.news ? { news: record.news } : {}),
    };
    let text: string;
    const started = Date.now();
    try {
      const { provider, config } = this.deps.resolveProvider(agent);
      record.providerKind = config.kind;
      record.isMock = config.kind === 'MOCK';
      const response = await provider.complete({
        model: agent.model,
        system: buildSystemPrompt({ allowTrailingStop: agent.allowTrailingStop }),
        user: buildUserPrompt(context, { allowTrailingStop: agent.allowTrailingStop }),
        context,
      });
      text = response.text;
      record.inputTokens = response.inputTokens;
      record.outputTokens = response.outputTokens;
      record.estimatedCost = estimateCost(config, response.inputTokens, response.outputTokens);
    } catch (error) {
      record.latencyMs = Date.now() - started;
      const message = redactSecrets((error as Error).message, this.deps.secrets());
      const critical = error instanceof ProviderError && error.critical;
      return conclude('AI_ERROR', critical ? `[critical] ${message}` : message);
    }
    record.latencyMs = Date.now() - started;
    record.rawResponse = redactSecrets(text, this.deps.secrets()).slice(0, RAW_RESPONSE_LIMIT);

    // 7. Validate.
    const parsed = parseDecision(text);
    if (!parsed.ok) {
      record.validationError = parsed.error;
      return conclude('INVALID_OUTPUT', parsed.error);
    }
    record.decision = parsed.decision;

    // 8. Risk. Time and quote are re-read: the model may have taken a while.
    const latestQuote = market.getQuote(agent.symbol) ?? null;
    let protection;
    try {
      protection = await this.currentProtection(agent, engine);
    } catch (error) {
      return conclude('DATA_ERROR', `Could not read the account: ${(error as Error).message}`);
    }
    record.risk = await this.assess(agent, account, parsed.decision, engine, market, stale ? null : latestQuote, protection);

    if (trigger === 'ANALYZE') return conclude('ANALYSIS_ONLY', 'Analyze Now never places orders');
    if (!record.risk.approved) return conclude('RISK_REJECTED', record.risk.rejectionReasons.join(' | '));
    if (parsed.decision.action === 'HOLD') return conclude('HOLD');
    if (parsed.decision.action === 'PROTECT') {
      // Approval means a position was open; null here means it opened between the two reads.
      if (!protection) return conclude('DATA_ERROR', 'The position changed while it was being checked');
      if (sameProtection(targetProtection(parsed.decision, protection), protection)) {
        return conclude('HOLD', 'Protection already as requested');
      }
      record.protectionBefore = protection;
    }

    // A Stop pressed while the model was thinking must win.
    if (trigger === 'SCHEDULE' && !this.requireAgent(agent.id).running) {
      return conclude('ANALYSIS_ONLY', 'Agent was stopped before execution');
    }

    // 9. Suggest, or execute.
    if (agent.executionMode === 'SIGNAL') {
      const aiExpiry = parsed.decision.expiresAt ? Date.parse(parsed.decision.expiresAt) : Infinity;
      record.signalExpiresAt = Math.min(aiExpiry, market.now() + agent.intervalSec * 1000);
      return conclude('SIGNAL', 'Waiting for you to accept or dismiss');
    }
    this.update(agent, { status: 'EXECUTING' });
    return this.execute(agent, parsed.decision, record, engine);
  }

  private async toOrder(agent: Agent, decision: AiDecisionData, record: DecisionRecord, engine: ExecutionEngine): Promise<OrderRequest> {
    const common = {
      accountId: agent.accountId,
      clientOrderId: `agent:${agent.id}:${record.id}`,
      symbol: agent.symbol,
      agentId: agent.id,
      decisionId: record.id,
    };
    if (decision.action === 'CLOSE') {
      const position = (await engine.getPositions(agent.accountId)).find((p) => p.symbol === agent.symbol)!;
      return {
        ...common,
        side: position.side === 'LONG' ? 'SELL' : 'BUY',
        type: 'MARKET',
        quantity: position.quantity,
        leverage: position.leverage,
        reduceOnly: true,
      };
    }
    const sized = record.risk!.sized!;
    return {
      ...common,
      side: decision.action === 'LONG' ? 'BUY' : 'SELL',
      type: decision.orderType,
      quantity: sized.quantity,
      price: decision.orderType === 'LIMIT' ? plain(decision.entryPrice) : null,
      leverage: sized.leverage,
      stopLoss: plain(decision.stopLoss),
      takeProfit: plain(decision.takeProfit),
      trailingStop: trailingSpec(decision.trailingStop ?? null),
    };
  }

  private async cancelStaleEntries(agent: Agent, engine: ExecutionEngine) {
    for (const order of await engine.getOpenOrders(agent.accountId)) {
      if (order.agentId === agent.id && order.type === 'LIMIT' && !order.reduceOnly) await engine.cancelOrder(order.id);
    }
  }
}

function settle(record: DecisionRecord, outcome: DecisionOutcome, detail: string | null = null) {
  record.outcome = outcome;
  record.outcomeDetail = detail;
  return record;
}

/** String(0.0000001) is "1e-7", which neither the API schema nor OKX accepts. */
const plain = (price: number | null) => (price === null ? null : d(price).toFixed());

const trailingSpec = (ai: AiTrailingStop | null): TrailingStopSpec | null =>
  ai === null ? null : { kind: ai.kind, callback: plain(ai.callback)!, activationPrice: plain(ai.activationPrice) };

/** What a PROTECT asks for: stop loss and take profit in full, and an absent trailingStop keeps the current one. */
function targetProtection(decision: AiDecisionData, current: Protection): Protection {
  return {
    stopLoss: plain(decision.stopLoss),
    takeProfit: plain(decision.takeProfit),
    trailingStop: decision.trailingStop === undefined ? current.trailingStop : trailingSpec(decision.trailingStop),
  };
}

/** Compared as decimals: the engine may store "60000.0" where the model wrote 60000. */
const sameLeg = (a: string | null, b: string | null) => (a === null || b === null ? a === b : d(a).eq(d(b)));

function sameProtection(a: Protection, b: Protection): boolean {
  const x = a.trailingStop;
  const y = b.trailingStop;
  const sameTrailing =
    x === null || y === null
      ? x === y
      : x.kind === y.kind && sameLeg(x.callback, y.callback) && sameLeg(x.activationPrice, y.activationPrice);
  return sameTrailing && sameLeg(a.stopLoss, b.stopLoss) && sameLeg(a.takeProfit, b.takeProfit);
}

/** An exchange account's ceilings win over an agent's settings, so sizing stays inside them. */
export function effectiveRisk(agent: RiskSettings, ceiling: RiskSettings | null): RiskSettings {
  if (!ceiling) return agent;
  return {
    maxLeverage: Math.min(agent.maxLeverage, ceiling.maxLeverage),
    maxRiskPerTradePct: Math.min(agent.maxRiskPerTradePct, ceiling.maxRiskPerTradePct),
    maxDailyLossPct: Math.min(agent.maxDailyLossPct, ceiling.maxDailyLossPct),
    maxOpenPositions: Math.min(agent.maxOpenPositions, ceiling.maxOpenPositions),
    requireStopLoss: agent.requireStopLoss || ceiling.requireStopLoss,
    maxQuoteAgeSec: Math.min(agent.maxQuoteAgeSec, ceiling.maxQuoteAgeSec),
    maxNotional: Math.min(agent.maxNotional, ceiling.maxNotional),
  };
}

/** Returns why the data is stale, or null when it is fresh. */
function isStale(quote: Quote, lastClosedBar: number, barMs: number, now: number, maxQuoteAgeSec: number): string | null {
  const age = (now - quote.ts) / 1000;
  if (age > maxQuoteAgeSec) return `Quote is ${age.toFixed(0)}s old, limit ${maxQuoteAgeSec}s`;
  if (now - (lastClosedBar + barMs) > 2 * barMs) return 'Candles lag the market by more than two bars';
  return null;
}

function estimateCost(config: ProviderConfig, input: number | null, output: number | null): string | null {
  if (config.kind === 'MOCK') return '0';
  if (config.inputPricePerMTok === null || config.outputPricePerMTok === null || input === null || output === null) return null;
  return ((input * config.inputPricePerMTok + output * config.outputPricePerMTok) / 1_000_000).toFixed(6);
}

/** Keep validated numbers intact while removing echoed credentials from persisted model text. */
function sanitizeRecord(record: DecisionRecord, secrets: Iterable<string>): DecisionRecord {
  const known = [...secrets];
  return JSON.parse(JSON.stringify(record, (_key, value: unknown) =>
    typeof value === 'string' ? redactSecrets(value, known) : value,
  )) as DecisionRecord;
}
