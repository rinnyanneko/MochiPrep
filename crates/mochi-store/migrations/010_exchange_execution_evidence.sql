CREATE TABLE exchange_order_executions (
  order_id TEXT NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
  trade_id TEXT NOT NULL,
  executed_at INTEGER NOT NULL,
  PRIMARY KEY (order_id, trade_id)
);

-- Historical rows without exchange mode and direction are not safe to attribute.
ALTER TABLE exchange_position_history ADD COLUMN opening_side TEXT;

-- Reread available old position history once to recover exchange mode and direction.
INSERT INTO exchange_history_cursor(account_id, backfill_before)
  SELECT account_id, MAX(closed_at) + 1 FROM exchange_position_history GROUP BY account_id
  ON CONFLICT(account_id) DO UPDATE SET backfill_before=excluded.backfill_before;

CREATE TABLE exchange_execution_cursor (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  completed_bill TEXT,
  scan_head TEXT,
  scan_after TEXT
);
