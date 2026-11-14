import type { DatabaseSync, SQLInputValue, StatementSync } from 'node:sqlite';
import type { PaperStore } from '@mochiprep/core';
import type { AgentStore } from '@mochiprep/agents';
import type {
  Agent,
  DataSource,
  DecisionRecord,
  EquityPoint,
  Fill,
  InstrumentSpecification,
  LedgerEvent,
  Order,
  PaperAccount,
  Position,
  ProviderConfig,
  SymbolId,
} from '@mochiprep/shared';

type Row = Record<string, any>;

const toAccount = (r: Row): PaperAccount => ({
  id: r.id,
  name: r.name,
  source: r.source,
  venue: r.venue,
  exchangeUid: r.exchange_uid,
  liveLimits: r.live_limits ? JSON.parse(r.live_limits) : null,
  isDemo: !!r.is_demo,
  initialBalance: r.initial_balance,
  walletBalance: r.wallet_balance,
  usedMargin: r.used_margin,
  realizedPnl: r.realized_pnl,
  feesPaid: r.fees_paid,
  fundingPaid: r.funding_paid,
  config: JSON.parse(r.config),
  createdAt: r.created_at,
});

const toOrder = (r: Row): Order => ({
  id: r.id,
  accountId: r.account_id,
  clientOrderId: r.client_order_id,
  symbol: r.symbol,
  side: r.side,
  type: r.type,
  quantity: r.quantity,
  price: r.price,
  triggerPrice: r.trigger_price,
  leverage: r.leverage,
  reduceOnly: !!r.reduce_only,
  closePosition: !!r.close_position,
  status: r.status,
  filledQuantity: r.filled_quantity,
  avgFillPrice: r.avg_fill_price,
  reservedMargin: r.reserved_margin,
  attachedStopLoss: r.attached_stop_loss,
  attachedTakeProfit: r.attached_take_profit,
  trailingKind: r.trailing_kind,
  trailingCallback: r.trailing_callback,
  trailingActivationPrice: r.trailing_activation_price,
  trailingExtreme: r.trailing_extreme,
  attachedTrailingStop: r.attached_trailing_stop ? JSON.parse(r.attached_trailing_stop) : null,
  rejectReason: r.reject_reason,
  exchangeOrderId: r.exchange_order_id,
  agentId: r.agent_id,
  decisionId: r.decision_id,
  createdAt: r.created_at,
  updatedAt: r.updated_at,
});

const toFill = (r: Row): Fill => ({
  id: r.id,
  orderId: r.order_id,
  accountId: r.account_id,
  symbol: r.symbol,
  side: r.side,
  quantity: r.quantity,
  price: r.price,
  fee: r.fee,
  realizedPnl: r.realized_pnl,
  liquidity: r.liquidity,
  ts: r.ts,
});

const toPosition = (r: Row): Position => ({
  id: r.id,
  accountId: r.account_id,
  symbol: r.symbol,
  side: r.side,
  quantity: r.quantity,
  entryPrice: r.entry_price,
  leverage: r.leverage,
  margin: r.margin,
  liquidationPrice: r.liquidation_price,
  status: r.status,
  realizedPnl: r.realized_pnl,
  feesPaid: r.fees_paid,
  fundingPaid: r.funding_paid,
  maxQuantity: r.max_quantity,
  exitPrice: r.exit_price,
  lastFundingTime: r.last_funding_time,
  openedAt: r.opened_at,
  closedAt: r.closed_at,
});

const toLedger = (r: Row): LedgerEvent => ({
  seq: r.seq,
  id: r.id,
  accountId: r.account_id,
  type: r.type,
  amount: r.amount,
  marginDelta: r.margin_delta,
  walletAfter: r.wallet_after,
  symbol: r.symbol,
  orderId: r.order_id,
  fillId: r.fill_id,
  positionId: r.position_id,
  memo: r.memo,
  ts: r.ts,
});

const toAgent = (r: Row): Agent => ({
  id: r.id,
  name: r.name,
  providerId: r.provider_id,
  model: r.model,
  symbol: r.symbol,
  timeframe: r.timeframe,
  strategy: r.strategy,
  accountId: r.account_id,
  risk: JSON.parse(r.risk),
  intervalSec: r.interval_sec,
  executionMode: r.execution_mode,
  useNews: !!r.use_news,
  allowTrailingStop: !!r.allow_trailing_stop,
  status: r.status,
  running: !!r.running,
  nextRunAt: r.next_run_at,
  lastRunAt: r.last_run_at,
  statusDetail: r.status_detail,
  consecutiveErrors: r.consecutive_errors,
  isDemo: !!r.is_demo,
  createdAt: r.created_at,
});

const toProvider = (r: Row): ProviderConfig => ({
  id: r.id,
  name: r.name,
  kind: r.kind,
  baseUrl: r.base_url,
  defaultModel: r.default_model,
  inputPricePerMTok: r.input_price,
  outputPricePerMTok: r.output_price,
  extraHeaders: JSON.parse(r.extra_headers),
  createdAt: r.created_at,
});

/** Single SQLite-backed store for the paper engine, agents, and app settings. */
export class SqliteStore implements PaperStore, AgentStore {
  private readonly statements = new Map<string, StatementSync>();
  private txDepth = 0;

  constructor(readonly db: DatabaseSync) {}

  private q(sql: string): StatementSync {
    let statement = this.statements.get(sql);
    if (!statement) {
      statement = this.db.prepare(sql);
      this.statements.set(sql, statement);
    }
    return statement;
  }
  private all(sql: string, ...params: SQLInputValue[]): Row[] {
    return this.q(sql).all(...params) as Row[];
  }
  private one(sql: string, ...params: SQLInputValue[]): Row | undefined {
    return this.q(sql).get(...params) as Row | undefined;
  }
  private run(sql: string, ...params: SQLInputValue[]) {
    this.q(sql).run(...params);
  }

  /**
   * Folds the WAL into the main file and empties it. With secure_delete on, this is what makes
   * deleted plaintext actually gone: until then old page images still sit in the WAL.
   * Returns true when that did not happen: another reader held the WAL (SQLite reports it in
   * the pragma's `busy` column rather than failing), or a transaction is still open.
   */
  scrub(): boolean {
    if (this.txDepth > 0) return true;
    const { busy } = this.db.prepare('PRAGMA wal_checkpoint(TRUNCATE)').get() as { busy: number };
    return busy !== 0;
  }

  transaction<T>(fn: () => T): T {
    const savepoint = `sp_${this.txDepth}`;
    this.db.exec(this.txDepth === 0 ? 'BEGIN IMMEDIATE' : `SAVEPOINT ${savepoint}`);
    this.txDepth++;
    try {
      const result = fn();
      this.txDepth--;
      this.db.exec(this.txDepth === 0 ? 'COMMIT' : `RELEASE ${savepoint}`);
      return result;
    } catch (error) {
      this.txDepth--;
      this.db.exec(this.txDepth === 0 ? 'ROLLBACK' : `ROLLBACK TO ${savepoint}; RELEASE ${savepoint}`);
      throw error;
    }
  }

  // accounts
  insertAccount(a: PaperAccount) {
    this.run(
      `INSERT INTO accounts (id,name,source,venue,exchange_uid,live_limits,is_demo,initial_balance,wallet_balance,used_margin,realized_pnl,fees_paid,funding_paid,config,created_at)
       VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
      a.id, a.name, a.source, a.venue, a.exchangeUid, a.liveLimits ? JSON.stringify(a.liveLimits) : null, a.isDemo ? 1 : 0,
      a.initialBalance, a.walletBalance, a.usedMargin, a.realizedPnl,
      a.feesPaid, a.fundingPaid, JSON.stringify(a.config), a.createdAt,
    );
  }
  saveAccount(a: PaperAccount) {
    this.run(
      'UPDATE accounts SET wallet_balance=?, used_margin=?, realized_pnl=?, fees_paid=?, funding_paid=? WHERE id=?',
      a.walletBalance, a.usedMargin, a.realizedPnl, a.feesPaid, a.fundingPaid, a.id,
    );
  }
  getAccount(id: string) {
    const r = this.one('SELECT * FROM accounts WHERE id=?', id);
    return r && toAccount(r);
  }
  saveAccountProfile(a: PaperAccount) {
    this.run('UPDATE accounts SET name=?, initial_balance=?, live_limits=? WHERE id=?', a.name, a.initialBalance, a.liveLimits ? JSON.stringify(a.liveLimits) : null, a.id);
  }
  listAccounts() {
    return this.all('SELECT * FROM accounts ORDER BY created_at, id').map(toAccount);
  }
  deleteAccountData(id: string, keepAccountRow: boolean) {
    for (const table of ['fills', 'orders', 'positions', 'ledger_events', 'equity_history']) {
      this.run(`DELETE FROM ${table} WHERE account_id=?`, id);
    }
    this.run('DELETE FROM decisions WHERE account_id=?', id);
    if (!keepAccountRow) this.run('DELETE FROM accounts WHERE id=?', id);
  }

  // orders
  insertOrder(o: Order) {
    this.run(
      `INSERT INTO orders (id,account_id,client_order_id,symbol,side,type,quantity,price,trigger_price,leverage,reduce_only,close_position,status,
        filled_quantity,avg_fill_price,reserved_margin,attached_stop_loss,attached_take_profit,trailing_kind,trailing_callback,trailing_activation_price,trailing_extreme,
        attached_trailing_stop,reject_reason,exchange_order_id,agent_id,decision_id,created_at,updated_at)
       VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
      o.id, o.accountId, o.clientOrderId, o.symbol, o.side, o.type, o.quantity, o.price, o.triggerPrice, o.leverage,
      o.reduceOnly ? 1 : 0, o.closePosition ? 1 : 0, o.status, o.filledQuantity, o.avgFillPrice, o.reservedMargin,
      o.attachedStopLoss, o.attachedTakeProfit, o.trailingKind, o.trailingCallback, o.trailingActivationPrice, o.trailingExtreme,
      o.attachedTrailingStop ? JSON.stringify(o.attachedTrailingStop) : null, o.rejectReason, o.exchangeOrderId, o.agentId, o.decisionId, o.createdAt, o.updatedAt,
    );
  }
  saveOrder(o: Order) {
    this.run(
      'UPDATE orders SET quantity=?, status=?, filled_quantity=?, avg_fill_price=?, reserved_margin=?, trailing_extreme=?, reject_reason=?, exchange_order_id=?, updated_at=? WHERE id=?',
      o.quantity, o.status, o.filledQuantity, o.avgFillPrice, o.reservedMargin, o.trailingExtreme, o.rejectReason, o.exchangeOrderId, o.updatedAt, o.id,
    );
  }
  getOrder(id: string) {
    const r = this.one('SELECT * FROM orders WHERE id=?', id);
    return r && toOrder(r);
  }
  getOrderByClientId(accountId: string, clientOrderId: string) {
    const r = this.one('SELECT * FROM orders WHERE account_id=? AND client_order_id=?', accountId, clientOrderId);
    return r && toOrder(r);
  }
  /** Oldest first: creation time, ties in insertion order (rowid). OkxExecutionEngine's reconciliation depends on it. */
  listOpenOrders(accountId: string) {
    return this.all("SELECT * FROM orders WHERE account_id=? AND status='OPEN' ORDER BY created_at, rowid", accountId).map(toOrder);
  }
  listOpenOrdersForMarket(source: DataSource, symbol: SymbolId) {
    // Paper accounts only: an OKX account's OPEN rows are OKX's to settle, never the simulator's.
    return this.all(
      `SELECT o.* FROM orders o JOIN accounts a ON a.id=o.account_id
       WHERE o.status='OPEN' AND o.symbol=? AND a.source=? AND a.venue='PAPER' ORDER BY o.created_at, o.rowid`,
      symbol, source,
    ).map(toOrder);
  }
  listOrders(accountId: string, limit: number) {
    return this.all('SELECT * FROM orders WHERE account_id=? ORDER BY created_at DESC, rowid DESC LIMIT ?', accountId, limit).map(toOrder);
  }

  // fills
  insertFill(f: Fill) {
    this.run(
      'INSERT INTO fills (id,order_id,account_id,symbol,side,quantity,price,fee,realized_pnl,liquidity,ts) VALUES (?,?,?,?,?,?,?,?,?,?,?)',
      f.id, f.orderId, f.accountId, f.symbol, f.side, f.quantity, f.price, f.fee, f.realizedPnl, f.liquidity, f.ts,
    );
  }
  listFillsByOrder(orderId: string) {
    return this.all('SELECT * FROM fills WHERE order_id=? ORDER BY ts, rowid', orderId).map(toFill);
  }
  listFills(accountId: string, limit: number) {
    return this.all('SELECT * FROM fills WHERE account_id=? ORDER BY ts DESC, rowid DESC LIMIT ?', accountId, limit).map(toFill);
  }

  // positions
  insertPosition(p: Position) {
    this.run(
      `INSERT INTO positions (id,account_id,symbol,side,quantity,entry_price,leverage,margin,liquidation_price,status,realized_pnl,fees_paid,
        funding_paid,max_quantity,exit_price,last_funding_time,opened_at,closed_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
      p.id, p.accountId, p.symbol, p.side, p.quantity, p.entryPrice, p.leverage, p.margin, p.liquidationPrice, p.status, p.realizedPnl,
      p.feesPaid, p.fundingPaid, p.maxQuantity, p.exitPrice, p.lastFundingTime, p.openedAt, p.closedAt,
    );
  }
  savePosition(p: Position) {
    this.run(
      `UPDATE positions SET quantity=?, entry_price=?, margin=?, liquidation_price=?, status=?, realized_pnl=?, fees_paid=?, funding_paid=?,
        max_quantity=?, exit_price=?, last_funding_time=?, closed_at=? WHERE id=?`,
      p.quantity, p.entryPrice, p.margin, p.liquidationPrice, p.status, p.realizedPnl, p.feesPaid, p.fundingPaid, p.maxQuantity,
      p.exitPrice, p.lastFundingTime, p.closedAt, p.id,
    );
  }
  getOpenPosition(accountId: string, symbol: SymbolId) {
    const r = this.one("SELECT * FROM positions WHERE account_id=? AND symbol=? AND status='OPEN'", accountId, symbol);
    return r && toPosition(r);
  }
  listOpenPositions(accountId: string) {
    return this.all("SELECT * FROM positions WHERE account_id=? AND status='OPEN' ORDER BY opened_at", accountId).map(toPosition);
  }
  listOpenPositionsForMarket(source: DataSource, symbol: SymbolId) {
    // Paper accounts only, as with orders: the simulator never funds or liquidates an OKX account.
    return this.all(
      `SELECT p.* FROM positions p JOIN accounts a ON a.id=p.account_id
       WHERE p.status='OPEN' AND p.symbol=? AND a.source=? AND a.venue='PAPER'`,
      symbol, source,
    ).map(toPosition);
  }
  listClosedPositions(accountId: string, limit: number) {
    return this.all("SELECT * FROM positions WHERE account_id=? AND status<>'OPEN' ORDER BY closed_at DESC LIMIT ?", accountId, limit).map(toPosition);
  }

  // ledger
  appendLedger(e: Omit<LedgerEvent, 'seq'>) {
    this.run(
      `INSERT INTO ledger_events (id,account_id,type,amount,margin_delta,wallet_after,symbol,order_id,fill_id,position_id,memo,ts)
       VALUES (?,?,?,?,?,?,?,?,?,?,?,?)`,
      e.id, e.accountId, e.type, e.amount, e.marginDelta, e.walletAfter, e.symbol, e.orderId, e.fillId, e.positionId, e.memo, e.ts,
    );
  }
  listLedger(accountId: string, sinceTs = 0) {
    return this.all('SELECT * FROM ledger_events WHERE account_id=? AND ts>=? ORDER BY seq', accountId, sinceTs).map(toLedger);
  }

  // equity
  upsertEquityPoint(p: EquityPoint) {
    this.run(
      `INSERT INTO equity_history (account_id,ts,equity,wallet,unrealized) VALUES (?,?,?,?,?)
       ON CONFLICT(account_id,ts) DO UPDATE SET equity=excluded.equity, wallet=excluded.wallet, unrealized=excluded.unrealized`,
      p.accountId, p.ts, p.equity, p.wallet, p.unrealized,
    );
  }
  listEquity(accountId: string, limit: number) {
    return this.all(
      'SELECT * FROM (SELECT * FROM equity_history WHERE account_id=? ORDER BY ts DESC LIMIT ?) ORDER BY ts',
      accountId, limit,
    ).map((r) => ({ accountId: r.account_id, ts: r.ts, equity: r.equity, wallet: r.wallet, unrealized: r.unrealized }));
  }

  // agents
  insertAgent(a: Agent) {
    this.run(
      `INSERT INTO agents (id,name,provider_id,model,symbol,timeframe,strategy,account_id,risk,interval_sec,execution_mode,use_news,allow_trailing_stop,status,running,next_run_at,last_run_at,
        status_detail,consecutive_errors,is_demo,created_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
      a.id, a.name, a.providerId, a.model, a.symbol, a.timeframe, a.strategy, a.accountId, JSON.stringify(a.risk), a.intervalSec, a.executionMode, a.useNews ? 1 : 0, a.allowTrailingStop ? 1 : 0, a.status,
      a.running ? 1 : 0, a.nextRunAt, a.lastRunAt, a.statusDetail, a.consecutiveErrors, a.isDemo ? 1 : 0, a.createdAt,
    );
  }
  saveAgent(a: Agent) {
    this.run(
      `UPDATE agents SET name=?, provider_id=?, model=?, symbol=?, timeframe=?, strategy=?, account_id=?, risk=?, interval_sec=?, execution_mode=?, use_news=?, allow_trailing_stop=?, status=?, running=?,
        next_run_at=?, last_run_at=?, status_detail=?, consecutive_errors=? WHERE id=?`,
      a.name, a.providerId, a.model, a.symbol, a.timeframe, a.strategy, a.accountId, JSON.stringify(a.risk), a.intervalSec, a.executionMode, a.useNews ? 1 : 0, a.allowTrailingStop ? 1 : 0, a.status,
      a.running ? 1 : 0, a.nextRunAt, a.lastRunAt, a.statusDetail, a.consecutiveErrors, a.id,
    );
  }
  getAgent(id: string) {
    const r = this.one('SELECT * FROM agents WHERE id=?', id);
    return r && toAgent(r);
  }
  listAgents() {
    return this.all('SELECT * FROM agents ORDER BY created_at, id').map(toAgent);
  }
  deleteAgent(id: string) {
    this.run('DELETE FROM decisions WHERE agent_id=?', id);
    this.run('DELETE FROM agents WHERE id=?', id);
  }

  // decisions
  insertDecision(rec: DecisionRecord) {
    this.run(
      'INSERT INTO decisions (id,agent_id,account_id,ts,outcome,body) VALUES (?,?,?,?,?,?)',
      rec.id, rec.agentId, rec.accountId, rec.ts, rec.outcome, JSON.stringify(rec),
    );
  }
  saveDecision(rec: DecisionRecord) {
    this.run('UPDATE decisions SET outcome=?, body=? WHERE id=?', rec.outcome, JSON.stringify(rec), rec.id);
  }
  getDecision(id: string): DecisionRecord | undefined {
    const r = this.one('SELECT body FROM decisions WHERE id=?', id);
    return r ? JSON.parse(r.body) : undefined;
  }
  listPendingSignals(): DecisionRecord[] {
    return this.all("SELECT body FROM decisions WHERE outcome='SIGNAL' ORDER BY ts DESC").map((r) => JSON.parse(r.body));
  }
  listDecisions(filter: { agentId?: string; limit: number }): DecisionRecord[] {
    const rows = filter.agentId
      ? this.all('SELECT body FROM decisions WHERE agent_id=? ORDER BY ts DESC, rowid DESC LIMIT ?', filter.agentId, filter.limit)
      : this.all('SELECT body FROM decisions ORDER BY ts DESC, rowid DESC LIMIT ?', filter.limit);
    return rows.map((r) => JSON.parse(r.body));
  }
  decisionStats(agentId: string) {
    const rows = this.all('SELECT body FROM decisions WHERE agent_id=?', agentId);
    let cost: number | null = null;
    for (const r of rows) {
      const rec = JSON.parse(r.body) as DecisionRecord;
      if (rec.estimatedCost !== null) cost = (cost ?? 0) + Number(rec.estimatedCost);
    }
    return { count: rows.length, estimatedCost: cost };
  }

  // providers
  insertProvider(p: ProviderConfig) {
    this.run(
      'INSERT INTO providers (id,name,kind,base_url,default_model,input_price,output_price,extra_headers,created_at) VALUES (?,?,?,?,?,?,?,?,?)',
      p.id, p.name, p.kind, p.baseUrl, p.defaultModel, p.inputPricePerMTok, p.outputPricePerMTok, JSON.stringify(p.extraHeaders ?? {}), p.createdAt,
    );
  }
  saveProvider(p: ProviderConfig) {
    this.run(
      'UPDATE providers SET name=?, kind=?, base_url=?, default_model=?, input_price=?, output_price=?, extra_headers=? WHERE id=?',
      p.name, p.kind, p.baseUrl, p.defaultModel, p.inputPricePerMTok, p.outputPricePerMTok, JSON.stringify(p.extraHeaders ?? {}), p.id,
    );
  }
  getProvider(id: string) {
    const r = this.one('SELECT * FROM providers WHERE id=?', id);
    return r && toProvider(r);
  }
  listProviders() {
    return this.all('SELECT * FROM providers ORDER BY created_at, id').map(toProvider);
  }
  deleteProvider(id: string) {
    this.run('DELETE FROM providers WHERE id=?', id);
  }

  // instruments
  listInstruments(): InstrumentSpecification[] {
    return this.all('SELECT spec FROM instruments ORDER BY symbol').map((r) => JSON.parse(r.spec));
  }
  saveInstrument(spec: InstrumentSpecification) {
    this.run(
      `INSERT INTO instruments (symbol,spec,added_at) VALUES (?,?,?)
       ON CONFLICT(symbol) DO UPDATE SET spec=excluded.spec`,
      spec.symbol, JSON.stringify(spec), Date.now(),
    );
  }
  deleteInstrument(symbol: SymbolId) {
    this.run('DELETE FROM instruments WHERE symbol=?', symbol);
  }

  // AI provider keys
  getProviderKeys(): Map<string, string> {
    return new Map(this.all('SELECT provider_id, api_key FROM provider_keys').map((r) => [r.provider_id, r.api_key]));
  }
  saveProviderKey(providerId: string, key: string) {
    this.run(
      `INSERT INTO provider_keys (provider_id,api_key,updated_at) VALUES (?,?,?)
       ON CONFLICT(provider_id) DO UPDATE SET api_key=excluded.api_key, updated_at=excluded.updated_at`,
      providerId, key, Date.now(),
    );
  }
  deleteProviderKey(providerId: string) {
    this.run('DELETE FROM provider_keys WHERE provider_id=?', providerId);
    this.run('DELETE FROM sealed_provider_keys WHERE provider_id=?', providerId);
  }
  getSealedProviderKeys(): Map<string, string> {
    return new Map(this.all('SELECT provider_id, sealed FROM sealed_provider_keys').map((r) => [r.provider_id, r.sealed]));
  }
  saveSealedProviderKey(providerId: string, sealed: string) {
    this.run(
      `INSERT INTO sealed_provider_keys (provider_id,sealed,updated_at) VALUES (?,?,?)
       ON CONFLICT(provider_id) DO UPDATE SET sealed=excluded.sealed, updated_at=excluded.updated_at`,
      providerId, sealed, Date.now(),
    );
    this.run('DELETE FROM provider_keys WHERE provider_id=?', providerId);
  }

  // vault
  getVaultKey<T>(id: string): T | undefined {
    const r = this.one('SELECT params FROM vault_keys WHERE id=?', id);
    return r ? (JSON.parse(r.params) as T) : undefined;
  }
  hasVault() {
    return !!this.one('SELECT 1 AS x FROM vault_keys LIMIT 1');
  }
  saveVaultKey(id: string, kind: string, params: unknown) {
    this.run(
      `INSERT INTO vault_keys (id,kind,params,created_at) VALUES (?,?,?,?)
       ON CONFLICT(id) DO UPDATE SET params=excluded.params`,
      id, kind, JSON.stringify(params), Date.now(),
    );
  }
  /** Drops the vault and everything only it could open. */
  deleteVault() {
    this.run('DELETE FROM sealed_exchange_credentials');
    this.run('DELETE FROM sealed_provider_keys');
    this.run('DELETE FROM vault_keys');
  }
  listSealedExchangeCredentials(): { accountId: string; sealed: string }[] {
    return this.all('SELECT account_id, sealed FROM sealed_exchange_credentials').map((r) => ({ accountId: r.account_id, sealed: r.sealed }));
  }
  hasSealedExchangeCredentials(accountId: string) {
    return !!this.one('SELECT 1 AS x FROM sealed_exchange_credentials WHERE account_id=?', accountId);
  }
  saveSealedExchangeCredentials(accountId: string, sealed: string) {
    this.run(
      `INSERT INTO sealed_exchange_credentials (account_id,sealed,updated_at) VALUES (?,?,?)
       ON CONFLICT(account_id) DO UPDATE SET sealed=excluded.sealed, updated_at=excluded.updated_at`,
      accountId, sealed, Date.now(),
    );
  }
  deleteSealedExchangeCredentials(accountId: string) {
    this.run('DELETE FROM sealed_exchange_credentials WHERE account_id=?', accountId);
  }

  // settings
  getSetting<T>(key: string): T | undefined {
    const r = this.one('SELECT value FROM settings WHERE key=?', key);
    return r ? (JSON.parse(r.value) as T) : undefined;
  }
  setSetting(key: string, value: unknown) {
    this.run(
      'INSERT INTO settings (key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value',
      key, JSON.stringify(value),
    );
  }
}
