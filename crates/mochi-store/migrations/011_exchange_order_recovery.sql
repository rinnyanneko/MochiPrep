ALTER TABLE orders ADD COLUMN exchange_recovery_checked_at INTEGER;

CREATE INDEX idx_orders_exchange_recovery
  ON orders (account_id, reject_reason, COALESCE(exchange_recovery_checked_at, created_at), id)
  WHERE status = 'CANCELED' AND reject_reason IS NOT NULL;
