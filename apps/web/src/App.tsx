import { useEffect } from 'react';
import { Shell } from './components/Shell';
import { AgentsPage } from './pages/Agents';
import { ArenaPage } from './pages/Arena';
import { DashboardPage } from './pages/Dashboard';
import { HistoryPage } from './pages/History';
import { MarketsPage } from './pages/Markets';
import { PaperPage } from './pages/Paper';
import { SettingsPage } from './pages/Settings';
import { useApp, type Page } from './store';

const PAGES: Record<Page, () => React.ReactNode> = {
  dashboard: DashboardPage,
  markets: MarketsPage,
  agents: AgentsPage,
  paper: PaperPage,
  arena: ArenaPage,
  history: HistoryPage,
  settings: SettingsPage,
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
  const Current = PAGES[page] ?? DashboardPage;
  return (
    <Shell>
      <Current />
    </Shell>
  );
}
