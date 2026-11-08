import { KeyRound, Pencil, Plug, Plus, RotateCcw, ShieldCheck, Trash2, X } from 'lucide-react';
import { ExchangeCard } from '@/components/ExchangeCard';
import { NewsSourcesCard } from '@/components/NewsPanel';
import { PairsCard } from '@/components/PairsCard';
import { useMemo, useState } from 'react';
import type { ProviderKind, ProviderView } from '@mochiprep/shared';
import { PageTitle } from '@/components/Shell';
import { VaultCard } from '@/components/VaultCard';
import { Badge, Button, Card, CardHeader, Dialog, Field, Input, Select, Textarea } from '@/components/ui';
import { api } from '@/lib/api';
import { formatDateTime, listTimeZones, resolveTimeZone } from '@/lib/display';
import { pct } from '@/lib/format';
import { attempt, useApp } from '@/store';

const KINDS: { value: Exclude<ProviderKind, 'MOCK'>; label: string; baseUrl: string; model: string }[] = [
  { value: 'OPENAI_COMPATIBLE', label: 'OpenAI-compatible (custom URL)', baseUrl: 'http://localhost:11434/v1', model: '' },
  { value: 'OPENAI', label: 'OpenAI', baseUrl: '', model: '' },
  { value: 'ANTHROPIC', label: 'Anthropic Claude', baseUrl: '', model: 'claude-sonnet-5-5' },
  { value: 'GEMINI', label: 'Google Gemini', baseUrl: '', model: '' },
  { value: 'DEEPSEEK', label: 'DeepSeek', baseUrl: '', model: 'deepseek-flash' },
  { value: 'OPENCODE', label: 'OpenCode', baseUrl: '', model: 'deepseek-v4.1-flash' },
];

function parseHeaders(text: string): Record<string, string> {
  const entries = text.split('\n').flatMap((line) => {
    const colon = line.indexOf(':');
    return line.trim() && colon > 0 ? [[line.slice(0, colon).trim(), line.slice(colon + 1).trim()]] : [];
  });
  return Object.fromEntries(entries);
}

function ProviderDialog({ provider, onClose }: { provider: ProviderView | null; onClose(): void }) {
  const [kind, setKind] = useState<(typeof KINDS)[number]['value']>(provider && provider.kind !== 'MOCK' ? provider.kind : 'OPENAI_COMPATIBLE');
  const [name, setName] = useState(provider?.name ?? '');
  const [baseUrl, setBaseUrl] = useState(provider?.baseUrl ?? KINDS[0]!.baseUrl);
  const [model, setModel] = useState(provider?.defaultModel ?? '');
  const [inputPrice, setInputPrice] = useState(provider?.inputPricePerMTok?.toString() ?? '');
  const [outputPrice, setOutputPrice] = useState(provider?.outputPricePerMTok?.toString() ?? '');
  const [headers, setHeaders] = useState('');
  const savedHeaders = provider?.headerNames ?? [];

  const pickKind = (value: typeof kind) => {
    const preset = KINDS.find((k) => k.value === value)!;
    setKind(value);
    setBaseUrl(preset.baseUrl);
    if (!provider) setModel(preset.model);
  };

  const save = async () => {
    const body = {
      name,
      kind,
      baseUrl: baseUrl.trim() || null,
      defaultModel: model,
      inputPricePerMTok: inputPrice === '' ? null : Number(inputPrice),
      outputPricePerMTok: outputPrice === '' ? null : Number(outputPrice),
      // Left empty on edit, the saved headers stay: their values are not sent back to fill in.
      ...(headers.trim() || !provider ? { extraHeaders: parseHeaders(headers) } : {}),
    };
    const ok = await attempt(() => (provider ? api.put(`/providers/${provider.id}`, body) : api.post('/providers', body)));
    if (ok) onClose();
  };

  return (
    <Dialog open onOpenChange={onClose} title={provider ? `Edit ${provider.name}` : 'Add AI provider'}>
      <div className="space-y-3">
        <Field label="Type">
          <Select value={kind} onChange={(e) => pickKind(e.target.value as typeof kind)}>
            {KINDS.map((k) => <option key={k.value} value={k.value}>{k.label}</option>)}
          </Select>
        </Field>
        <Field label="Display name"><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="My provider" /></Field>
        <Field label="Base URL" hint={kind === 'OPENAI_COMPATIBLE' ? 'Up to and including /v1.' : 'Leave empty for the official endpoint.'}>
          <Input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} />
        </Field>
        <Field label="Default model"><Input value={model} onChange={(e) => setModel(e.target.value)} placeholder="Exact model identifier" /></Field>
        <div className="grid grid-cols-2 gap-3">
          <Field label="Input $ / 1M tokens"><Input type="number" min={0} step="any" value={inputPrice} onChange={(e) => setInputPrice(e.target.value)} /></Field>
          <Field label="Output $ / 1M tokens"><Input type="number" min={0} step="any" value={outputPrice} onChange={(e) => setOutputPrice(e.target.value)} /></Field>
        </div>
        <Field
          label="Extra headers"
          hint={savedHeaders.length ? `Saved: ${savedHeaders.join(', ')}. Leave empty to keep them, or enter new ones to replace them.` : 'Optional, for gateways that need them. One "Name: value" per line. Saved unencrypted, even with a master password.'}
        >
          <Textarea className="num h-20 text-xs" autoComplete="off" spellCheck={false} value={headers} onChange={(e) => setHeaders(e.target.value)} placeholder={'HTTP-Referer: https://example.com\nX-Title: MochiPrep'} />
        </Field>
        <p className="text-[11px] text-ink-3">Prices are optional. They are only used to estimate cost, so enter your provider's current rates.</p>
      </div>
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="primary" disabled={!name.trim() || !model.trim()} onClick={save}>Save</Button>
      </div>
    </Dialog>
  );
}

function ProviderRow({ provider, onEdit }: { provider: ProviderView; onEdit(): void }) {
  const [key, setKey] = useState('');
  const [testing, setTesting] = useState(false);
  const toast = useApp((s) => s.toast);

  const saveKey = async () => {
    const ok = await attempt(() => api.put(`/providers/${provider.id}/key`, { apiKey: key }));
    if (ok) {
      setKey('');
      toast('info', 'Key saved.');
    }
  };
  const test = async () => {
    setTesting(true);
    const result = await attempt(() => api.post<{ sample: string }>(`/providers/${provider.id}/test`, {}));
    setTesting(false);
    if (result) toast('info', `Connected. Reply: ${result.sample}`);
  };

  return (
    <div className="border-t border-line px-5 py-3" data-testid="provider-row">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-semibold">{provider.name}</span>
        <Badge>{provider.kind.replaceAll('_', ' ')}</Badge>
        {provider.requiresKey ? (
          provider.hasKey ? <Badge tone="up"><KeyRound /> Key saved</Badge> : <Badge tone="warn"><KeyRound /> Key needed</Badge>
        ) : (
          <Badge tone="info">No key needed</Badge>
        )}
        <span className="num text-xs text-ink-3">{provider.defaultModel}</span>
        {provider.headerNames.length > 0 && <Badge title={provider.headerNames.join(', ')}>{provider.headerNames.length} extra header{provider.headerNames.length === 1 ? '' : 's'}</Badge>}
        {provider.requiresKey && (
          <span className="ml-auto flex gap-1">
            <Button size="sm" disabled={!provider.hasKey || testing} onClick={test}><Plug /> Test</Button>
            <Button size="icon" variant="ghost" aria-label={`Edit ${provider.name}`} onClick={onEdit}><Pencil /></Button>
            <Button size="icon" variant="ghost" aria-label={`Delete ${provider.name}`} onClick={() => attempt(() => api.del(`/providers/${provider.id}`))}><Trash2 /></Button>
          </span>
        )}
      </div>
      {provider.requiresKey && (
        <div className="mt-2 flex max-w-lg gap-2">
          <Input type="password" autoComplete="off" aria-label={`API key for ${provider.name}`} value={key} onChange={(e) => setKey(e.target.value)} placeholder={provider.hasKey ? 'Replace the key' : 'Paste your API key'} />
          <Button disabled={!key.trim()} onClick={saveKey}>Save key</Button>
          {provider.hasKey && (
            <Button variant="ghost" onClick={() => attempt(() => api.del(`/providers/${provider.id}/key`))}><X /> Delete key</Button>
          )}
        </div>
      )}
    </div>
  );
}

function DisplayCard() {
  const timeZone = useApp((s) => s.timeZone);
  const flipColors = useApp((s) => s.flipColors);
  const setTimeZone = useApp((s) => s.setTimeZone);
  const setFlipColors = useApp((s) => s.setFlipColors);
  const zones = useMemo(() => listTimeZones().filter((zone) => zone !== 'UTC'), []);
  const now = Date.now();

  return (
    <Card data-testid="display-card">
      <CardHeader title="Display" description="Saved in this browser only. Stored data and the API keep UTC timestamps; this changes how they are shown." />
      <div className="grid gap-4 px-5 pb-4 sm:grid-cols-2">
        <Field label="Time zone" hint={`Times show as ISO 8601 with the offset, for example ${formatDateTime(now, timeZone)}.`}>
          <Select value={timeZone} onChange={(e) => setTimeZone(e.target.value)} data-testid="time-zone">
            <option value="local">Browser local ({resolveTimeZone('local')})</option>
            <option value="UTC">UTC</option>
            <optgroup label="All time zones">
              {zones.map((zone) => <option key={zone} value={zone}>{zone}</option>)}
            </optgroup>
          </Select>
        </Field>
        <div>
          <span className="mb-1 block text-xs font-semibold text-ink-2">Up and down colors</span>
          <label className="flex h-9 items-center gap-2 text-[13px]">
            <input type="checkbox" className="accent-[var(--accent)]" checked={flipColors} onChange={(e) => setFlipColors(e.target.checked)} data-testid="flip-colors" />
            Red for up, green for down
          </label>
          <span className="mt-1 block text-[11px] text-ink-3">
            Applies to prices, PnL, long and short, and charts:{' '}
            <span className="num font-semibold text-rise">{pct(1.25)}</span> <span className="num font-semibold text-fall">{pct(-1.25)}</span>. Warnings and errors stay red.
          </span>
        </div>
      </div>
    </Card>
  );
}

export function SettingsPage() {
  const providers = useApp((s) => s.providers);
  const demoActive = useApp((s) => s.demoActive);
  const vault = useApp((s) => s.vault);
  const [editing, setEditing] = useState<ProviderView | 'new' | null>(null);
  const [confirm, setConfirm] = useState<'reset' | 'exit' | null>(null);

  const demoAction = async () => {
    await attempt(() => api.post(confirm === 'reset' ? '/demo/reset' : '/demo/exit'));
    await useApp.getState().refreshState();
    setConfirm(null);
  };

  return (
    <>
      <PageTitle title="Settings" />
      <div className="space-y-4">
        <Card>
          <CardHeader
            title="AI providers"
            description="Bring your own key. Calls leave from the local backend, straight to the provider you configure."
            action={<Button variant="primary" onClick={() => setEditing('new')}><Plus /> Add provider</Button>}
          />
          {providers.map((p) => <ProviderRow key={p.id} provider={p} onEdit={() => setEditing(p)} />)}
          <p className="flex items-center gap-2 border-t border-line px-5 py-3 text-xs text-ink-3">
            <ShieldCheck className={`size-4 shrink-0 ${vault === 'OFF' ? 'text-warn' : 'text-up'}`} />
            {vault === 'OFF'
              ? 'AI keys are saved unencrypted in the local database (.mochiprep, readable only by your user) and survive restarts. Anyone who can read that file can read them. Set a master password below to encrypt them.'
              : 'AI keys are saved encrypted with your master password and load when you unlock.'}{' '}
            They are never sent to the browser.
          </p>
        </Card>

        <DisplayCard />

        <VaultCard />

        <Card>
          <CardHeader title="Demo Mode" description={demoActive ? 'Active: synthetic tape, mock agents, 10,000 virtual USDT per account.' : 'Not active. Start it from the Dashboard.'} />
          {demoActive && (
            <div className="flex flex-wrap gap-2 px-5 pb-4">
              <Button onClick={() => setConfirm('reset')}><RotateCcw /> Reset demo</Button>
              <Button variant="danger" onClick={() => setConfirm('exit')}>Exit Demo Mode</Button>
            </div>
          )}
        </Card>

        <PairsCard />

        <NewsSourcesCard />

        <ExchangeCard />
      </div>
      {editing && <ProviderDialog provider={editing === 'new' ? null : editing} onClose={() => setEditing(null)} />}
      <Dialog
        open={confirm !== null}
        onOpenChange={() => setConfirm(null)}
        title={confirm === 'reset' ? 'Reset the demo?' : 'Exit Demo Mode?'}
        description={`Deletes all demo accounts, agents and trades${confirm === 'reset' ? ', then rebuilds them from the first bar' : ''}. Your own accounts and agents are untouched.`}
      >
        <div className="flex justify-end gap-2">
          <Button onClick={() => setConfirm(null)}>Cancel</Button>
          <Button variant="danger" onClick={demoAction} data-testid="confirm-demo-action">{confirm === 'reset' ? 'Reset demo' : 'Exit demo'}</Button>
        </div>
      </Dialog>
    </>
  );
}
