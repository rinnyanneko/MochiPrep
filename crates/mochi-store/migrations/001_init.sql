CREATE TABLE accounts (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('REPLAY','LIVE')),
  is_demo INTEGER NOT NULL DEFAULT 0,
  initial_balance TEXT NOT NULL,
  wallet_balance TEXT NOT NULL,
  used_margin TEXT NOT NULL,
  realized_pnl TEXT NOT NULL,
  fees_paid TEXT NOT NULL,
  funding_paid TEXT NOT NULL,
  config TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE orders (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  client_order_id TEXT NOT NULL,
  symbol TEXT NOT NULL,
  side TEXT NOT NULL,
  type TEXT NOT NULL,
  quantity TEXT NOT NULL,
  price TEXT,
  trigger_price TEXT,
  leverage INTEGER NOT NULL,
  reduce_only INTEGER NOT NULL,
  close_position INTEGER NOT NULL,
  status TEXT NOT NULL,
  filled_quantity TEXT NOT NULL,
  avg_fill_price TEXT,
  reserved_margin TEXT NOT NULL,
  attached_stop_loss TEXT,
  attached_take_profit TEXT,
  reject_reason TEXT,
  agent_id TEXT,
  decision_id TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE (account_id, client_order_id)
);
CREATE INDEX idx_orders_open ON orders(status, symbol);
CREATE INDEX idx_orders_account ON orders(account_id, created_at);

CREATE TABLE fills (
  id TEXT PRIMARY KEY,
  order_id TEXT NOT NULL REFERENCES orders(id),
  account_id TEXT NOT NULL REFERENCES accounts(id),
  symbol TEXT NOT NULL,
  side TEXT NOT NULL,
  quantity TEXT NOT NULL,
  price TEXT NOT NULL,
  fee TEXT NOT NULL,
  realized_pnl TEXT NOT NULL,
  liquidity TEXT NOT NULL,
  ts INTEGER NOT NULL
);
CREATE INDEX idx_fills_order ON fills(order_id);
CREATE INDEX idx_fills_account ON fills(account_id, ts);

CREATE TABLE positions (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  symbol TEXT NOT NULL,
  side TEXT NOT NULL,
  quantity TEXT NOT NULL,
  entry_price TEXT NOT NULL,
  leverage INTEGER NOT NULL,
  margin TEXT NOT NULL,
  liquidation_price TEXT NOT NULL,
  status TEXT NOT NULL,
  realized_pnl TEXT NOT NULL,
  fees_paid TEXT NOT NULL,
  funding_paid TEXT NOT NULL,
  max_quantity TEXT NOT NULL,
  exit_price TEXT,
  last_funding_time INTEGER NOT NULL,
  opened_at INTEGER NOT NULL,
  closed_at INTEGER
);
-- One-way mode: at most one open position per account and symbol, enforced by the database.
CREATE UNIQUE INDEX idx_positions_one_way ON positions(account_id, symbol) WHERE status = 'OPEN';
CREATE INDEX idx_positions_market ON positions(status, symbol);

CREATE TABLE ledger_events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  id TEXT NOT NULL UNIQUE,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  type TEXT NOT NULL,
  amount TEXT NOT NULL,
  margin_delta TEXT NOT NULL,
  wallet_after TEXT NOT NULL,
  symbol TEXT,
  order_id TEXT,
  fill_id TEXT,
  position_id TEXT,
  memo TEXT NOT NULL,
  ts INTEGER NOT NULL
);
CREATE INDEX idx_ledger_account ON ledger_events(account_id, ts);

CREATE TABLE equity_history (
  account_id TEXT NOT NULL REFERENCES accounts(id),
  ts INTEGER NOT NULL,
  equity TEXT NOT NULL,
  wallet TEXT NOT NULL,
  unrealized TEXT NOT NULL,
  PRIMARY KEY (account_id, ts)
);

CREATE TABLE providers (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  base_url TEXT,
  default_model TEXT NOT NULL,
  input_price REAL,
  output_price REAL,
  created_at INTEGER NOT NULL
);

CREATE TABLE agents (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  provider_id TEXT NOT NULL REFERENCES providers(id),
  model TEXT NOT NULL,
  symbol TEXT NOT NULL,
  timeframe TEXT NOT NULL,
  strategy TEXT NOT NULL,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  risk TEXT NOT NULL,
  interval_sec INTEGER NOT NULL,
  status TEXT NOT NULL,
  running INTEGER NOT NULL,
  next_run_at INTEGER,
  last_run_at INTEGER,
  status_detail TEXT,
  consecutive_errors INTEGER NOT NULL DEFAULT 0,
  is_demo INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);

CREATE TABLE decisions (
  id TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  ts INTEGER NOT NULL,
  outcome TEXT NOT NULL,
  body TEXT NOT NULL
);
CREATE INDEX idx_decisions_agent ON decisions(agent_id, ts);
CREATE INDEX idx_decisions_ts ON decisions(ts);

CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
