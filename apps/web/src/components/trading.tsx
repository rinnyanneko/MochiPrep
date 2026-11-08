import { CircleCheck, CircleX, Pencil, Trash2 } from 'lucide-react';
import { useState } from 'react';
import type {
  AccountVenue,
  AiTrailingStop,
  DecisionOutcome,
  DecisionRecord,
  Fill,
  LedgerEvent,
  Order,
  Position,
  PositionView,
  Protection,
  TrailingStopSpec,
} from '@mochiprep/shared';
import { api } from '@/lib/api';
import { money, pct, pnlClass, price, qty, shortId, signedMoney } from '@/lib/format';
import { cn } from '@/lib/utils';
import { attempt, useApp, useDateTime, useSourceAccounts } from '@/store';
import { Badge, Button, Dialog, Field, Input, Select, Table, type Tone } from './ui';

export function AccountPicker({ allowAll, className }: { allowAll?: boolean; className?: string }) {
  const accounts = useSourceAccounts();
  const accountId = useApp((s) => s.accountId);
  const setAccountId = useApp((s) => s.setAccountId);
  const value = accountId && accounts.some((a) => a.id === accountId) ? accountId : allowAll ? '' : (accounts[0]?.id ?? '');
  return (
    <Select aria-label="Paper account" className={cn('w-56', className)} value={value} onChange={(e) => setAccountId(e.target.value || null)}>
      {allowAll && <option value="">All accounts</option>}
      {accounts.length === 0 && !allowAll && <option value="">No accounts yet</option>}
      {accounts.map((a) => (
        <option key={a.id} value={a.id}>{a.name}{VENUE_SUFFIX[a.venue]}</option>
      ))}
    </Select>
  );
}

const VENUE_SUFFIX: Record<AccountVenue, string> = { PAPER: '', OKX_DEMO: ' (OKX demo)', OKX_LIVE: ' (REAL MONEY)' };

export function VenueBadge({ venue }: { venue: AccountVenue }) {
  if (venue === 'OKX_LIVE') return <Badge tone="down">Real money</Badge>;
  if (venue === 'OKX_DEMO') return <Badge tone="info">OKX demo</Badge>;
  return <Badge>Paper</Badge>;
}

/** The account in view: the picked one if it belongs to this source, else the first. */
export function useCurrentAccount() {
  const accounts = useSourceAccounts();
  const accountId = useApp((s) => s.accountId);
  return accounts.find((a) => a.id === accountId) ?? accounts[0];
}

export const sideBadge = (side: Position['side']) => <Badge tone={side === 'LONG' ? 'rise' : 'fall'}>{side}</Badge>;
const orderSide = (side: Order['side']) => <span className={side === 'BUY' ? 'text-rise' : 'text-fall'}>{side}</span>;

function ProtectionDialog({ position, onClose }: { position: PositionView; onClose(): void }) {
  const [stop, setStop] = useState(position.stopLoss ?? '');
  const [target, setTarget] = useState(position.takeProfit ?? '');
  const save = async () => {
    const ok = await attempt(() =>
      api.put(`/positions/${position.accountId}/${position.symbol}/protection`, {
        stopLoss: stop.trim() || null,
        takeProfit: target.trim() || null,
      }),
    );
    if (ok) onClose();
  };
  return (
    <Dialog open onOpenChange={onClose} title={`Protection for ${position.symbol}`} description={`${position.side} ${qty(position.quantity)} from ${price(position.entryPrice, position.symbol)}. Leave a field empty to remove it.`}>
      <div className="grid grid-cols-2 gap-3">
        <Field label="Stop loss"><Input inputMode="decimal" value={stop} onChange={(e) => setStop(e.target.value)} /></Field>
        <Field label="Take profit"><Input inputMode="decimal" value={target} onChange={(e) => setTarget(e.target.value)} /></Field>
      </div>
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="primary" onClick={save}>Save</Button>
      </div>
    </Dialog>
  );
}

export function PositionsTable({ positions, actions = true, editable = true, accountNames }: { positions: PositionView[]; actions?: boolean; editable?: boolean; accountNames?: Map<string, string> }) {
  const [editing, setEditing] = useState<PositionView | null>(null);
  const close = (p: PositionView, fraction: number) =>
    attempt(() => api.post(`/positions/${p.accountId}/${p.symbol}/close`, { fraction, clientOrderId: `ui:${crypto.randomUUID()}` }));
  return (
    <>
      <Table
        head={[...(accountNames ? ['Account'] : []), 'Symbol', 'Side', 'Size', 'Entry', 'Mark', 'Liq. price', 'Margin', 'Lev.', 'Stop / Target', 'Unrealized', ...(actions ? [''] : [])]}
        empty={positions.length === 0 && 'No open positions.'}
      >
        {positions.map((p) => (
          <tr key={p.id} data-testid="position-row">
            {accountNames && <td className="text-ink-2">{accountNames.get(p.accountId)}</td>}
            <td className="font-semibold">{p.symbol}</td>
            <td>{sideBadge(p.side)}</td>
            <td className="num">{qty(p.quantity)}</td>
            <td className="num">{price(p.entryPrice, p.symbol)}</td>
            <td className="num">{price(p.markPrice, p.symbol)}</td>
            <td className="num text-warn">{price(p.liquidationPrice, p.symbol)}</td>
            <td className="num">{money(p.margin)}</td>
            <td className="num">{p.leverage}x</td>
            <td className="num text-ink-2">{p.stopLoss ? price(p.stopLoss, p.symbol) : '–'} / {p.takeProfit ? price(p.takeProfit, p.symbol) : '–'}</td>
            <td className={cn('num font-semibold', pnlClass(p.unrealizedPnl))}>{signedMoney(p.unrealizedPnl)}</td>
            {actions && (
              <td>
                <div className="flex justify-end gap-1">
                  {editable && <Button size="icon" variant="ghost" aria-label={`Edit stop and target for ${p.symbol}`} onClick={() => setEditing(p)}><Pencil /></Button>}
                  <Button size="sm" onClick={() => close(p, 0.5)}>Close 50%</Button>
                  <Button size="sm" variant="danger" onClick={() => close(p, 1)}>Close</Button>
                </div>
              </td>
            )}
          </tr>
        ))}
      </Table>
      {editing && <ProtectionDialog position={editing} onClose={() => setEditing(null)} />}
    </>
  );
}

const ORDER_LABEL: Record<Order['type'], string> = { MARKET: 'Market', LIMIT: 'Limit', STOP_MARKET: 'Stop loss', TAKE_PROFIT_MARKET: 'Take profit', TRAILING_STOP_MARKET: 'Trailing stop' };
const ORDER_TONE: Record<Order['status'], Tone> = { OPEN: 'info', FILLED: 'up', CANCELED: 'neutral', REJECTED: 'down' };

export function OrdersTable({ orders, cancellable, emptyText }: { orders: Order[]; cancellable?: boolean; emptyText: string }) {
  const dateTime = useDateTime();
  return (
    <Table head={['Time', 'Symbol', 'Side', 'Type', 'Size', 'Price', 'Status', 'Order', ...(cancellable ? [''] : [])]} empty={orders.length === 0 && emptyText}>
      {orders.map((o) => (
        <tr key={o.id}>
          <td className="num text-ink-2">{dateTime(o.createdAt)}</td>
          <td className="font-semibold">{o.symbol}</td>
          <td className="font-semibold">{orderSide(o.side)}</td>
          <td>{ORDER_LABEL[o.type]}{o.reduceOnly ? <span className="text-ink-3"> · reduce</span> : ''}</td>
          <td className="num">{o.closePosition ? 'Position' : qty(o.quantity)}</td>
          <td className="num">{price(o.avgFillPrice ?? o.price ?? o.triggerPrice, o.symbol)}</td>
          <td title={o.rejectReason ?? undefined}>
            <Badge tone={ORDER_TONE[o.status]}>{o.status}</Badge>
            {o.rejectReason && <span className="ml-2 text-xs text-ink-3">{o.rejectReason}</span>}
          </td>
          <td className="num text-ink-3">{shortId(o.id)}</td>
          {cancellable && (
            <td className="text-right">
              <Button size="icon" variant="ghost" aria-label={`Cancel order ${shortId(o.id)}`} onClick={() => attempt(() => api.del(`/orders/${o.id}`))}><Trash2 /></Button>
            </td>
          )}
        </tr>
      ))}
    </Table>
  );
}

export function FillsTable({ fills, emptyText = 'No fills yet.' }: { fills: Fill[]; emptyText?: string }) {
  const dateTime = useDateTime();
  return (
    <Table head={['Time', 'Symbol', 'Side', 'Size', 'Price', 'Fee', 'Realized PnL', 'Liquidity', 'Fill']} empty={fills.length === 0 && emptyText}>
      {fills.map((f) => (
        <tr key={f.id} data-testid="fill-row">
          <td className="num text-ink-2">{dateTime(f.ts)}</td>
          <td className="font-semibold">{f.symbol}</td>
          <td className="font-semibold">{orderSide(f.side)}</td>
          <td className="num">{qty(f.quantity)}</td>
          <td className="num">{price(f.price, f.symbol)}</td>
          <td className="num">{money(f.fee, 4)}</td>
          <td className={cn('num', pnlClass(f.realizedPnl))}>{signedMoney(f.realizedPnl)}</td>
          <td>{f.liquidity === 'LIQUIDATION' ? <Badge tone="down">Liquidation</Badge> : <span className="text-ink-2">{f.liquidity === 'MAKER' ? 'Maker' : 'Taker'}</span>}</td>
          <td className="num text-ink-3">{shortId(f.id)}</td>
        </tr>
      ))}
    </Table>
  );
}

export function ClosedPositionsTable({ positions }: { positions: Position[] }) {
  const dateTime = useDateTime();
  return (
    <Table head={['Closed', 'Symbol', 'Side', 'Max size', 'Entry', 'Exit', 'Gross PnL', 'Fees', 'Funding', 'Net PnL', 'Result']} empty={positions.length === 0 && 'No closed positions yet.'}>
      {positions.map((p) => {
        const net = Number(p.realizedPnl) - Number(p.feesPaid) - Number(p.fundingPaid);
        return (
          <tr key={p.id}>
            <td className="num text-ink-2">{dateTime(p.closedAt)}</td>
            <td className="font-semibold">{p.symbol}</td>
            <td>{sideBadge(p.side)}</td>
            <td className="num">{qty(p.maxQuantity)}</td>
            <td className="num">{price(p.entryPrice, p.symbol)}</td>
            <td className="num">{price(p.exitPrice, p.symbol)}</td>
            <td className={cn('num', pnlClass(p.realizedPnl))}>{signedMoney(p.realizedPnl)}</td>
            <td className="num">{money(p.feesPaid)}</td>
            <td className="num">{signedMoney(Number(p.fundingPaid) * -1, 4)}</td>
            <td className={cn('num font-semibold', pnlClass(net))}>{signedMoney(net)}</td>
            <td>{p.status === 'LIQUIDATED' ? <Badge tone="down">Liquidated</Badge> : <Badge>Closed</Badge>}</td>
          </tr>
        );
      })}
    </Table>
  );
}

export function LedgerTable({ events }: { events: LedgerEvent[] }) {
  const dateTime = useDateTime();
  return (
    <Table head={['#', 'Time', 'Event', 'Wallet change', 'Margin change', 'Wallet after', 'Memo']} empty={events.length === 0 && 'The ledger is empty.'}>
      {events.map((e) => (
        <tr key={e.id}>
          <td className="num text-ink-3">{e.seq}</td>
          <td className="num text-ink-2">{dateTime(e.ts)}</td>
          <td className="text-xs font-semibold">{e.type.replaceAll('_', ' ')}</td>
          <td className={cn('num', pnlClass(e.amount))}>{Number(e.amount) === 0 ? '–' : signedMoney(e.amount, 4)}</td>
          <td className="num text-ink-2">{Number(e.marginDelta) === 0 ? '–' : signedMoney(e.marginDelta, 4)}</td>
          <td className="num">{money(e.walletAfter, 4)}</td>
          <td className="text-ink-3">{e.memo}</td>
        </tr>
      ))}
    </Table>
  );
}

const OUTCOME: Record<DecisionOutcome, { label: string; tone: Tone }> = {
  HOLD: { label: 'Hold', tone: 'neutral' },
  ANALYSIS_ONLY: { label: 'Analysis only', tone: 'info' },
  SIGNAL: { label: 'Suggested', tone: 'accent' },
  SIGNAL_DISMISSED: { label: 'Dismissed', tone: 'neutral' },
  SIGNAL_EXPIRED: { label: 'Expired', tone: 'neutral' },
  EXECUTED: { label: 'Executed', tone: 'up' },
  ORDER_PLACED: { label: 'Order resting', tone: 'info' },
  ORDER_REJECTED: { label: 'Order rejected', tone: 'down' },
  RISK_REJECTED: { label: 'Risk rejected', tone: 'warn' },
  INVALID_OUTPUT: { label: 'Invalid AI output', tone: 'down' },
  AI_ERROR: { label: 'AI error', tone: 'down' },
  DATA_ERROR: { label: 'Data error', tone: 'warn' },
};

export const OutcomeBadge = ({ outcome }: { outcome: DecisionOutcome }) => <Badge tone={OUTCOME[outcome].tone}>{OUTCOME[outcome].label}</Badge>;
export const OUTCOME_OPTIONS = Object.entries(OUTCOME).map(([value, o]) => ({ value: value as DecisionOutcome, label: o.label }));

export const actionTone = (action?: string): Tone => (action === 'LONG' ? 'rise' : action === 'SHORT' ? 'fall' : action === 'CLOSE' ? 'accent' : 'neutral');

export function DecisionList({ decisions, selected, onSelect, emptyText }: { decisions: DecisionRecord[]; selected?: string; onSelect(d: DecisionRecord): void; emptyText: string }) {
  const dateTime = useDateTime();
  if (decisions.length === 0) return <p className="px-5 py-8 text-center text-[13px] text-ink-3">{emptyText}</p>;
  return (
    <ul className="divide-y divide-line">
      {decisions.map((d) => (
        <li key={d.id}>
          <button
            type="button"
            data-testid="decision-row"
            onClick={() => onSelect(d)}
            className={cn('flex w-full flex-col gap-1 px-5 py-2.5 text-left transition-colors hover:bg-surface-2', selected === d.id && 'bg-surface-2')}
          >
            <span className="flex flex-wrap items-center gap-2">
              <span className="text-[13px] font-semibold">{d.agentName}</span>
              {d.decision && <Badge tone={actionTone(d.decision.action)}>{d.decision.action}</Badge>}
              <OutcomeBadge outcome={d.outcome} />
              <span className="num ml-auto text-[11px] text-ink-3">{dateTime(d.ts)}</span>
            </span>
            <span className="line-clamp-1 text-xs text-ink-3">{d.decision?.reason.replace(/^\[DEMO[^\]]*\]\s*/, '') ?? d.outcomeDetail ?? 'No decision'}</span>
          </button>
        </li>
      ))}
    </ul>
  );
}

const Row = ({ label, children }: { label: string; children: React.ReactNode }) => (
  <div className="flex justify-between gap-4 py-1 text-[13px]">
    <span className="text-ink-3">{label}</span>
    <span className="num min-w-0 text-right break-words">{children}</span>
  </div>
);

// Display only: the ratio is shown as a percent, trimmed so 0.015 reads 1.5% rather than 1.5000000000000002%.
const trailingText = (t: TrailingStopSpec | AiTrailingStop | null) =>
  !t ? 'none' : `${t.kind === 'RATIO' ? `${Number((Number(t.callback) * 100).toPrecision(10))}%` : `${t.callback} USDT`} callback${t.activationPrice === null ? '' : `, from ${t.activationPrice}`}`;

const protectionText = (p: Protection) => `SL ${p.stopLoss ?? '–'} · TP ${p.takeProfit ?? '–'} · trailing ${trailingText(p.trailingStop)}`;

export function DecisionDetail({ decision: d }: { decision: DecisionRecord }) {
  const dateTime = useDateTime();
  const [showRaw, setShowRaw] = useState(false);
  const ai = d.decision;
  return (
    <div className="space-y-4 p-5 pt-3" data-testid="decision-detail">
      <div className="flex flex-wrap items-center gap-2">
        {ai && <Badge tone={actionTone(ai.action)}>{ai.action}</Badge>}
        <OutcomeBadge outcome={d.outcome} />
        <Badge tone={d.source === 'REPLAY' ? 'info' : 'accent'}>{d.source === 'REPLAY' ? 'Replay' : 'Live data'}</Badge>
        <VenueBadge venue={d.venue} />
        {d.isMock && <Badge tone="info">Mock AI</Badge>}
      </div>

      <section>
        <Row label="Time">{dateTime(d.ts)}</Row>
        <Row label="Agent">{d.agentName}</Row>
        <Row label="Model">{d.model}</Row>
        <Row label="Symbol">{d.symbol} · {d.timeframe}</Row>
        {d.snapshot && <Row label="Bid / Ask">{price(d.snapshot.bid, d.symbol)} / {price(d.snapshot.ask, d.symbol)}</Row>}
        {d.snapshot && <Row label="Mark · funding">{price(d.snapshot.mark, d.symbol)} · {d.snapshot.fundingRate ? pct(Number(d.snapshot.fundingRate) * 100, 4) : 'n/a'}</Row>}
        {d.indicators && <Row label="RSI · EMA20 · EMA50">{d.indicators.rsi14?.toFixed(1) ?? '–'} · {money(d.indicators.ema20)} · {money(d.indicators.ema50)}</Row>}
      </section>

      {ai && (
        <section>
          <h3 className="mb-1 text-xs font-semibold text-ink-2">AI proposal</h3>
          <p className="rounded-xl bg-surface-2 px-3 py-2 text-[13px] leading-relaxed">{ai.reason}</p>
          <p className="mt-1 text-[11px] text-ink-3">The model's own explanation. It is unverified and does not show the trade is sound. DYOR.</p>
          <div className="mt-2">
            <Row label="Order">{ai.orderType}{ai.entryPrice ? ` @ ${ai.entryPrice}` : ''}</Row>
            <Row label="Stop / Target">{ai.stopLoss ?? '–'} / {ai.takeProfit ?? '–'}</Row>
            {ai.trailingStop !== undefined && <Row label="Trailing stop">{ai.trailingStop === null ? 'Remove' : trailingText(ai.trailingStop)}</Row>}
            <Row label="Confidence (uncalibrated)">{(ai.confidence * 100).toFixed(0)}%</Row>
          </div>
        </section>
      )}

      {d.news && (
        <section data-testid="decision-news">
          <h3 className="mb-1 text-xs font-semibold text-ink-2">Headlines the AI saw ({d.news.length})</h3>
          {d.newsError && <p className="mb-1 text-xs text-warn">{d.newsError}</p>}
          <ul className="space-y-1">
            {d.news.map((item) => (
              <li key={item.id} className="text-xs text-ink-2">
                <span className="text-ink-3">{item.source} · {dateTime(item.publishedAt)}</span>
                <span className="block">{item.title}</span>
              </li>
            ))}
          </ul>
          <p className="mt-1 text-[11px] text-ink-3">Third-party text, unverified.</p>
        </section>
      )}

      {d.risk && (
        <section data-testid="risk-checks">
          <h3 className="mb-1 text-xs font-semibold text-ink-2">Risk engine: {d.risk.approved ? 'approved' : 'rejected'}</h3>
          <ul className="space-y-1">
            {d.risk.checks.map((c) => (
              <li key={c.id} className="flex items-start gap-2 text-[13px]">
                {c.passed ? <CircleCheck className="mt-0.5 size-4 shrink-0 text-up" /> : <CircleX className="mt-0.5 size-4 shrink-0 text-down" />}
                <span>
                  {c.label}
                  <span className="block text-xs text-ink-3">{c.detail}</span>
                </span>
              </li>
            ))}
          </ul>
          {d.risk.sized && (
            <div className="mt-2">
              <Row label="Sized quantity">{d.risk.sized.quantity}</Row>
              <Row label="Notional · margin">{money(d.risk.sized.notional)} · {money(d.risk.sized.margin)}</Row>
              <Row label="Risk budget">{money(d.risk.sized.riskAmount)} USDT at {d.risk.sized.leverage}x</Row>
            </div>
          )}
        </section>
      )}

      <section>
        <h3 className="mb-1 text-xs font-semibold text-ink-2">Execution</h3>
        <Row label="Result">{d.outcomeDetail ?? 'No order placed'}</Row>
        {d.protectionBefore && <Row label="Protection before">{protectionText(d.protectionBefore)}</Row>}
        {d.protectionAfter && <Row label="Protection after">{protectionText(d.protectionAfter)}</Row>}
        <Row label="Order ID">{shortId(d.orderId)}</Row>
        <Row label="Fill IDs">{d.fillIds.length ? d.fillIds.map(shortId).join(', ') : '–'}</Row>
        <Row label="Latency">{d.latencyMs === null ? '–' : `${d.latencyMs} ms`}</Row>
        <Row label="Tokens in / out">{d.inputTokens ?? '–'} / {d.outputTokens ?? '–'}</Row>
        <Row label="Estimated cost">{d.estimatedCost === null ? 'Unknown (set prices in Settings)' : `$${d.estimatedCost}`}</Row>
      </section>

      {d.validationError && <p className="rounded-xl bg-down-soft px-3 py-2 text-[13px] text-down">Rejected output: {d.validationError}</p>}
      {d.rawResponse && (
        <section>
          <Button size="sm" variant="ghost" onClick={() => setShowRaw(!showRaw)}>{showRaw ? 'Hide' : 'Show'} raw AI response</Button>
          {showRaw && <pre className="num mt-2 max-h-56 overflow-auto rounded-xl bg-surface-2 p-3 text-[11px] break-words whitespace-pre-wrap">{d.rawResponse}</pre>}
        </section>
      )}
    </div>
  );
}
