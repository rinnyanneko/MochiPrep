use mochi_decimal::Dec;
use mochi_domain::{LedgerEvent, LedgerEventType, PaperAccount};

use crate::CoreError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerTotals {
    pub wallet: String,
    pub used_margin: String,
    pub realized_pnl: String,
    pub fees_paid: String,
    pub funding_paid: String,
}

fn zero() -> Dec {
    Dec::from_js_number(0.0)
}

/// Rebuilds account totals purely from the event stream. Fails if an event holds an amount that
/// is not a decimal, where TypeScript would throw.
pub fn fold_ledger(events: &[LedgerEvent]) -> Result<LedgerTotals, CoreError> {
    let mut wallet = zero();
    let mut used_margin = zero();
    let mut realized = zero();
    let mut fees = zero();
    let mut funding = zero();
    for e in events {
        let amount: Dec = e.amount.parse()?;
        wallet = wallet.add(&amount);
        used_margin = used_margin.add(&e.margin_delta.parse()?);
        match e.event_type {
            LedgerEventType::RealizedPnl | LedgerEventType::LiquidationPenalty => {
                realized = realized.add(&amount);
            }
            LedgerEventType::TradeFee => fees = fees.sub(&amount),
            LedgerEventType::Funding => funding = funding.sub(&amount),
            _ => {}
        }
    }
    Ok(LedgerTotals {
        wallet: wallet.to_fixed(),
        used_margin: used_margin.to_fixed(),
        realized_pnl: realized.to_fixed(),
        fees_paid: fees.to_fixed(),
        funding_paid: funding.to_fixed(),
    })
}

/// Returns discrepancies between stored totals and the ledger; empty means consistent.
pub fn audit_ledger(
    account: &PaperAccount,
    events: &[LedgerEvent],
) -> Result<Vec<String>, CoreError> {
    let totals = fold_ledger(events)?;
    let mut issues = Vec::new();
    let mut compare = |label: &str, stored: &str, derived: &str| -> Result<(), CoreError> {
        // Compared as numbers: "10.0" and "10" are the same balance.
        if !stored.parse::<Dec>()?.eq_value(&derived.parse()?) {
            issues.push(format!("{label}: stored {stored}, ledger {derived}"));
        }
        Ok(())
    };
    compare("walletBalance", &account.wallet_balance, &totals.wallet)?;
    compare("usedMargin", &account.used_margin, &totals.used_margin)?;
    compare("realizedPnl", &account.realized_pnl, &totals.realized_pnl)?;
    compare("feesPaid", &account.fees_paid, &totals.fees_paid)?;
    compare("fundingPaid", &account.funding_paid, &totals.funding_paid)?;
    let mut running = zero();
    for e in events {
        running = running.add(&e.amount.parse()?);
        if !running.eq_value(&e.wallet_after.parse()?) {
            issues.push(format!(
                "event {}: walletAfter {}, running {}",
                e.seq,
                e.wallet_after,
                running.to_fixed()
            ));
            break;
        }
    }
    Ok(issues)
}
