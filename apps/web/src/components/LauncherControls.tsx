import { useEffect, useState } from 'react';
import { Download, Power, X } from 'lucide-react';
import { api } from '@/lib/api';
import { useApp } from '@/store';
import { Button, Dialog } from './ui';

type LauncherCommand = 'interface_status' | 'interface_quit';

function launcherInvoke() {
  return (window as Window & {
    __TAURI__?: { core?: { invoke: <T>(command: LauncherCommand) => Promise<T> } };
  }).__TAURI__?.core?.invoke;
}

const RELEASES_PAGE = 'https://github.com/rinnyanneko/MochiPrep/releases/latest';
const DISMISSED_UPDATE = 'mochi.dismissedUpdate';

function dismissedUpdate() {
  try {
    return localStorage.getItem(DISMISSED_UPDATE);
  } catch {
    return null;
  }
}

export function UpdateBanner() {
  const [version, setVersion] = useState<string | null>(null);
  const [dismissed, setDismissed] = useState(dismissedUpdate);

  useEffect(() => {
    const invoke = launcherInvoke();
    if (!invoke) return;
    let active = true;
    const check = async () => {
      const offer = await invoke<string | null>('interface_status').catch(() => null);
      if (active) setVersion(offer);
    };
    void check();
    const timer = window.setInterval(check, 60_000);
    return () => {
      active = false;
      window.clearInterval(timer);
    };
  }, []);

  if (!version || version === dismissed) return null;

  const dismiss = () => {
    setDismissed(version);
    try {
      localStorage.setItem(DISMISSED_UPDATE, version);
    } catch { /* Dismiss for this page when storage is unavailable. */ }
  };
  return (
    <div className="flex items-center gap-2 bg-info-soft px-4 py-2 text-xs text-info">
      <Download className="size-4 shrink-0" aria-hidden="true" />
      <span role="status">MochiPrep v{version} is available.</span>
      <a className="font-semibold underline" href={RELEASES_PAGE} target="_blank" rel="noreferrer">View release</a>
      <Button className="ml-auto" size="icon" variant="ghost" aria-label="Dismiss update" onClick={dismiss}>
        <X />
      </Button>
    </div>
  );
}

export function ShutdownButton({ onStopped }: Readonly<{ onStopped: () => void }>) {
  const connected = useApp((s) => s.connected);
  const [confirm, setConfirm] = useState(false);
  const [stopping, setStopping] = useState(false);
  const invoke = launcherInvoke();
  const label = invoke ? 'Quit MochiPrep' : 'Stop backend';

  const stop = async () => {
    setStopping(true);
    try {
      if (invoke) {
        await invoke('interface_quit');
      } else {
        await api.post('/shutdown');
        onStopped();
      }
      setConfirm(false);
    } catch (error) {
      useApp.getState().toast('error', error instanceof Error ? error.message : String(error));
    } finally {
      setStopping(false);
    }
  };

  return (
    <>
      <Button size="sm" variant="ghost" disabled={stopping || (!invoke && !connected)} onClick={() => {
        if (invoke) void stop();
        else setConfirm(true);
      }}>
        <Power /> {label}
      </Button>
      <Dialog title="Stop the backend?" open={confirm} onOpenChange={setConfirm}>
        <p className="text-sm text-ink-2">
          This stops local agents and disconnects all MochiPrep tabs. Positions and orders on OKX remain active.
        </p>
        <div className="mt-4 flex justify-end gap-2">
          <Button variant="ghost" disabled={stopping} onClick={() => setConfirm(false)}>Cancel</Button>
          <Button variant="danger" disabled={stopping} onClick={() => void stop()}>{label}</Button>
        </div>
      </Dialog>
    </>
  );
}
