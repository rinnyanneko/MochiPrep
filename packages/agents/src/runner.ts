import {
  evaluateRisk,
  type PaperExecutionEngine,
} from '@mochiprep/core';
import { ProviderError, redactSecrets, type AiProvider, type MarketContext } from '@mochiprep/ai-providers';
import type { MarketDataProvider } from '@mochiprep/market-data';
import {
  indicatorSnapshot,
  TIMEFRAME_MS,
  type Agent,
  type AiDecisionData,
  type DataSource,
  type DecisionOutcome,
  type DecisionRecord,
  type OrderRequest,
  type PaperAccount,
  type ProviderConfig,
  type Quote,
  type Topic,
} from '@mochiprep/shared';
import { parseDecision } from './parse';
import { buildUserPrompt, SYSTEM_PROMPT } from './prompt';

export const MAX_CONSECUTIVE_ERRORS = 3;
const MIN_CANDLES = 30;
const RAW_RESPONSE_LIMIT = 8_000;

export interface AgentStore {
  insertAgent(agent: Agent): void;
  saveAgent(agent: Agent): void;
  getAgent(id: string): Agent | undefined;
  listAgents(): Agent[];
  insertDecision(record: DecisionRecord): void;
}

export interface AgentRunnerDeps {
  store: AgentStore;
  engine: PaperExecutionEngine;
  market(source: DataSource): MarketDataProvider;
  account(id: string): PaperAccount | undefined;
  /** Throws ProviderError('NO_KEY') when the key is not loaded. */
  resolveProvider(agent: Agent): { provider: AiProvider; config: ProviderConfig };
  secrets(): Iterable<string>;
  onChange(topics: Topic[]): void;
  onNotice?(level: 'warn' | 'error', message: string): void;
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
      const record = await this.analyzeAndAct(agent, market, source, trigger);
      this.deps.store.insertDecision(record);
      this.finishCycle(agent, market, record);
      this.deps.onChange(['decisions', 'accounts', 'orders', 'positions', 'arena']);
      return record;
    } finally {
      this.inFlight.delete(agentId);
    }
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
    const { engine } = this.deps;
    const now = market.now();
    const record: DecisionRecord = {
      id: this.newId(),
      agentId: agent.id,
      agentName: agent.name,
      accountId: agent.accountId,
      source,
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
      orderId: null,
      fillIds: [],
      latencyMs: null,
      inputTokens: null,
      outputTokens: null,
      estimatedCost: null,
      isMock: false,
    };
    const conclude = (outcome: DecisionOutcome, detail: string | null = null) => {
      record.outcome = outcome;
      record.outcomeDetail = detail;
      return record;
    };

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
    this.cancelStaleEntries(agent);
    const balance = engine.balanceOf(agent.accountId);
    const positions = await engine.getPositions(agent.accountId);
    const position = positions.find((p) => p.symbol === agent.symbol) ?? null;

    const stale = isStale(quote, closed[closed.length - 1]!.time, TIMEFRAME_MS[agent.timeframe], now, agent.risk.maxQuoteAgeSec);
    if (stale && !position) return conclude('DATA_ERROR', stale);

    const indicators = indicatorSnapshot(closed);
    record.indicators = indicators;

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
    };
    let text: string;
    const started = Date.now();
    try {
      const { provider, config } = this.deps.resolveProvider(agent);
      record.providerKind = config.kind;
      record.isMock = config.kind === 'MOCK';
      const response = await provider.complete({
        model: agent.model,
        system: SYSTEM_PROMPT,
        user: buildUserPrompt(context),
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
    record.rawResponse = redactSecrets(text.slice(0, RAW_RESPONSE_LIMIT), this.deps.secrets());

    // 7. Validate.
    const parsed = parseDecision(text);
    if (!parsed.ok) {
      record.validationError = parsed.error;
      return conclude('INVALID_OUTPUT', parsed.error);
    }
    record.decision = parsed.decision;

    // 8. Risk. Time and quote are re-read: the model may have taken a while.
    const latestQuote = market.getQuote(agent.symbol) ?? null;
    const daily = engine.dailyRisk(agent.accountId);
    const account = this.deps.account(agent.accountId)!;
    record.risk = evaluateRisk({
      decision: parsed.decision,
      symbol: agent.symbol,
      settings: agent.risk,
      quote: stale ? null : latestQuote,
      now: market.now(),
      balance: engine.balanceOf(agent.accountId),
      openPositions: await engine.getPositions(agent.accountId),
      dailyPnl: daily.dailyPnl,
      dayStartEquity: daily.dayStartEquity,
      fees: account.config,
    });

    if (trigger === 'ANALYZE') return conclude('ANALYSIS_ONLY', 'Analyze Now never places orders');
    if (!record.risk.approved) return conclude('RISK_REJECTED', record.risk.rejectionReasons.join(' | '));
    if (parsed.decision.action === 'HOLD') return conclude('HOLD');

    // A Stop pressed while the model was thinking must win.
    if (trigger === 'SCHEDULE' && !this.requireAgent(agent.id).running) {
      return conclude('ANALYSIS_ONLY', 'Agent was stopped before execution');
    }

    // 9. Execute.
    this.update(agent, { status: 'EXECUTING' });
    const result = await engine.placeOrder(this.toOrder(agent, parsed.decision, record));
    record.orderId = result.order.id;
    record.fillIds = result.fills.map((f) => f.id);
    switch (result.order.status) {
      case 'FILLED':
        return conclude('EXECUTED', `Filled ${result.order.quantity} @ ${result.order.avgFillPrice}`);
      case 'OPEN':
        return conclude('ORDER_PLACED', `Resting ${result.order.side} limit @ ${result.order.price}`);
      default:
        return conclude('ORDER_REJECTED', result.order.rejectReason);
    }
  }

  private toOrder(agent: Agent, decision: AiDecisionData, record: DecisionRecord): OrderRequest {
    const common = {
      accountId: agent.accountId,
      clientOrderId: `agent:${agent.id}:${record.id}`,
      symbol: agent.symbol,
      agentId: agent.id,
      decisionId: record.id,
    };
    if (decision.action === 'CLOSE') {
      const position = this.deps.engine.positionViews(agent.accountId).find((p) => p.symbol === agent.symbol)!;
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
      price: decision.orderType === 'LIMIT' ? String(decision.entryPrice) : null,
      leverage: sized.leverage,
      stopLoss: decision.stopLoss === null ? null : String(decision.stopLoss),
      takeProfit: decision.takeProfit === null ? null : String(decision.takeProfit),
    };
  }

  private cancelStaleEntries(agent: Agent) {
    const { engine } = this.deps;
    for (const order of engine.openOrders(agent.accountId)) {
      if (order.agentId === agent.id && order.type === 'LIMIT' && !order.reduceOnly) engine.cancelOrderSync(order.id);
    }
  }
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

