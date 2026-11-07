import { Plus, RotateCcw, TriangleAlert } from 'lucide-react';
import { useMemo, useState } from 'react';
import { INSTRUMENTS, type Fill, type LedgerEvent, type Order, type Position, type PositionView, type SymbolId, type Timeframe } from '@mochiprep/shared';
import { MarketChart, type PriceLineSpec } from '@/components/charts';
import { PageTitle } from '@/components/Shell';
import { AccountPicker, ClosedPositionsTable, FillsTable, LedgerTable, OrdersTable, PositionsTable, useCurrentAccount, VenueBadge } from '@/components/trading';
import { Badge, Button, Card, Dialog, Field, Input, Segmented, Select, Stat, Tabs } from '@/components/ui';
import { api } from '@/lib/api';
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

function OrderTicket({ account, symbol }: { account: AccountView; symbol: SymbolId }) {
  const quote = useApp((s) => s.quotes[account.source][symbol]);
  const [side, setSide] = useState<'BUY' | 'SELL'>('BUY');
  const [type, setType] = useState<'MARKET' | 'LIMIT'>('MARKET');
  const [mode, setMode] = useState<'QTY' | 'MARGIN'>('MARGIN');
  const [amount, setAmount] = useState('500');
  const [limit, setLimit] = useState('');
  const [leverage, setLeverage] = useState(3);
  const [stop, setStop] = useState('');
  const [target, setTarget] = useState('');
  const [reduceOnly, setReduceOnly] = useState(false);
  const [confirmId, setConfirmId] = useState<string | null>(null);

  const instrument = INSTRUMENTS[symbol]!;
  const reference = type === 'LIMIT' ? Number(limit) : Number(side === 'BUY' ? quote?.ask : quote?.bid);
  const step = Number(instrument.quantityStep);
  const rawQty = mode === 'QTY' ? Number(amount) : (Number(amount) * leverage) / (reference || Infinity);
  const quantity = Number((Math.floor(rawQty / step + 1e-9) * step).toFixed(8));
  const notional = quantity * reference;
  const margin = notional / leverage;
  const feeRate = Number(type === 'LIMIT' ? account.config.makerFeeRate : account.config.takerFeeRate);
  const fee = notional * feeRate;
  const mmr = Number(instrument.maintenanceMarginRate);
  const liq = side === 'BUY' ? reference * (1 - 1 / leverage + mmr) : reference * (1 + 1 / leverage - mmr);
  const valid = quantity >= Number(instrument.minQuantity) && reference > 0;

  const submit = async () => {
    const result = await attempt(() =>
      api.post<{ order: Order }>('/orders', {
        accountId: account.id,
        clientOrderId: confirmId!,
        symbol,
        side,
        type,
        quantity: String(quantity),
        price: type === 'LIMIT' ? limit : null,
        leverage,
        reduceOnly,
        stopLoss: !reduceOnly && stop.trim() ? stop.trim() : null,
        takeProfit: !reduceOnly && target.trim() ? target.trim() : null,
      }),
    );
    setConfirmId(null);
    if (result) {
      const { order } = result;
      const toast = useApp.getState().toast;
      if (order.status === 'REJECTED') toast('error', `Order rejected: ${order.rejectReason}`);
      else toast('info', order.status === 'FILLED' ? `Filled ${order.quantity} @ ${order.avgFillPrice}` : 'Limit order is resting');
      if (order.status === 'FILLED') useApp.setState({ lastFillAt: Date.now() });
    }
  };

  return (
    <Card className="space-y-3 p-4" data-testid="order-ticket">
      <div className="grid grid-cols-2 gap-1 rounded-xl bg-surface-2 p-1">
        <Button size="sm" variant={side === 'BUY' ? 'long' : 'ghost'} aria-pressed={side === 'BUY'} onClick={() => setSide('BUY')}>Long</Button>
        <Button size="sm" variant={side === 'SELL' ? 'short' : 'ghost'} aria-pressed={side === 'SELL'} onClick={() => setSide('SELL')}>Short</Button>
      </div>
      <div className="flex items-center justify-between gap-2">
        <Segmented label="Order type" value={type} onChange={setType} options={[{ value: 'MARKET', label: 'Market' }, { value: 'LIMIT', label: 'Limit' }]} />
        <Segmented label="Size unit" value={mode} onChange={setMode} options={[{ value: 'MARGIN', label: 'Margin' }, { value: 'QTY', label: 'Quantity' }]} />
      </div>
      {type === 'LIMIT' && (
        <Field label="Limit price"><Input inputMode="decimal" value={limit} onChange={(e) => setLimit(e.target.value)} placeholder={quote?.last} /></Field>
      )}
      <Field label={mode === 'MARGIN' ? 'Margin (USDT)' : `Quantity (${instrument.baseCurrency})`}>
        <Input inputMode="decimal" aria-label="Order size" value={amount} onChange={(e) => setAmount(e.target.value)} />
      </Field>
      <Field label={`Leverage: ${leverage}x`}>
        <input aria-label="Leverage" type="range" min={1} max={20} value={leverage} onChange={(e) => setLeverage(Number(e.target.value))} className="w-full accent-[var(--accent)]" />
      </Field>
      {!reduceOnly && (
        <div className="grid grid-cols-2 gap-2">
          <Field label="Stop loss"><Input inputMode="decimal" value={stop} onChange={(e) => setStop(e.target.value)} placeholder="Optional" /></Field>
          <Field label="Take profit"><Input inputMode="decimal" value={target} onChange={(e) => setTarget(e.target.value)} placeholder="Optional" /></Field>
        </div>
      )}
      <label className="flex items-center gap-2 text-xs text-ink-2">
        <input type="checkbox" checked={reduceOnly} onChange={(e) => setReduceOnly(e.target.checked)} className="accent-[var(--accent)]" /> Reduce only
      </label>
      <div className="space-y-0.5 rounded-xl bg-surface-2 p-2.5 text-xs">
        <div className="flex justify-between"><span className="text-ink-3">Quantity</span><span className="num">{valid ? `${qty(quantity)} ${instrument.baseCurrency}` : '–'}</span></div>
        <div className="flex justify-between"><span className="text-ink-3">Notional</span><span className="num">{valid ? money(notional) : '–'}</span></div>
        <div className="flex justify-between"><span className="text-ink-3">Available</span><span className="num">{money(account.balance.availableBalance)}</span></div>
      </div>
      <Button className="w-full" size="lg" variant={side === 'BUY' ? 'long' : 'short'} disabled={!valid || !quote} onClick={() => setConfirmId(`ui:${crypto.randomUUID()}`)}>
        Review {side === 'BUY' ? 'long' : 'short'}
      </Button>
      <Dialog
        open={confirmId !== null}
        onOpenChange={(open) => !open && setConfirmId(null)}
        title={`Confirm ${side === 'BUY' ? 'long' : 'short'} ${symbol}`}
        description={account.venue === 'PAPER' ? 'Simulated order. Estimates only: the engine fills against the live quote, with spread and slippage.' : 'Sent to OKX. Estimates only: the exchange decides the fill.'}
      >
        {account.venue === 'OKX_LIVE' && (
          <p className="mb-3 rounded-xl bg-down-soft px-3 py-2 text-[13px] font-semibold text-down" data-testid="real-order-warning">
            This is a real order with real money. It cannot be undone.
          </p>
        )}
        <dl className="num space-y-1 text-[13px]">
          {[
            ['Type', `${type}${reduceOnly ? ' · reduce only' : ''}`],
            ['Quantity', `${qty(quantity)} ${instrument.baseCurrency}`],
            ['Reference price', price(reference, symbol)],
            ['Notional', `${money(notional)} USDT`],
            ['Margin', reduceOnly ? '–' : `${money(margin)} USDT at ${leverage}x`],
            ['Estimated fee', `${money(fee, 4)} USDT`],
            ['Estimated liquidation', reduceOnly ? '–' : price(liq, symbol)],
            ['Stop / Target', `${stop || '–'} / ${target || '–'}`],
          ].map(([k, v]) => (
            <div key={k} className="flex justify-between gap-4"><dt className="text-ink-3">{k}</dt><dd>{v}</dd></div>
          ))}
        </dl>
        <div className="mt-4 flex justify-end gap-2">
          <Button onClick={() => setConfirmId(null)}>Cancel</Button>
          <Button variant={side === 'BUY' ? 'long' : 'short'} onClick={submit}>{account.venue === 'OKX_LIVE' ? 'Place real order' : 'Place order'}</Button>
        </div>
      </Dialog>
    </Card>
  );
}

export function PaperPage() {
  const account = useCurrentAccount();
  const source = useApp((s) => s.source);
  const symbols = useSymbols();
  const [picked, setSymbol] = useState<SymbolId>('BTCUSDT');
  const symbol = symbols.includes(picked) ? picked : 'BTCUSDT';
  const [timeframe] = useState<Timeframe>('5m');
  const [tab, setTab] = useState('positions');
  const quote = useApp((s) => s.quotes[source][symbol]);
  const { data: candles } = useCandles(symbol, timeframe, 300);
  const id = account?.id;

  const { data: positions } = useQuery(() => (id ? api.get<PositionView[]>(`/accounts/${id}/positions`) : Promise.resolve([])), [id], ['positions']);
  const { data: orders } = useQuery(() => (id ? api.get<Order[]>(`/accounts/${id}/orders?limit=200`) : Promise.resolve([])), [id], ['orders']);
  const { data: fills } = useQuery(() => (id ? api.get<Fill[]>(`/accounts/${id}/fills?limit=200`) : Promise.resolve([])), [id], ['orders']);
  const { data: closed } = useQuery(() => (id ? api.get<Position[]>(`/accounts/${id}/closed-positions?limit=200`) : Promise.resolve([])), [id], ['positions']);
  const { data: ledger } = useQuery(() => (id ? api.get<LedgerEvent[]>(`/accounts/${id}/ledger?limit=300`) : Promise.resolve([])), [id], ['orders', 'positions']);

  const open = (orders ?? []).filter((o) => o.status === 'OPEN');
  const lines = useMemo<PriceLineSpec[]>(
    () =>
      (positions ?? [])
        .filter((p) => p.symbol === symbol)
        .flatMap((p) => [
          { price: Number(p.entryPrice), label: 'Entry', tone: 'neutral' as const },
          ...(p.stopLoss ? [{ price: Number(p.stopLoss), label: 'Stop', tone: 'down' as const }] : []),
          ...(p.takeProfit ? [{ price: Number(p.takeProfit), label: 'Target', tone: 'up' as const }] : []),
        ]),
    [positions, symbol],
  );

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
        <PageTitle title="Paper Trading" action={actions} />
        <Card className="px-6 py-10 text-center text-[13px] text-ink-3">Create a {source === 'LIVE' ? 'live-data' : 'replay'} paper account to start trading.</Card>
      </>
    );
  }

  const b = account.balance;
  const exchange = account.venue !== 'PAPER';
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
          </div>
          <div className="h-[470px]"><MarketChart candles={candles ?? []} symbol={symbol} timeframe={timeframe} quote={quote} showIndicators={false} priceLines={lines} /></div>
        </Card>
        <OrderTicket account={account} symbol={symbol} />
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
          {tab === 'orders' && <OrdersTable orders={open} cancellable emptyText="No pending orders." />}
          {tab === 'fills' && <FillsTable fills={fills ?? []} />}
          {tab === 'closed' && <ClosedPositionsTable positions={closed ?? []} />}
          {tab === 'all' && <OrdersTable orders={orders ?? []} emptyText="No orders yet." />}
          {tab === 'ledger' && !exchange && <LedgerTable events={ledger ?? []} />}
        </Tabs>
      </Card>
    </>
  );
}
