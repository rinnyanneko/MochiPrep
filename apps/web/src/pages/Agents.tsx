import { Pause, Pencil, Play, Plus, ScanSearch, Square, StepForward, Trash2 } from 'lucide-react';
import { useState } from 'react';
import { DEFAULT_RISK_SETTINGS, TIMEFRAMES, type ExecutionMode, type Agent, type AgentStatus, type DecisionRecord, type RiskSettings, type SymbolId, type Timeframe } from '@mochiprep/shared';
import { Mochi, type MochiMood } from '@/components/Mochi';
import { PageTitle } from '@/components/Shell';
import { DecisionDetail, DecisionList, VenueBadge } from '@/components/trading';
import { Badge, Button, Card, CardHeader, Dialog, Field, Input, Segmented, Select, Textarea, type Tone } from '@/components/ui';
import { api } from '@/lib/api';
import { money, pct, pnlClass } from '@/lib/format';
import { cn } from '@/lib/utils';
import { attempt, useApp, useDateTime, useQuery, useSourceAccounts, useSymbols, type AccountView } from '@/store';

const STATUS: Record<AgentStatus, { tone: Tone; mood: MochiMood; label: string }> = {
  IDLE: { tone: 'neutral', mood: 'sleeping', label: 'Idle' },
  ANALYZING: { tone: 'info', mood: 'thinking', label: 'Analyzing' },
  WAITING: { tone: 'up', mood: 'thinking', label: 'Waiting' },
  EXECUTING: { tone: 'accent', mood: 'happy', label: 'Executing' },
  PAUSED: { tone: 'warn', mood: 'sleeping', label: 'Paused' },
  ERROR: { tone: 'down', mood: 'confused', label: 'Error' },
};

const MOCK_MODELS = ['mock-momentum-v1', 'mock-reversion-v1', 'mock-reckless-v1'];
const INTERVALS = [
  { value: 30, label: 'Every 30 seconds (demo pace)' },
  { value: 60, label: 'Every minute' },
  { value: 300, label: 'Every 5 minutes' },
  { value: 900, label: 'Every 15 minutes (default)' },
  { value: 3600, label: 'Every hour' },
];

type Form = {
  name: string;
  providerId: string;
  model: string;
  symbol: SymbolId;
  timeframe: Timeframe;
  strategy: string;
  accountId: string;
  risk: RiskSettings;
  intervalSec: number;
  executionMode: ExecutionMode;
  useNews: boolean;
  allowTrailingStop: boolean;
};

function AgentDialog({ agent, onClose }: { agent: Agent | null; onClose(): void }) {
  const providers = useApp((s) => s.providers);
  const source = useApp((s) => s.source);
  const accounts = useSourceAccounts();
  const symbols = useSymbols();
  const [form, setForm] = useState<Form>(
    agent ?? {
      name: '',
      providerId: providers[0]?.id ?? '',
      model: providers[0]?.defaultModel ?? '',
      symbol: 'BTCUSDT',
      timeframe: '15m',
      strategy: '',
      accountId: 'NEW',
      risk: DEFAULT_RISK_SETTINGS,
      intervalSec: 900,
      executionMode: 'SIGNAL',
      useNews: source === 'LIVE',
      allowTrailingStop: false,
    },
  );
  const set = (patch: Partial<Form>) => setForm((f) => ({ ...f, ...patch }));
  const setRisk = (patch: Partial<RiskSettings>) => setForm((f) => ({ ...f, risk: { ...f.risk, ...patch } }));
  const provider = providers.find((p) => p.id === form.providerId);
  const selectedAccount = accounts.find((a) => a.id === form.accountId);

  const save = async () => {
    const done = await attempt(async () => {
      let accountId = form.accountId;
      if (accountId === 'NEW') {
        const created = await api.post<AccountView>('/accounts', { name: `${form.name} account`, source, initialBalance: '10000' });
        accountId = created.id;
      }
      const body = { ...form, accountId };
      const fields = { name: body.name, providerId: body.providerId, model: body.model, symbol: body.symbol, timeframe: body.timeframe, strategy: body.strategy, accountId, risk: body.risk, intervalSec: body.intervalSec, executionMode: body.executionMode, useNews: body.useNews, allowTrailingStop: body.allowTrailingStop };
      return agent ? api.put(`/agents/${agent.id}`, fields) : api.post('/agents', fields);
    });
    if (done) {
      void useApp.getState().refresh(['accounts', 'agents', 'arena']);
      onClose();
    }
  };

  const number = (label: string, key: keyof RiskSettings, hint?: string) => (
    <Field label={label} hint={hint}>
      <Input type="number" step="any" min={0} value={form.risk[key] as number} onChange={(e) => setRisk({ [key]: Number(e.target.value) })} />
    </Field>
  );

  return (
    <Dialog open onOpenChange={onClose} title={agent ? `Edit ${agent.name}` : 'New AI agent'} description="Changes apply from the next cycle." wide>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name"><Input value={form.name} onChange={(e) => set({ name: e.target.value })} placeholder="Careful Kinako" /></Field>
        <Field label="AI provider">
          <Select value={form.providerId} onChange={(e) => {
            const next = providers.find((p) => p.id === e.target.value);
            set({ providerId: e.target.value, model: next?.defaultModel ?? '' });
          }}>
            {providers.map((p) => <option key={p.id} value={p.id}>{p.name}{p.requiresKey && !p.hasKey ? ' (key needed)' : ''}</option>)}
          </Select>
        </Field>
        <Field label="Model">
          {provider?.kind === 'MOCK' ? (
            <Select value={form.model} onChange={(e) => set({ model: e.target.value })}>
              {MOCK_MODELS.map((m) => <option key={m}>{m}</option>)}
            </Select>
          ) : (
            <Input value={form.model} onChange={(e) => set({ model: e.target.value })} />
          )}
        </Field>
        <Field label="Paper account">
          <Select value={form.accountId} onChange={(e) => set({ accountId: e.target.value })}>
            {!agent && <option value="NEW">Create a new 10,000 USDT account</option>}
            {accounts.map((a) => <option key={a.id} value={a.id}>{a.name}{a.venue === 'OKX_LIVE' ? ' (REAL MONEY)' : a.venue === 'OKX_DEMO' ? ' (OKX demo)' : ''}</option>)}
          </Select>
        </Field>
        <Field label="Symbol" hint={source === 'REPLAY' ? 'Added pairs need Live OKX: the replay tape has only these.' : 'Add more pairs in Settings.'}>
          <Select value={form.symbol} onChange={(e) => set({ symbol: e.target.value as SymbolId })}>{symbols.map((s) => <option key={s}>{s}</option>)}</Select>
        </Field>
        <Field label="Timeframe">
          <Select value={form.timeframe} onChange={(e) => set({ timeframe: e.target.value as Timeframe })}>{TIMEFRAMES.map((t) => <option key={t}>{t}</option>)}</Select>
        </Field>
        <Field label="Schedule" className="sm:col-span-2" hint={source === 'REPLAY' ? 'Replay agents run on tape time, not wall time.' : undefined}>
          <Select value={form.intervalSec} onChange={(e) => set({ intervalSec: Number(e.target.value) })}>
            {INTERVALS.map((i) => <option key={i.value} value={i.value}>{i.label}</option>)}
          </Select>
        </Field>
        <div className="sm:col-span-2">
          <span className="mb-1 block text-xs font-semibold text-ink-2">When the AI finds a trade</span>
          <Segmented
            label="Execution mode"
            value={form.executionMode}
            onChange={(executionMode) => set({ executionMode })}
            options={[
              { value: 'SIGNAL', label: 'Suggest to me' },
              { value: 'AUTO', label: 'Trade automatically' },
            ]}
          />
          <span className="mt-1 block text-[11px] text-ink-3">
            {form.executionMode === 'SIGNAL' ? 'You see the entry, stop and target, and decide. Nothing is ordered until you accept.' : 'Approved orders are placed immediately, with no confirmation.'}
          </span>
        </div>
        <label className="flex items-start gap-2 text-xs text-ink-2 sm:col-span-2">
          <input type="checkbox" className="mt-0.5 accent-[var(--accent)]" disabled={source === 'REPLAY'} checked={form.useNews && source === 'LIVE'} onChange={(e) => set({ useNews: e.target.checked })} />
          <span>
            Read recent news headlines
            <span className="block text-[11px] text-ink-3">
              {source === 'REPLAY' ? 'Live OKX only: real headlines would not match the synthetic tape.' : 'Headlines are unverified third-party text. They can be wrong or written to mislead an AI. Sources are in Settings.'}
            </span>
          </span>
        </label>
        <Field label="Strategy instructions" className="sm:col-span-2" hint="Sent to the model as style guidance. It cannot override the risk engine.">
          <Textarea value={form.strategy} onChange={(e) => set({ strategy: e.target.value })} placeholder="Trade pullbacks in the direction of the 1h trend..." />
        </Field>
      </div>
      <h3 className="mt-4 mb-2 font-display text-sm font-extrabold">Risk limits</h3>
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
        {number('Max leverage (x)', 'maxLeverage')}
        {number('Risk per trade (%)', 'maxRiskPerTradePct')}
        {number('Max daily loss (%)', 'maxDailyLossPct')}
        {number('Max open positions', 'maxOpenPositions')}
        {number('Max quote age (s)', 'maxQuoteAgeSec')}
        {number('Max notional (USDT)', 'maxNotional')}
      </div>
      <label className="mt-3 flex items-center gap-2 text-xs text-ink-2">
        <input type="checkbox" checked={form.risk.requireStopLoss} onChange={(e) => setRisk({ requireStopLoss: e.target.checked })} className="accent-[var(--accent)]" />
        Require a stop loss on new positions
      </label>
      <label className="mt-2 flex items-start gap-2 text-xs text-ink-2">
        <input type="checkbox" checked={form.allowTrailingStop} onChange={(e) => set({ allowTrailingStop: e.target.checked })} className="mt-0.5 accent-[var(--accent)]" />
        <span>
          Allow trailing stops
          <span className="block text-[11px] text-ink-3">Off: this agent cannot set, change or remove trailing stops.</span>
        </span>
      </label>
      {selectedAccount?.venue === 'OKX_LIVE' && form.executionMode === 'AUTO' && (
        <p className="mt-3 rounded-xl bg-down-soft px-3 py-2 text-[13px] font-semibold text-down">
          This agent will place real orders on its own while running. The account's limits in Settings cap everything above.
        </p>
      )}
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="primary" disabled={!form.name.trim() || !form.model.trim()} onClick={save}>{agent ? 'Save' : 'Create agent'}</Button>
      </div>
    </Dialog>
  );
}

function AgentCard({ agent, selected, onSelect, onEdit }: { agent: Agent; selected: boolean; onSelect(): void; onEdit(): void }) {
  const dateTime = useDateTime();
  const account = useApp((s) => s.accounts.find((a) => a.id === agent.accountId));
  const provider = useApp((s) => s.providers.find((p) => p.id === agent.providerId));
  const [busy, setBusy] = useState(false);
  const status = STATUS[agent.status];
  const initial = Number(account?.initialBalance ?? 0);
  const equity = Number(account?.balance.equity ?? 0);
  const change = initial ? ((equity - initial) / initial) * 100 : 0;

  const act = (path: string) => attempt(() => api.post(`/agents/${agent.id}/${path}`));
  const cycle = async (path: 'analyze' | 'run') => {
    setBusy(true);
    await attempt(() => api.post<DecisionRecord>(`/agents/${agent.id}/${path}`));
    setBusy(false);
    onSelect();
  };

  return (
    <Card className={cn('p-4 transition-colors', selected && 'border-accent')} data-testid="agent-card">
      <button type="button" className="flex w-full items-center gap-3 text-left" onClick={onSelect} aria-pressed={selected}>
        <Mochi mood={status.mood} size={48} />
        <span className="min-w-0 flex-1">
          <span className="flex items-center gap-2">
            <span className="truncate font-display text-[15px] font-extrabold">{agent.name}</span>
            <Badge tone={status.tone} data-testid="agent-status">{status.label}</Badge>
            {account && account.venue !== 'PAPER' && <VenueBadge venue={account.venue} />}
            <Badge tone={agent.executionMode === 'AUTO' ? 'accent' : 'info'} data-testid="agent-mode">{agent.executionMode === 'AUTO' ? 'Auto' : 'Suggest'}</Badge>
            {agent.useNews && <Badge data-testid="agent-news">News</Badge>}
          </span>
          <span className="mt-0.5 block truncate text-xs text-ink-3">{provider?.name} · {agent.model}</span>
          <span className="block text-xs text-ink-3">{agent.symbol} · {agent.timeframe} · every {agent.intervalSec >= 60 ? `${agent.intervalSec / 60}m` : `${agent.intervalSec}s`}</span>
        </span>
        <span className="text-right">
          <span className="num block text-sm font-semibold">{money(equity)}</span>
          <span className={cn('num block text-xs', pnlClass(change))}>{pct(change)}</span>
        </span>
      </button>
      {agent.statusDetail && <p className="mt-2 rounded-lg bg-warn-soft px-2.5 py-1.5 text-xs text-warn">{agent.statusDetail}</p>}
      <p className="mt-2 text-[11px] text-ink-3">Last run {dateTime(agent.lastRunAt)}{agent.nextRunAt ? ` · next ${dateTime(agent.nextRunAt)}` : ''}</p>
      <div className="mt-3 flex flex-wrap gap-1.5">
        {agent.running ? (
          <>
            <Button size="sm" onClick={() => act('pause')}><Pause /> Pause</Button>
            <Button size="sm" onClick={() => act('stop')}><Square /> Stop</Button>
          </>
        ) : agent.status === 'PAUSED' ? (
          <>
            <Button size="sm" variant="primary" onClick={() => act('resume')}><Play /> Resume</Button>
            <Button size="sm" onClick={() => act('stop')}><Square /> Stop</Button>
          </>
        ) : (
          <Button size="sm" variant="primary" onClick={() => act('start')}><Play /> Start</Button>
        )}
        <Button size="sm" disabled={busy} onClick={() => cycle('analyze')} title="Ask the model and show the result without trading"><ScanSearch /> Analyze now</Button>
        <Button size="sm" disabled={busy} onClick={() => cycle('run')} title={agent.executionMode === 'AUTO' ? 'One full cycle, placing an order if approved' : 'One full cycle, suggesting a trade if approved'}><StepForward /> Run one cycle</Button>
        <span className="ml-auto flex gap-1">
          <Button size="icon" variant="ghost" aria-label={`Edit ${agent.name}`} onClick={onEdit}><Pencil /></Button>
          <Button size="icon" variant="ghost" aria-label={`Delete ${agent.name}`} disabled={agent.running} onClick={() => attempt(() => api.del(`/agents/${agent.id}`))}><Trash2 /></Button>
        </span>
      </div>
    </Card>
  );
}

export function AgentsPage() {
  const source = useApp((s) => s.source);
  const allAgents = useApp((s) => s.agents);
  const accounts = useSourceAccounts();
  const ids = new Set(accounts.map((a) => a.id));
  const agents = allAgents.filter((a) => ids.has(a.accountId));
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [editing, setEditing] = useState<Agent | 'new' | null>(null);
  const [decisionId, setDecisionId] = useState<string | null>(null);

  const selected = agents.find((a) => a.id === selectedId) ?? agents[0];
  const { data: decisions } = useQuery(
    () => (selected ? api.get<DecisionRecord[]>(`/decisions?agentId=${selected.id}&limit=60`) : Promise.resolve([])),
    [selected?.id],
    ['decisions'],
  );
  const decision = decisions?.find((d) => d.id === decisionId) ?? decisions?.[0];

  return (
    <>
      <PageTitle
        title="AI Agents"
        description={`Agents on ${source === 'REPLAY' ? 'the replay tape' : 'live OKX data'}. They run in the backend and stop when it stops.`}
        action={<Button variant="primary" onClick={() => setEditing('new')}><Plus /> New agent</Button>}
      />
      {agents.length === 0 ? (
        <Card className="px-6 py-10 text-center text-[13px] text-ink-3">No agents on this data source yet.</Card>
      ) : (
        <div className="grid gap-4 xl:grid-cols-[minmax(0,5fr)_minmax(0,4fr)_minmax(0,4fr)]">
          <div className="space-y-3">
            {agents.map((agent) => (
              <AgentCard key={agent.id} agent={agent} selected={agent.id === selected?.id} onSelect={() => { setSelectedId(agent.id); setDecisionId(null); }} onEdit={() => setEditing(agent)} />
            ))}
          </div>
          <Card className="flex max-h-[720px] flex-col self-start">
            <CardHeader title="Decisions" description={selected?.name} />
            <div className="min-h-0 flex-1 overflow-y-auto">
              <DecisionList decisions={decisions ?? []} selected={decision?.id} onSelect={(d) => setDecisionId(d.id)} emptyText="No decisions yet. Try Analyze now." />
            </div>
          </Card>
          <Card className="max-h-[720px] self-start overflow-y-auto">
            <CardHeader title="Decision detail" />
            {decision ? <DecisionDetail decision={decision} /> : <p className="px-5 pb-6 text-[13px] text-ink-3">Select a decision to inspect it.</p>}
          </Card>
        </div>
      )}
      {editing && <AgentDialog agent={editing === 'new' ? null : editing} onClose={() => setEditing(null)} />}
    </>
  );
}
