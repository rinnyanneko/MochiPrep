import { AgentRunner } from '@mochiprep/agents';
import { mockProvider, MOCK_MODELS, ProviderError, createRemoteProvider, redactSecrets, type AiProvider } from '@mochiprep/ai-providers';
import { maxDrawdownPct, PaperExecutionEngine, winRatePct, d, str, type EngineEvent, type ExecutionEngine } from '@mochiprep/core';
import { OkxClient, OkxExecutionEngine, type OkxCredentials } from '@mochiprep/exchanges';
import { DEFAULT_NEWS_SOURCES, NewsService, OkxMarketData, ReplayMarketData, validateFeedUrl, type MarketDataProvider } from '@mochiprep/market-data';
import {
  ARM_PHRASE,
  DEFAULT_ACCOUNT_CONFIG,
  DEFAULT_LIVE_LIMITS,
  DEFAULT_RISK_SETTINGS,
  INSTRUMENTS,
  isDefaultSymbol,
  listSymbols,
  registerInstrument,
  unregisterInstrument,
  type AccountBalance,
  type Agent,
  type ArenaRow,
  type DataSource,
  type DecisionRecord,
  type ExchangeStatus,
  type FeedStatus,
  type InstrumentSpecification,
  type NewsSource,
  type PositionView,
  type ProviderConfig,
  type ProviderView,
  type Quote,
  type ReplayState,
  type RiskSettings,
  type ServerEvent,
  type SymbolId,
  type Timeframe,
  type Topic,
  type VaultStatus,
  VAULT_RESET_PHRASE,
} from '@mochiprep/shared';
import { randomUUID } from 'node:crypto';
import type { DatabaseSync } from 'node:sqlite';
import { SqliteStore } from './db/store';
import { DEFAULT_KDF, newDataKey, open, seal, unwrapWithPassword, wrapWithPassword, type KdfParams, type PasswordWrap } from './vault';

export const MOCK_PROVIDER_ID = 'mock';
const REPLAY_BATCH_MS = 100;
const QUOTE_BROADCAST_MS = 250;
const INVALIDATE_MS = 200;
const REPLAY_EQUITY_EVERY_MS = 15 * 60_000;
const LIVE_EQUITY_EVERY_MS = 60_000;
const DEFAULT_SPEED = 20;
const EXCHANGE_SYNC_MS = 5_000;
const PASSWORD_WRAP_ID = 'password';
const providerContext = (id: string) => `provider-key:${id}`;
const exchangeContext = (id: string) => `okx-credentials:${id}`;

interface SavedOkxCredentials extends OkxCredentials {
  environment: 'OKX_DEMO' | 'OKX_LIVE';
}

interface ExchangeSession {
  engine: OkxExecutionEngine;
  armed: boolean;
  secrets: string[];
  wasHealthy: boolean;
}

interface RuntimeOptions {
  db: DatabaseSync;
  live?: MarketDataProvider;
  replayMinutes?: number;
  /** Provider override for tests. */
  providerFactory?: (config: ProviderConfig, key: string) => AiProvider;
  /** Feed transport override for tests. */
  newsFetch?: typeof fetch;
  /** OKX transport override for tests. */
  okx?: { fetchImpl?: typeof fetch; baseUrl?: string };
  /** Key-derivation cost override for tests. */
  kdf?: KdfParams;
}

type Listener = (event: ServerEvent) => void;

/** Owns every long-lived piece: feeds, engine, agents, in-memory keys and the event stream. */
export class Runtime {
  readonly store: SqliteStore;
  readonly replay: ReplayMarketData;
  readonly live: MarketDataProvider;
  readonly engine: PaperExecutionEngine;
  readonly runner: AgentRunner;
  readonly news: NewsService;

  /** BYOK keys, mirrored from SQLite: plaintext without a master password, otherwise loaded on unlock. */
  private readonly keys: Map<string, string>;
  /** OKX sessions. Credentials reach disk only when the owner saves them, sealed by the master password. */
  private readonly exchanges = new Map<string, ExchangeSession>();
  /** Opens every sealed secret. Held only in memory, from unlock until the process exits. */
  private dataKey: Buffer | null = null;
  private vaultBusy = false;
  private readonly kdf: KdfParams;
  private readonly okxOptions: RuntimeOptions['okx'];
  private lastExchangeSyncAt = 0;
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
  private signalRaised = false;

  constructor(options: RuntimeOptions) {
    this.store = new SqliteStore(options.db);
    this.keys = this.store.hasVault() ? new Map() : this.store.getProviderKeys();
    this.kdf = options.kdf ?? DEFAULT_KDF;
    this.okxOptions = options.okx;
    this.replay = new ReplayMarketData(options.replayMinutes);
    this.live = options.live ?? new OkxMarketData();
    this.providerFactory = options.providerFactory ?? ((config, key) => createRemoteProvider(config, key));

    this.news = new NewsService(() => this.newsSources(), options.newsFetch);

    this.engine = new PaperExecutionEngine({
      store: this.store,
      getQuote: (source, symbol) => this.market(source).getQuote(symbol),
      now: (source) => this.market(source).now(),
      onEvent: (event) => this.onEngineEvent(event),
    });

    this.runner = new AgentRunner({
      store: this.store,
      execution: (id) => this.execution(id),
      market: (source) => this.market(source),
      account: (id) => this.store.getAccount(id),
      resolveProvider: (agent) => this.resolveProvider(agent),
      secrets: () => this.secrets(),
      onChange: (topics) => this.invalidate(...topics),
      onNotice: (level, message) => this.emit({ type: 'toast', level, message }),
      onSignal: (record) => this.onSignal(record),
      news: (agent) => this.newsFor(agent.symbol),
    });

    this.seedMockProvider();
    for (const spec of this.store.listInstruments()) {
      registerInstrument(spec);
      this.live.trackSymbol?.(spec.symbol);
    }
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
      const now = Date.now();
      if (now - this.lastExchangeSyncAt >= EXCHANGE_SYNC_MS) {
        this.lastExchangeSyncAt = now;
        await this.syncExchanges();
      }
      await this.runner.tick('LIVE');
      if (now - this.lastLiveEquityAt >= LIVE_EQUITY_EVERY_MS) {
        this.lastLiveEquityAt = now;
        this.recordEquity('LIVE');
        this.invalidate('arena');
      }
    } catch (error) {
      this.emit({ type: 'toast', level: 'error', message: redactSecrets(String(error), this.secrets()) });
    } finally {
      this.liveTicking = false;
    }
  }

  private recordEquity(source: DataSource) {
    for (const account of this.store.listAccounts()) {
      if (account.source === source && account.venue === 'PAPER') this.engine.recordEquity(account.id);
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

  /** The tape holds still at a suggestion, or it would expire before anyone could read it. */
  private onSignal(record: DecisionRecord) {
    if (record.source === 'REPLAY') {
      this.signalRaised = true;
      this.setPlaying(false);
    }
    const { action } = record.decision!;
    this.emit({ type: 'toast', level: 'info', message: `${record.agentName} suggests ${action} ${record.symbol}` });
  }

  private async doSteps(steps: number) {
    let quotes: Quote[] = [];
    try {
      this.signalRaised = false;
      for (let i = 0; i < steps && !this.signalRaised; i++) {
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
      this.emit({ type: 'toast', level: 'error', message: `Replay stopped: ${redactSecrets(String(error), this.secrets())}` });
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
          executionMode: 'AUTO',
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
    input: Omit<Agent, 'id' | 'status' | 'running' | 'nextRunAt' | 'lastRunAt' | 'statusDetail' | 'consecutiveErrors' | 'isDemo' | 'createdAt' | 'executionMode' | 'useNews' | 'allowTrailingStop'> & Partial<Pick<Agent, 'executionMode' | 'useNews' | 'allowTrailingStop'>>,
    isDemo = false,
  ): Agent {
    this.assertAgentRefs(input.providerId, input.accountId, input.symbol);
    const agent: Agent = {
      executionMode: 'AUTO',
      useNews: false,
      allowTrailingStop: false,
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

  assertAgentRefs(providerId: string, accountId: string, symbol: SymbolId) {
    if (!this.store.getProvider(providerId)) throw new Error('Unknown AI provider');
    const account = this.store.getAccount(accountId);
    if (!account) throw new Error('Unknown paper account');
    this.assertTradable(symbol, account.source);
  }

  /** Custom pairs exist only on live data: the replay tape has just the built-in three. */
  assertTradable(symbol: SymbolId, source: DataSource) {
    if (!INSTRUMENTS[symbol]) throw new Error(`${symbol} is not an added pair. Add it in Settings first.`);
    if (source === 'REPLAY' && !isDefaultSymbol(symbol)) throw new Error(`${symbol} has no replay data. Use a Live OKX account.`);
  }

  // ---- news -----------------------------------------------------------------------------

  newsSources(): NewsSource[] {
    const saved = this.store.getSetting<NewsSource[]>('news.sources') ?? [];
    // Built-ins come from code, so a corrected feed URL reaches existing installs too.
    const builtIn = DEFAULT_NEWS_SOURCES.map((source) => ({ ...source, enabled: saved.find((s) => s.id === source.id)?.enabled ?? source.enabled }));
    return [...builtIn, ...saved.filter((source) => !source.builtIn)];
  }

  private saveNewsSources(sources: NewsSource[]) {
    this.store.setSetting('news.sources', sources);
    this.news.invalidate();
    this.invalidate('news');
  }

  newsFor(symbol: SymbolId, limit?: number) {
    return this.news.forSymbol(symbol, INSTRUMENTS[symbol]?.baseCurrency ?? symbol.replace(/USDT$/, ''), limit);
  }

  addNewsSource(name: string, url: string): NewsSource {
    const problem = validateFeedUrl(url);
    if (problem) throw new Error(problem);
    if (this.newsSources().some((s) => s.url === url)) throw new Error('This feed is already added');
    const source: NewsSource = { id: `news_${randomUUID()}`, name, url, enabled: true, builtIn: false };
    this.saveNewsSources([...this.newsSources(), source]);
    return source;
  }

  setNewsSourceEnabled(id: string, enabled: boolean) {
    const sources = this.newsSources();
    if (!sources.some((s) => s.id === id)) throw new Error('Unknown news source');
    this.saveNewsSources(sources.map((s) => (s.id === id ? { ...s, enabled } : s)));
  }

  removeNewsSource(id: string) {
    const source = this.newsSources().find((s) => s.id === id);
    if (!source) throw new Error('Unknown news source');
    if (source.builtIn) throw new Error('Built-in sources can be turned off, not removed');
    this.saveNewsSources(this.newsSources().filter((s) => s.id !== id));
  }

  // ---- trading pairs --------------------------------------------------------------------

  async availableInstruments(): Promise<InstrumentSpecification[]> {
    if (!this.live.listInstruments) throw new Error('This feed cannot list instruments');
    return (await this.live.listInstruments()).filter((spec) => !INSTRUMENTS[spec.symbol]);
  }

  /** Specs always come from OKX at this moment, so orders are never sized from a guess. */
  async addInstrument(symbol: SymbolId): Promise<InstrumentSpecification> {
    if (INSTRUMENTS[symbol]) throw new Error(`${symbol} is already added`);
    if (!this.live.listInstruments) throw new Error('This feed cannot list instruments');
    const spec = (await this.live.listInstruments(true)).find((s) => s.symbol === symbol);
    if (!spec) throw new Error(`${symbol} is not a live USDT crypto perpetual on OKX`);
    this.store.saveInstrument(spec);
    registerInstrument(spec);
    this.live.trackSymbol?.(symbol);
    this.invalidate('instruments');
    return spec;
  }

  async removeInstrument(symbol: SymbolId) {
    if (isDefaultSymbol(symbol)) throw new Error(`${symbol} is built in`);
    if (!INSTRUMENTS[symbol]) throw new Error(`${symbol} is not an added pair`);
    if (this.store.listAgents().some((a) => a.symbol === symbol)) throw new Error('Agents still use this pair');
    for (const account of this.store.listAccounts()) {
      if ((await this.positionsOf(account.id)).some((p) => p.symbol === symbol)) throw new Error(`${account.name} has an open ${symbol} position`);
      if (this.store.listOpenOrders(account.id).some((o) => o.symbol === symbol)) throw new Error(`${account.name} has open ${symbol} orders`);
    }
    this.store.deleteInstrument(symbol);
    this.live.untrackSymbol?.(symbol);
    unregisterInstrument(symbol);
    this.invalidate('instruments');
  }

  private resolveProvider(agent: Agent) {
    const config = this.store.getProvider(agent.providerId);
    if (!config) throw new ProviderError('NO_KEY', 'The AI provider for this agent was removed');
    return { config, provider: this.providerFor(config) };
  }

  providerFor(config: ProviderConfig): AiProvider {
    if (config.kind === 'MOCK') return mockProvider;
    const key = this.keys.get(config.id);
    if (!key && this.vaultStatus() === 'LOCKED') throw new ProviderError('NO_KEY', `Unlock MochiPrep with your master password to load the key for "${config.name}".`);
    if (!key) throw new ProviderError('NO_KEY', `No API key saved for "${config.name}". Enter it in Settings.`);
    return this.providerFactory(config, key);
  }

  setKey(providerId: string, key: string) {
    const status = this.vaultStatus();
    if (status === 'LOCKED') throw new Error('Unlock MochiPrep with your master password first');
    if (status === 'UNLOCKED') this.store.saveSealedProviderKey(providerId, seal(this.dataKey!, key, providerContext(providerId)));
    else this.store.saveProviderKey(providerId, key);
    this.keys.set(providerId, key);
    this.invalidate('providers');
  }

  clearKey(providerId: string) {
    this.store.deleteProviderKey(providerId);
    this.keys.delete(providerId);
    this.invalidate('providers');
  }

  private *secrets() {
    yield* this.keys.values();
    for (const provider of this.store.listProviders()) yield* Object.values(provider.extraHeaders ?? {});
    for (const session of this.exchanges.values()) yield* session.secrets;
  }

  redact(text: string) {
    return redactSecrets(text, this.secrets());
  }

  // ---- master password ------------------------------------------------------------------

  vaultStatus(): VaultStatus {
    if (this.dataKey) return 'UNLOCKED';
    return this.store.hasVault() ? 'LOCKED' : 'OFF';
  }

  /** Each change derives a key for hundreds of ms; one at a time keeps two from racing. */
  private async exclusive<T>(fn: () => Promise<T>): Promise<T> {
    if (this.vaultBusy) throw new Error('Another master password action is in progress');
    this.vaultBusy = true;
    try {
      return await fn();
    } finally {
      this.vaultBusy = false;
    }
  }

  private async openVault(password: string): Promise<Buffer> {
    const wrap = this.store.getVaultKey<PasswordWrap>(PASSWORD_WRAP_ID);
    if (!wrap) throw new Error('No master password is set');
    return unwrapWithPassword(wrap, password);
  }

  /** Seals the AI keys already saved and removes their plaintext, in one transaction. */
  enableVault(password: string) {
    return this.exclusive(async () => {
      if (this.vaultStatus() !== 'OFF') throw new Error('A master password is already set');
      const dataKey = newDataKey();
      const wrap = await wrapWithPassword(dataKey, password, this.kdf);
      this.store.transaction(() => {
        this.store.saveVaultKey(PASSWORD_WRAP_ID, 'PASSWORD', wrap);
        for (const [id, key] of this.store.getProviderKeys()) this.store.saveSealedProviderKey(id, seal(dataKey, key, providerContext(id)));
      });
      this.store.scrub();
      this.dataKey = dataKey;
      this.invalidate('vault', 'providers', 'exchange');
    });
  }

  /**
   * Loads sealed AI keys and reconnects saved OKX accounts. Reconnecting runs every check a
   * manual connect does and never arms a real account: arming always takes the typed phrase.
   */
  unlock(password: string) {
    return this.exclusive(async () => {
      if (this.vaultStatus() !== 'LOCKED') throw new Error('MochiPrep is not locked');
      const dataKey = await this.openVault(password);
      const keys = [...this.store.getSealedProviderKeys()].map(([id, value]) => [id, open(dataKey, value, providerContext(id))] as const);
      this.dataKey = dataKey;
      for (const [id, key] of keys) this.keys.set(id, key);
      this.invalidate('vault', 'providers', 'exchange');

      const failed: string[] = [];
      for (const { accountId, sealed } of this.store.listSealedExchangeCredentials()) {
        const account = this.store.getAccount(accountId);
        if (!account || this.exchanges.has(accountId)) continue;
        try {
          const saved = JSON.parse(open(dataKey, sealed, exchangeContext(accountId))) as SavedOkxCredentials;
          await this.connectOkx(
            { ...saved, name: account.name, limits: account.liveLimits ?? DEFAULT_LIVE_LIMITS, remember: true },
            accountId,
          );
        } catch (error) {
          failed.push(account.name);
          this.emit({ type: 'toast', level: 'error', message: `${account.name}: could not reconnect to OKX. ${this.redact((error as Error).message)}` });
        }
      }
      return { failed };
    });
  }

  changePassword(current: string, next: string) {
    return this.exclusive(async () => {
      const dataKey = await this.openVault(current);
      this.store.saveVaultKey(PASSWORD_WRAP_ID, 'PASSWORD', await wrapWithPassword(dataKey, next, this.kdf));
    });
  }

  /** Back to unencrypted AI keys. Saved OKX credentials are deleted: they never exist in plaintext. */
  disableVault(password: string) {
    return this.exclusive(async () => {
      const dataKey = await this.openVault(password);
      const keys = [...this.store.getSealedProviderKeys()].map(([id, value]) => [id, open(dataKey, value, providerContext(id))] as const);
      this.store.transaction(() => {
        for (const [id, key] of keys) this.store.saveProviderKey(id, key);
        this.store.deleteVault();
      });
      for (const [id, key] of keys) this.keys.set(id, key);
      this.dataKey = null;
      this.invalidate('vault', 'providers', 'exchange');
    });
  }

  /** For a lost password. Nothing sealed can be recovered, so all of it is deleted. */
  resetVault(confirmation: string) {
    if (confirmation !== VAULT_RESET_PHRASE) throw new Error(`Type exactly: ${VAULT_RESET_PHRASE}`);
    if (this.vaultBusy) throw new Error('Another master password action is in progress');
    if (this.vaultStatus() === 'OFF') throw new Error('No master password is set');
    const sealed = [...this.store.getSealedProviderKeys().keys()];
    this.store.transaction(() => this.store.deleteVault());
    for (const id of sealed) this.keys.delete(id);
    this.dataKey = null;
    this.invalidate('vault', 'providers', 'exchange');
  }

  // ---- exchange accounts ----------------------------------------------------------------

  /** The engine an account trades through. Throws when an exchange account is not connected. */
  execution(accountId: string): ExecutionEngine {
    const account = this.store.getAccount(accountId);
    if (!account) throw new Error('Unknown account');
    if (account.venue === 'PAPER') return this.engine;
    const session = this.exchanges.get(accountId);
    if (!session) throw new Error(`${account.name} is not connected to OKX. Enter its API key in Settings.`);
    return session.engine;
  }

  /**
   * Verifies a key before anything can trade on it. A key that can withdraw is refused outright:
   * a leak of a trade-only key cannot move funds off the exchange.
   */
  async connectOkx(
    input: OkxCredentials & { name: string; environment: 'OKX_DEMO' | 'OKX_LIVE'; limits: RiskSettings; remember?: boolean },
    /** Set when reconnecting saved credentials, which must still belong to that account. */
    expectedAccountId?: string,
  ) {
    if (input.remember && this.vaultStatus() !== 'UNLOCKED') throw new Error('Set a master password and unlock it to save OKX keys');
    const credentials = { apiKey: input.apiKey, secretKey: input.secretKey, passphrase: input.passphrase };
    const client = new OkxClient(credentials, input.environment, this.okxOptions);
    const config = await client.getConfig();
    const permissions = (config.perm ?? '').split(',').map((p) => p.trim());
    if (permissions.includes('withdraw')) throw new Error('This API key has withdrawal permission. Create a key without it.');
    if (!permissions.includes('trade')) throw new Error('This API key has no trade permission.');
    if (config.posMode !== 'net_mode') throw new Error('Set OKX position mode to one-way (net) first. MochiPrep does not support hedge mode.');
    if (config.acctLv === '1') throw new Error('This OKX account is in Spot mode, which cannot trade perpetuals. Switch it to Futures mode.');
    for (const symbol of listSymbols()) {
      const spec = await client.getContractValue(symbol);
      const known = INSTRUMENTS[symbol]!;
      if (Number(spec.ctVal) !== Number(known.venueContractValue) || Number(spec.lotSz) !== Number(known.venueLotSize)) {
        throw new Error(`OKX contract specs for ${symbol} changed. Refusing to size orders from stale values.`);
      }
    }

    const uid = config.uid ?? '';
    let account = this.store.listAccounts().find((a) => a.venue === input.environment && a.exchangeUid === uid);
    if (expectedAccountId && account?.id !== expectedAccountId) throw new Error('The saved API key now belongs to a different OKX account. Reconnect it by hand.');
    if (account) {
      account.liveLimits = input.limits;
      this.store.saveAccountProfile(account);
    } else {
      account = {
        id: `acct_${randomUUID()}`,
        name: input.name,
        source: 'LIVE',
        venue: input.environment,
        exchangeUid: uid,
        liveLimits: input.limits,
        isDemo: false,
        initialBalance: '0',
        walletBalance: '0',
        usedMargin: '0',
        realizedPnl: '0',
        feesPaid: '0',
        fundingPaid: '0',
        config: DEFAULT_ACCOUNT_CONFIG,
        createdAt: Date.now(),
      };
      this.store.insertAccount(account);
    }

    const { id, venue } = account;
    const session: ExchangeSession = {
      armed: venue === 'OKX_DEMO',
      secrets: [input.apiKey, input.secretKey, input.passphrase],
      wasHealthy: true,
      engine: new OkxExecutionEngine({
        accountId: id,
        venue: input.environment,
        client,
        store: this.store,
        getQuote: (symbol) => this.live.getQuote(symbol),
        now: () => Date.now(),
        isArmed: () => this.exchanges.get(id)?.armed ?? false,
        limits: () => this.store.getAccount(id)?.liveLimits ?? DEFAULT_LIVE_LIMITS,
      }),
    };
    // Trading is only possible once a full reconciliation has succeeded.
    await session.engine.sync();
    this.exchanges.set(id, session);
    if (input.remember) {
      // Checked again: the vault may have been reset while OKX was answering.
      if (!this.dataKey) throw new Error('Connected, but the key was not saved: the master password was removed meanwhile');
      const saved: SavedOkxCredentials = { ...credentials, environment: input.environment };
      this.store.saveSealedExchangeCredentials(id, seal(this.dataKey, JSON.stringify(saved), exchangeContext(id)));
    } else if (this.dataKey) {
      // While locked the save box cannot be ticked, so an unticked box is not a choice to forget the key.
      this.store.deleteSealedExchangeCredentials(id);
    }
    if (account.initialBalance === '0') {
      account.initialBalance = (await session.engine.getBalance()).equity;
      this.store.saveAccountProfile(account);
    }
    this.invalidate('accounts', 'positions', 'orders', 'exchange', 'arena');
    return account;
  }

  /** Also forgets saved credentials, so a disconnected account does not return on the next unlock. */
  disconnectExchange(accountId: string) {
    this.exchanges.delete(accountId);
    this.store.deleteSealedExchangeCredentials(accountId);
    this.runner.pauseAccountAgents(accountId, 'OKX disconnected');
    this.invalidate('accounts', 'exchange');
  }

  arm(accountId: string, confirmation: string) {
    const session = this.exchanges.get(accountId);
    if (!session) throw new Error('Connect this account first');
    if (confirmation !== ARM_PHRASE) throw new Error(`Type exactly: ${ARM_PHRASE}`);
    if (!session.engine.healthy) throw new Error('Cannot arm until sync with OKX succeeds');
    session.armed = true;
    this.invalidate('exchange');
  }

  /** Real accounts only: demo funds need no safety catch. Exits stay possible when disarmed. */
  disarm(accountId?: string) {
    for (const [id, session] of this.exchanges) {
      if ((!accountId || id === accountId) && this.store.getAccount(id)?.venue === 'OKX_LIVE') session.armed = false;
    }
    this.invalidate('exchange');
  }

  /** An agent may only be scheduled on an exchange account that could accept its orders. */
  assertCanRun(agent: Agent) {
    const account = this.store.getAccount(agent.accountId);
    if (!account || account.venue === 'PAPER') return;
    const session = this.exchanges.get(account.id);
    if (!session) throw new Error(`${account.name} is not connected to OKX`);
    // A suggestion places nothing, so analysis does not need real orders switched on.
    if (agent.executionMode === 'AUTO' && !session.armed) throw new Error(`${account.name} is not armed. Arm it in Settings first.`);
  }

  /**
   * Checked before a suggestion is consumed: an order refused here would burn its idempotency
   * key, and the user could not retry after arming. Exits need no arming.
   */
  assertCanExecuteSignal(signal: DecisionRecord) {
    const account = this.store.getAccount(signal.accountId);
    if (account?.venue !== 'OKX_LIVE' || signal.decision?.action === 'CLOSE') return;
    if (!this.exchanges.get(account.id)?.armed) throw new Error(`${account.name} is not armed. Arm it in Settings first.`);
  }

  private async syncExchanges() {
    for (const [id, session] of this.exchanges) {
      try {
        await session.engine.sync();
        session.wasHealthy = true;
      } catch (error) {
        if (session.wasHealthy) {
          session.wasHealthy = false;
          const account = this.store.getAccount(id);
          this.runner.pauseAccountAgents(id, 'Lost sync with OKX');
          this.emit({ type: 'toast', level: 'error', message: `${account?.name}: lost sync with OKX. Agents paused. ${this.redact((error as Error).message)}` });
        }
      }
      this.invalidate('accounts', 'positions', 'orders', 'exchange', 'arena');
    }
  }

  exchangeStatuses(): ExchangeStatus[] {
    return this.store.listAccounts().flatMap((account) => {
      if (account.venue === 'PAPER') return [];
      const session = this.exchanges.get(account.id);
      return [
        {
          accountId: account.id,
          name: account.name,
          venue: account.venue,
          connected: !!session,
          saved: this.store.hasSealedExchangeCredentials(account.id),
          armed: session?.armed ?? false,
          healthy: session?.engine.healthy ?? false,
          lastSyncAt: session?.engine.lastSync ?? null,
          error: session?.engine.error ? this.redact(session.engine.error) : null,
          limits: account.liveLimits ?? DEFAULT_LIVE_LIMITS,
        },
      ];
    });
  }

  /** Read-model balance: never throws, so a disconnected exchange account still renders. */
  balanceOf(accountId: string): AccountBalance {
    const account = this.store.getAccount(accountId)!;
    if (account.venue === 'PAPER') return this.engine.balanceOf(accountId);
    const cached = this.exchanges.get(accountId)?.engine.cachedBalance();
    if (cached) return cached;
    const point = this.store.listEquity(accountId, 1)[0];
    const equity = point?.equity ?? account.initialBalance;
    return {
      accountId,
      initialBalance: account.initialBalance,
      walletBalance: point?.wallet ?? equity,
      availableBalance: '0',
      usedMargin: '0',
      orderMargin: '0',
      equity,
      realizedPnl: '0',
      unrealizedPnl: point?.unrealized ?? '0',
      feesPaid: '0',
      fundingPaid: '0',
    };
  }

  async positionsOf(accountId: string): Promise<PositionView[]> {
    const account = this.store.getAccount(accountId)!;
    if (account.venue !== 'PAPER' && !this.exchanges.has(accountId)) return [];
    return this.execution(accountId).getPositions(accountId);
  }

  providerViews(): ProviderView[] {
    return this.store.listProviders().map(({ extraHeaders, ...p }) => ({
      ...p,
      headerNames: Object.keys(extraHeaders ?? {}),
      requiresKey: p.kind !== 'MOCK',
      hasKey: p.kind === 'MOCK' || this.keys.has(p.id),
    }));
  }

  // ---- read models ----------------------------------------------------------------------

  feeds(): FeedStatus[] {
    return [this.replay.status(), this.live.status()];
  }

  quotes(source: DataSource): Quote[] {
    return listSymbols().flatMap((s) => this.market(source).getQuote(s) ?? []);
  }

  async candles(source: DataSource, symbol: SymbolId, timeframe: Timeframe, limit: number) {
    return this.market(source).getCandles(symbol, timeframe, limit);
  }

  arena(): ArenaRow[] {
    const providers = new Map(this.store.listProviders().map((p) => [p.id, p]));
    return this.store.listAgents().flatMap((agent) => {
      const account = this.store.getAccount(agent.accountId);
      if (!account) return [];
      const balance = this.balanceOf(account.id);
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
          venue: account.venue,
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
