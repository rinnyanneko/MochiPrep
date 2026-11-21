CREATE TABLE okx_close_intents (
    order_id TEXT PRIMARY KEY REFERENCES orders(id) ON DELETE CASCADE,
    position_id TEXT,
    fraction REAL NOT NULL
);
