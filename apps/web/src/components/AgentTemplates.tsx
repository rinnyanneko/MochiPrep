import { ClipboardCopy, Copy, Download, FileDown, FileUp, Pencil, Plus, RotateCcw, Trash2 } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { DEFAULT_RISK_SETTINGS, TIMEFRAMES, type AgentTemplate, type AgentTemplateFields, type DataSource, type RiskSettings, type Timeframe } from '@mochiprep/shared';
import { Badge, Button, Dialog, Field, Input, Segmented, Select, Textarea } from '@/components/ui';
import { api } from '@/lib/api';
import { duplicateTemplate, exportTemplates, importNames, importReport, intervalOptions, parseTemplateFile, templateBody, type AgentSettings } from '@/lib/templates';
import { attempt, useApp, useQuery } from '@/store';

/**
 * Every template, refetched whenever the backend reports a change. `templates` is null until the
 * first load succeeds, so a failed load is never shown as an empty list.
 */
export function useAgentTemplates() {
  const { data, error, reload } = useQuery(() => api.get<AgentTemplate[]>('/agent-templates'), [], ['templates']);
  return { templates: data, error, reload };
}

/**
 * The agent settings shared by the agent form and the template editor. `source` is the data
 * source of the agent being edited; a template has none, since it can be used on either.
 */
function trailingNote(trails: boolean, allowed: boolean): string {
  if (trails) return 'On, because this agent trails instead of taking profit.';
  if (allowed) return 'On: this agent can set, change and remove trailing stops.';
  return 'Off: this agent cannot set, change or remove trailing stops.';
}

export function AgentSettingsFields<T extends AgentSettings>({ value, onChange, source }: Readonly<{ value: T; onChange(patch: Partial<AgentSettings>): void; source?: DataSource }>) {
  const setRisk = (patch: Partial<RiskSettings>) => onChange({ risk: { ...value.risk, ...patch } });
  const trails = value.risk.trailInsteadOfTakeProfit === true;
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
      <h3 className="mt-4 mb-2 text-sm font-semibold">Risk limits</h3>
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
        {number('Max leverage (x)', 'maxLeverage')}
        {number('Risk per trade (%)', 'maxRiskPerTradePct')}
        {number('Max daily loss (%)', 'maxDailyLossPct')}
        {number('Max open positions', 'maxOpenPositions')}
        {number('Max quote age (s)', 'maxQuoteAgeSec')}
        {number('Max notional (USDT)', 'maxNotional')}
        <Field label="Min confidence (%)" className="col-span-2 sm:col-span-3" hint="New positions only open when the model states at least this confidence. 0 is off. Closing and protecting a position are never held back. The model's confidence is its own, uncalibrated guess.">
          <Input
            type="number"
            step={5}
            min={0}
            max={100}
            value={Math.round((value.risk.minConfidence ?? 0) * 100)}
            onChange={(e) => setRisk({ minConfidence: Math.min(100, Math.max(0, Number(e.target.value) || 0)) / 100 })}
          />
        </Field>
      </div>
      <label className="mt-3 flex items-center gap-2 text-xs text-ink-2">
        <input type="checkbox" checked={value.risk.requireStopLoss} onChange={(e) => setRisk({ requireStopLoss: e.target.checked })} className="accent-[var(--accent)]" />{' '}
        Require a stop loss on new positions
      </label>
      <label className="mt-2 flex items-start gap-2 text-xs text-ink-2">
        <input
          type="checkbox"
          checked={value.allowTrailingStop || trails}
          disabled={trails}
          onChange={(e) => onChange({ allowTrailingStop: e.target.checked })}
          className="mt-0.5 accent-[var(--accent)]"
        />
        <span>
          Allow trailing stops{' '}
          <span className="block text-[11px] text-ink-3">{trailingNote(trails, value.allowTrailingStop)}</span>
        </span>
      </label>
      <label className="mt-2 flex items-start gap-2 text-xs text-ink-2">
        <input
          type="checkbox"
          checked={trails}
          // Turning it on also turns trailing stops on, so the saved agent reads the same without it.
          onChange={(e) => onChange({ allowTrailingStop: value.allowTrailingStop || e.target.checked, risk: { ...value.risk, trailInsteadOfTakeProfit: e.target.checked } })}
          className="mt-0.5 accent-[var(--accent)]"
          data-testid="trail-instead"
        />
        <span>
          Trail instead of take profit{' '}
          <span className="block text-[11px] text-ink-3">
            New positions carry no take profit. Each needs a trailing stop with an activation price, so it only starts following the price once the trade is in profit. A stop loss stays required. Positions that already have a take profit keep it.
          </span>
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
function TemplateEditor({ id, initial, onClose }: Readonly<{ id?: string; initial: AgentTemplateFields; onClose(): void }>) {
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

/** Shows an export document to download or copy: file downloads do not work in every desktop webview. */
function ExportDialog({ text, count, onClose }: Readonly<{ text: string; count: number; onClose(): void }>) {
  const download = () => {
    const url = URL.createObjectURL(new Blob([text], { type: 'application/json' }));
    const link = document.createElement('a');
    link.href = url;
    link.download = 'mochiprep-agent-templates.json';
    link.click();
    // WebKit webviews can cancel a download whose URL is revoked in the same task as the click.
    setTimeout(() => URL.revokeObjectURL(url), 0);
  };
  const copy = () =>
    attempt(async () => {
      await navigator.clipboard.writeText(text);
      useApp.getState().toast('info', 'Copied the template export');
    });
  const plural = count === 1 ? '' : 's';
  return (
    <Dialog open onOpenChange={onClose} title="Export templates" description={`${count} template${plural}. Import this on another MochiPrep. It holds no keys or account details.`} wide>
      <Textarea readOnly aria-label="Template export" value={text} className="h-64 font-mono text-[11px]" />
      <div className="mt-4 flex flex-wrap justify-end gap-2">
        <Button onClick={onClose}>Back</Button>
        <Button onClick={copy}><ClipboardCopy /> Copy</Button>
        <Button variant="primary" onClick={download}><Download /> Download .json</Button>
      </div>
    </Dialog>
  );
}

/**
 * Reads an export from a file or pasted text and creates each template through the normal
 * template route, so the backend validates every field. An imported template is still only a
 * form pre-fill, but its strategy text is sent to the model, so the dialog asks for a review.
 */
function ImportDialog({ existing, onClose }: Readonly<{ existing: AgentTemplate[]; onClose(): void }>) {
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  // A slow read of an earlier pick must not replace the text of a later one.
  const selection = useRef(0);
  const [loading, setLoading] = useState(false);
  const pick = async (file: File | undefined) => {
    const mine = ++selection.current;
    if (!file) {
      setLoading(false);
      return;
    }
    setLoading(true);
    const content = await attempt(() => file.text());
    if (mine !== selection.current || !mounted.current) return;
    setLoading(false);
    if (content !== undefined) setText(content);
  };
  // Typing supersedes any file still being read.
  const edit = (value: string) => {
    selection.current++;
    setLoading(false);
    setText(value);
  };
  const run = async () => {
    if (busy || loading) return;
    const parsed = await attempt(() => importNames(parseTemplateFile(text), existing));
    if (!parsed) return;
    setBusy(true);
    let imported = 0;
    const failures: string[] = [];
    const failed: AgentTemplateFields[] = [];
    let cancelled = 0;
    // One at a time, so a refused template leaves the others in and the report can name it.
    for (const [index, fields] of parsed.entries()) {
      // Closing the dialog cancels: the request in flight finishes, but no further one starts.
      if (!mounted.current) {
        cancelled = parsed.length - index;
        break;
      }
      try {
        await api.post('/agent-templates', fields);
        imported++;
      } catch (error) {
        failures.push(`${fields.name || 'Unnamed'}: ${(error as Error).message}`);
        failed.push(fields);
      }
    }
    const toast = useApp.getState().toast;
    if (imported) toast('info', `Imported ${imported} template${imported === 1 ? '' : 's'}`);
    if (failures.length) toast('error', importReport(failures));
    if (cancelled) toast('warn', `Import cancelled: ${cancelled} template${cancelled === 1 ? '' : 's'} not imported`);
    if (!mounted.current) return;
    setBusy(false);
    if (!failed.length) onClose();
    // Only the refused templates stay, so a corrected retry does not import the others twice.
    else setText(exportTemplates(failed));
  };
  return (
    <Dialog open onOpenChange={onClose} title="Import templates" description="Pick an exported .json file or paste its text. Names that already exist get a copy suffix." wide>
      <label className="mb-2 flex items-center gap-2 text-xs text-ink-2">
        <span className="font-semibold">File</span>
        <input type="file" accept="application/json,.json" aria-label="Template file" disabled={busy} onChange={(e) => void pick(e.target.files?.[0])} className="text-xs" />
      </label>
      <Textarea aria-label="Template export text" value={text} readOnly={busy} onChange={(e) => edit(e.target.value)} placeholder="Paste an exported template file here" className="h-48 font-mono text-[11px]" />
      <p className="mt-2 rounded-md bg-warn-soft px-3 py-2 text-xs text-warn">
        Only import files you trust, and review each template before using it. Strategy text is sent to the AI model, and an imported template may use Trade automatically. Every order is still limited by the risk checks.
      </p>
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="primary" disabled={busy || loading || !text.trim()} onClick={run}><FileUp /> Import</Button>
      </div>
    </Dialog>
  );
}

type Editing = { id?: string; initial: AgentTemplateFields } | null;
type Panel = { kind: 'export'; text: string; count: number } | { kind: 'import' } | null;

/** Lists every template with create, edit, duplicate, delete, export, import and restore-built-ins. */
export function TemplatesDialog({ onClose }: Readonly<{ onClose(): void }>) {
  const { templates: loaded, error, reload } = useAgentTemplates();
  const templates = loaded ?? [];
  const [editing, setEditing] = useState<Editing>(null);
  const [panel, setPanel] = useState<Panel>(null);
  const exportList = (list: AgentTemplate[]) =>
    void attempt(() => setPanel({ kind: 'export', text: exportTemplates(list), count: list.length }));
  const mine = templates.filter((t) => !t.builtIn);

  if (editing) return <TemplateEditor id={editing.id} initial={editing.initial} onClose={() => setEditing(null)} />;
  if (panel?.kind === 'export') return <ExportDialog text={panel.text} count={panel.count} onClose={() => setPanel(null)} />;
  if (panel?.kind === 'import') return <ImportDialog existing={templates} onClose={() => setPanel(null)} />;

  return (
    <Dialog open onOpenChange={onClose} title="Agent templates" description="Pick one when creating an agent to pre-fill its settings. Built-ins are read-only: duplicate one to change it." wide>
      <ul className="max-h-[60vh] space-y-2 overflow-y-auto">
        {templates.map((t) => (
          <li key={t.id} className="flex items-start gap-3 rounded-md border border-line px-3 py-2" data-testid="template-row">
            <span className="min-w-0 flex-1">
              <span className="flex items-center gap-2">
                <span className="truncate text-sm font-semibold">{t.name}</span>
                {t.builtIn && <Badge>Built-in</Badge>}
                <Badge tone={t.executionMode === 'AUTO' ? 'accent' : 'info'}>{t.executionMode === 'AUTO' ? 'Auto' : 'Suggest'}</Badge>
              </span>
              {t.description && <span className="block text-xs text-ink-3">{t.description}</span>}
              <span className="block text-[11px] text-ink-3">{t.timeframe} · max {t.risk.maxLeverage}x · {t.risk.maxRiskPerTradePct}% per trade · {t.risk.maxDailyLossPct}% daily loss</span>
            </span>
            <span className="flex gap-1">
              {!t.builtIn && <Button size="icon" variant="ghost" aria-label={`Edit ${t.name}`} onClick={() => setEditing({ id: t.id, initial: templateBody(t) })}><Pencil /></Button>}
              <Button size="icon" variant="ghost" aria-label={`Duplicate ${t.name}`} onClick={() => setEditing({ initial: duplicateTemplate(t, templates) })}><Copy /></Button>
              <Button size="icon" variant="ghost" aria-label={`Export ${t.name}`} onClick={() => exportList([t])}><FileDown /></Button>
              <Button size="icon" variant="ghost" aria-label={`Delete ${t.name}`} onClick={() => attempt(() => api.del(`/agent-templates/${t.id}`))}><Trash2 /></Button>
            </span>
          </li>
        ))}
        {error && (
          <li className="flex flex-col items-center gap-2 py-6 text-center text-[13px] text-down" role="alert">
            Templates could not be loaded: {error}
            <Button size="sm" onClick={reload}>Retry</Button>
          </li>
        )}
        {!loaded && !error && <li className="py-6 text-center text-[13px] text-ink-3">Loading templates...</li>}
        {loaded?.length === 0 && <li className="py-6 text-center text-[13px] text-ink-3">No templates. Create one or restore the built-ins.</li>}
      </ul>
      <div className="mt-4 flex flex-wrap justify-end gap-2">
        <Button onClick={() => attempt(() => api.post('/agent-templates/restore-builtins'))} title="Show the built-in templates you deleted again"><RotateCcw /> Restore built-ins</Button>
        {/* Needs the loaded list to rename imports that clash with existing names. */}
        <Button disabled={!loaded} onClick={() => setPanel({ kind: 'import' })}><FileUp /> Import</Button>
        <Button disabled={mine.length === 0} onClick={() => exportList(mine)} title="Export every template you created"><FileDown /> Export mine</Button>
        <Button variant="primary" onClick={() => setEditing({ initial: BLANK })}><Plus /> New template</Button>
      </div>
    </Dialog>
  );
}
