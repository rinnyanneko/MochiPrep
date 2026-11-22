-- An agent can trade several pairs. `symbol` stays as the first of them, for readers that know
-- one pair per agent; `symbols` holds the whole list as a JSON array.
ALTER TABLE agents ADD COLUMN symbols TEXT NOT NULL DEFAULT '[]';
UPDATE agents SET symbols = json_array(symbol);
