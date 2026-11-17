import { lazy, Suspense, useEffect } from 'react';
import { Shell } from './components/Shell';
import { useApp, type Page } from './store';

// Each page is its own chunk, so opening the interface loads only the page in view; the charting
// library, for one, arrives only with a page that draws a chart.
const PAGES: Record<Page, React.LazyExoticComponent<() => React.ReactNode>> = {
  dashboard: lazy(() => import('./pages/Dashboard').then((m) => ({ default: m.DashboardPage }))),
  markets: lazy(() => import('./pages/Markets').then((m) => ({ default: m.MarketsPage }))),
  agents: lazy(() => import('./pages/Agents').then((m) => ({ default: m.AgentsPage }))),
  paper: lazy(() => import('./pages/Paper').then((m) => ({ default: m.PaperPage }))),
  arena: lazy(() => import('./pages/Arena').then((m) => ({ default: m.ArenaPage }))),
  history: lazy(() => import('./pages/History').then((m) => ({ default: m.HistoryPage }))),
  settings: lazy(() => import('./pages/Settings').then((m) => ({ default: m.SettingsPage }))),
};

export function App() {
  const page = useApp((s) => s.page);
  useEffect(() => {
    const onHash = () => {
      const next = location.hash.slice(1) as Page;
      if (PAGES[next]) useApp.setState({ page: next });
    };
    window.addEventListener('hashchange', onHash);
    return () => window.removeEventListener('hashchange', onHash);
  }, []);
  const Current = PAGES[page] ?? PAGES.dashboard;
  return (
    <Shell>
      {/* Chunks are local files served by the backend, so the wait is a few milliseconds: no spinner. */}
      <Suspense fallback={null}>
        <Current />
      </Suspense>
    </Shell>
  );
}
