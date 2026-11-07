import { Info } from 'lucide-react';
import { useMemo, useState } from 'react';
import type { ArenaRow, DataSource } from '@mochiprep/shared';
import { EquityChart } from '@/components/charts';
import { PageTitle } from '@/components/Shell';
import { VenueBadge } from '@/components/trading';
import { Badge, Card, CardHeader, Segmented, Table } from '@/components/ui';
import { money, pct, pnlClass, signedMoney } from '@/lib/format';
import { cn } from '@/lib/utils';
import { useApp } from '@/store';

const SERIES = ['--series-1', '--series-2', '--series-3', '--series-4', '--series-5', '--series-6'];
const CHARTED = SERIES.length;

export function ArenaPage() {
  const arena = useApp((s) => s.arena);
  const globalSource = useApp((s) => s.source);
  const [source, setSource] = useState<DataSource>(globalSource);
  const rows = arena.filter((r) => r.source === source);

  /** Color follows creation order, so sorting by return never repaints an agent. */
  const colors = useMemo(() => new Map(rows.map((r, i) => [r.agentId, SERIES[i]])), [rows.map((r) => r.agentId).join()]);
  const ranked = [...rows].sort((a, b) => b.returnPct - a.returnPct);
  const series = useMemo(
    () => rows.slice(0, CHARTED).map((r) => ({ id: r.agentId, name: r.agentName, color: colors.get(r.agentId)!, points: r.curve })),
    [arena, source],
  );
  const capitals = new Set(rows.map((r) => Number(r.initialBalance)));

  return (
    <>
      <PageTitle
        title="Trading Arena"
        description="Agents trade the same feed from separate paper accounts."
        action={
          <Segmented
            label="Arena market"
            value={source}
            onChange={setSource}
            options={[{ value: 'REPLAY', label: 'Historical replay' }, { value: 'LIVE', label: 'Live market' }]}
          />
        }
      />
      <div className="mb-3 flex items-start gap-2 rounded-2xl bg-info-soft px-4 py-2.5 text-xs text-info">
        <Info className="mt-px size-4 shrink-0" />
        <span>
          {source === 'REPLAY'
            ? 'Synthetic replay results. Every agent saw the same tape, so runs are comparable, but they say nothing about real markets.'
            : 'Live OKX prices. Accounts marked OKX demo or Real money trade on the exchange; the rest are simulated. Agents started at different times are not directly comparable.'}{' '}
          Short-term returns do not predict future performance.
          {capitals.size > 1 && ' Starting capital differs between these agents, so compare returns, not USDT.'}
        </span>
      </div>
      {rows.length === 0 ? (
        <Card className="px-6 py-10 text-center text-[13px] text-ink-3">No agents on this market yet.</Card>
      ) : (
        <div className="space-y-3">
          <Card>
            <CardHeader title="Equity curves" description={rows.length > CHARTED ? `First ${CHARTED} of ${rows.length} agents. USDT.` : 'USDT, sampled on the backend.'} />
            <div className="h-80 pb-3"><EquityChart series={series} /></div>
          </Card>
          <Card>
            <CardHeader title="Leaderboard" description="Sorted by return." />
            <Table head={['#', 'Agent', 'Provider / Model', 'Equity', 'Return', 'Realized', 'Max DD', 'Win rate', 'Trades', 'AI cost']}>
              {ranked.map((r: ArenaRow, i) => (
                <tr key={r.agentId} data-testid="arena-row">
                  <td className="num text-ink-3">{i + 1}</td>
                  <td>
                    <span className="inline-flex items-center gap-2 font-semibold">
                      <span className="size-2 rounded-full" style={{ background: colors.get(r.agentId) ? `var(${colors.get(r.agentId)})` : 'var(--ink-3)' }} />
                      {r.agentName}
                      {r.isMock && <Badge tone="info">Mock</Badge>}
                      {r.venue !== 'PAPER' && <VenueBadge venue={r.venue} />}
                    </span>
                  </td>
                  <td className="text-ink-2">{r.providerName}<span className="block text-xs text-ink-3">{r.model}</span></td>
                  <td className="num">{money(r.equity)}</td>
                  <td className={cn('num font-semibold', pnlClass(r.returnPct))}>{pct(r.returnPct)}</td>
                  <td className={cn('num', pnlClass(r.realizedPnl))}>{signedMoney(r.realizedPnl)}</td>
                  <td className="num">{r.maxDrawdownPct.toFixed(2)}%</td>
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
