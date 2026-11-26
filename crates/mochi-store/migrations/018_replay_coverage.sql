-- Which minutes of each pair have been downloaded, as half-open [start, end) intervals in ms.
-- A minute inside an interval with no bar had no trade on OKX; one outside was never fetched.
CREATE TABLE replay_coverage (
  symbol TEXT NOT NULL,
  start INTEGER NOT NULL,
  end INTEGER NOT NULL,
  PRIMARY KEY (symbol, start)
) WITHOUT ROWID;
-- Migration 017 saved whole windows, so each pair's saved bars are one interval.
INSERT INTO replay_coverage (symbol, start, end)
  SELECT symbol, MIN(ts), MAX(ts) + 60000 FROM replay_bars GROUP BY symbol;
