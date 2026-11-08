//! Performance stats. These stay in f64 because the TypeScript works on JS numbers (equity
//! samples and net PnL as reported to the UI), and the golden tests compare bit for bit.

/// Largest peak-to-trough fall, in percent.
pub fn max_drawdown_pct(equity: &[f64]) -> f64 {
    let mut peak = f64::NEG_INFINITY;
    let mut worst = 0.0_f64;
    for &e in equity {
        if e > peak {
            peak = e;
        }
        if peak > 0.0 {
            worst = worst.max(((peak - e) / peak) * 100.0);
        }
    }
    worst
}

/// Win rate over closed positions. `None` when none closed yet, so 0% is never implied.
pub fn win_rate_pct(closed_net_pnl: &[f64]) -> Option<f64> {
    if closed_net_pnl.is_empty() {
        return None;
    }
    let wins = closed_net_pnl.iter().filter(|&&p| p > 0.0).count();
    Some((wins as f64 / closed_net_pnl.len() as f64) * 100.0)
}
