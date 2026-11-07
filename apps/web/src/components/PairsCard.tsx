import { Plus, X } from 'lucide-react';
import { useState } from 'react';
import { INSTRUMENTS, isDefaultSymbol, type InstrumentSpecification } from '@mochiprep/shared';
import { api } from '@/lib/api';
import { attempt, useApp, useQuery } from '@/store';
import { Badge, Button, Card, CardHeader, Input } from './ui';

/** Accepts doge, DOGE, DOGEUSDT or DOGE-USDT-SWAP. */
const normalize = (text: string) => {
  const base = text.trim().toUpperCase().replace(/-USDT(-SWAP)?$/, '').replace(/USDT$/, '');
  return `${base}USDT`;
};

export function PairsCard() {
  const symbols = useApp((s) => s.symbols);
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const { data: available, error } = useQuery(() => api.get<InstrumentSpecification[]>('/instruments/available'), [], ['instruments']);

  const add = async () => {
    setBusy(true);
    const spec = await attempt(() => api.post<InstrumentSpecification>('/instruments', { symbol: normalize(text) }));
    setBusy(false);
    if (spec) {
      setText('');
      useApp.getState().toast('info', `${spec.symbol} added, up to ${spec.maxLeverage}x on OKX.`);
    }
  };

  return (
    <Card>
      <CardHeader
        title="Trading pairs"
        description="Add any live USDT crypto perpetual listed on OKX. Added pairs work on Live OKX accounts, not on the replay tape."
      />
      <div className="flex flex-wrap gap-1.5 px-5 pb-3" data-testid="pair-list">
        {symbols.map((symbol) =>
          isDefaultSymbol(symbol) ? (
            <Badge key={symbol}>{symbol}</Badge>
          ) : (
            <Badge key={symbol} tone="accent" title={`Up to ${INSTRUMENTS[symbol]?.maxLeverage}x, min size ${INSTRUMENTS[symbol]?.minQuantity}`}>
              {symbol}
              <button type="button" aria-label={`Remove ${symbol}`} onClick={() => attempt(() => api.del(`/instruments/${symbol}`))}>
                <X />
              </button>
            </Badge>
          ),
        )}
      </div>
      <form
        className="flex max-w-md gap-2 px-5 pb-4"
        onSubmit={(event) => {
          event.preventDefault();
          void add();
        }}
      >
        <Input aria-label="Pair to add" list="okx-pairs" value={text} onChange={(e) => setText(e.target.value)} placeholder="DOGE" />
        <datalist id="okx-pairs">
          {available?.map((spec) => <option key={spec.symbol} value={spec.symbol} />)}
        </datalist>
        <Button type="submit" disabled={busy || !text.trim()}><Plus /> Add pair</Button>
      </form>
      {error && <p className="px-5 pb-4 text-xs text-warn">Could not load the OKX pair list: {error}</p>}
    </Card>
  );
}
