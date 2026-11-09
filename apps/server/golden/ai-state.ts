import { DEFAULT_RISK_SETTINGS, type Agent, type ProviderConfig, type DecisionRecord } from '@mochiprep/shared';
import { openDatabase } from '../src/db/database.js';
import { SqliteStore } from '../src/db/store.js';

/** Exact Node writes/reads, including immutable SQL index fields when bodies are saved. */
export function aiStateFixture() {
  const db = openDatabase(':memory:');
  try {
    db.exec(`INSERT INTO accounts (id,name,source,initial_balance,wallet_balance,used_margin,realized_pnl,fees_paid,funding_paid,config,created_at)
      VALUES ('account','Account','REPLAY','10000','10000','0','0','0','0','{}',1)`);
    const store = new SqliteStore(db);
    const provider: ProviderConfig = {
      id: 'provider', name: 'Gateway', kind: 'OPENAI', baseUrl: 'https://example.test/v1', defaultModel: 'model',
      inputPricePerMTok: 1.5, outputPricePerMTok: null, extraHeaders: { 'x-gateway': 'synthetic-header' }, createdAt: 2,
    };
    const agent: Agent = {
      id: 'agent', name: 'Agent', providerId: provider.id, model: 'model', symbol: 'DOGEUSDT', timeframe: '1m',
      strategy: 'Wait', accountId: 'account', risk: DEFAULT_RISK_SETTINGS, intervalSec: 60, executionMode: 'SIGNAL',
      useNews: false, allowTrailingStop: false, status: 'IDLE', running: false, nextRunAt: null, lastRunAt: null,
      statusDetail: null, consecutiveErrors: 0, isDemo: true, createdAt: 3,
    };
    const decision = (id: string, trailing?: null): DecisionRecord => ({
      id, agentId: agent.id, agentName: agent.name, accountId: agent.accountId, source: 'REPLAY', venue: 'PAPER', ts: 10,
      trigger: 'ANALYZE', providerKind: 'OPENAI', model: 'model', symbol: agent.symbol, timeframe: agent.timeframe,
      snapshot: null, indicators: null, decision: {
        symbol: agent.symbol, action: 'HOLD', orderType: 'MARKET', entryPrice: null, stopLoss: null, takeProfit: null,
        reason: 'Wait', confidence: 0.5, expiresAt: null, ...(trailing === null ? { trailingStop: null } : {}),
      }, rawResponse: null, validationError: null, risk: null, outcome: 'SIGNAL', outcomeDetail: null, news: null,
      newsError: null, signalExpiresAt: 100, orderId: null, fillIds: [], latencyMs: 2, inputTokens: 1,
      outputTokens: 2, estimatedCost: '0.1', isMock: false,
    });
    const decisions = [decision('decision-absent'), decision('decision-null', null)];
    const providerSave: ProviderConfig = { ...provider, name: 'Edited gateway', extraHeaders: {}, createdAt: 999 };
    const agentSave: Agent = {
      ...agent, name: 'Edited agent', model: 'other', strategy: 'Changed', executionMode: 'AUTO', useNews: true,
      allowTrailingStop: true, status: 'PAUSED', running: true, nextRunAt: 42, lastRunAt: 40, statusDetail: 'Paused',
      consecutiveErrors: 2, intervalSec: 100, isDemo: false, createdAt: 999,
    };
    const decisionSave: DecisionRecord = {
      ...decisions[0]!, outcome: 'SIGNAL_DISMISSED', outcomeDetail: 'Dismissed', agentId: 'body-agent', accountId: 'body-account', ts: 999,
    };
    const snapshot = () => Object.fromEntries(['providers', 'agents', 'decisions'].map(table => [table, db.prepare(`SELECT * FROM ${table} ORDER BY rowid`).all()]));
    store.insertProvider(provider);
    store.insertAgent(agent);
    decisions.forEach(value => store.insertDecision(value));
    const inserted = snapshot();
    const reads = { providers: store.listProviders(), agents: store.listAgents(), decisions: store.listDecisions({ limit: 100 }), pending: store.listPendingSignals() };
    store.saveProvider(providerSave);
    store.saveAgent(agentSave);
    store.saveDecision(decisionSave);
    const saved = snapshot();
    const savedReads = { provider: store.getProvider(provider.id), agent: store.getAgent(agent.id), decision: store.getDecision(decisionSave.id), pending: store.listPendingSignals() };
    store.deleteAgent(agent.id);
    const deletedAgent = snapshot();
    store.deleteProvider(provider.id);
    return { inputs: { provider, agent, decisions, providerSave, agentSave, decisionSave }, inserted, reads, saved, savedReads, deletedAgent, deletedProvider: snapshot() };
  } finally { db.close(); }
}
