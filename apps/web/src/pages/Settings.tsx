import { KeyRound, Pencil, Plug, Plus, RotateCcw, ShieldCheck, Trash2, X } from 'lucide-react';
import { useState } from 'react';
import type { ProviderKind, ProviderView } from '@mochiprep/shared';
import { PageTitle } from '@/components/Shell';
import { Badge, Button, Card, CardHeader, Dialog, Field, Input, Select } from '@/components/ui';
import { api } from '@/lib/api';
import { attempt, useApp } from '@/store';

const KINDS: { value: Exclude<ProviderKind, 'MOCK'>; label: string; baseUrl: string; model: string }[] = [
  { value: 'OPENAI_COMPATIBLE', label: 'OpenAI-compatible (custom URL)', baseUrl: 'http://localhost:11434/v1', model: '' },
  { value: 'OPENAI', label: 'OpenAI', baseUrl: '', model: '' },
  { value: 'ANTHROPIC', label: 'Anthropic Claude', baseUrl: '', model: 'claude-sonnet-5-5' },
  { value: 'GEMINI', label: 'Google Gemini', baseUrl: '', model: '' },
];

function ProviderDialog({ provider, onClose }: { provider: ProviderView | null; onClose(): void }) {
  const [kind, setKind] = useState<(typeof KINDS)[number]['value']>(provider && provider.kind !== 'MOCK' ? provider.kind : 'OPENAI_COMPATIBLE');
  const [name, setName] = useState(provider?.name ?? '');
  const [baseUrl, setBaseUrl] = useState(provider?.baseUrl ?? KINDS[0]!.baseUrl);
  const [model, setModel] = useState(provider?.defaultModel ?? '');
  const [inputPrice, setInputPrice] = useState(provider?.inputPricePerMTok?.toString() ?? '');
  const [outputPrice, setOutputPrice] = useState(provider?.outputPricePerMTok?.toString() ?? '');

  const pickKind = (value: typeof kind) => {
    const preset = KINDS.find((k) => k.value === value)!;
    setKind(value);
    setBaseUrl(preset.baseUrl);
    if (preset.model && !model) setModel(preset.model);
  };

  const save = async () => {
    const body = {
      name,
      kind,
      baseUrl: baseUrl.trim() || null,
      defaultModel: model,
      inputPricePerMTok: inputPrice === '' ? null : Number(inputPrice),
      outputPricePerMTok: outputPrice === '' ? null : Number(outputPrice),
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
      toast('info', 'Key loaded into backend memory.');
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
          provider.hasKey ? <Badge tone="up"><KeyRound /> Key in memory</Badge> : <Badge tone="warn"><KeyRound /> Key needed</Badge>
        ) : (
          <Badge tone="info">No key needed</Badge>
        )}
        <span className="num text-xs text-ink-3">{provider.defaultModel}</span>
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
          <Button disabled={!key.trim()} onClick={saveKey}>Load key</Button>
          {provider.hasKey && (
            <Button variant="ghost" onClick={() => attempt(() => api.del(`/providers/${provider.id}/key`))}><X /> Forget</Button>
          )}
        </div>
      )}
    </div>
  );
}

export function SettingsPage() {
  const providers = useApp((s) => s.providers);
  const demoActive = useApp((s) => s.demoActive);
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
            <ShieldCheck className="size-4 shrink-0 text-up" />
            Keys are held in backend memory only, never written to SQLite or the browser. Re-enter them after every restart.
          </p>
        </Card>

        <Card>
          <CardHeader title="Demo Mode" description={demoActive ? 'Active: synthetic tape, mock agents, 10,000 virtual USDT per account.' : 'Not active. Start it from the Dashboard.'} />
          {demoActive && (
            <div className="flex flex-wrap gap-2 px-5 pb-4">
              <Button onClick={() => setConfirm('reset')}><RotateCcw /> Reset demo</Button>
              <Button variant="danger" onClick={() => setConfirm('exit')}>Exit Demo Mode</Button>
            </div>
          )}
        </Card>

        <Card>
          <CardHeader title="Live trading" description="Not implemented. MochiPrep has no way to place a real order, and it never asks for exchange keys. Live prices come from the OKX public feed." />
        </Card>
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
