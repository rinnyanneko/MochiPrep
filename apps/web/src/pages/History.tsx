import { useState } from 'react';
import type { DecisionOutcome, DecisionRecord, Fill, LedgerEvent, Position } from '@mochiprep/shared';
import { PageTitle } from '@/components/Shell';
import { AccountPicker, ClosedPositionsTable, DecisionDetail, DecisionList, FillsTable, LedgerTable, OUTCOME_OPTIONS, useCurrentAccount } from '@/components/trading';
import { Badge, Card, CardHeader, Select, Tabs } from '@/components/ui';
import { api } from '@/lib/api';
import { useApp, useQuery, useSourceAccounts } from '@/store';

function Decisions() {
  const accounts = useSourceAccounts();
  const ids = new Set(accounts.map((a) => a.id));
  const [outcome, setOutcome] = useState<DecisionOutcome | ''>('');
  const [selected, setSelected] = useState<string | null>(null);
  const { data } = useQuery(() => api.get<DecisionRecord[]>('/decisions?limit=1000'), [], ['decisions']);
  const decisions = (data ?? []).filter((d) => ids.has(d.accountId) && (!outcome || d.outcome === outcome));
  const current = decisions.find((d) => d.id === selected) ?? decisions[0];
  return (
    <div className="grid gap-3 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
      <Card className="flex max-h-[760px] flex-col">
        <CardHeader
          title="AI decisions"
          description={`${decisions.length} shown, newest first`}
          action={
            <Select aria-label="Filter by outcome" className="w-44" value={outcome} onChange={(e) => setOutcome(e.target.value as DecisionOutcome | '')}>
              <option value="">All outcomes</option>
              {OUTCOME_OPTIONS.map((o) => <option key={o.value} value={o.value}>{o.label}</option>)}
            </Select>
          }
        />
        <div className="min-h-0 flex-1 overflow-y-auto">
          <DecisionList decisions={decisions.slice(0, 300)} selected={current?.id} onSelect={(d) => setSelected(d.id)} emptyText="No decisions match." />
        </div>
      </Card>
      <Card className="max-h-[760px] overflow-y-auto">
        <CardHeader title="Decision detail" />
        {current ? <DecisionDetail decision={current} /> : <p className="px-5 pb-6 text-[13px] text-ink-3">Nothing selected.</p>}
      </Card>
    </div>
  );
}

function AccountRecords() {
  const account = useCurrentAccount();
  const id = account?.id;
  const [tab, setTab] = useState('closed');
  const { data: fills } = useQuery(() => (id ? api.get<Fill[]>(`/accounts/${id}/fills?limit=500`) : Promise.resolve([])), [id], ['orders']);
  const { data: closed } = useQuery(() => (id ? api.get<Position[]>(`/accounts/${id}/closed-positions?limit=500`) : Promise.resolve([])), [id], ['positions']);
  const { data: ledger } = useQuery(() => (id ? api.get<LedgerEvent[]>(`/accounts/${id}/ledger?limit=1000`) : Promise.resolve([])), [id], ['orders', 'positions']);
  const { data: audit } = useQuery(() => (id ? api.get<{ consistent: boolean; issues: string[] }>(`/accounts/${id}/audit`) : Promise.resolve(null)), [id], ['orders', 'positions']);
  return (
    <Card>
      <div className="flex flex-wrap items-center justify-between gap-2 px-5 pt-4 pb-2">
        <AccountPicker />
        {audit && <Badge tone={audit.consistent ? 'up' : 'down'} title={audit.issues.join('\n')} data-testid="ledger-audit">{audit.consistent ? 'Ledger balances match' : 'Ledger mismatch'}</Badge>}
      </div>
      <Tabs value={tab} onChange={setTab} tabs={[{ id: 'closed', label: 'Closed positions' }, { id: 'fills', label: 'Fills' }, { id: 'ledger', label: 'Ledger' }]}>
        {tab === 'closed' && <ClosedPositionsTable positions={closed ?? []} />}
        {tab === 'fills' && <FillsTable fills={fills ?? []} />}
        {tab === 'ledger' && <LedgerTable events={ledger ?? []} />}
      </Tabs>
    </Card>
  );
}

export function HistoryPage() {
  const source = useApp((s) => s.source);
  return (
    <>
      <PageTitle title="Trade History" description={`Every decision, fill and ledger event on ${source === 'REPLAY' ? 'the replay tape' : 'live data'}.`} />
      <div className="space-y-3">
        <Decisions />
        <AccountRecords />
      </div>
    </>
  );
}
