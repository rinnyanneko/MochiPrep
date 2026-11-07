import { KeyRound, Lock, LockOpen, ShieldCheck, ShieldOff, TriangleAlert } from 'lucide-react';
import { useState, type FormEvent } from 'react';
import { VAULT_RESET_PHRASE, type VaultStatus } from '@mochiprep/shared';
import { api } from '@/lib/api';
import { attempt, useApp } from '@/store';
import { Badge, Button, Card, CardHeader, Dialog, Field, Input } from './ui';

const MIN_LENGTH = 10;

const applyStatus = (result: { status: VaultStatus } | undefined) => {
  if (result) useApp.setState({ vault: result.status });
  return !!result;
};

/** Shown on every page while locked: paper trading works, but saved keys are not loaded yet. */
export function UnlockBanner() {
  const vault = useApp((s) => s.vault);
  const [password, setPassword] = useState('');
  const [busy, setBusy] = useState(false);
  if (vault !== 'LOCKED') return null;

  const unlock = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    const result = await attempt(() => api.post<{ status: VaultStatus; failed: string[] }>('/vault/unlock', { password }));
    setBusy(false);
    if (applyStatus(result)) {
      setPassword('');
      useApp.getState().toast('info', result!.failed.length ? 'Unlocked. Some OKX accounts could not reconnect.' : 'Unlocked.');
    }
  };

  return (
    <form onSubmit={unlock} className="flex flex-wrap items-center gap-2 bg-warn-soft px-4 py-2 text-xs font-medium text-warn" data-testid="unlock-banner">
      <Lock className="size-3.5 shrink-0" />
      <span className="mr-auto">Locked. Saved AI and OKX keys stay sealed until you enter your master password.</span>
      <Input
        type="password"
        autoComplete="current-password"
        aria-label="Master password"
        placeholder="Master password"
        className="h-8 w-56"
        value={password}
        onChange={(e) => setPassword(e.target.value)}
      />
      <Button type="submit" size="sm" variant="primary" disabled={busy || !password}>
        <LockOpen /> {busy ? 'Unlocking...' : 'Unlock'}
      </Button>
    </form>
  );
}

function CreateDialog({ onClose }: { onClose(): void }) {
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [busy, setBusy] = useState(false);
  const mismatch = confirm.length > 0 && confirm !== password;

  const create = async () => {
    setBusy(true);
    const ok = applyStatus(await attempt(() => api.post<{ status: VaultStatus }>('/vault', { password })));
    setBusy(false);
    if (ok) {
      useApp.getState().toast('info', 'Master password set. Your AI keys are now encrypted.');
      onClose();
    }
  };

  return (
    <Dialog open onOpenChange={onClose} title="Set a master password" description="Encrypts saved AI keys, and lets you save OKX keys. You enter it once after every backend restart.">
      <div className="space-y-3">
        <Field label="Master password" hint={`At least ${MIN_LENGTH} characters. A passphrase of several words works well.`}>
          <Input type="password" autoComplete="new-password" value={password} onChange={(e) => setPassword(e.target.value)} />
        </Field>
        <Field label="Repeat it" hint={mismatch ? 'Does not match.' : undefined}>
          <Input type="password" autoComplete="new-password" value={confirm} onChange={(e) => setConfirm(e.target.value)} />
        </Field>
        <p className="flex items-start gap-2 rounded-xl bg-warn-soft px-3 py-2 text-[13px] font-semibold text-warn">
          <TriangleAlert className="mt-0.5 size-4 shrink-0" />
          There is no recovery. If you forget it, the saved keys are lost and you enter them again. Your accounts and trade history are not affected.
        </p>
      </div>
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="primary" disabled={busy || password.length < MIN_LENGTH || confirm !== password} onClick={create}>
          <ShieldCheck /> {busy ? 'Encrypting...' : 'Set password'}
        </Button>
      </div>
    </Dialog>
  );
}

function ChangeDialog({ onClose }: { onClose(): void }) {
  const [current, setCurrent] = useState('');
  const [next, setNext] = useState('');
  const [confirm, setConfirm] = useState('');
  const [busy, setBusy] = useState(false);

  const change = async () => {
    setBusy(true);
    const ok = applyStatus(await attempt(() => api.post<{ status: VaultStatus }>('/vault/password', { current, next })));
    setBusy(false);
    if (ok) {
      useApp.getState().toast('info', 'Master password changed.');
      onClose();
    }
  };

  return (
    <Dialog open onOpenChange={onClose} title="Change master password">
      <div className="space-y-3">
        <Field label="Current password"><Input type="password" autoComplete="current-password" value={current} onChange={(e) => setCurrent(e.target.value)} /></Field>
        <Field label="New password" hint={`At least ${MIN_LENGTH} characters.`}><Input type="password" autoComplete="new-password" value={next} onChange={(e) => setNext(e.target.value)} /></Field>
        <Field label="Repeat new password" hint={confirm && confirm !== next ? 'Does not match.' : undefined}>
          <Input type="password" autoComplete="new-password" value={confirm} onChange={(e) => setConfirm(e.target.value)} />
        </Field>
      </div>
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="primary" disabled={busy || !current || next.length < MIN_LENGTH || confirm !== next} onClick={change}>Change password</Button>
      </div>
    </Dialog>
  );
}

function DisableDialog({ onClose }: { onClose(): void }) {
  const [password, setPassword] = useState('');
  const [busy, setBusy] = useState(false);
  const disable = async () => {
    setBusy(true);
    const ok = applyStatus(await attempt(() => api.post<{ status: VaultStatus }>('/vault/disable', { password })));
    setBusy(false);
    if (ok) onClose();
  };
  return (
    <Dialog open onOpenChange={onClose} title="Turn off the master password?" description="AI keys go back to being saved unencrypted. Saved OKX keys are deleted, since they are never stored unencrypted. Connected accounts stay connected until the next restart.">
      <Field label="Master password"><Input type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} /></Field>
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="danger" disabled={busy || !password} onClick={disable}><ShieldOff /> Turn off</Button>
      </div>
    </Dialog>
  );
}

function ResetDialog({ onClose }: { onClose(): void }) {
  const [phrase, setPhrase] = useState('');
  const reset = async () => {
    if (applyStatus(await attempt(() => api.post<{ status: VaultStatus }>('/vault/reset', { confirmation: phrase })))) onClose();
  };
  return (
    <Dialog open onOpenChange={onClose} title="Forgot the master password?" description="It cannot be recovered. Resetting deletes every saved AI key and saved OKX key, then you enter them again. Accounts, agents and trade history stay.">
      <Field label={`Type ${VAULT_RESET_PHRASE} to confirm`}>
        <Input autoComplete="off" aria-label="Reset confirmation" value={phrase} onChange={(e) => setPhrase(e.target.value)} />
      </Field>
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="danger" disabled={phrase !== VAULT_RESET_PHRASE} onClick={reset}>Delete saved keys</Button>
      </div>
    </Dialog>
  );
}

const DESCRIPTION: Record<VaultStatus, string> = {
  OFF: 'Off. AI keys are saved unencrypted, and OKX keys are never saved.',
  LOCKED: 'Locked. Enter the password in the bar at the top to load saved keys.',
  UNLOCKED: 'On. AI keys and any OKX keys you choose to save are encrypted with AES-256-GCM. The password itself is never stored.',
};

export function VaultCard() {
  const vault = useApp((s) => s.vault);
  const [dialog, setDialog] = useState<'create' | 'change' | 'disable' | 'reset' | null>(null);
  const close = () => setDialog(null);
  return (
    <Card>
      <CardHeader
        title={
          <span className="flex items-center gap-2">
            Master password
            {vault === 'OFF' ? <Badge tone="warn">Off</Badge> : vault === 'LOCKED' ? <Badge tone="warn"><Lock /> Locked</Badge> : <Badge tone="up"><ShieldCheck /> On</Badge>}
          </span>
        }
        description={DESCRIPTION[vault]}
        action={vault === 'OFF' && <Button variant="primary" onClick={() => setDialog('create')}><KeyRound /> Set password</Button>}
      />
      {vault !== 'OFF' && (
        <div className="flex flex-wrap gap-2 px-5 pb-4">
          {vault === 'UNLOCKED' && <Button onClick={() => setDialog('change')}>Change password</Button>}
          {vault === 'UNLOCKED' && <Button onClick={() => setDialog('disable')}><ShieldOff /> Turn off</Button>}
          <Button variant="ghost" onClick={() => setDialog('reset')}>Forgot password</Button>
        </div>
      )}
      <p className="border-t border-line px-5 py-3 text-xs text-ink-3">
        Protects the database file if it is copied, synced or backed up. It does not protect against malware on this computer while MochiPrep is unlocked. Backups made before you set it still hold the old unencrypted keys.
      </p>
      {dialog === 'create' && <CreateDialog onClose={close} />}
      {dialog === 'change' && <ChangeDialog onClose={close} />}
      {dialog === 'disable' && <DisableDialog onClose={close} />}
      {dialog === 'reset' && <ResetDialog onClose={close} />}
    </Card>
  );
}
