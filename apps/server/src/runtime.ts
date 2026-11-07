import { AgentRunner } from '@mochiprep/agents';
import { mockProvider, MOCK_MODELS, ProviderError, createRemoteProvider, redactSecrets, type AiProvider } from '@mochiprep/ai-providers';
import { maxDrawdownPct, PaperExecutionEngine, winRatePct, d, str, type EngineEvent } from '@mochiprep/core';
import { OkxMarketData, ReplayMarketData, type MarketDataProvider } from '@mochiprep/market-data';
import {
  DEFAULT_RISK_SETTINGS,
  SYMBOLS,
  type Agent,
  type ArenaRow,
  type DataSource,
  type FeedStatus,
  type ProviderConfig,
  type ProviderView,
  type Quote,
  type ReplayState,
  type ServerEvent,
  type SymbolId,
  type Timeframe,
  type Topic,
} from '@mochiprep/shared';
import { randomUUID } from 'node:crypto';
import type { DatabaseSync } from 'node:sqlite';
import { SqliteStore } from './db/store';

export const MOCK_PROVIDER_ID = 'mock';
const REPLAY_BATCH_MS = 100;
const QUOTE_BROADCAST_MS = 250;
const INVALIDATE_MS = 200;
const REPLAY_EQUITY_EVERY_MS = 15 * 60_000;
const LIVE_EQUITY_EVERY_MS = 60_000;
const DEFAULT_SPEED = 20;

interface RuntimeOptions {
  db: DatabaseSync;
  live?: MarketDataProvider;
  replayMinutes?: number;
  /** Provider override for tests. */
  providerFactory?: (config: ProviderConfig, key: string) => AiProvider;
}

type Listener = (event: ServerEvent) => void;

/** Owns every long-lived piece: feeds, engine, agents, in-memory keys and the event stream. */
export class Runtime {
  readonly store: SqliteStore;
  readonly replay: ReplayMarketData;
  readonly live: MarketDataProvider;
  readonly engine: PaperExecutionEngine;
  readonly runner: AgentRunner;

  /** BYOK keys. In this process's memory only: never written to disk, gone on restart. */
  private readonly keys = new Map<string, string>();
  private readonly providerFactory: (config: ProviderConfig, key: string) => AiProvider;
  private readonly listeners = new Set<Listener>();
  private readonly dirty = new Set<Topic>();
  private readonly lastBroadcast = new Map<SymbolId, number>();

  private playing = false;
  private speed = DEFAULT_SPEED;
  private stepCredit = 0;
  private stepping: Promise<void> | null = null;
  private replayTimer: NodeJS.Timeout | null = null;
  private liveTimer: NodeJS.Timeout | null = null;
  private invalidateTimer: NodeJS.Timeout | null = null;
  private lastReplayEquityAt = 0;
  private lastLiveEquityAt = 0;
  private liveTicking = false;

  constructor(options: RuntimeOptions) {
    this.store = new SqliteStore(options.db);
    this.replay = new ReplayMarketData(options.replayMinutes);
    this.live = options.live ?? new OkxMarketData();
    this.providerFactory = options.providerFactory ?? ((config, key) => createRemoteProvider(config, key));

    this.engine = new PaperExecutionEngine({
      store: this.store,
      getQuote: (source, symbol) => this.market(source).getQuote(symbol),
      now: (source) => this.market(source).now(),
      onEvent: (event) => this.onEngineEvent(event),
    });

    this.runner = new AgentRunner({
      store: this.store,
      engine: this.engine,
      market: (source) => this.market(source),
      account: (id) => this.store.getAccount(id),
      resolveProvider: (agent) => this.resolveProvider(agent),
      secrets: () => this.keys.values(),
      onChange: (topics) => this.invalidate(...topics),
      onNotice: (level, message) => this.emit({ type: 'toast', level, message }),
    });

    this.seedMockProvider();
    this.restore();
  }

  market(source: DataSource): MarketDataProvider {
    return source === 'REPLAY' ? this.replay : this.live;
  }

  private seedMockProvider() {
    if (this.store.getProvider(MOCK_PROVIDER_ID)) return;
    this.store.insertProvider({
      id: MOCK_PROVIDER_ID,
      name: 'Mock AI (offline demo)',
      kind: 'MOCK',
      baseUrl: null,
      defaultModel: MOCK_MODELS.momentum,
      inputPricePerMTok: null,
      outputPricePerMTok: null,
      createdAt: Date.now(),
    });
  }

  /** Rebuilds state after a restart. All durable state is in SQLite; only the tape cursor needs seeking. */
  private restore() {
    const cursor = this.store.getSetting<number>('replay.cursor');
    if (cursor !== undefined) this.replay.seek(cursor);
    this.speed = this.store.getSetting<number>('replay.speed') ?? DEFAULT_SPEED;
    this.lastReplayEquityAt = this.replay.now();
    this.runner.disarmAfterRestart();
  }

  start() {
    this.live.onQuote((quote) => this.onLiveQuote(quote));
    this.live.onStatus((status) => this.emit({ type: 'feed', status }));
    this.live.start();
    this.liveTimer = setInterval(() => void this.liveTick(), 1000);
  }

  async stop() {
    this.setPlaying(false);
    if (this.stepping) await this.stepping;
    if (this.liveTimer) clearInterval(this.liveTimer);
    if (this.invalidateTimer) clearTimeout(this.invalidateTimer);
    this.live.stop();
  }

  // ---- events ---------------------------------------------------------------------------

  subscribe(listener: Listener) {
    this.listeners.add(listener);
    return () => void this.listeners.delete(listener);
  }

  emit(event: ServerEvent) {
    for (const listener of this.listeners) listener(event);
  }

  invalidate(...topics: Topic[]) {
    for (const topic of topics) this.dirty.add(topic);
    this.invalidateTimer ??= setTimeout(() => {
      this.invalidateTimer = null;
      const list = [...this.dirty];
      this.dirty.clear();
      if (list.length) this.emit({ type: 'invalidate', topics: list });
    }, INVALIDATE_MS);
  }

  private onEngineEvent(event: EngineEvent) {
    this.invalidate('accounts', 'orders', 'positions', 'arena');
    if (event.kind === 'liquidation') {
      const account = this.store.getAccount(event.position.accountId);
      this.runner.pauseAccountAgents(event.position.accountId, `Position liquidated on ${event.position.symbol}`);
      this.emit({
        type: 'toast',
        level: 'error',
        message: `${account?.name ?? 'Account'}: ${event.position.side} ${event.position.symbol} was liquidated (simulated)`,
      });
    }
  }

  // ---- live feed ------------------------------------------------------------------------

  private onLiveQuote(quote: Quote) {
    this.engine.onQuote(quote);
    const now = Date.now();
    if (now - (this.lastBroadcast.get(quote.symbol) ?? 0) >= QUOTE_BROADCAST_MS) {
      this.lastBroadcast.set(quote.symbol, now);
      this.emit({ type: 'quote', quote });
    }
  }

  private async liveTick() {
    if (this.liveTicking) return;
    this.liveTicking = true;
    try {
      await this.runner.tick('LIVE');
      const now = Date.now();
      if (now - this.lastLiveEquityAt >= LIVE_EQUITY_EVERY_MS) {
        this.lastLiveEquityAt = now;
        this.recordEquity('LIVE');
        this.invalidate('arena');
      }
    } catch (error) {
      this.emit({ type: 'toast', level: 'error', message: redactSecrets(String(error), this.keys.values()) });
    } finally {
      this.liveTicking = false;
    }
  }

  private recordEquity(source: DataSource) {
    for (const account of this.store.listAccounts()) {
      if (account.source === source) this.engine.recordEquity(account.id);
    }
  }

  // ---- replay ---------------------------------------------------------------------------

  replayState(): ReplayState {
    const start = this.replay.firstPlayableTime;
    const end = this.replay.endTime;
    const now = this.replay.now();
    return {
      loaded: true,
      playing: this.playing,
      speed: this.speed,
      now,
      startTime: start,
      endTime: end,
      atEnd: this.replay.atEnd,
      progress: Math.min(1, Math.max(0, (now - start) / (end - start))),
    };
  }

  setSpeed(speed: number) {
    this.speed = speed;
    this.store.setSetting('replay.speed', speed);
    this.emit({ type: 'replay', state: this.replayState() });
  }

  setPlaying(playing: boolean) {
    if (playing === this.playing) return;
    this.playing = playing;
    if (playing && !this.replayTimer) {
      this.replayTimer = setInterval(() => this.onReplayTimer(), REPLAY_BATCH_MS);
    } else if (!playing && this.replayTimer) {
      clearInterval(this.replayTimer);
      this.replayTimer = null;
    }
    this.emit({ type: 'replay', state: this.replayState() });
  }

  private onReplayTimer() {
    // The tape waits for agents: a slow model pauses market time rather than missing bars.
    if (this.stepping) return;
    this.stepCredit += (this.speed * REPLAY_BATCH_MS) / 1000;
    const steps = Math.floor(this.stepCredit);
    if (steps < 1) return;
    this.stepCredit -= steps;
    void this.stepReplay(steps);
  }

  /** Advances the tape, awaiting all due agents at every bar. Calls are serialized. */
  async stepReplay(steps: number): Promise<void> {
    while (this.stepping) await this.stepping;
    this.stepping = this.doSteps(steps).finally(() => {
      this.stepping = null;
    });
    return this.stepping;
  }

  private async doSteps(steps: number) {
    let quotes: Quote[] = [];
    try {
      for (let i = 0; i < steps; i++) {
        const next = this.replay.step();
        if (next.length === 0) break;
        quotes = next;
        for (const quote of next) this.engine.onQuote(quote);
        await this.runner.tick('REPLAY');
        if (this.replay.now() - this.lastReplayEquityAt >= REPLAY_EQUITY_EVERY_MS) {
          this.lastReplayEquityAt = this.replay.now();
          this.recordEquity('REPLAY');
          this.invalidate('arena', 'accounts');
        }
      }
    } catch (error) {
      this.setPlaying(false);
      this.emit({ type: 'toast', level: 'error', message: `Replay stopped: ${redactSecrets(String(error), this.keys.values())}` });
    }
    this.store.setSetting('replay.cursor', this.replay.position);
    for (const quote of quotes) this.emit({ type: 'quote', quote });
    if (this.replay.atEnd) this.setPlaying(false);
    this.emit({ type: 'replay', state: this.replayState() });
    this.invalidate('accounts', 'positions');
  }

  // ---- demo -----------------------------------------------------------------------------

  isDemoActive() {
    return this.store.getSetting<boolean>('demo.active') === true;
  }

  startDemo() {
    if (this.isDemoActive()) return;
    this.replay.seek(0);
    this.store.setSetting('replay.cursor', this.replay.position);
    this.lastReplayEquityAt = this.replay.now();

    const seed = (name: string, model: string, strategy: string, label: string, risk = DEFAULT_RISK_SETTINGS) => {
      const account = this.engine.createAccount({ name: `${label} account`, source: 'REPLAY', isDemo: true });
      this.createAgent(
        {
          name,
          providerId: MOCK_PROVIDER_ID,
          model,
          symbol: 'BTCUSDT',
          timeframe: '15m',
          strategy,
          accountId: account.id,
          risk,
          intervalSec: 900,
        },
        true,
      );
    };
    seed('Momentum Mochi', MOCK_MODELS.momentum, 'Follow the trend: EMA20/EMA50 with MACD confirmation.', 'Momentum');
    seed('Reversion Daifuku', MOCK_MODELS.reversion, 'Fade RSI extremes and exit at the midline.', 'Reversion');
    seed('Reckless Rice Cake', MOCK_MODELS.reckless, 'Misbehaves on purpose to show the risk engine at work.', 'Reckless');
    this.engine.createAccount({ name: 'Demo terminal', source: 'REPLAY', isDemo: true });

    this.store.setSetting('demo.active', true);
    this.invalidate('accounts', 'agents', 'arena', 'decisions', 'orders', 'positions');
    this.emit({ type: 'replay', state: this.replayState() });
  }

  /** Removes everything demo-owned and rebuilds it from the first bar. User data is untouched. */
  async resetDemo() {
    this.setPlaying(false);
    if (this.stepping) await this.stepping;
    this.clearDemo();
    this.startDemo();
  }

  /** Leaves Demo Mode: removes demo data without rebuilding it. */
  async exitDemo() {
    this.setPlaying(false);
    if (this.stepping) await this.stepping;
    this.clearDemo();
    this.invalidate('accounts', 'agents', 'arena', 'decisions', 'orders', 'positions');
  }

  private clearDemo() {
    this.store.transaction(() => {
      for (const agent of this.store.listAgents()) if (agent.isDemo) this.store.deleteAgent(agent.id);
      for (const account of this.store.listAccounts()) {
        if (account.isDemo) this.store.deleteAccountData(account.id, false);
      }
      this.store.setSetting('demo.active', false);
    });
  }

  // ---- agents & providers ---------------------------------------------------------------

  createAgent(
    input: Omit<Agent, 'id' | 'status' | 'running' | 'nextRunAt' | 'lastRunAt' | 'statusDetail' | 'consecutiveErrors' | 'isDemo' | 'createdAt'>,
    isDemo = false,
  ): Agent {
    this.assertAgentRefs(input.providerId, input.accountId);
    const agent: Agent = {
      ...input,
      id: `agent_${randomUUID()}`,
      status: 'IDLE',
      running: false,
      nextRunAt: null,
      lastRunAt: null,
      statusDetail: null,
      consecutiveErrors: 0,
      isDemo,
      createdAt: Date.now(),
    };
    this.store.insertAgent(agent);
    this.invalidate('agents', 'arena');
    return agent;
  }

  assertAgentRefs(providerId: string, accountId: string) {
    if (!this.store.getProvider(providerId)) throw new Error('Unknown AI provider');
    if (!this.store.getAccount(accountId)) throw new Error('Unknown paper account');
  }

  private resolveProvider(agent: Agent) {
    const config = this.store.getProvider(agent.providerId);
    if (!config) throw new ProviderError('NO_KEY', 'The AI provider for this agent was removed');
    return { config, provider: this.providerFor(config) };
  }

  providerFor(config: ProviderConfig): AiProvider {
    if (config.kind === 'MOCK') return mockProvider;
    const key = this.keys.get(config.id);
    if (!key) throw new ProviderError('NO_KEY', `API key for "${config.name}" is not loaded. Keys live in memory, so re-enter it in Settings.`);
    return this.providerFactory(config, key);
  }

  setKey(providerId: string, key: string) {
    this.keys.set(providerId, key);
    this.invalidate('providers');
  }

  clearKey(providerId: string) {
    this.keys.delete(providerId);
    this.invalidate('providers');
  }

  redact(text: string) {
    return redactSecrets(text, this.keys.values());
  }

  providerViews(): ProviderView[] {
    return this.store.listProviders().map((p) => ({
      ...p,
      requiresKey: p.kind !== 'MOCK',
      hasKey: p.kind === 'MOCK' || this.keys.has(p.id),
    }));
  }

  // ---- read models ----------------------------------------------------------------------

  feeds(): FeedStatus[] {
    return [this.replay.status(), this.live.status()];
  }

  quotes(source: DataSource): Quote[] {
    return SYMBOLS.flatMap((s) => this.market(source).getQuote(s) ?? []);
  }

  async candles(source: DataSource, symbol: SymbolId, timeframe: Timeframe, limit: number) {
    return this.market(source).getCandles(symbol, timeframe, limit);
  }

  arena(): ArenaRow[] {
    const providers = new Map(this.store.listProviders().map((p) => [p.id, p]));
    return this.store.listAgents().flatMap((agent) => {
      const account = this.store.getAccount(agent.accountId);
      if (!account) return [];
      const balance = this.engine.balanceOf(account.id);
      const history = this.store.listEquity(account.id, 5000);
      const closed = this.store.listClosedPositions(account.id, 5000);
      const net = closed.map((p) => d(p.realizedPnl).minus(p.feesPaid).minus(p.fundingPaid).toNumber());
      const points = history.map((p) => ({ ts: p.ts, equity: Number(p.equity) }));
      const equities = [...points.map((p) => p.equity), Number(balance.equity)];
      const stats = this.store.decisionStats(agent.id);
      const provider = providers.get(agent.providerId);
      const stride = Math.max(1, Math.ceil(points.length / 400));
      const initial = d(account.initialBalance);
      return [
        {
          agentId: agent.id,
          agentName: agent.name,
          accountId: account.id,
          source: account.source,
          isMock: provider?.kind === 'MOCK',
          providerName: provider?.name ?? 'Removed provider',
          model: agent.model,
          symbol: agent.symbol,
          status: agent.status,
          initialBalance: account.initialBalance,
          equity: balance.equity,
          returnPct: d(balance.equity).minus(initial).div(initial).mul(100).toNumber(),
          realizedPnl: balance.realizedPnl,
          maxDrawdownPct: maxDrawdownPct(equities),
          winRatePct: winRatePct(net),
          tradeCount: closed.length,
          decisionCount: stats.count,
          estimatedAiCost: stats.estimatedCost === null ? null : str(d(stats.estimatedCost)),
          curve: points.filter((_, i) => i % stride === 0 || i === points.length - 1),
        },
      ];
    });
  }
}
