CREATE INDEX idx_orders_exchange_order_id
    ON orders (account_id, exchange_order_id, created_at)
    WHERE exchange_order_id IS NOT NULL;
