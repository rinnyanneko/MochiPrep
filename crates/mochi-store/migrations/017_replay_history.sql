-- The replay's recorded one-minute bars, downloaded from OKX the first time the replay is used
-- and read from here afterwards. The window they cover is the `replay.window` setting.
CREATE TABLE replay_bars (
  symbol TEXT NOT NULL,
  ts INTEGER NOT NULL,
  open REAL NOT NULL,
  high REAL NOT NULL,
  low REAL NOT NULL,
  close REAL NOT NULL,
  volume REAL NOT NULL,
  PRIMARY KEY (symbol, ts)
) WITHOUT ROWID;
