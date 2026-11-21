import { Plus, RotateCcw, TriangleAlert } from 'lucide-react';
import { useEffect, useState } from 'react';
import { INSTRUMENTS, TIMEFRAMES, type Fill, type LedgerEvent, type Order, type OrderRequest, type OrderResult, type Position, type PositionView, type SymbolId, type Timeframe } from '@mochiprep/shared';
import { ChartTradeSummary, MarketChart } from '@/components/charts';
import { useTradeOverlay } from '@/components/useTradeOverlay';
import { PageTitle } from '@/components/Shell';
import { SignalsCard } from '@/components/SignalsCard';
import { AccountPicker, ClosedPositionsTable, FillsTable, LedgerTable, OrdersTable, PositionsTable, useCurrentAccount, VenueBadge } from '@/components/trading';
import { Badge, Button, Card, Dialog, Field, Input, Segmented, Select, Stat, Tabs } from '@/components/ui';
import { api, isNotDispatched } from '@/lib/api';
import { isInteger, isString, oneOf, useRemembered } from '@/lib/remembered';
import { money, pnlClass, price, qty, signedMoney } from '@/lib/format';
import { attempt, useApp, useQuery, useSymbols, type AccountView } from '@/store';
import { useCandles } from './Markets';

function CreateAccount() {
  const source = useApp((s) => s.source);
  const [open, setOpen] = useState(false);
  const [name, setName] = useState('');
  const [balance, setBalance] = useState('10000');
  const create = async () => {
    const account = await attempt(() => api.post<AccountView>('/accounts', { name, source, initialBalance: balance }));
    if (account) {
      await useApp.getState().refresh(['accounts']);
      useApp.getState().setAccountId(account.id);
      setOpen(false);
      setName('');
    }
  };
  return (
    <>
      <Button onClick={() => setOpen(true)}><Plus /> New account</Button>
      <Dialog open={open} onOpenChange={setOpen} title="New paper account" description={source === 'REPLAY' ? 'Trades against the synthetic replay tape.' : 'Trades against live OKX prices, simulated locally.'}>
        <div className="space-y-3">
          <Field label="Name"><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="My paper account" /></Field>
          <Field label="Starting balance (USDT)"><Input inputMode="decimal" value={balance} onChange={(e) => setBalance(e.target.value)} /></Field>
        </div>
        <div className="mt-4 flex justify-end gap-2">
          <Button onClick={() => setOpen(false)}>Cancel</Button>
          <Button variant="primary" disabled={!name.trim()} onClick={create}>Create</Button>
        </div>
      </Dialog>
    </>
  );
}

function ResetAccount({ account }: { account: AccountView }) {
  const [open, setOpen] = useState(false);
  const reset = async () => {
    await attempt(() => api.post(`/accounts/${account.id}/reset`));
    setOpen(false);
  };
  return (
    <>
      <Button variant="ghost" onClick={() => setOpen(true)}><RotateCcw /> Reset</Button>
      <Dialog open={open} onOpenChange={setOpen} title={`Reset ${account.name}?`} description={`Deletes this account's orders, fills, positions, ledger, equity history and AI decisions, then restores ${money(account.initialBalance)} USDT. This cannot be undone.`}>
        <div className="flex justify-end gap-2">
          <Button onClick={() => setOpen(false)}>Cancel</Button>
          <Button variant="danger" onClick={reset}>Reset account</Button>
        </div>
      </Dialog>
    </>
  );
}

// The API's decimal rule needs a digit before the point, so `.5` is sent as `0.5`.
// The engine's ORDER_UNCONFIRMED reason (crates/mochi-okx/src/engine/mod.rs): OKX had no
// record of the order yet, so the server keeps looking it up.
const UNCONFIRMED_EXPIRY = 'OKX has no record of this order yet; check its outcome in OKX';

// An order OKX acknowledged can also expire this way, so its exchange ID is not checked.
function isUnconfirmedExpiry(order: Order): boolean {
  return order.status === 'CANCELED' && order.rejectReason === UNCONFIRMED_EXPIRY;
}

function normalizeDecimal(value: string): string {
  const trimmed = value.trim();
  return trimmed.startsWith('.') ? `0${trimmed}` : trimmed;
}

// Mirrors the server's decimal string limit, applied after normalization.
const MAX_DECIMAL_LENGTH = 40;

function matchesStep(value: string, step: string): boolean {
  if (value.length > MAX_DECIMAL_LENGTH) return false;
  if (!/^\d*\.?\d+$/.test(value)) return false;
  // The instrument registry keeps the exchange's tick string, which may include an exponent.
  const parsedStep = /^\+?(\d*\.?\d*)(?:e([+-]?\d+))?$/i.exec(step);
  const stepExponent = Number(parsedStep?.[2] ?? 0);
  if (!parsedStep || Math.abs(stepExponent) > 100_000) return false;
  const [whole = '', fraction = ''] = value.split('.');
  const [stepWhole = '', stepFraction = ''] = parsedStep[1]!.split('.');
  const stepPrecision = stepFraction.length - stepExponent;
  const precision = Math.max(fraction.length, stepPrecision);
  const units = BigInt(whole + fraction.padEnd(precision, '0'));
  const increment = BigInt(stepWhole + stepFraction + '0'.repeat(precision - stepPrecision));
  return increment > 0n && units % increment === 0n;
}

type PendingReduction = {
  request: Readonly<OrderRequest>;
  lookupId?: string;
};

type OrderConfirmation = PendingReduction & {
  retry: boolean;
  summary?: [string, string][];
  /** This request's key was saved to localStorage before it was first sent. */
  durable?: boolean;
};

function parsePendingReduction(raw: string | null, accountId: string): PendingReduction | null {
  try {
    const stored = JSON.parse(raw ?? 'null') as PendingReduction | OrderRequest | null;
    if (!stored) return null;
    const request = 'request' in stored ? stored.request : stored;
    if (request?.accountId === accountId && request.reduceOnly === true
      && typeof request.clientOrderId === 'string' && request.clientOrderId.startsWith('ui:')
      && typeof request.positionId === 'string' && typeof request.symbol === 'string'
      && (request.side === 'BUY' || request.side === 'SELL') && typeof request.quantity === 'string'
      && isInteger(request.leverage) && request.stopLoss === null && request.takeProfit === null
      && ((request.type === 'MARKET' && request.price === null)
        || (request.type === 'LIMIT' && typeof request.price === 'string'))) {
      return { request, lookupId: 'lookupId' in stored && typeof stored.lookupId === 'string' ? stored.lookupId : undefined };
    }
  } catch {
    // Ignore invalid entries without losing other pending reductions.
  }
  return null;
}

function readPendingReductions(key: string, accountId: string): PendingReduction[] {
  const pending = new Map<string, PendingReduction>();
  const add = (raw: string | null) => {
    const value = parsePendingReduction(raw, accountId);
    if (value && (value.lookupId || !pending.get(value.request.clientOrderId)?.lookupId)) {
      pending.set(value.request.clientOrderId, value);
    }
  };
  for (const name of ['sessionStorage', 'localStorage'] as const) {
    try {
      const storage = window[name];
      add(storage.getItem(key));
      for (let i = 0; i < storage.length; i += 1) {
        const entry = storage.key(i);
        if (entry?.startsWith(`${key}/`)) add(storage.getItem(entry));
      }
    } catch {
      // The other storage may still be available.
    }
  }
  return [...pending.values()];
}

/** Saves (or clears) a pending reduction. Answers the store that holds it, or null. */
function persistPendingReduction(key: string, pending: PendingReduction, clear = false): 'localStorage' | 'sessionStorage' | null {
  const { request } = pending;
  const requestKey = `${key}/${request.clientOrderId}`;
  const saved = clear ? undefined : readPendingReductions(key, request.accountId)
    .find((item) => item.request.clientOrderId === request.clientOrderId);
  const entry = { request, lookupId: saved?.lookupId ?? pending.lookupId };
  let persisted: 'localStorage' | 'sessionStorage' | null = null;
  for (const name of ['localStorage', 'sessionStorage'] as const) {
    try {
      const storage = window[name];
      if (clear || persisted) {
        storage.removeItem(requestKey);
      } else {
        const current = parsePendingReduction(storage.getItem(requestKey), request.accountId);
        if (!current || (entry.lookupId && !current.lookupId)) {
          storage.setItem(requestKey, JSON.stringify(entry));
        }
        persisted = name;
      }
      // Remove matching legacy entries once per-request storage has succeeded.
      if (parsePendingReduction(storage.getItem(key), request.accountId)?.request.clientOrderId === request.clientOrderId) {
        storage.removeItem(key);
      }
    } catch {
      // The other store may still be available.
    }
  }
  return persisted;
}

function reductionSummary(request: Readonly<OrderRequest>): [string, string][] {
  const currency = INSTRUMENTS[request.symbol]?.baseCurrency;
  const summary: [string, string][] = [
    ['Type', `${request.type} · reduce only`],
    ['Quantity', currency ? `${request.quantity} ${currency}` : request.quantity],
    ['Position', `${request.side === 'SELL' ? 'LONG' : 'SHORT'} · Isolated`],
  ];
  if (request.price != null) summary.push(['Limit price', request.price]);
  return summary;
}

function OrderTicket({ account, symbol, positions }: { account: AccountView; symbol: SymbolId; positions: PositionView[] }) {
  const quote = useApp((s) => s.quotes[account.source][symbol]);
  const [side, setSide] = useRemembered<'BUY' | 'SELL'>(`ticket.${account.id}.side`, 'BUY', oneOf(['BUY', 'SELL'] as const));
  const [type, setType] = useRemembered<'MARKET' | 'LIMIT'>(`ticket.${account.id}.type`, 'MARKET', oneOf(['MARKET', 'LIMIT'] as const));
  const [mode, setMode] = useRemembered<'QTY' | 'MARGIN'>(`ticket.${account.id}.mode`, 'MARGIN', oneOf(['QTY', 'MARGIN'] as const));
  const [amount, setAmount] = useState(() => mode === 'MARGIN' && account.venue !== 'OKX_LIVE' ? '500' : '');
  const [limit, setLimit] = useState('');
  const [leverage, setLeverage] = useRemembered(`ticket.${account.id}.leverage`, 3, (v): v is number => isInteger(v) && v >= 1 && v <= 20);
  const [stop, setStop] = useState('');
  const [target, setTarget] = useState('');
  const [reduceOnly, setReduceOnly] = useState(false);
  const [positionId, setPositionId] = useState('');
  const [reduceQuantity, setReduceQuantity] = useState('');
  const exchange = account.venue !== 'PAPER';
  const pendingKey = `mochiprep.pending-reduction.${account.id}`;
  const [pendingReductions, setPendingReductions] = useState(() =>
    exchange ? readPendingReductions(pendingKey, account.id) : []);
  const [confirmation, setConfirmation] = useState<OrderConfirmation | null>(() => {
    const [pending] = pendingReductions;
    return pending ? { ...pending, retry: true } : null;
  });
  const [dialogOpen, setDialogOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  useEffect(() => {
    if (!exchange) return;
    const updateFromOtherWindow = (event: StorageEvent) => {
      if (event.key && event.key !== pendingKey && !event.key.startsWith(`${pendingKey}/`)) return;
      const pending = readPendingReductions(pendingKey, account.id);
      setPendingReductions(pending);
      setConfirmation((current) => {
        if (submitting || (current && (!current.retry || dialogOpen))) return current;
        const next = pending.find((item) => item.request.clientOrderId === current?.request.clientOrderId) ?? pending[0];
        return next ? { ...next, retry: true } : null;
      });
    };
    window.addEventListener('storage', updateFromOtherWindow);
    return () => window.removeEventListener('storage', updateFromOtherWindow);
  }, [account.id, dialogOpen, exchange, pendingKey, submitting]);

  const boundReduction = exchange && reduceOnly;
  const reductionLimit = boundReduction && type === 'LIMIT';
  const reducible = positions.filter((position) => position.accountId === account.id && position.symbol === symbol && position.closeSupported === true);
  const selected = reducible.find((position) => position.id === positionId);
  const orderSide = boundReduction && selected ? (selected.side === 'LONG' ? 'SELL' : 'BUY') : side;
  const changeReduceOnly = (enabled: boolean) => {
    setReduceOnly(enabled);
    if (enabled && exchange) setPositionId(reducible.length === 1 ? reducible[0]!.id : '');
  };
  const instrument = INSTRUMENTS[symbol]!;
  const limitPrice = reductionLimit ? normalizeDecimal(limit) : limit;
  const reference = type === 'LIMIT' ? Number(limitPrice) : Number(orderSide === 'BUY' ? quote?.ask : quote?.bid);
  const step = Number(instrument.quantityStep);
  const rawQty = mode === 'QTY' ? Number(amount) : (Number(amount) * leverage) / (reference || Infinity);
  const quantity = boundReduction ? Number(reduceQuantity) : Number((Math.floor(rawQty / step + 1e-9) * step).toFixed(8));
  const notional = quantity * reference;
  const margin = notional / leverage;
  const feeRate = Number(type === 'LIMIT' ? account.config.makerFeeRate : account.config.takerFeeRate);
  const fee = notional * feeRate;
  const mmr = Number(instrument.maintenanceMarginRate);
  const liq = side === 'BUY' ? reference * (1 - 1 / leverage + mmr) : reference * (1 + 1 / leverage - mmr);
  const reductionSize = normalizeDecimal(reduceQuantity);
  const validStep = !boundReduction || matchesStep(reductionSize, instrument.quantityStep);
  const invalidLimitTick = reductionLimit && !!limitPrice && !matchesStep(limitPrice, instrument.priceTick);
  const valid = Number.isFinite(quantity) && quantity >= Number(instrument.minQuantity)
    && (boundReduction
      ? !!selected && quantity <= Number(selected.quantity)
        && validStep && (type === 'MARKET' || (Number.isFinite(reference) && reference > 0 && !invalidLimitTick))
      : reference > 0 && !!quote);
  const displayedQuantity = boundReduction ? reductionSize : qty(quantity);
  const canConfirm = confirmation !== null && !submitting && (confirmation.retry || (
    valid && confirmation.request.symbol === symbol && (!boundReduction || confirmation.request.positionId === selected?.id)
  ));

  const refreshPendingReductions = () => {
    const pending = exchange ? readPendingReductions(pendingKey, account.id) : [];
    setPendingReductions(pending);
    return pending;
  };

  const review = () => {
    const [pending] = refreshPendingReductions();
    if (!confirmation?.retry && pending) {
      setConfirmation({ ...pending, retry: true });
    } else if (!confirmation?.retry) {
      setConfirmation({
        request: {
          accountId: account.id,
          clientOrderId: `ui:${crypto.randomUUID()}`,
          ...(boundReduction && selected ? { positionId: selected.id } : {}),
          symbol,
          side: orderSide,
          type,
          quantity: boundReduction ? reductionSize : String(quantity),
          price: type === 'LIMIT' ? limitPrice : null,
          leverage,
          reduceOnly,
          stopLoss: !reduceOnly && stop.trim() ? stop.trim() : null,
          takeProfit: !reduceOnly && target.trim() ? target.trim() : null,
        },
        retry: false,
        summary: [
          ['Type', `${type}${reduceOnly ? ' · reduce only' : ''}`],
          ['Quantity', `${displayedQuantity} ${instrument.baseCurrency}`],
          ...(boundReduction && selected ? [['Position', `${selected.side} · Isolated`] as [string, string]] : []),
          [reductionLimit ? 'Limit price' : 'Reference price', reductionLimit ? limitPrice : price(reference, symbol)],
          ['Notional', `${money(notional)} USDT`],
          ['Margin', reduceOnly ? '–' : `${money(margin)} USDT at ${leverage}x`],
          ['Estimated fee', `${money(fee, 4)} USDT`],
          ['Estimated liquidation', reduceOnly ? '–' : price(liq, symbol)],
          ['Stop / Target', reduceOnly ? '–' : `${stop || '–'} / ${target || '–'}`],
        ],
      });
    }
    setDialogOpen(true);
  };

  const closeDialog = () => {
    if (submitting) return;
    setDialogOpen(false);
    if (!confirmation?.retry) setConfirmation(null);
  };

  const finishConfirmation = () => {
    const [next] = refreshPendingReductions();
    setConfirmation(next ? { ...next, retry: true } : null);
    setDialogOpen(false);
  };

  const discardRetry = () => {
    if (!confirmation?.retry || submitting) return;
    persistPendingReduction(pendingKey, confirmation, true);
    finishConfirmation();
  };

  const submit = async () => {
    if (!confirmation || !canConfirm) return;
    const { request } = confirmation;
    let { lookupId } = confirmation;
    const selectedReduction = request.positionId !== undefined;
    if (selectedReduction && !lookupId) {
      lookupId = readPendingReductions(pendingKey, account.id)
        .find((item) => item.request.clientOrderId === request.clientOrderId)?.lookupId;
    }
    // Persist before sending so a reload or a new tab can use the same idempotency key.
    // Session storage ends with its tab, so it cannot recover a lost answer from another one.
    // A retry of a request saved before it was sent reuses its key, so it may still be
    // sent even if storage became unavailable since.
    const stored = selectedReduction && (confirmation.durable === true
      || readPendingReductions(pendingKey, account.id)
        .some((item) => item.request.clientOrderId === request.clientOrderId));
    let durable = confirmation.durable === true;
    if (selectedReduction && !lookupId && !stored) {
      durable = persistPendingReduction(pendingKey, { request }) === 'localStorage';
      if (!durable) {
        // Only the entry this attempt just wrote is removed: nothing was sent under it.
        persistPendingReduction(pendingKey, { request }, true);
        useApp.getState().toast('error', 'Browser storage is required to confirm this reduction.');
        return;
      }
    }
    setSubmitting(true);
    let notDispatched = false;
    const result = await attempt(async () => {
      try {
        const order = lookupId
          ? await api.get<Order>(`/orders/${encodeURIComponent(lookupId)}`)
          : (await api.post<OrderResult>('/orders', request))?.order;
        if (!order?.status || (selectedReduction && order.clientOrderId !== request.clientOrderId)) {
          throw new Error('The order response was incomplete.');
        }
        return order;
      } catch (error) {
        notDispatched = !lookupId && isNotDispatched(error);
        throw error;
      }
    });
    setSubmitting(false);
    // Refused before reaching OKX: drop the key and let the user correct the ticket.
    if (selectedReduction && notDispatched) {
      persistPendingReduction(pendingKey, confirmation, true);
      finishConfirmation();
      return;
    }
    // An open order without an exchange ID may still have reached OKX, and a local expiry
    // is not an OKX cancellation: the server keeps looking it up.
    if (selectedReduction && (!result || (result.status === 'OPEN' && !result.exchangeOrderId) || isUnconfirmedExpiry(result))) {
      const pending = { request, lookupId: result?.id ?? lookupId };
      if (result) persistPendingReduction(pendingKey, pending);
      const saved = refreshPendingReductions()
        .find((item) => item.request.clientOrderId === request.clientOrderId);
      setConfirmation({ ...pending, lookupId: saved?.lookupId ?? pending.lookupId, retry: true, durable: durable || stored || undefined });
      return;
    }
    if (selectedReduction) persistPendingReduction(pendingKey, confirmation, true);
    finishConfirmation();
    if (result) {
      const order = result;
      const toast = useApp.getState().toast;
      if (order.status === 'REJECTED') toast('error', `Order rejected: ${order.rejectReason}`);
      else if (order.status === 'CANCELED') toast('info', 'Order canceled');
      else toast('info', order.status === 'FILLED' ? `Filled ${order.quantity} @ ${order.avgFillPrice}` : 'Limit order is resting');
      if (order.status === 'FILLED') useApp.setState({ lastFillAt: Date.now() });
    }
  };

  return (
    <Card className="space-y-3 p-4" data-testid="order-ticket">
      {!boundReduction && <div className="grid grid-cols-2 gap-1 rounded-xl bg-surface-2 p-1">
        <Button size="sm" variant={side === 'BUY' ? 'long' : 'ghost'} aria-pressed={side === 'BUY'} onClick={() => setSide('BUY')}>Long</Button>
        <Button size="sm" variant={side === 'SELL' ? 'short' : 'ghost'} aria-pressed={side === 'SELL'} onClick={() => setSide('SELL')}>Short</Button>
      </div>}
      <div className="flex items-center justify-between gap-2">
        <Segmented label="Order type" value={type} onChange={setType} options={[{ value: 'MARKET', label: 'Market' }, { value: 'LIMIT', label: 'Limit' }]} />
        {!boundReduction && <Segmented label="Size unit" value={mode} onChange={setMode} options={[{ value: 'MARGIN', label: 'Margin' }, { value: 'QTY', label: 'Quantity' }]} />}
      </div>
      {type === 'LIMIT' && (
        <Field label="Limit price">
          <Input inputMode="decimal" aria-label="Limit price" aria-invalid={invalidLimitTick || undefined} value={limit} onChange={(e) => setLimit(e.target.value)} placeholder={quote?.last} />
          {invalidLimitTick && <span className="mt-1 block text-xs text-warn">Limit price must be a multiple of {instrument.priceTick} {instrument.quoteCurrency}.</span>}
        </Field>
      )}
      {boundReduction && (
        <Field label="Position to reduce">
          <Select aria-label="Position to reduce" value={selected ? positionId : ''} onChange={(e) => setPositionId(e.target.value)}>
            <option value="">{reducible.length ? 'Select a position' : 'No isolated position for this symbol'}</option>
            {reducible.map((position) => (
              <option key={position.id} value={position.id}>{position.side} · {position.quantity} {instrument.baseCurrency} · Isolated</option>
            ))}
          </Select>
        </Field>
      )}
      <Field label={!boundReduction && mode === 'MARGIN' ? 'Margin (USDT)' : `Quantity (${instrument.baseCurrency})`}>
        <Input inputMode="decimal" aria-label="Order size" value={boundReduction ? reduceQuantity : amount} onChange={(e) => (boundReduction ? setReduceQuantity : setAmount)(e.target.value)} />
      </Field>
      {boundReduction && reductionSize && !validStep && (
        <p className="text-xs text-warn">Enter a multiple of {instrument.quantityStep} {instrument.baseCurrency}.</p>
      )}
      {!boundReduction && <Field label={`Leverage: ${leverage}x`}>
        <input aria-label="Leverage" type="range" min={1} max={20} value={leverage} onChange={(e) => setLeverage(Number(e.target.value))} className="w-full accent-[var(--accent)]" />
      </Field>}
      {!reduceOnly && (
        <div className="grid grid-cols-2 gap-2">
          <Field label="Stop loss"><Input inputMode="decimal" value={stop} onChange={(e) => setStop(e.target.value)} placeholder="Optional" /></Field>
          <Field label="Take profit"><Input inputMode="decimal" value={target} onChange={(e) => setTarget(e.target.value)} placeholder="Optional" /></Field>
        </div>
      )}
      <label className="flex items-center gap-2 text-xs text-ink-2">
        <input type="checkbox" checked={reduceOnly} onChange={(e) => changeReduceOnly(e.target.checked)} className="accent-[var(--accent)]" /> Reduce only
      </label>
      <div className="space-y-0.5 rounded-xl bg-surface-2 p-2.5 text-xs">
        <div className="flex justify-between"><span className="text-ink-3">Quantity</span><span className="num">{valid ? `${displayedQuantity} ${instrument.baseCurrency}` : '–'}</span></div>
        <div className="flex justify-between"><span className="text-ink-3">Notional</span><span className="num">{valid ? money(notional) : '–'}</span></div>
        <div className="flex justify-between"><span className="text-ink-3">Available</span><span className="num">{money(account.balance.availableBalance)}</span></div>
      </div>
      <Button className="w-full" size="lg" variant={(confirmation?.request.side ?? orderSide) === 'BUY' ? 'long' : 'short'} disabled={!confirmation?.retry && !valid} onClick={review}>
        {confirmation?.retry ? 'Review pending reduction' : boundReduction ? 'Review reduction' : `Review ${side === 'BUY' ? 'long' : 'short'}`}
      </Button>
      <Dialog
        open={dialogOpen}
        onOpenChange={(open) => !open && closeDialog()}
        title={`${confirmation?.retry ? 'Pending' : 'Confirm'} ${confirmation?.request.positionId ? 'reduction' : confirmation?.request.side === 'BUY' ? 'long' : 'short'} ${confirmation?.request.symbol ?? symbol}`}
        description={account.venue === 'PAPER' ? 'Simulated order. Estimates only: the engine fills against the live quote, with spread and slippage.' : 'Sent to OKX. Estimates only: the exchange decides the fill.'}
      >
        {account.venue === 'OKX_LIVE' && (
          <p className="mb-3 rounded-xl bg-down-soft px-3 py-2 text-[13px] font-semibold text-down" data-testid="real-order-warning">
            This is a real order with real money. It cannot be undone.
          </p>
        )}
        {confirmation?.retry && (
          <p role="status" className="mb-3 text-sm text-warn">{confirmation.lookupId
            ? 'OKX has yet to confirm this order. Check its status or review Order history before discarding it.'
            : 'The order outcome is unclear. Retry with the same order ID, or check Order history before discarding the retry.'}</p>
        )}
        {confirmation?.retry && pendingReductions.length > 1 && (
          <Field label="Pending reduction">
            <Select
              aria-label="Pending reduction"
              disabled={submitting}
              value={confirmation.request.clientOrderId}
              onChange={(event) => {
                const pending = pendingReductions.find((item) => item.request.clientOrderId === event.target.value);
                if (pending) setConfirmation({ ...pending, retry: true });
              }}
            >
              {pendingReductions.map(({ request }) => (
                <option key={request.clientOrderId} value={request.clientOrderId}>
                  {request.symbol} · {request.type} · {request.quantity}{request.price ? ` @ ${request.price}` : ''}
                </option>
              ))}
            </Select>
          </Field>
        )}
        <dl className="num space-y-1 text-[13px]">
          {confirmation && (confirmation.summary ?? reductionSummary(confirmation.request)).map(([k, v]) => (
            <div key={k} className="flex justify-between gap-4"><dt className="text-ink-3">{k}</dt><dd>{v}</dd></div>
          ))}
        </dl>
        <div className="mt-4 flex justify-end gap-2">
          {confirmation?.retry
            ? <Button disabled={submitting} onClick={discardRetry}>Discard retry</Button>
            : <Button disabled={submitting} onClick={closeDialog}>Cancel</Button>}
          <Button variant={confirmation?.request.side === 'BUY' ? 'long' : 'short'} disabled={!canConfirm} onClick={submit}>
            {submitting ? 'Submitting…' : confirmation?.lookupId ? 'Check order status' : confirmation?.retry ? 'Retry reduction' : account.venue === 'OKX_LIVE' ? 'Place real order' : 'Place order'}
          </Button>
        </div>
      </Dialog>
    </Card>
  );
}

const TRADING_TABS = ['positions', 'orders', 'fills', 'closed', 'all', 'ledger'] as const;

export function PaperPage() {
  const account = useCurrentAccount();
  const source = useApp((s) => s.source);
  const symbols = useSymbols();
  // A remembered pair that was removed since falls back to BTC.
  const [picked, setSymbol] = useRemembered<SymbolId>('trading.symbol', 'BTCUSDT', isString);
  const symbol = symbols.includes(picked) ? picked : 'BTCUSDT';
  const [timeframe, setTimeframe] = useRemembered<Timeframe>('trading.timeframe', '5m', oneOf(TIMEFRAMES));
  const [rememberedTab, setTab] = useRemembered('trading.tab', 'positions', oneOf(TRADING_TABS));
  const quote = useApp((s) => s.quotes[source][symbol]);
  const { data: candles } = useCandles(symbol, timeframe, 300);
  const id = account?.id;
  const overlay = useTradeOverlay(id, symbol, timeframe, quote);

  const { data: positions } = useQuery(() => (id ? api.get<PositionView[]>(`/accounts/${id}/positions`) : Promise.resolve([])), [id], ['positions']);
  const { data: orders } = useQuery(() => (id ? api.get<Order[]>(`/accounts/${id}/orders?limit=200`) : Promise.resolve([])), [id], ['orders']);
  const { data: fills } = useQuery(() => (id ? api.get<Fill[]>(`/accounts/${id}/fills?limit=200`) : Promise.resolve([])), [id], ['orders']);
  const { data: closed } = useQuery(() => (id ? api.get<Position[]>(`/accounts/${id}/closed-positions?limit=200`) : Promise.resolve([])), [id], ['positions']);
  const { data: ledger } = useQuery(() => (id ? api.get<LedgerEvent[]>(`/accounts/${id}/ledger?limit=300`) : Promise.resolve([])), [id], ['orders', 'positions']);

  const open = (orders ?? []).filter((o) => o.status === 'OPEN');
  const actions = (
    <div className="flex flex-wrap items-center gap-2">
      <AccountPicker />
      <CreateAccount />
      {account?.venue === 'PAPER' && <ResetAccount account={account} />}
    </div>
  );

  if (!account) {
    return (
      <>
        <PageTitle title="Trading" action={actions} />
        <Card className="px-6 py-10 text-center text-[13px] text-ink-3">Create a {source === 'LIVE' ? 'live-data' : 'replay'} paper account to start trading.</Card>
      </>
    );
  }

  const b = account.balance;
  const exchange = account.venue !== 'PAPER';
  // An exchange account has no ledger tab.
  const tab = exchange && rememberedTab === 'ledger' ? 'positions' : rememberedTab;
  return (
    <>
      <PageTitle
        title={exchange ? 'OKX Trading' : 'Paper Trading'}
        description={exchange ? 'Orders on this account go to OKX. Balances and positions come from the exchange.' : 'Everything here is simulated locally. No order on this account reaches an exchange.'}
        action={actions}
      />
      {account.venue === 'OKX_LIVE' && (
        <div className="mb-3 flex items-center gap-2 rounded-2xl bg-down-soft px-4 py-2.5 text-[13px] font-semibold text-down" data-testid="real-money-banner">
          <TriangleAlert className="size-4 shrink-0" />
          Real money. Orders here, and from any agent on this account, are placed on your OKX account.
        </div>
      )}
      <div className="mb-3 empty:hidden"><SignalsCard /></div>
      <div className="mb-3 grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-6">
        <Stat label="Equity" value={<span data-testid="account-equity">{money(b.equity)}</span>} />
        <Stat label="Wallet" value={money(b.walletBalance)} />
        <Stat label="Available" value={money(b.availableBalance)} sub={Number(b.orderMargin) > 0 ? `${money(b.orderMargin)} held by orders` : undefined} />
        <Stat label="Used margin" value={money(b.usedMargin)} />
        <Stat label="Realized · Unrealized" value={<span className="text-sm"><span className={pnlClass(b.realizedPnl)}>{signedMoney(b.realizedPnl)}</span> · <span className={pnlClass(b.unrealizedPnl)}>{signedMoney(b.unrealizedPnl)}</span></span>} />
        <Stat label="Fees · Funding" value={<span className="text-sm">{money(b.feesPaid)} · {signedMoney(Number(b.fundingPaid) * -1)}</span>} sub="Funding: + received, − paid" />
      </div>
      <div className="grid gap-3 xl:grid-cols-[1fr_320px]">
        <Card className="p-3">
          <div className="mb-2 flex items-center gap-3 px-2">
            <Select aria-label="Trading symbol" className="w-36" value={symbol} onChange={(e) => setSymbol(e.target.value)}>
              {symbols.map((s) => <option key={s} value={s}>{s}</option>)}
            </Select>
            <span className="num text-lg font-semibold">{price(quote?.last, symbol)}</span>
            <span className="num text-xs text-ink-3">{price(quote?.bid, symbol)} / {price(quote?.ask, symbol)}</span>
            {source === 'REPLAY' ? <Badge tone="info">Synthetic</Badge> : exchange ? <VenueBadge venue={account.venue} /> : <Badge tone="accent">Live prices, paper fills</Badge>}
            <span className="ml-auto"><Segmented label="Timeframe" value={timeframe} onChange={setTimeframe} options={TIMEFRAMES.map((t) => ({ value: t, label: t }))} /></span>
          </div>
          <div className="mb-1 px-2"><ChartTradeSummary summary={overlay.summary} /></div>
          <div className="h-[470px]"><MarketChart candles={candles ?? []} symbol={symbol} timeframe={timeframe} quote={quote} showIndicators={false} priceLines={overlay.lines} markers={overlay.markers} notes={overlay.notes} /></div>
        </Card>
        <OrderTicket key={account.id} account={account} symbol={symbol} positions={positions ?? []} />
      </div>
      <Card className="mt-3">
        <Tabs
          value={tab}
          onChange={setTab}
          tabs={[
            { id: 'positions', label: `Positions (${positions?.length ?? 0})` },
            { id: 'orders', label: `Pending orders (${open.length})` },
            { id: 'fills', label: 'Fills' },
            { id: 'closed', label: 'Closed positions' },
            { id: 'all', label: 'Order history' },
            ...(exchange ? [] : [{ id: 'ledger', label: 'Ledger' }]),
          ]}
        >
          {tab === 'positions' && <PositionsTable positions={positions ?? []} editable={!exchange} />}
          {tab === 'orders' && <OrdersTable orders={open} exchange={exchange} cancellable emptyText="No pending orders." />}
          {tab === 'fills' && <FillsTable fills={fills ?? []} />}
          {tab === 'closed' && <ClosedPositionsTable positions={closed ?? []} />}
          {tab === 'all' && <OrdersTable orders={orders ?? []} exchange={exchange} emptyText="No orders yet." />}
          {tab === 'ledger' && !exchange && <LedgerTable events={ledger ?? []} />}
        </Tabs>
      </Card>
    </>
  );
}
