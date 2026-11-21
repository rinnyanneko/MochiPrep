import { Copy, Pencil, Plus, RotateCcw, Trash2 } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { DEFAULT_RISK_SETTINGS, TIMEFRAMES, type AgentTemplate, type AgentTemplateFields, type DataSource, type RiskSettings, type Timeframe } from '@mochiprep/shared';
import { Badge, Button, Dialog, Field, Input, Segmented, Select, Textarea } from '@/components/ui';
import { api } from '@/lib/api';
import { duplicateTemplate, intervalOptions, templateBody, type AgentSettings } from '@/lib/templates';
import { attempt, useQuery } from '@/store';

/** Every template, refetched whenever the backend reports a change. */
export function useAgentTemplates() {
  const { data } = useQuery(() => api.get<AgentTemplate[]>('/agent-templates'), [], ['templates']);
  return data ?? [];
}

/**
 * The agent settings shared by the agent form and the template editor. `source` is the data
 * source of the agent being edited; a template has none, since it can be used on either.
 */
export function AgentSettingsFields<T extends AgentSettings>({ value, onChange, source }: { value: T; onChange(patch: Partial<AgentSettings>): void; source?: DataSource }) {
  const setRisk = (patch: Partial<RiskSettings>) => onChange({ risk: { ...value.risk, ...patch } });
  const number = (label: string, key: keyof RiskSettings) => (
    <Field label={label}>
      <Input type="number" step="any" min={0} value={value.risk[key] as number} onChange={(e) => setRisk({ [key]: Number(e.target.value) })} />
    </Field>
  );
  const replay = source === 'REPLAY';
  let newsHint = 'Headlines are unverified third-party text. They can be wrong or written to mislead an AI. Sources are in Settings.';
  if (replay) newsHint = 'Live OKX only: real headlines would not match the synthetic tape.';
  else if (!source) newsHint = `Used only by agents on Live OKX. ${newsHint}`;

  return (
    <>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Timeframe">
          <Select value={value.timeframe} onChange={(e) => onChange({ timeframe: e.target.value as Timeframe })}>{TIMEFRAMES.map((t) => <option key={t}>{t}</option>)}</Select>
        </Field>
        <Field label="Schedule" hint={replay ? 'Replay agents run on tape time, not wall time.' : undefined}>
          <Select value={value.intervalSec} onChange={(e) => onChange({ intervalSec: Number(e.target.value) })}>
            {intervalOptions(value.intervalSec).map((i) => <option key={i.value} value={i.value}>{i.label}</option>)}
          </Select>
        </Field>
        <div className="sm:col-span-2">
          <span className="mb-1 block text-xs font-semibold text-ink-2">When the AI finds a trade</span>
          <Segmented
            label="Execution mode"
            value={value.executionMode}
            onChange={(executionMode) => onChange({ executionMode })}
            options={[
              { value: 'SIGNAL', label: 'Suggest to me' },
              { value: 'AUTO', label: 'Trade automatically' },
            ]}
          />
          <span className="mt-1 block text-[11px] text-ink-3">
            {value.executionMode === 'SIGNAL' ? 'You see the entry, stop and target, and decide. Nothing is ordered until you accept.' : 'Approved orders are placed immediately, with no confirmation.'}
          </span>
        </div>
        <label className="flex items-start gap-2 text-xs text-ink-2 sm:col-span-2">
          <input type="checkbox" className="mt-0.5 accent-[var(--accent)]" disabled={replay} checked={value.useNews && !replay} onChange={(e) => onChange({ useNews: e.target.checked })} />
          <span>
            Read recent news headlines{' '}
            <span className="block text-[11px] text-ink-3">{newsHint}</span>
          </span>
        </label>
        <Field label="Strategy instructions" className="sm:col-span-2" hint="Sent to the model as style guidance. It cannot override the risk engine.">
          <Textarea value={value.strategy} onChange={(e) => onChange({ strategy: e.target.value })} placeholder="Trade pullbacks in the direction of the 1h trend..." />
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
        <input type="checkbox" checked={value.risk.requireStopLoss} onChange={(e) => setRisk({ requireStopLoss: e.target.checked })} className="accent-[var(--accent)]" />{' '}
        Require a stop loss on new positions
      </label>
      <label className="mt-2 flex items-start gap-2 text-xs text-ink-2">
        <input type="checkbox" checked={value.allowTrailingStop} onChange={(e) => onChange({ allowTrailingStop: e.target.checked })} className="mt-0.5 accent-[var(--accent)]" />
        <span>
          Allow trailing stops{' '}
          <span className="block text-[11px] text-ink-3">Off: this agent cannot set, change or remove trailing stops.</span>
        </span>
      </label>
    </>
  );
}

const BLANK: AgentTemplateFields = {
  name: '',
  description: '',
  strategy: '',
  timeframe: '15m',
  intervalSec: 900,
  executionMode: 'SIGNAL',
  risk: DEFAULT_RISK_SETTINGS,
  useNews: false,
  allowTrailingStop: false,
};

/** Creates a template (no `id`) or saves changes to a user template. */
function TemplateEditor({ id, initial, onClose }: { id?: string; initial: AgentTemplateFields; onClose(): void }) {
  const [form, setForm] = useState<AgentTemplateFields>(initial);
  const set = (patch: Partial<AgentTemplateFields>) => setForm((f) => ({ ...f, ...patch }));
  const [saving, setSaving] = useState(false);
  // Cancelling unmounts this editor; a save still in flight must not then close the next one.
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const save = async () => {
    if (saving) return;
    setSaving(true);
    const body = templateBody(form);
    const done = await attempt(() => (id ? api.put(`/agent-templates/${id}`, body) : api.post('/agent-templates', body)));
    if (!mounted.current) return;
    setSaving(false);
    if (done) onClose();
  };
  return (
    <Dialog open onOpenChange={onClose} title={id ? `Edit ${initial.name}` : 'New template'} description="A template only pre-fills the new-agent form. Limits are checked again when the agent is created and on every order." wide>
      <div className="mb-3 grid gap-3 sm:grid-cols-2">
        <Field label="Template name"><Input value={form.name} maxLength={60} onChange={(e) => set({ name: e.target.value })} placeholder="My breakout setup" /></Field>
        <Field label="Description"><Input value={form.description} maxLength={300} onChange={(e) => set({ description: e.target.value })} placeholder="Optional" /></Field>
      </div>
      <AgentSettingsFields value={form} onChange={set} />
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="primary" disabled={saving || !form.name.trim()} onClick={save}>{id ? 'Save template' : 'Create template'}</Button>
      </div>
    </Dialog>
  );
}

type Editing = { id?: string; initial: AgentTemplateFields } | null;

/** Lists every template with create, edit, duplicate, delete and restore-built-ins. */
export function TemplatesDialog({ onClose }: { onClose(): void }) {
  const templates = useAgentTemplates();
  const [editing, setEditing] = useState<Editing>(null);

  if (editing) return <TemplateEditor id={editing.id} initial={editing.initial} onClose={() => setEditing(null)} />;

  return (
    <Dialog open onOpenChange={onClose} title="Agent templates" description="Pick one when creating an agent to pre-fill its settings. Built-ins are read-only: duplicate one to change it." wide>
      <ul className="max-h-[60vh] space-y-2 overflow-y-auto">
        {templates.map((t) => (
          <li key={t.id} className="flex items-start gap-3 rounded-xl border border-line px-3 py-2" data-testid="template-row">
            <span className="min-w-0 flex-1">
              <span className="flex items-center gap-2">
                <span className="truncate text-sm font-bold">{t.name}</span>
                {t.builtIn && <Badge>Built-in</Badge>}
                <Badge tone={t.executionMode === 'AUTO' ? 'accent' : 'info'}>{t.executionMode === 'AUTO' ? 'Auto' : 'Suggest'}</Badge>
              </span>
              {t.description && <span className="block text-xs text-ink-3">{t.description}</span>}
              <span className="block text-[11px] text-ink-3">{t.timeframe} · max {t.risk.maxLeverage}x · {t.risk.maxRiskPerTradePct}% per trade · {t.risk.maxDailyLossPct}% daily loss</span>
            </span>
            <span className="flex gap-1">
              {!t.builtIn && <Button size="icon" variant="ghost" aria-label={`Edit ${t.name}`} onClick={() => setEditing({ id: t.id, initial: templateBody(t) })}><Pencil /></Button>}
              <Button size="icon" variant="ghost" aria-label={`Duplicate ${t.name}`} onClick={() => setEditing({ initial: duplicateTemplate(t, templates) })}><Copy /></Button>
              <Button size="icon" variant="ghost" aria-label={`Delete ${t.name}`} onClick={() => attempt(() => api.del(`/agent-templates/${t.id}`))}><Trash2 /></Button>
            </span>
          </li>
        ))}
        {templates.length === 0 && <li className="py-6 text-center text-[13px] text-ink-3">No templates. Create one or restore the built-ins.</li>}
      </ul>
      <div className="mt-4 flex flex-wrap justify-end gap-2">
        <Button onClick={() => attempt(() => api.post('/agent-templates/restore-builtins'))} title="Show the built-in templates you deleted again"><RotateCcw /> Restore built-ins</Button>
        <Button variant="primary" onClick={() => setEditing({ initial: BLANK })}><Plus /> New template</Button>
      </div>
    </Dialog>
  );
}
