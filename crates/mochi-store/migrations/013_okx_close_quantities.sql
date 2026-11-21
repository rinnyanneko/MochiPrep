CREATE TABLE okx_close_selections (
    order_id TEXT PRIMARY KEY REFERENCES orders(id) ON DELETE CASCADE,
    position_id TEXT,
    fraction REAL,
    quantity TEXT,
    limit_price TEXT,
    CHECK ((fraction IS NOT NULL AND quantity IS NULL AND limit_price IS NULL)
        OR (fraction IS NULL AND quantity IS NOT NULL AND position_id IS NOT NULL))
);

INSERT INTO okx_close_selections (order_id, position_id, fraction)
    SELECT order_id, position_id, fraction FROM okx_close_intents;

DROP TABLE okx_close_intents;
