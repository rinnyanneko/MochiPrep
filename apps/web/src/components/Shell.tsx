import {
  Bot,
  CircleAlert,
  History,
  Info,
  LayoutDashboard,
  CandlestickChart,
  Moon,
  OctagonX,
  Pause,
  Play,
  Settings,
  SkipForward,
  Sun,
  Swords,
  TriangleAlert,
  Wallet,
  X,
} from 'lucide-react';
import { useEffect, useState, type ReactNode } from 'react';
import { api } from '@/lib/api';
import { dateTime } from '@/lib/format';
import { DYOR_NOTICE } from '@/lib/notices';
import { cn } from '@/lib/utils';
import { attempt, useApp, useSourceSignals, type Page } from '@/store';
import { Mochi, MOOD_LABEL, type MochiMood } from './Mochi';
import { Badge, Button, Dialog, Segmented, Select, type Tone } from './ui';
import { UnlockBanner } from './VaultCard';

const NAV: { id: Page; label: string; icon: typeof Bot }[] = [
  { id: 'dashboard', label: 'Dashboard', icon: LayoutDashboard },
  { id: 'markets', label: 'Markets', icon: CandlestickChart },
  { id: 'agents', label: 'AI Agents', icon: Bot },
  { id: 'paper', label: 'Trading', icon: Wallet },
  { id: 'arena', label: 'Trading Arena', icon: Swords },
  { id: 'history', label: 'Trade History', icon: History },
  { id: 'settings', label: 'Settings', icon: Settings },
];

/** Mascot state always reflects real app state, most urgent first. */
function useMood(): MochiMood {
  const agents = useApp((s) => s.agents);
  const lastFillAt = useApp((s) => s.lastFillAt);
  const lastAlertAt = useApp((s) => s.lastAlertAt);
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  if (agents.some((a) => a.status === 'ERROR' || a.statusDetail?.startsWith('Auto-paused'))) return 'confused';
  if (now - lastAlertAt < 5000) return 'alert';
  if (now - lastFillAt < 4000) return 'happy';
  if (agents.some((a) => a.running || a.status === 'ANALYZING')) return 'thinking';
  return 'sleeping';
}

function StopAllButton({ className, compact }: { className?: string; compact?: boolean }) {
  const agents = useApp((s) => s.agents);
  const armed = useApp((s) => s.exchange.some((e) => e.venue === 'OKX_LIVE' && e.armed));
  const [confirming, setConfirming] = useState(false);
  const active = agents.filter((a) => a.running).length;

  const stopAll = async () => {
    const result = await attempt(() => api.post<{ stopped: number }>('/agents/stop-all'));
    setConfirming(false);
    if (result) useApp.getState().toast('info', `Stopped ${result.stopped} agent(s), disarmed real orders, paused the replay.`);
  };

  return (
    <>
      <Button variant="danger" size={compact ? 'sm' : 'md'} className={className} onClick={() => setConfirming(true)} disabled={active === 0 && !armed}>
        <OctagonX /> {compact ? 'Stop all' : 'Stop all agents'}{active > 0 ? ` (${active})` : ''}
      </Button>
      <Dialog open={confirming} onOpenChange={setConfirming} title="Stop all agents?" description="Every running agent stops, real orders are disarmed, and the replay pauses. Open positions, their stops and resting orders stay in place, on OKX too.">
        <div className="flex justify-end gap-2">
          <Button onClick={() => setConfirming(false)}>Keep running</Button>
          <Button variant="danger" onClick={stopAll}>
            <OctagonX /> Stop all
          </Button>
        </div>
      </Dialog>
    </>
  );
}

function Sidebar() {
  const page = useApp((s) => s.page);
  const setPage = useApp((s) => s.setPage);
  const mood = useMood();
  const pendingSignals = useSourceSignals().length;

  return (
    <aside className="flex shrink-0 flex-col border-line bg-surface max-lg:order-last max-lg:border-t lg:w-56 lg:border-r">
      <div className="items-center gap-2.5 px-4 pt-4 pb-3 max-lg:hidden lg:flex">
        <Mochi mood={mood} size={44} />
        <div>
          <div className="font-display text-lg leading-none font-black">MochiPrep</div>
          <div className="mt-1 text-[11px] text-ink-3" data-testid="mochi-mood">{MOOD_LABEL[mood]}</div>
        </div>
      </div>
      <nav className="flex gap-0.5 overflow-x-auto px-2 max-lg:py-1 lg:flex-1 lg:flex-col lg:py-2" aria-label="Main">
        {NAV.map((item) => (
          <button
            key={item.id}
            type="button"
            onClick={() => setPage(item.id)}
            aria-current={page === item.id ? 'page' : undefined}
            className={cn(
              'flex items-center gap-2.5 rounded-xl px-3 py-2 text-[13px] font-semibold whitespace-nowrap transition-colors max-lg:flex-col max-lg:gap-0.5 max-lg:px-2.5 max-lg:text-[10px]',
              page === item.id ? 'bg-accent-soft text-accent' : 'text-ink-2 hover:bg-surface-2 hover:text-ink',
            )}
          >
            <item.icon className="size-4 shrink-0" />
            {item.label}
            {item.id === 'paper' && pendingSignals > 0 && (
              <span className="ml-auto rounded-full bg-accent px-1.5 text-[11px] text-accent-ink" data-testid="signal-count">{pendingSignals}</span>
            )}
          </button>
        ))}
      </nav>
      <div className="p-3 max-lg:hidden">
        <StopAllButton className="w-full" />
      </div>
    </aside>
  );
}

const FEED_TONE: Record<string, Tone> = {
  CONNECTED: 'up',
  OFFLINE_REPLAY: 'info',
  CONNECTING: 'warn',
  DEGRADED: 'warn',
  DISCONNECTED: 'down',
};
const FEED_LABEL: Record<string, string> = {
  CONNECTED: 'OKX live',
  OFFLINE_REPLAY: 'Synthetic replay',
  CONNECTING: 'Connecting',
  DEGRADED: 'OKX degraded',
  DISCONNECTED: 'OKX offline',
};

export function FeedBadge() {
  const source = useApp((s) => s.source);
  const feed = useApp((s) => s.feeds[source]);
  if (!feed) return null;
  return (
    <Badge tone={FEED_TONE[feed.state]} title={feed.detail} data-testid="feed-status">
      <span className="size-1.5 rounded-full bg-current" />
      {FEED_LABEL[feed.state]}
    </Badge>
  );
}

function ReplayControls() {
  const replay = useApp((s) => s.replay);
  if (!replay) return null;
  const send = (body: object) => attempt(() => api.post('/replay', body));
  return (
    <div className="flex min-w-0 items-center gap-2" data-testid="replay-controls">
      <Button size="icon" variant={replay.playing ? 'secondary' : 'primary'} aria-label={replay.playing ? 'Pause replay' : 'Play replay'} disabled={replay.atEnd} onClick={() => send({ playing: !replay.playing })}>
        {replay.playing ? <Pause /> : <Play />}
      </Button>
      <Button size="icon" variant="ghost" aria-label="Step 15 minutes" title="Step 15 minutes" disabled={replay.atEnd || replay.playing} onClick={() => send({ steps: 15 })}>
        <SkipForward />
      </Button>
      <Select aria-label="Replay speed" className="h-8 w-[104px] text-xs" value={replay.speed} onChange={(e) => send({ speed: Number(e.target.value) })}>
        {[1, 5, 20, 60, 240].map((s) => (
          <option key={s} value={s}>{s} min/s</option>
        ))}
      </Select>
      <div className="min-w-0 max-sm:hidden">
        <div className="num truncate text-[11px] text-ink-2" data-testid="replay-time">{dateTime(replay.now)}</div>
        <div className="mt-1 h-1 w-36 overflow-hidden rounded-full bg-surface-3">
          <div className="h-full rounded-full bg-accent" style={{ width: `${replay.progress * 100}%` }} />
        </div>
      </div>
    </div>
  );
}

function TopBar() {
  const source = useApp((s) => s.source);
  const setSource = useApp((s) => s.setSource);
  const theme = useApp((s) => s.theme);
  const toggleTheme = useApp((s) => s.toggleTheme);
  const connected = useApp((s) => s.connected);
  const armedReal = useApp((s) => s.exchange.filter((e) => e.venue === 'OKX_LIVE' && e.armed).length);
  const anyExchange = useApp((s) => s.exchange.some((e) => e.connected));
  return (
    <header className="flex flex-wrap items-center gap-x-4 gap-y-2 border-b border-line bg-surface px-4 py-2.5">
      <Segmented
        label="Market data source"
        value={source}
        onChange={setSource}
        options={[
          { value: 'REPLAY', label: 'Replay' },
          { value: 'LIVE', label: 'Live OKX' },
        ]}
      />
      <FeedBadge />
      {source === 'REPLAY' && <ReplayControls />}
      <div className="ml-auto flex items-center gap-2">
        <StopAllButton compact className="lg:hidden" />
        {armedReal > 0 ? (
          <Badge tone="down" data-testid="real-money-badge"><TriangleAlert /> Real money armed</Badge>
        ) : (
          <Badge tone="accent">{anyExchange ? 'Real orders off' : 'Paper only'}</Badge>
        )}
        {!connected && <Badge tone="down">Backend offline</Badge>}
        <Button size="icon" variant="ghost" aria-label="Toggle theme" onClick={toggleTheme}>
          {theme === 'dark' ? <Sun /> : <Moon />}
        </Button>
      </div>
    </header>
  );
}

function DemoBanner() {
  const demoActive = useApp((s) => s.demoActive);
  const source = useApp((s) => s.source);
  if (!demoActive || source !== 'REPLAY') return null;
  return (
    <div className="flex items-center gap-2 bg-info-soft px-4 py-1.5 text-xs font-medium text-info" data-testid="demo-banner">
      <Info className="size-3.5 shrink-0" />
      Demo Mode: market data is synthetic and AI decisions come from a deterministic mock. No real market or model is involved.
    </div>
  );
}

function Toasts() {
  const toasts = useApp((s) => s.toasts);
  const dismiss = useApp((s) => s.dismissToast);
  const icons = { info: Info, warn: TriangleAlert, error: CircleAlert };
  const tones = { info: 'text-info', warn: 'text-warn', error: 'text-down' };
  return (
    <div className="fixed right-4 bottom-4 z-50 flex w-80 max-w-[calc(100vw-32px)] flex-col gap-2" role="status">
      {toasts.map((toast) => {
        const Icon = icons[toast.level];
        return (
          <div key={toast.id} className="flex items-start gap-2 rounded-2xl border border-line bg-surface-2 p-3 text-[13px] shadow-lg">
            <Icon className={cn('mt-0.5 size-4 shrink-0', tones[toast.level])} />
            <span className="min-w-0 flex-1 break-words">{toast.message}</span>
            <button type="button" aria-label="Dismiss" onClick={() => dismiss(toast.id)} className="text-ink-3 hover:text-ink">
              <X className="size-4" />
            </button>
          </div>
        );
      })}
    </div>
  );
}

export function Shell({ children }: { children: ReactNode }) {
  return (
    <div className="flex h-full max-lg:flex-col">
      <Sidebar />
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <TopBar />
        <UnlockBanner />
        <DemoBanner />
        <main className="min-h-0 flex-1 overflow-y-auto p-4 lg:p-5">
          <div className="mx-auto max-w-[1400px]">{children}</div>
        </main>
        <p className="border-t border-line bg-surface px-4 py-1.5 text-center text-[11px] text-ink-3" data-testid="dyor-notice">
          {DYOR_NOTICE}
        </p>
      </div>
      <Toasts />
    </div>
  );
}

export function PageTitle({ title, description, action }: { title: string; description?: string; action?: ReactNode }) {
  return (
    <div className="mb-4 flex flex-wrap items-end justify-between gap-3">
      <div>
        <h1 className="font-display text-2xl font-black">{title}</h1>
        {description && <p className="mt-0.5 text-[13px] text-ink-3">{description}</p>}
      </div>
      {action}
    </div>
  );
}
