import { useCallback, useEffect, useRef, useState } from 'react';
import { create } from 'zustand';
import { DEFAULT_SYMBOLS, registerInstrument } from '@mochiprep/shared';
import type {
  AccountBalance,
  Agent,
  ArenaRow,
  DataSource,
  DecisionRecord,
  ExchangeStatus,
  FeedStatus,
  InstrumentSpecification,
  PaperAccount,
  ProviderView,
  Quote,
  ReplayState,
  ServerEvent,
  SymbolId,
  Topic,
  VaultStatus,
} from '@mochiprep/shared';
import { api, forgetToken, getToken } from './lib/api';
import { formatDateTime, parseTimeZone, type TimeZonePref } from './lib/display';

import { applyDirectionColors, parseDirectionColors, type DirectionColors } from './lib/direction-colors';

export type Page = 'dashboard' | 'markets' | 'agents' | 'paper' | 'arena' | 'history' | 'settings';
export type AccountView = PaperAccount & { balance: AccountBalance };
export interface Toast {
  id: number;
  level: 'info' | 'warn' | 'error';
  message: string;
}

interface State {
  connected: boolean;
  /** Master password state. LOCKED means saved keys are sealed until the user unlocks. */
  vault: VaultStatus;
  replay: ReplayState | null;
  feeds: Partial<Record<DataSource, FeedStatus>>;
  quotes: Record<DataSource, Partial<Record<SymbolId, Quote>>>;
  accounts: AccountView[];
  agents: Agent[];
  providers: ProviderView[];
  decisions: DecisionRecord[];
  historyDecisionId: string | null;
  arena: ArenaRow[];
  exchange: ExchangeStatus[];
  /** Suggestions waiting for the user. */
  signals: DecisionRecord[];
  /** Every pair available on live data. Replay only has the built-in ones. */
  symbols: SymbolId[];
  versions: Record<Topic, number>;
  page: Page;
  source: DataSource;
  accountId: string | null;
  theme: 'dark' | 'light';
  timeZone: TimeZonePref;
  directionColors: DirectionColors;
  toasts: Toast[];
  lastFillAt: number;
  lastAlertAt: number;

  setPage(page: Page, historyDecisionId?: string): void;
  setSource(source: DataSource): void;
  setAccountId(id: string | null): void;
  toggleTheme(): void;
  setTimeZone(zone: TimeZonePref): void;
  setDirectionColors(colors: DirectionColors): void;
  toast(level: Toast['level'], message: string): void;
  dismissToast(id: number): void;
  refreshState(): Promise<void>;
  refresh(topics: Topic[]): Promise<void>;
  connect(): void;
}

const recall = (key: string) => {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
};
const remembered = (key: string, fallback: string) => recall(key) ?? fallback;
const remember = (key: string, value: string) => {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* Preferences are a convenience; private windows may refuse them. */
  }
};

/** Formats a timestamp in the chosen zone, and re-renders the caller when the choice changes. Every time shown goes through it. */
export function useDateTime() {
  const zone = useApp((s) => s.timeZone);
  return useCallback((ts: number | null | undefined) => formatDateTime(ts, zone), [zone]);
}

let toastId = 0;
const ALL_TOPICS: Topic[] = ['accounts', 'orders', 'positions', 'agents', 'decisions', 'providers', 'arena', 'exchange', 'instruments', 'vault', 'templates'];

export const useApp = create<State>((set, get) => ({
  connected: false,
  vault: 'OFF',
  replay: null,
  feeds: {},
  quotes: { REPLAY: {}, LIVE: {} },
  accounts: [],
  agents: [],
  providers: [],
  decisions: [],
  historyDecisionId: null,
  arena: [],
  exchange: [],
  signals: [],
  symbols: [...DEFAULT_SYMBOLS],
  versions: { accounts: 0, orders: 0, positions: 0, agents: 0, decisions: 0, providers: 0, arena: 0, exchange: 0, instruments: 0, news: 0, vault: 0, templates: 0 },
  page: (location.hash.slice(1) as Page) || 'dashboard',
  source: remembered('mochi.source', 'REPLAY') as DataSource,
  accountId: null,
  theme: remembered('mochi.theme', 'dark') as 'dark' | 'light',
  timeZone: parseTimeZone(recall('mochi.timeZone')),
  directionColors: parseDirectionColors(recall('mochi.directionColors'), recall('mochi.flipColors')),
  toasts: [],
  lastFillAt: 0,
  lastAlertAt: 0,

  setPage(page, historyDecisionId) {
    location.hash = page;
    set({ page, historyDecisionId: page === 'history' ? historyDecisionId ?? null : null });
  },
  setSource(source) {
    remember('mochi.source', source);
    set({ source, accountId: null });
    void api.get<Quote[]>(`/market/quotes?source=${source}`).then((quotes) => {
      for (const quote of quotes) applyQuote(quote);
    });
  },
  setAccountId(accountId) {
    set({ accountId });
  },
  toggleTheme() {
    const theme = get().theme === 'dark' ? 'light' : 'dark';
    remember('mochi.theme', theme);
    document.documentElement.dataset.theme = theme;
    applyDirectionColors(get().directionColors, theme);
    set({ theme });
  },
  setTimeZone(timeZone) {
    remember('mochi.timeZone', timeZone);
    set({ timeZone });
  },
  setDirectionColors(colors) {
    const directionColors = parseDirectionColors(JSON.stringify(colors), null);
    remember('mochi.directionColors', JSON.stringify(directionColors));
    applyDirectionColors(directionColors, get().theme);
    set({ directionColors });
  },
  toast(level, message) {
    const id = ++toastId;
    set((s) => ({ toasts: [...s.toasts.slice(-3), { id, level, message }] }));
    setTimeout(() => get().dismissToast(id), level === 'error' ? 9000 : 5000);
  },
  dismissToast(id) {
    set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }));
  },
  async refreshState() {
    const state = await api.get<{ vault: VaultStatus; replay: ReplayState; feeds: FeedStatus[] }>('/state');
    set({
      vault: state.vault,
      replay: state.replay,
      feeds: Object.fromEntries(state.feeds.map((f) => [f.source, f])),
    });
  },
  async refresh(topics) {
    const jobs: Promise<void>[] = [];
    if (topics.includes('accounts')) jobs.push(api.get<AccountView[]>('/accounts').then((accounts) => set({ accounts })));
    if (topics.includes('agents')) jobs.push(api.get<Agent[]>('/agents').then((agents) => set({ agents })));
    if (topics.includes('providers')) jobs.push(api.get<ProviderView[]>('/providers').then((providers) => set({ providers })));
    if (topics.includes('decisions')) {
      jobs.push(api.get<DecisionRecord[]>('/signals').then((signals) => set({ signals })));
      jobs.push(
        api.get<DecisionRecord[]>('/decisions?limit=200').then((decisions) => {
          const previous = get().decisions[0];
          const newest = decisions[0];
          if (newest && newest.id !== previous?.id) {
            if (newest.outcome === 'EXECUTED') set({ lastFillAt: Date.now() });
            if (newest.outcome === 'RISK_REJECTED') set({ lastAlertAt: Date.now() });
          }
          set({ decisions });
        }),
      );
    }
    if (topics.includes('instruments')) {
      jobs.push(
        api.get<InstrumentSpecification[]>('/instruments').then((specs) => {
          for (const spec of specs) registerInstrument(spec);
          set({ symbols: specs.map((s) => s.symbol) });
        }),
      );
    }
    if (topics.includes('vault')) jobs.push(api.get<{ status: VaultStatus }>('/vault').then(({ status }) => set({ vault: status })));
    if (topics.includes('exchange')) jobs.push(api.get<ExchangeStatus[]>('/exchange/status').then((exchange) => set({ exchange })));
    if (topics.includes('arena')) jobs.push(api.get<ArenaRow[]>('/arena').then((arena) => set({ arena })));
    await Promise.allSettled(jobs);
    set((s) => {
      const versions = { ...s.versions };
      for (const topic of topics) versions[topic]++;
      return { versions };
    });
  },
  connect() {
    void boot();
  },
}));

function applyQuote(quote: Quote) {
  useApp.setState((s) => ({
    quotes: { ...s.quotes, [quote.source]: { ...s.quotes[quote.source], [quote.symbol]: quote } },
  }));
}

function onEvent(event: ServerEvent) {
  const app = useApp.getState();
  switch (event.type) {
    case 'heartbeat':
      break;
    case 'quote':
      applyQuote(event.quote);
      break;
    case 'feed':
      useApp.setState((s) => ({ feeds: { ...s.feeds, [event.status.source]: event.status } }));
      break;
    case 'replay':
      useApp.setState({ replay: event.state });
      break;
    case 'invalidate':
      void app.refresh(event.topics);
      break;
    case 'toast':
      app.toast(event.level, event.message);
      if (event.level === 'error') useApp.setState({ lastAlertAt: Date.now() });
      break;
  }
}

let source: EventSource | null = null;
let intentionallyDisconnected = false;

export function disconnectBackend() {
  intentionallyDisconnected = true;
  source?.close();
  source = null;
  forgetToken();
  useApp.setState({ connected: false });
}
let lastEventAt = Date.now();
const STREAM_SILENCE_MS = 40_000;

async function openStream() {
  if (intentionallyDisconnected) return;
  source?.close();
  try {
    const token = await getToken();
    if (intentionallyDisconnected) return;
    source = new EventSource(`/api/events?token=${token}`);
  } catch {
    reconnectStream();
    return;
  }
  source.onopen = () => {
    lastEventAt = Date.now();
    useApp.setState({ connected: true });
    const app = useApp.getState();
    void app.refreshState();
    void app.refresh(ALL_TOPICS);
  };
  source.onmessage = (message) => {
    lastEventAt = Date.now();
    onEvent(JSON.parse(message.data) as ServerEvent);
  };
  source.onerror = reconnectStream;
}

function reconnectStream() {
  if (intentionallyDisconnected) return;
  useApp.setState({ connected: false });
  source?.close();
  forgetToken();
  setTimeout(() => void openStream(), 2000);
}

async function boot() {
  document.documentElement.dataset.theme = useApp.getState().theme;
  applyDirectionColors(useApp.getState().directionColors, useApp.getState().theme);
  useApp.getState().setSource(useApp.getState().source);
  await openStream();
  // A proxy can keep a dead stream looking open, so silence counts as a disconnect.
  setInterval(() => {
    if (Date.now() - lastEventAt > STREAM_SILENCE_MS) {
      lastEventAt = Date.now();
      reconnectStream();
    }
  }, 5000);
  // Live marks move without engine events, so open PnL is re-read on a timer.
  setInterval(() => {
    if (!intentionallyDisconnected && useApp.getState().source === 'LIVE' && !document.hidden) {
      void useApp.getState().refresh(['accounts', 'positions']);
    }
  }, 3000);
}

/** Fetches on mount and whenever one of the topics is invalidated by the backend. */
export function useQuery<T>(fetcher: () => Promise<T>, deps: unknown[], topics: Topic[] = []) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const versions = useApp((s) => topics.reduce((sum, t) => sum + s.versions[t], 0));
  const sequence = useRef(0);
  const latest = useRef(fetcher);
  latest.current = fetcher;

  const load = useCallback(() => {
    const mine = ++sequence.current;
    latest
      .current()
      .then((value) => {
        if (mine === sequence.current) {
          setData(value);
          setError(null);
        }
      })
      .catch((e: Error) => {
        if (mine === sequence.current) setError(e.message);
      });
  }, []);

  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [...deps, versions]);
  return { data, error, reload: load };
}

/** Runs an action, turns failure into a toast, and reports whether it succeeded. */
export async function attempt<T>(action: () => T | Promise<T>): Promise<T | undefined> {
  try {
    return await action();
  } catch (error) {
    useApp.getState().toast('error', (error as Error).message);
    return undefined;
  }
}

/** Pairs that exist on the data source in view. */
/** The replay plays recorded OKX bars; false while it plays the bundled synthetic tape (offline). */
export const useRecordedReplay = (): boolean => useApp((s) => s.feeds.REPLAY?.provider === 'OKX history replay');

export const useSymbols = (): SymbolId[] => {
  const symbols = useApp((s) => s.symbols);
  const source = useApp((s) => s.source);
  return source === 'REPLAY' ? [...DEFAULT_SYMBOLS] : symbols;
};

/** Pending suggestions on the data source in view. */
export const useSourceSignals = () => {
  const signals = useApp((s) => s.signals);
  const source = useApp((s) => s.source);
  return signals.filter((s) => s.source === source);
};

/** Accounts on the data source currently in view. */
export const useSourceAccounts = () => {
  const accounts = useApp((s) => s.accounts);
  const source = useApp((s) => s.source);
  return accounts.filter((a) => a.source === source);
};
