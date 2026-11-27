import { writeRemembered } from '@/lib/remembered';
import { ArrowRight } from 'lucide-react';
import { useMemo } from 'react';
import type { EquityPoint, Fill, PositionView } from '@mochiprep/shared';
import { EquityChart } from '@/components/charts';
import { Mochi } from '@/components/Mochi';
import { PageTitle } from '@/components/Shell';
import { SignalsCard } from '@/components/SignalsCard';
import { AccountPicker, DecisionList, FillsTable, PositionsTable } from '@/components/trading';
import { Button, Card, CardHeader, EmptyState, Stat, StatStrip } from '@/components/ui';
import { api } from '@/lib/api';
import { money, pct, pnlClass, signedMoney } from '@/lib/format';
import { useApp, useQuery, useSourceAccounts } from '@/store';

function Welcome() {
  const setPage = useApp((s) => s.setPage);
  const steps = [
    { title: 'Add an AI provider key', detail: 'Settings → AI providers. Keys stay on this computer.', page: 'settings', action: 'Open settings', testid: 'add-ai-key' },
    { title: 'Create an agent', detail: 'Pick a model, pairs and risk limits. It trades a paper account until you connect OKX.', page: 'agents', action: 'Create an agent' },
    { title: 'Watch it trade', detail: 'Positions, P&L and every decision show up here and in Trade History.', page: 'paper', action: 'Open trading' },
  ] as const;
  return (
    <Card className="mx-auto mt-6 max-w-2xl">
      <div className="flex items-center gap-3 border-b border-line px-5 py-4">
        <Mochi mood="sleeping" size={36} />
        <div>
          <h1 className="text-base font-semibold">Get started</h1>
          <p className="text-xs text-ink-3">AI agents for USDT perpetuals, on paper or on your OKX account.</p>
        </div>
      </div>
      <ol className="divide-y divide-line">
        {steps.map((step, i) => (
          <li key={step.title} className="flex items-center gap-4 px-5 py-3.5">
            <span className="num grid size-6 shrink-0 place-content-center rounded-full border border-line text-xs text-ink-2">{i + 1}</span>
            <div className="min-w-0 flex-1">
              <div className="text-[13px] font-medium">{step.title}</div>
              <div className="text-xs text-ink-3">{step.detail}</div>
            </div>
            <Button size="sm" variant={i === 0 ? 'primary' : 'secondary'} onClick={() => setPage(step.page)} data-testid={'testid' in step ? step.testid : undefined}>
              {step.action} <ArrowRight />
            </Button>
          </li>
        ))}
      </ol>
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
  const accountsLoaded = useApp((s) => s.versions.accounts > 0);
  const source = useApp((s) => s.source);
  const accountId = useApp((s) => s.accountId);
  const agents = useApp((s) => s.agents);
  const decisions = useApp((s) => s.decisions);
  const setPage = useApp((s) => s.setPage);

  // The picked account can vanish under us (deleted elsewhere), so an unknown id means all accounts.
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

  // Until the first load, "no accounts" only means "not fetched yet".
  if (!accountsLoaded) return null;
  if (allAccounts.length === 0) return <Welcome />;

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
        description={`${scope.length} ${source === 'REPLAY' ? 'replay' : 'live-data'} account${scope.length === 1 ? '' : 's'}. Paper numbers come from the local ledger, exchange numbers from OKX.`}
        action={<AccountPicker allowAll />}
      />
      {scope.length === 0 ? (
        <EmptyState
          title={`No ${source === 'LIVE' ? 'live-data' : 'replay'} accounts yet`}
          action={<Button onClick={() => setPage('paper')}>Create one in Trading <ArrowRight /></Button>}
        />
      ) : (
        <div className="space-y-4">
          <SignalsCard />
          <StatStrip className="md:grid-cols-6">
            <Stat label="Total equity" value={<span data-testid="total-equity">{money(equity)}</span>} sub={<span className={pnlClass(equity - initial)}>{pct(initial ? ((equity - initial) / initial) * 100 : 0)}</span>} />
            <Stat label="Wallet balance" value={money(sum((a) => a.balance.walletBalance))} sub="USDT" />
            <Stat label="Available" value={money(sum((a) => a.balance.availableBalance))} sub={`${money(sum((a) => a.balance.usedMargin))} in margin`} />
            <Stat label="Realized PnL" value={<span className={pnlClass(realized)}>{signedMoney(realized)}</span>} sub={`Fees ${money(sum((a) => a.balance.feesPaid))}`} />
            <Stat label="Unrealized PnL" value={<span className={pnlClass(unrealized)}>{signedMoney(unrealized)}</span>} sub={`${positions?.length ?? 0} open`} />
            <Stat label="Active agents" value={active.length} sub={`of ${agents.filter((a) => scopeIds.has(a.accountId)).length}`} />
          </StatStrip>

          <div className="grid gap-4 xl:grid-cols-3">
            <Card className="xl:col-span-2">
              <CardHeader title="Performance" description="Equity in USDT. The dashed line is starting capital." />
              <div className="h-72 pb-3">
                <EquityChart series={series} baseline={initial} />
              </div>
            </Card>
            <Card className="flex max-h-[376px] flex-col">
              <CardHeader title="Latest AI decisions" action={<Button size="sm" variant="ghost" onClick={() => { writeRemembered('history.outcome', ''); setPage('history'); }}>All</Button>} />
              <div className="min-h-0 flex-1 overflow-y-auto">
                <DecisionList
                  decisions={decisions.filter((d) => scopeIds.has(d.accountId)).slice(0, 12)}
                  onSelect={(decision) => {
                    writeRemembered('history.outcome', '');
                    setPage('history', decision.id);
                  }}
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
