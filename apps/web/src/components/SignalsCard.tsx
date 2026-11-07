import { Check, TriangleAlert, X } from 'lucide-react';
import { useState } from 'react';
import type { DecisionRecord } from '@mochiprep/shared';
import { api } from '@/lib/api';
import { dateTime, money, price, qty } from '@/lib/format';
import { attempt, useApp, useSourceSignals } from '@/store';
import { actionTone, VenueBadge } from './trading';
import { Badge, Button, Card, CardHeader, Dialog } from './ui';

function SignalRow({ signal, onExecute }: { signal: DecisionRecord; onExecute(): void }) {
  const ai = signal.decision!;
  const sized = signal.risk?.sized;
  const venue = useApp((s) => s.accounts.find((a) => a.id === signal.accountId)?.venue) ?? signal.venue;
  return (
    <div className="border-t border-line px-5 py-3" data-testid="signal-row">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-semibold">{signal.agentName}</span>
        <Badge tone={actionTone(ai.action)}>{ai.action}</Badge>
        <span className="text-[13px] font-semibold">{signal.symbol}</span>
        {venue !== 'PAPER' && <VenueBadge venue={venue} />}
        {signal.isMock && <Badge tone="info">Mock AI</Badge>}
        <span className="ml-auto flex gap-1.5">
          <Button size="sm" variant="primary" onClick={onExecute}><Check /> {ai.action === 'CLOSE' ? 'Close position' : 'Place order'}</Button>
          <Button size="sm" variant="ghost" onClick={() => attempt(() => api.post(`/signals/${signal.id}/dismiss`))}><X /> Dismiss</Button>
        </span>
      </div>
      {sized && (
        <div className="num mt-1.5 flex flex-wrap gap-x-4 gap-y-0.5 text-xs text-ink-2">
          <span>Entry ~{price(sized.referencePrice, signal.symbol)}</span>
          <span>SL {ai.stopLoss === null ? '–' : price(ai.stopLoss, signal.symbol)}</span>
          <span>TP {ai.takeProfit === null ? '–' : price(ai.takeProfit, signal.symbol)}</span>
          <span>Size {qty(sized.quantity)}</span>
          <span>Notional {money(sized.notional)}</span>
          <span>Risk {money(sized.riskAmount)}</span>
        </div>
      )}
      <p className="mt-1 line-clamp-2 text-xs text-ink-3">{ai.reason.replace(/^\[DEMO[^\]]*\]\s*/, '')}</p>
      <p className="mt-0.5 text-[11px] text-ink-3">Confidence {(ai.confidence * 100).toFixed(0)}% (uncalibrated) · expires {dateTime(signal.signalExpiresAt)}</p>
    </div>
  );
}

export function SignalsCard() {
  const signals = useSourceSignals();
  const accounts = useApp((s) => s.accounts);
  const toast = useApp((s) => s.toast);
  const [confirming, setConfirming] = useState<DecisionRecord | null>(null);
  const [busy, setBusy] = useState(false);

  const run = async (signal: DecisionRecord) => {
    setBusy(true);
    const result = await attempt(() => api.post<DecisionRecord>(`/signals/${signal.id}/execute`));
    setBusy(false);
    setConfirming(null);
    if (!result) return;
    if (result.outcome === 'EXECUTED') useApp.setState({ lastFillAt: Date.now() });
    const failed = result.outcome === 'RISK_REJECTED' || result.outcome === 'ORDER_REJECTED' || result.outcome === 'SIGNAL_EXPIRED';
    toast(failed ? 'error' : 'info', result.outcomeDetail ?? 'Done');
  };

  const onExecute = (signal: DecisionRecord) => {
    const venue = accounts.find((a) => a.id === signal.accountId)?.venue;
    if (venue === 'PAPER') void run(signal);
    else setConfirming(signal);
  };

  if (signals.length === 0) return null;
  return (
    <Card data-testid="signals-card">
      <CardHeader title={`AI suggestions (${signals.length})`} description="Size and risk are checked again at current prices when you accept. AI output can be wrong. DYOR." />
      {signals.map((signal) => <SignalRow key={signal.id} signal={signal} onExecute={() => onExecute(signal)} />)}
      <Dialog open={confirming !== null} onOpenChange={() => setConfirming(null)} title={`${confirming?.decision?.action} ${confirming?.symbol}?`} description="The order is sized again at current prices, so it can differ from the suggestion.">
        {confirming?.venue === 'OKX_LIVE' && (
          <p className="mb-3 flex items-start gap-2 rounded-xl bg-down-soft px-3 py-2 text-[13px] font-semibold text-down">
            <TriangleAlert className="mt-0.5 size-4 shrink-0" /> This places a real order with real money. It cannot be undone.
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button onClick={() => setConfirming(null)}>Cancel</Button>
          <Button variant="danger" disabled={busy} onClick={() => confirming && run(confirming)}>{confirming?.venue === 'OKX_LIVE' ? 'Place real order' : 'Place order'}</Button>
        </div>
      </Dialog>
    </Card>
  );
}
