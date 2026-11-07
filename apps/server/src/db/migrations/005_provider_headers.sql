-- Extra request headers some gateways need. Values may be secret, so API views omit them.
ALTER TABLE providers ADD COLUMN extra_headers TEXT NOT NULL DEFAULT '{}';
