import { ArrowRight, Sparkles } from 'lucide-react';
import { useMemo, useState } from 'react';
import type { EquityPoint, Fill, PositionView } from '@mochiprep/shared';
import { EquityChart } from '@/components/charts';
import { Mochi } from '@/components/Mochi';
import { PageTitle } from '@/components/Shell';
import { AccountPicker, DecisionList, FillsTable, PositionsTable } from '@/components/trading';
import { Button, Card, CardHeader, Stat } from '@/components/ui';
import { api } from '@/lib/api';
import { money, pct, pnlClass, signedMoney } from '@/lib/format';
import { attempt, useApp, useQuery, useSourceAccounts } from '@/store';

function Welcome() {
  const setPage = useApp((s) => s.setPage);
  const [starting, setStarting] = useState(false);
  const start = async () => {
    setStarting(true);
    await attempt(() => api.post('/demo/start'));
    await useApp.getState().refreshState();
    useApp.getState().setSource('REPLAY');
    setStarting(false);
  };
  return (
    <Card className="mx-auto mt-10 max-w-xl px-8 py-10 text-center">
      <Mochi mood="sleeping" size={110} className="mx-auto" />
      <h1 className="mt-4 font-display text-3xl font-black">Your cute little AI trading companion</h1>
      <p className="mx-auto mt-2 max-w-md text-[13px] leading-relaxed text-ink-2">
        Local-first paper trading for crypto perpetuals. Demo Mode needs no API keys and no internet: it loads synthetic data, three mock agents and 10,000 virtual USDT per account.
      </p>
      <div className="mt-6 flex flex-wrap justify-center gap-2">
        <Button variant="primary" size="lg" onClick={start} disabled={starting} data-testid="try-demo">
          <Sparkles /> Try Demo Mode
        </Button>
        <Button size="lg" onClick={() => setPage('settings')}>
          Add my AI key <ArrowRight />
        </Button>
      </div>
    </Card>
  );
}

/** Sums accounts at each timestamp, carrying every account's last value forward. */
function combine(curves: EquityPoint[][]): { ts: number; equity: number }[] {
  const times = [...new Set(curves.flat().map((p) => p.ts))].sort((a, b) => a - b);
  const cursors = curves.map(() => 0);
  // An account that does not exist yet counts at its opening equity, not as a jump from zero.
  const current = curves.map((curve) => Number(curve[0]?.equity ?? 0));
  return times.map((ts) => {
    curves.forEach((curve, i) => {
      while (cursors[i]! < curve.length && curve[cursors[i]!]!.ts <= ts) {
        current[i] = Number(curve[cursors[i]!]!.equity);
        cursors[i]!++;
      }
    });
    return { ts, equity: current.reduce((a, b) => a + b, 0) };
  });
}

export function DashboardPage() {
  const accounts = useSourceAccounts();
  const allAccounts = useApp((s) => s.accounts);
  const demoActive = useApp((s) => s.demoActive);
  const source = useApp((s) => s.source);
  const accountId = useApp((s) => s.accountId);
  const agents = useApp((s) => s.agents);
  const decisions = useApp((s) => s.decisions);
  const setPage = useApp((s) => s.setPage);

  // The picked account can vanish under us (demo reset), so an unknown id means all accounts.
  const picked = accounts.find((a) => a.id === accountId);
  const scope = picked ? [picked] : accounts;
  const ids = scope.map((a) => a.id).join(',');
  const names = useMemo(() => new Map(accounts.map((a) => [a.id, a.name])), [accounts]);

  const { data: curves } = useQuery(() => Promise.all(scope.map((a) => api.get<EquityPoint[]>(`/accounts/${a.id}/equity?limit=800`))), [ids], ['arena']);
  const { data: positions } = useQuery(() => Promise.all(scope.map((a) => api.get<PositionView[]>(`/accounts/${a.id}/positions`))).then((r) => r.flat()), [ids], ['positions']);
  const { data: fills } = useQuery(
    () => Promise.all(scope.map((a) => api.get<Fill[]>(`/accounts/${a.id}/fills?limit=8`))).then((r) => r.flat().sort((a, b) => b.ts - a.ts).slice(0, 8)),
    [ids],
    ['orders'],
  );

  const series = useMemo(() => [{ id: 'equity', name: 'Equity', color: '--series-1', points: combine(curves ?? []) }], [curves]);

  if (allAccounts.length === 0 && !demoActive) return <Welcome />;

  const sum = (pick: (a: (typeof scope)[number]) => string) => scope.reduce((total, a) => total + Number(pick(a)), 0);
  const initial = sum((a) => a.initialBalance);
  const equity = sum((a) => a.balance.equity);
  const realized = sum((a) => a.balance.realizedPnl);
  const unrealized = sum((a) => a.balance.unrealizedPnl);
  const scopeIds = new Set(scope.map((a) => a.id));
  const active = agents.filter((a) => a.running && scopeIds.has(a.accountId));

  return (
    <>
      <PageTitle
        title="Dashboard"
        description={`${scope.length} ${source === 'REPLAY' ? 'replay' : 'live-data'} paper account${scope.length === 1 ? '' : 's'}. Every number comes from the paper engine's ledger.`}
        action={<AccountPicker allowAll />}
      />
      {scope.length === 0 ? (
        <Card className="px-6 py-10 text-center text-[13px] text-ink-3">
          No {source === 'LIVE' ? 'live-data' : 'replay'} paper accounts yet.{' '}
          <button type="button" className="font-semibold text-accent" onClick={() => setPage('paper')}>Create one in Paper Trading</button>.
        </Card>
      ) : (
        <div className="space-y-4">
          <div className="grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-6">
            <Stat label="Total equity" value={<span data-testid="total-equity">{money(equity)}</span>} sub={<span className={pnlClass(equity - initial)}>{pct(initial ? ((equity - initial) / initial) * 100 : 0)}</span>} />
            <Stat label="Wallet balance" value={money(sum((a) => a.balance.walletBalance))} sub="USDT" />
            <Stat label="Available" value={money(sum((a) => a.balance.availableBalance))} sub={`${money(sum((a) => a.balance.usedMargin))} in margin`} />
            <Stat label="Realized PnL" value={<span className={pnlClass(realized)}>{signedMoney(realized)}</span>} sub={`Fees ${money(sum((a) => a.balance.feesPaid))}`} />
            <Stat label="Unrealized PnL" value={<span className={pnlClass(unrealized)}>{signedMoney(unrealized)}</span>} sub={`${positions?.length ?? 0} open`} />
            <Stat label="Active agents" value={active.length} sub={`of ${agents.filter((a) => scopeIds.has(a.accountId)).length}`} />
          </div>

          <div className="grid gap-4 xl:grid-cols-3">
            <Card className="xl:col-span-2">
              <CardHeader title="Performance" description="Equity in USDT. The dashed line is starting capital." />
              <div className="h-72 pb-3">
                <EquityChart series={series} baseline={initial} />
              </div>
            </Card>
            <Card className="flex max-h-[376px] flex-col">
              <CardHeader title="Latest AI decisions" action={<Button size="sm" variant="ghost" onClick={() => setPage('history')}>All</Button>} />
              <div className="min-h-0 flex-1 overflow-y-auto">
                <DecisionList
                  decisions={decisions.filter((d) => scopeIds.has(d.accountId)).slice(0, 12)}
                  onSelect={() => setPage('history')}
                  emptyText="No decisions yet. Start an agent, then press play."
                />
              </div>
            </Card>
          </div>

          <Card>
            <CardHeader title="Open positions" />
            <PositionsTable positions={positions ?? []} actions={false} accountNames={names} />
          </Card>
          <Card>
            <CardHeader title="Recent trades" />
            <FillsTable fills={fills ?? []} />
          </Card>
        </div>
      )}
    </>
  );
}
