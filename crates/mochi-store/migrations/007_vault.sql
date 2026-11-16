-- Optional master password. The password itself is never stored: each row here wraps the one
-- random data key that seals the secrets below. With no row, provider_keys stays in use as before.
CREATE TABLE vault_keys (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  params TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

-- AI keys once a master password is set. Rows move here from provider_keys, never both.
CREATE TABLE sealed_provider_keys (
  provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
  sealed TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);

-- OKX credentials, only when the owner opts in and only ever sealed. There is no plaintext form.
CREATE TABLE sealed_exchange_credentials (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  sealed TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
