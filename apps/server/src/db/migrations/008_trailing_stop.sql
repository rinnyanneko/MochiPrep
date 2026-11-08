-- Trailing stop: a TRAILING_STOP_MARKET order carries its callback and the best price seen so far,
-- so a restart resumes the trail instead of resetting it. attached_trailing_stop is the JSON spec
-- an entry order arms when it fills.
ALTER TABLE orders ADD COLUMN trailing_kind TEXT;
ALTER TABLE orders ADD COLUMN trailing_callback TEXT;
ALTER TABLE orders ADD COLUMN trailing_activation_price TEXT;
ALTER TABLE orders ADD COLUMN trailing_extreme TEXT;
ALTER TABLE orders ADD COLUMN attached_trailing_stop TEXT;

-- Owner's switch for agents; existing agents stay off.
ALTER TABLE agents ADD COLUMN allow_trailing_stop INTEGER NOT NULL DEFAULT 0;
