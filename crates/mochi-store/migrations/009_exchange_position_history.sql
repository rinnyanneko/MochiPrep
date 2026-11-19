-- Closed positions as OKX reports them (account/positions-history), so the arena can tell what an
-- agent's pair made without counting deposits and transfers. realized_pnl is OKX's realizedPnl:
-- PnL plus fees, funding and any liquidation penalty. OKX keeps three months, so rows are kept
-- here once seen. In one-way mode a pair can reuse its posId, so the open time is part of the key.
-- Rows are keyed by OKX's instId, pairs not (yet) in the registry included, so the newest record
-- always marks how far the history has been read.
CREATE TABLE exchange_position_history (
  account_id TEXT NOT NULL REFERENCES accounts(id),
  pos_id TEXT NOT NULL,
  opened_at INTEGER NOT NULL,
  inst_id TEXT NOT NULL,
  closed_at INTEGER NOT NULL,
  realized_pnl TEXT NOT NULL,
  close_type TEXT NOT NULL,
  PRIMARY KEY (account_id, pos_id, opened_at)
);
CREATE INDEX idx_exchange_position_history_inst
  ON exchange_position_history(account_id, inst_id, opened_at);

-- How far back the history has been read. backfill_before is the uTime older records are still to
-- be read before; NULL once OKX had nothing older. Kept apart from the newest record, so a read cut
-- short by its page budget resumes where it stopped instead of leaving a gap.
CREATE TABLE exchange_history_cursor (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id),
  backfill_before INTEGER
);
