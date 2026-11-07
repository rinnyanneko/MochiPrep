-- Pairs the user added on top of the built-in ones. Specs come from OKX at the time of adding.
CREATE TABLE instruments (
  symbol TEXT PRIMARY KEY,
  spec TEXT NOT NULL,
  added_at INTEGER NOT NULL
);
