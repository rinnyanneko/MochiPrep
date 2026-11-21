CREATE TABLE deferred_target_replacements (
    deferred_order_id TEXT NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    replacement_order_id TEXT NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    PRIMARY KEY (deferred_order_id, replacement_order_id)
);

CREATE INDEX idx_deferred_target_replacements_order
    ON deferred_target_replacements(replacement_order_id);
