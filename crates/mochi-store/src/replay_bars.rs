//! The replay's recorded bars, and which minutes of each pair have been downloaded. REAL
//! columns hold an `f64` exactly, so a bar reads back as the number OKX's text parsed to.
use crate::StoreError;
use mochi_domain::Candle;
use rusqlite::{params, Connection};

/// A half-open `[start, end)` span of time in ms.
pub type Span = (i64, i64);

/// Saves `bars` of `symbol`, replacing any at the same minute, and records `span` as
/// downloaded, merged with what touches it. One transaction.
pub fn save_replay_bars(
    conn: &Connection,
    symbol: &str,
    span: Span,
    bars: &[Candle],
) -> Result<(), StoreError> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut insert = tx.prepare(
            "INSERT OR REPLACE INTO replay_bars (symbol,ts,open,high,low,close,volume)
             VALUES (?,?,?,?,?,?,?)",
        )?;
        for b in bars {
            insert.execute(params![
                symbol, b.time, b.open, b.high, b.low, b.close, b.volume
            ])?;
        }
    }
    let mut merged = span;
    let touching = replay_coverage_of(&tx, symbol)?
        .into_iter()
        .filter(|&(s, e)| s <= span.1 && e >= span.0);
    for (s, e) in touching {
        merged = (merged.0.min(s), merged.1.max(e));
        tx.execute(
            "DELETE FROM replay_coverage WHERE symbol=? AND start=?",
            params![symbol, s],
        )?;
    }
    tx.execute(
        "INSERT INTO replay_coverage (symbol,start,end) VALUES (?,?,?)",
        params![symbol, merged.0, merged.1],
    )?;
    tx.commit()?;
    Ok(())
}

/// The downloaded spans of `symbol`, oldest first, never overlapping.
pub fn replay_coverage_of(conn: &Connection, symbol: &str) -> Result<Vec<Span>, StoreError> {
    let mut stmt =
        conn.prepare("SELECT start,end FROM replay_coverage WHERE symbol=? ORDER BY start")?;
    let spans = stmt
        .query_map([symbol], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(spans)
}

/// Every pair's downloaded spans.
pub fn replay_coverage(conn: &Connection) -> Result<Vec<(String, Span)>, StoreError> {
    let mut stmt = conn.prepare("SELECT symbol,start,end FROM replay_coverage ORDER BY start")?;
    let spans = stmt
        .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(spans)
}

/// `symbol`'s saved bars in `span`, oldest first. Minutes without a trade have none.
pub fn load_replay_bars(
    conn: &Connection,
    symbol: &str,
    span: Span,
) -> Result<Vec<Candle>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT ts,open,high,low,close,volume FROM replay_bars
         WHERE symbol=? AND ts>=? AND ts<? ORDER BY ts",
    )?;
    let bars = stmt
        .query_map(params![symbol, span.0, span.1], |row| {
            Ok(Candle {
                time: row.get(0)?,
                open: row.get(1)?,
                high: row.get(2)?,
                low: row.get(3)?,
                close: row.get(4)?,
                volume: row.get(5)?,
                closed: true,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(bars)
}

/// `symbol`'s last saved bar before `at`, if any: what a minute without a trade repeats.
pub fn replay_bar_before(
    conn: &Connection,
    symbol: &str,
    at: i64,
) -> Result<Option<Candle>, StoreError> {
    Ok(load_replay_bars(conn, symbol, (i64::MIN, at))?.pop())
}

/// How many bars every pair has saved in `span`.
pub fn count_replay_bars(conn: &Connection, span: Span) -> Result<u64, StoreError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM replay_bars WHERE ts>=? AND ts<?",
        params![span.0, span.1],
        |row| row.get(0),
    )?)
}

/// Forgets every pair's bars in `span`, and that they were downloaded. One transaction.
pub fn delete_replay_span(conn: &Connection, span: Span) -> Result<(), StoreError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM replay_bars WHERE ts>=? AND ts<?",
        params![span.0, span.1],
    )?;
    for (symbol, (s, e)) in replay_coverage(&tx)? {
        if s >= span.1 || e <= span.0 {
            continue;
        }
        tx.execute(
            "DELETE FROM replay_coverage WHERE symbol=? AND start=?",
            params![symbol, s],
        )?;
        for (keep_s, keep_e) in [(s, span.0.min(e)), (span.1.max(s), e)] {
            if keep_s < keep_e {
                tx.execute(
                    "INSERT INTO replay_coverage (symbol,start,end) VALUES (?,?,?)",
                    params![symbol, keep_s, keep_e],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}
