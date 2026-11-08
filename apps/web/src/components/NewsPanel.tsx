import { Plus, Trash2 } from 'lucide-react';
import { useState } from 'react';
import type { NewsItem, NewsSource, SymbolId } from '@mochiprep/shared';
import { api } from '@/lib/api';

import { attempt, useDateTime, useQuery } from '@/store';
import { Badge, Button, Card, CardHeader, Input } from './ui';

export function NewsPanel({ symbol }: { symbol: SymbolId }) {
  const dateTime = useDateTime();
  const { data, error } = useQuery(() => api.get<{ items: NewsItem[]; error: string | null }>(`/news?symbol=${symbol}`), [symbol], ['news']);
  return (
    <Card className="mt-3" data-testid="news-panel">
      <CardHeader title="News" description="Latest headlines, this coin first. Third-party text, unverified. DYOR." />
      {(error || data?.error) && <p className="px-5 pb-2 text-xs text-warn">{error ?? data?.error}</p>}
      {data && data.items.length === 0 && !error && <p className="px-5 pb-4 text-[13px] text-ink-3">No recent headlines.</p>}
      <ul className="divide-y divide-line">
        {data?.items.map((item) => (
          <li key={item.id} className="px-5 py-2" data-testid="news-item">
            {item.url ? (
              <a href={item.url} target="_blank" rel="noopener noreferrer" className="text-[13px] hover:underline">{item.title}</a>
            ) : (
              <span className="text-[13px]">{item.title}</span>
            )}
            <span className="block text-[11px] text-ink-3">{item.source} · {dateTime(item.publishedAt)}</span>
          </li>
        ))}
      </ul>
    </Card>
  );
}

export function NewsSourcesCard() {
  const { data: sources } = useQuery(() => api.get<NewsSource[]>('/news/sources'), [], ['news']);
  const [name, setName] = useState('');
  const [url, setUrl] = useState('');

  const add = async () => {
    const ok = await attempt(() => api.post('/news/sources', { name, url }));
    if (ok) {
      setName('');
      setUrl('');
    }
  };

  return (
    <Card>
      <CardHeader title="News sources" description="RSS feeds agents can read on Live OKX. Turn news on per agent. Feeds are third-party text, so add only ones you trust." />
      {sources?.map((source) => (
        <div key={source.id} className="flex items-center gap-2 border-t border-line px-5 py-2" data-testid="news-source">
          <label className="flex min-w-0 items-center gap-2 text-[13px]">
            <input
              type="checkbox"
              className="accent-[var(--accent)]"
              checked={source.enabled}
              onChange={(e) => attempt(() => api.put(`/news/sources/${source.id}`, { enabled: e.target.checked }))}
            />
            <span className="font-semibold">{source.name}</span>
          </label>
          <span className="num truncate text-xs text-ink-3">{source.url}</span>
          {!source.builtIn && <Badge tone="accent">Custom</Badge>}
          {!source.builtIn && (
            <Button className="ml-auto" size="icon" variant="ghost" aria-label={`Remove ${source.name}`} onClick={() => attempt(() => api.del(`/news/sources/${source.id}`))}><Trash2 /></Button>
          )}
        </div>
      ))}
      <form
        className="flex flex-wrap gap-2 border-t border-line px-5 py-3"
        onSubmit={(event) => {
          event.preventDefault();
          void add();
        }}
      >
        <Input aria-label="Feed name" className="w-40" value={name} onChange={(e) => setName(e.target.value)} placeholder="Name" />
        <Input aria-label="Feed URL" className="min-w-52 flex-1" value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://example.com/feed" />
        <Button type="submit" disabled={!name.trim() || !url.trim()}><Plus /> Add feed</Button>
      </form>
    </Card>
  );
}
