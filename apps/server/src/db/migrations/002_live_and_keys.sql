ALTER TABLE accounts ADD COLUMN venue TEXT NOT NULL DEFAULT 'PAPER';
ALTER TABLE accounts ADD COLUMN exchange_uid TEXT;
ALTER TABLE accounts ADD COLUMN live_limits TEXT;
ALTER TABLE orders ADD COLUMN exchange_order_id TEXT;

-- AI provider keys, in plaintext by the owner's explicit choice. The database file is 0600.
-- Exchange credentials are deliberately not stored anywhere on disk.
CREATE TABLE provider_keys (
  provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
  api_key TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
