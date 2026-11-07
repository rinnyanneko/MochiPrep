import { Link2, ShieldAlert, ShieldOff, TriangleAlert, Unplug } from 'lucide-react';
import { useState } from 'react';
import { ARM_PHRASE, DEFAULT_LIVE_LIMITS, type ExchangeStatus, type RiskSettings } from '@mochiprep/shared';
import { api } from '@/lib/api';
import { dateTime } from '@/lib/format';
import { DYOR_NOTICE } from '@/lib/notices';
import { attempt, useApp } from '@/store';
import { VenueBadge } from './trading';
import { Badge, Button, Card, CardHeader, Dialog, Field, Input, Select } from './ui';

type Environment = 'OKX_DEMO' | 'OKX_LIVE';

function ConnectDialog({ preset, onClose }: { preset: ExchangeStatus | null; onClose(): void }) {
  const [environment, setEnvironment] = useState<Environment>(preset?.venue === 'OKX_LIVE' ? 'OKX_LIVE' : 'OKX_DEMO');
  const [name, setName] = useState(preset?.name ?? '');
  const [apiKey, setApiKey] = useState('');
  const [secretKey, setSecretKey] = useState('');
  const [passphrase, setPassphrase] = useState('');
  const [limits, setLimits] = useState<RiskSettings>(preset?.limits ?? DEFAULT_LIVE_LIMITS);
  const [busy, setBusy] = useState(false);
  const toast = useApp((s) => s.toast);
  const unlocked = useApp((s) => s.vault === 'UNLOCKED');
  const [remember, setRemember] = useState(preset?.saved ?? false);

  const connect = async () => {
    setBusy(true);
    const ok = await attempt(() => api.post('/exchange/okx/connect', { name, environment, apiKey, secretKey, passphrase, limits, remember: remember && unlocked }));
    setBusy(false);
    if (ok) {
      toast('info', 'Connected to OKX.');
      onClose();
    }
  };

  const limit = (label: string, key: 'maxNotional' | 'maxLeverage' | 'maxDailyLossPct' | 'maxOpenPositions') => (
    <Field label={label}>
      <Input type="number" min={0} step="any" value={limits[key]} onChange={(e) => setLimits({ ...limits, [key]: Number(e.target.value) })} />
    </Field>
  );

  return (
    <Dialog open onOpenChange={onClose} title={preset ? `Reconnect ${preset.name}` : 'Connect OKX'} description="Credentials stay in backend memory unless you save them encrypted below. Unsaved, you enter them again after a restart." wide>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Environment" hint="Try Demo Trading first: it needs a key created in OKX demo mode.">
          <Select value={environment} disabled={!!preset} onChange={(e) => setEnvironment(e.target.value as Environment)}>
            <option value="OKX_DEMO">Demo Trading (virtual funds)</option>
            <option value="OKX_LIVE">Live (REAL MONEY)</option>
          </Select>
        </Field>
        <Field label="Account name"><Input value={name} disabled={!!preset} onChange={(e) => setName(e.target.value)} placeholder="My OKX" /></Field>
        <Field label="API key"><Input type="password" autoComplete="off" value={apiKey} onChange={(e) => setApiKey(e.target.value)} /></Field>
        <Field label="Secret key"><Input type="password" autoComplete="off" value={secretKey} onChange={(e) => setSecretKey(e.target.value)} /></Field>
        <Field label="Passphrase" className="sm:col-span-2"><Input type="password" autoComplete="off" value={passphrase} onChange={(e) => setPassphrase(e.target.value)} /></Field>
        <label className="flex items-start gap-2 text-xs text-ink-2 sm:col-span-2">
          <input type="checkbox" className="mt-0.5 accent-[var(--accent)]" disabled={!unlocked} checked={remember && unlocked} onChange={(e) => setRemember(e.target.checked)} />
          <span>
            Save encrypted with my master password
            <span className="block text-[11px] text-ink-3">
              {unlocked
                ? 'Reconnects when you unlock after a restart. Real accounts still start disarmed. Disconnecting deletes the saved key.'
                : 'Set and unlock a master password in Settings to use this.'}
            </span>
          </span>
        </label>
      </div>
      <h3 className="mt-4 mb-2 font-display text-sm font-extrabold">Account ceilings</h3>
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
        {limit('Max per order (USDT)', 'maxNotional')}
        {limit('Max leverage (x)', 'maxLeverage')}
        {limit('Max daily loss (%)', 'maxDailyLossPct')}
        {limit('Max positions', 'maxOpenPositions')}
      </div>
      <p className="mt-3 text-xs text-ink-3">
        Enforced on every order from you or any agent. A stop loss is always required. The key must be Trade only: a key with withdrawal permission is refused. The OKX account must use one-way position mode.
      </p>
      {environment === 'OKX_LIVE' && (
        <p className="mt-3 flex items-start gap-2 rounded-xl bg-down-soft px-3 py-2 text-[13px] font-semibold text-down">
          <TriangleAlert className="mt-0.5 size-4 shrink-0" />
          This connects real funds. MochiPrep's OKX integration has only been tested against a simulated exchange, so use small limits, and try Demo Trading first.
        </p>
      )}
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="primary" disabled={busy || !name.trim() || !apiKey.trim() || !secretKey.trim() || !passphrase.trim()} onClick={connect}>
          <Link2 /> {busy ? 'Verifying...' : 'Verify and connect'}
        </Button>
      </div>
    </Dialog>
  );
}

function ArmDialog({ status, onClose }: { status: ExchangeStatus; onClose(): void }) {
  const [phrase, setPhrase] = useState('');
  const arm = async () => {
    const ok = await attempt(() => api.post(`/exchange/okx/${status.accountId}/arm`, { confirmation: phrase }));
    if (ok) onClose();
  };
  return (
    <Dialog open onOpenChange={onClose} title={`Arm ${status.name}?`} description="Arming allows real orders from you and from running agents. It ends at Stop All, Disarm, or a backend restart, even when the key is saved.">
      <ul className="mb-3 space-y-1 text-[13px] text-ink-2">
        <li>Up to {status.limits.maxNotional} USDT per order, {status.limits.maxLeverage}x leverage</li>
        <li>Up to {status.limits.maxOpenPositions} positions, new orders stop after a {status.limits.maxDailyLossPct}% daily loss</li>
      </ul>
      <p className="mb-3 rounded-xl bg-down-soft px-3 py-2 text-[13px] font-semibold text-down" data-testid="arm-dyor">{DYOR_NOTICE}</p>
      <Field label={`Type ${ARM_PHRASE} to confirm`}>
        <Input autoComplete="off" value={phrase} onChange={(e) => setPhrase(e.target.value)} aria-label="Arm confirmation" />
      </Field>
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="danger" disabled={phrase !== ARM_PHRASE} onClick={arm}><ShieldAlert /> Arm real orders</Button>
      </div>
    </Dialog>
  );
}

function StatusRow({ status, onReconnect, onArm }: { status: ExchangeStatus; onReconnect(): void; onArm(): void }) {
  const real = status.venue === 'OKX_LIVE';
  return (
    <div className="flex flex-wrap items-center gap-2 border-t border-line px-5 py-3" data-testid="exchange-row">
      <span className="font-semibold">{status.name}</span>
      <VenueBadge venue={status.venue} />
      {!status.connected ? (
        <Badge tone="warn">Not connected</Badge>
      ) : status.healthy ? (
        <Badge tone="up">In sync</Badge>
      ) : (
        <Badge tone="down" title={status.error ?? undefined}>Sync lost</Badge>
      )}
      {real && status.connected && (status.armed ? <Badge tone="down">Armed</Badge> : <Badge>Disarmed</Badge>)}
      {status.saved && <Badge tone="info" title="Encrypted with your master password">Key saved</Badge>}
      <span className="num text-xs text-ink-3">{status.lastSyncAt ? dateTime(status.lastSyncAt) : ''}</span>
      <span className="ml-auto flex gap-1">
        {!status.connected && <Button size="sm" onClick={onReconnect}><Link2 /> Reconnect</Button>}
        {status.connected && real && !status.armed && <Button size="sm" variant="danger" onClick={onArm}><ShieldAlert /> Arm</Button>}
        {status.connected && real && status.armed && (
          <Button size="sm" onClick={() => attempt(() => api.post(`/exchange/okx/${status.accountId}/disarm`))}><ShieldOff /> Disarm</Button>
        )}
        {status.connected && (
          <Button size="sm" variant="ghost" onClick={() => attempt(() => api.post(`/exchange/okx/${status.accountId}/disconnect`))}><Unplug /> Disconnect</Button>
        )}
      </span>
    </div>
  );
}

export function ExchangeCard() {
  const exchange = useApp((s) => s.exchange);
  const [connecting, setConnecting] = useState<ExchangeStatus | 'new' | null>(null);
  const [arming, setArming] = useState<ExchangeStatus | null>(null);
  return (
    <Card>
      <CardHeader
        title="OKX trading"
        description="Lets you and your agents place orders on OKX. Exchange accounts appear under Live OKX."
        action={<Button variant="primary" onClick={() => setConnecting('new')}><Link2 /> Connect OKX</Button>}
      />
      {exchange.map((status) => (
        <StatusRow key={status.accountId} status={status} onReconnect={() => setConnecting(status)} onArm={() => setArming(status)} />
      ))}
      {connecting && <ConnectDialog preset={connecting === 'new' ? null : connecting} onClose={() => setConnecting(null)} />}
      {arming && <ArmDialog status={arming} onClose={() => setArming(null)} />}
    </Card>
  );
}
