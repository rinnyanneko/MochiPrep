import { ArrowRight, Info } from 'lucide-react';
import { useMemo } from 'react';
import type { ArenaRow } from '@mochiprep/shared';
import { EquityChart } from '@/components/charts';
import { PageTitle } from '@/components/Shell';
import { VenueBadge } from '@/components/trading';
import { Badge, Button, Card, CardHeader, EmptyState, Segmented, Table } from '@/components/ui';
import { ARENA_VIEWS, arenaView, compareArenaRows, type ArenaView } from '@/lib/arena';
import { money, pct, pnlClass, signedMoney } from '@/lib/format';
import { oneOf, useRemembered } from '@/lib/remembered';
import { cn } from '@/lib/utils';
import { useApp, useRecordedReplay } from '@/store';

const SERIES = ['--series-1', '--series-2', '--series-3', '--series-4', '--series-5', '--series-6'];
const CHARTED = SERIES.length;

const NOTES: Record<ArenaView, string> = {
  REPLAY: 'Replay results on recorded OKX bars. Every agent saw the same bars, so runs are comparable, but past results say nothing about what comes next.',
  LIVE: 'Live OKX prices, simulated paper accounts. Agents started at different times are not directly comparable.',
  OKX: "Agents trading on OKX accounts, demo or real money. P&L is the agent's own: deposits and transfers do not count, but manual trades that change its net position do. Agents sharing a pair on one account have no separate P&L.",
};

function chartNote(charted: number, okx: boolean): string {
  if (charted > CHARTED) return `First ${CHARTED} of ${charted} agents. USDT.`;
  if (okx) return "Each agent's own P&L since it was made, USDT. A position adds its result when it closes.";
  return 'Paper accounts, USDT, sampled on the backend.';
}

export function ArenaPage() {
  const arena = useApp((s) => s.arena);
  const globalSource = useApp((s) => s.source);
  const setPage = useApp((s) => s.setPage);
  const [view, setView] = useRemembered<ArenaView>('arena.view', globalSource, oneOf(ARENA_VIEWS));
  const rows = arena.filter((r) => arenaView(r) === view);
  const okx = view === 'OKX';
  const recorded = useRecordedReplay();

  // Paper rows chart their account's equity; OKX rows their own P&L, since deposits move equity.
  const charted = rows.filter((r) => (okx ? r.pnlCurve.length > 0 : r.attribution === 'ACCOUNT'));
  /** Color follows creation order among charted agents, so sorting never repaints one. */
  const colors = useMemo(() => new Map(charted.map((r, i) => [r.agentId, SERIES[i]])), [charted.map((r) => r.agentId).join()]);
  const ranked = [...rows].sort(compareArenaRows);
  const series = useMemo(
    () =>
      charted.slice(0, CHARTED).map((r) => ({
        id: r.agentId,
        name: r.agentName,
        color: colors.get(r.agentId)!,
        points: okx ? r.pnlCurve.map((p) => ({ ts: p.ts, equity: p.pnl })) : r.curve,
      })),
    [arena, view],
  );
  const capitals = new Set(charted.map((r) => Number(r.initialBalance)));

  return (
    <>
      <PageTitle
        title="Trading Arena"
        description="Agents ranked side by side: on the replay, on live prices with paper money, or on OKX."
        action={
          <Segmented
            label="Arena market"
            value={view}
            onChange={setView}
            options={[
              { value: 'REPLAY', label: 'Historical replay' },
              { value: 'LIVE', label: 'Live paper' },
              { value: 'OKX', label: 'OKX' },
            ]}
          />
        }
      />
      <p className="mb-3 flex items-start gap-1.5 text-xs text-ink-2">
        <Info className="mt-px size-3.5 shrink-0" />
        <span>
          {view === 'REPLAY' && !recorded
            ? 'Synthetic replay results. Every agent saw the same tape, so runs are comparable, but they say nothing about real markets.'
            : NOTES[view]}{' '}
          Short-term returns do not predict future performance.
          {!okx && capitals.size > 1 && ' Starting capital differs between these agents, so compare returns, not USDT.'}
        </span>
      </p>
      {rows.length === 0 ? (
        <EmptyState
          title={okx ? 'No agents on OKX accounts yet' : 'No agents on this market yet'}
          action={<Button onClick={() => setPage('agents')}>Create an agent <ArrowRight /></Button>}
        />
      ) : (
        <div className="space-y-3">
          <Card>
            <CardHeader
              title={okx ? 'P&L curves' : 'Equity curves'}
              description={chartNote(charted.length, okx)}
            />
            {charted.length === 0 ? (
              <p className="px-6 pb-6 text-[13px] text-ink-3">
                {okx ? 'No agent here has a P&L of its own yet: it has no attributed position, or it shares its pair.' : 'Nothing to chart yet.'}
              </p>
            ) : (
              <div className="h-80 pb-3"><EquityChart series={series} baseline={okx ? 0 : undefined} /></div>
            )}
          </Card>
          <Card>
            <CardHeader title="Leaderboard" description={okx ? 'Sorted by P&L.' : 'Sorted by return.'} />
            <Table head={['#', 'Agent', 'Provider / Model', 'Equity', 'Return', 'P&L', 'Realized', 'Max DD', 'Win rate', 'Trades', 'AI cost']}>
              {ranked.map((r: ArenaRow, i) => (
                <tr key={r.agentId} data-testid="arena-row">
                  <td className="num text-ink-3">{i + 1}</td>
                  <td>
                    <span className="inline-flex items-center gap-2 font-semibold">
                      <span className="size-2 rounded-full" style={{ background: colors.get(r.agentId) ? `var(${colors.get(r.agentId)})` : 'var(--ink-3)' }} />
                      {r.agentName}
                      {r.isMock && <Badge tone="info">Mock</Badge>}
                      {r.venue !== 'PAPER' && <VenueBadge venue={r.venue} />}
                      {r.attribution === 'SHARED' && (
                        <span title="Another agent on this account trades one of the same pairs, so its P&L cannot be split.">
                          <Badge tone="warn">Shared pair</Badge>
                        </span>
                      )}
                    </span>
                    <span className="block truncate text-xs text-ink-3" title={r.symbols.join(', ')}>{r.symbols.join(', ')}</span>
                  </td>
                  <td className="text-ink-2">{r.providerName}<span className="block text-xs text-ink-3">{r.model}</span></td>
                  <td className="num">
                    {money(r.equity)}
                    {r.attribution !== 'ACCOUNT' && <span className="block text-xs text-ink-3">whole account</span>}
                  </td>
                  <td className={cn('num font-semibold', pnlClass(r.returnPct))}>{pct(r.returnPct)}</td>
                  <td className={cn('num font-semibold', pnlClass(r.pnl))}>{signedMoney(r.pnl)}</td>
                  <td className={cn('num', pnlClass(r.realizedPnl))}>{signedMoney(r.realizedPnl)}</td>
                  <td className="num">{r.maxDrawdownPct === null ? '–' : `${r.maxDrawdownPct.toFixed(2)}%`}</td>
                  <td className="num">{r.winRatePct === null ? '–' : `${r.winRatePct.toFixed(0)}%`}</td>
                  <td className="num">{r.tradeCount}</td>
                  <td className="num">{r.isMock ? 'Free' : r.estimatedAiCost === null ? 'Unknown' : `$${Number(r.estimatedAiCost).toFixed(4)}`}</td>
                </tr>
              ))}
            </Table>
          </Card>
        </div>
      )}
    </>
  );
}
