//! The replay's recorded bars. REAL columns hold an `f64` exactly, so a bar reads back as the
//! number OKX's text parsed to.
use crate::StoreError;
use mochi_domain::Candle;
use rusqlite::{params, Connection};

/// Replaces `symbol`'s bars with `bars`, in one transaction.
pub fn save_replay_bars(
    conn: &Connection,
    symbol: &str,
    bars: &[Candle],
) -> Result<(), StoreError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM replay_bars WHERE symbol=?", [symbol])?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO replay_bars (symbol,ts,open,high,low,close,volume) VALUES (?,?,?,?,?,?,?)",
        )?;
        for b in bars {
            insert.execute(params![
                symbol, b.time, b.open, b.high, b.low, b.close, b.volume
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// `symbol`'s bars from `start` for `minutes` minutes, or `None` unless every one is saved.
pub fn load_replay_bars(
    conn: &Connection,
    symbol: &str,
    start: i64,
    minutes: usize,
) -> Result<Option<Vec<Candle>>, StoreError> {
    let end = start + minutes as i64 * 60_000;
    let mut stmt = conn.prepare(
        "SELECT ts,open,high,low,close,volume FROM replay_bars
         WHERE symbol=? AND ts>=? AND ts<? ORDER BY ts",
    )?;
    let bars = stmt
        .query_map(params![symbol, start, end], |row| {
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
    Ok((bars.len() == minutes).then_some(bars))
}

/// Forgets every saved bar, so the next load downloads a new window.
pub fn clear_replay_bars(conn: &Connection) -> Result<(), StoreError> {
    conn.execute("DELETE FROM replay_bars", [])?;
    Ok(())
}
