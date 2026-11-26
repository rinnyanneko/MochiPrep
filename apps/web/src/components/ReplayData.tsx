import { Download, Trash2 } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import type { ReplayDataView, ReplayPlan } from '@mochiprep/shared';
import { Badge, Button, Dialog, Field, Input } from '@/components/ui';
import { api } from '@/lib/api';
import { cancellable } from '@/lib/cancellable';
import { formatBytes, formatDuration } from '@/lib/estimates';
import { attempt, useApp } from '@/store';

const DAY = 86_400_000;

/** A UTC day as the `YYYY-MM-DD` a date input holds. */
const dayText = (ms: number) => new Date(ms).toISOString().slice(0, 10);
const dayMs = (text: string) => Date.parse(`${text}T00:00:00Z`);

/** What loading the picked dates takes, or why it cannot be worked out. */
function Estimate({ plan, error }: Readonly<{ plan: ReplayPlan | null; error: string | null }>) {
  if (error) return <span className="text-down">{error}</span>;
  if (!plan) return <>Working out what this needs...</>;
  if (plan.missingMinutes === 0) {
    return <>Everything is saved already ({formatBytes(plan.savedBytes)}): it loads with no download.</>;
  }
  return (
    <>
      Downloads {Math.round((plan.missingMinutes / 1440 / plan.pairs) * 10) / 10} days for each of {plan.pairs} pairs, in{' '}
      <strong>{formatDuration(plan.seconds)}</strong> at OKX's rate limit, and takes about <strong>{formatBytes(plan.downloadBytes)}</strong> on disk.
      {plan.savedBytes > 0 && ` ${formatBytes(plan.savedBytes)} already saved for these dates is reused.`}
    </>
  );
}

/** Saved bars cover `[start, end)`, not always whole days; people read the last day included. */
const spanText = (start: number, end: number) => `${dayText(start)} to ${dayText(end - 1)}`;

/**
 * Picks the replay's dates, says what loading them takes before anything downloads, and lists
 * the saved bars so the ones no longer needed can be deleted. Overlapping dates reuse saved bars.
 */
export function ReplayDataDialog({ onClose }: Readonly<{ onClose(): void }>) {
  const [view, setView] = useState<ReplayDataView | null>(null);
  const [from, setFrom] = useState('');
  const [to, setTo] = useState('');
  const [plan, setPlan] = useState<ReplayPlan | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<{ start: number; end: number } | null>(null);
  // Dates typed while the first answer was on its way are the owner's: defaults never replace them.
  const touched = useRef(false);
  const pick = (set: (text: string) => void) => (text: string) => {
    touched.current = true;
    set(text);
  };

  const reload = async () => {
    const next = await attempt(() => api.get<ReplayDataView>('/replay/data'));
    if (!next) return;
    setView(next);
    if (!touched.current && !from) {
      // The last dates picked, or the two weeks before OKX's history ends.
      const chosen = next.chosen ?? { from: next.latest - 14 * DAY, to: next.latest };
      setFrom(dayText(chosen.from));
      setTo(dayText(chosen.to - DAY));
    }
  };
  useEffect(() => void reload(), []);

  const range = from && to ? { from: dayMs(from), to: dayMs(to) + DAY } : null;
  useEffect(() => {
    // Cleared or changed dates: the old estimate no longer describes what Load would do.
    setPlan(null);
    setPlanError(null);
    if (!range || !view?.history) return;
    // An estimate for dates no longer picked must not describe what Load would download.
    const request = cancellable();
    const timer = setTimeout(() => {
      api.post<ReplayPlan>('/replay/estimate', range).then(
        request.live(setPlan),
        request.live((e: Error) => setPlanError(e.message)),
      );
    }, 250);
    return () => {
      clearTimeout(timer);
      request.cancel();
    };
  }, [from, to, view?.history, view?.saved.length]);

  const load = async () => {
    if (!range) return;
    const ok = await attempt(() => api.post('/replay/load', range));
    if (ok) {
      await useApp.getState().refreshState();
      onClose();
    }
  };
  const remove = async () => {
    if (!deleting) return;
    const next = await attempt(() => api.post<ReplayDataView>('/replay/data/delete', { from: deleting.start, to: deleting.end }));
    setDeleting(null);
    if (next) setView(next);
  };

  return (
    <Dialog open onOpenChange={onClose} title="Replay data" description="Recorded OKX one-minute bars of the built-in pairs. Pick the dates to replay; only the minutes not saved yet are downloaded." wide>
      {view && !view.history ? (
        <p className="text-[13px] text-ink-2">Offline, the replay plays the bundled synthetic tape, so there is no OKX history to download.</p>
      ) : (
        <div className="space-y-4" data-testid="replay-data">
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label="From (UTC)">
              <Input type="date" aria-label="Replay from" value={from} max={view ? dayText(view.latest - DAY) : undefined} onChange={(e) => pick(setFrom)(e.target.value)} />
            </Field>
            <Field label="To, included (UTC)">
              <Input type="date" aria-label="Replay to" value={to} max={view ? dayText(view.latest - DAY) : undefined} onChange={(e) => pick(setTo)(e.target.value)} />
            </Field>
          </div>
          <div className="rounded-xl bg-surface-2 px-3 py-2.5 text-[13px] text-ink-2" data-testid="replay-estimate">
            <Estimate plan={plan} error={planError} />
            {view && (
              <span className="mt-1 block text-[11px] text-ink-3">
                Includes {view.warmupDays} days before the start, so indicators are ready on the first bar. At most {view.maxDays} days; the last day is the day before today (UTC).
              </span>
            )}
          </div>
          <div className="flex justify-end gap-2">
            <Button onClick={onClose}>Cancel</Button>
            <Button variant="primary" disabled={!plan || !range} onClick={load}>
              <Download /> {plan && plan.missingMinutes > 0 ? 'Download and load' : 'Load'}
            </Button>
          </div>

          <section>
            <h3 className="mb-1.5 text-xs font-semibold text-ink-2">Saved on this computer</h3>
            {view?.saved.length === 0 ? (
              <p className="text-[13px] text-ink-3">Nothing yet.</p>
            ) : (
              <ul className="divide-y divide-line rounded-xl border border-line">
                {view?.saved.map((s) => (
                  <li key={s.start} className="flex flex-wrap items-center gap-2 px-3 py-2 text-[13px]" data-testid="saved-range">
                    <span className="num font-semibold">{spanText(s.start, s.end)}</span>
                    <span className="num text-ink-3">{formatBytes(s.bytes)}</span>
                    {!s.complete && <Badge tone="warn">Some pairs only</Badge>}
                    {s.inUse && <Badge tone="info">In use</Badge>}
                    <Button size="sm" variant="ghost" className="ml-auto" disabled={s.inUse} title={s.inUse ? 'The loaded replay uses these bars. Load other dates first.' : undefined} onClick={() => setDeleting({ start: s.start, end: s.end })}>
                      <Trash2 /> Delete
                    </Button>
                  </li>
                ))}
              </ul>
            )}
          </section>
        </div>
      )}
      {deleting && (
        <Dialog open onOpenChange={() => setDeleting(null)} title="Delete these bars?" description={`${spanText(deleting.start, deleting.end)}, every pair. Loading these dates again downloads them again.`}>
          <div className="flex justify-end gap-2">
            <Button onClick={() => setDeleting(null)}>Cancel</Button>
            <Button variant="danger" onClick={remove} data-testid="confirm-delete-replay">Delete</Button>
          </div>
        </Dialog>
      )}
    </Dialog>
  );
}
