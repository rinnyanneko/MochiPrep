//! The risk engine: the gate every AI decision passes before any order exists. It mirrors
//! `risk/engine.ts` check for check, because the order of the checks, their labels and their
//! detail strings are shown to users and compared against the TypeScript golden fixtures.

use mochi_decimal::{Dec, DecError};
use mochi_domain::jsnum::{js_number_string, js_to_fixed};
use mochi_domain::registry::Instruments;
use mochi_domain::{
    AccountBalance, AiAction, AiDecisionData, EntryOrderType, PaperAccountConfig, Position,
    PositionSide, Quote, RiskCheck, RiskResult, RiskSettings, SizedTrade,
};
use serde::{Deserialize, Serialize};

use crate::{perp, CoreError};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskInput {
    pub decision: AiDecisionData,
    /// The symbol this agent is allowed to trade.
    pub symbol: String,
    pub settings: RiskSettings,
    pub quote: Option<Quote>,
    pub now: i64,
    pub balance: AccountBalance,
    pub open_positions: Vec<Position>,
    /// Realized + unrealized since 00:00 UTC, and the equity base it is measured against.
    pub daily_pnl: String,
    pub day_start_equity: String,
    pub fees: PaperAccountConfig,
}

/// Keeps a little available balance free for the entry fee and adverse slippage.
const MARGIN_SAFETY: &str = "0.98";
/// Sizing distance when settings allow a position without a stop.
const ASSUMED_STOP_FRACTION: &str = "0.02";

/// Why a verdict could not be computed. TypeScript throws in these situations; the risk gate must
/// not crash its caller, so they become a failed check instead (see `evaluate_risk`).
#[derive(Debug, thiserror::Error)]
enum Unevaluable {
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error(transparent)]
    Decimal(#[from] DecError),
    #[error("{0}")]
    Invalid(String),
}

/// Collects the checks in the order they run. Every check is reported, passed or not.
struct Checks(Vec<RiskCheck>);

impl Checks {
    fn check(&mut self, id: &str, label: &str, passed: bool, detail: String) -> bool {
        self.0.push(RiskCheck {
            id: id.to_string(),
            label: label.to_string(),
            passed,
            detail,
        });
        passed
    }

    fn any_failed(&self) -> bool {
        self.0.iter().any(|c| !c.passed)
    }

    fn finish(self, sized: Option<SizedTrade>) -> RiskResult {
        let rejection_reasons: Vec<String> = self
            .0
            .iter()
            .filter(|c| !c.passed)
            .map(|c| format!("{}: {}", c.label, c.detail))
            .collect();
        let approved = rejection_reasons.is_empty();
        RiskResult {
            approved,
            checks: self.0,
            rejection_reasons,
            sized: if approved { sized } else { None },
        }
    }
}

/// Pure function: same input, same verdict. It sees AI output only as untrusted data, and
/// nothing the model says can change a limit.
///
/// Input on which TypeScript would throw or produce Infinity/NaN (a malformed decimal string, an
/// unparseable expiry, an unknown instrument, a zero divisor) is rejected here with a failed
/// `input` check, so the gate fails closed rather than panicking or approving.
pub fn evaluate_risk(input: &RiskInput, instruments: &Instruments) -> RiskResult {
    let mut checks = Checks(Vec::new());
    match run_checks(input, instruments, &mut checks) {
        Ok(sized) => checks.finish(sized),
        Err(problem) => {
            checks.check("input", "Input evaluable", false, problem.to_string());
            checks.finish(None)
        }
    }
}

fn run_checks(
    input: &RiskInput,
    instruments: &Instruments,
    checks: &mut Checks,
) -> Result<Option<SizedTrade>, Unevaluable> {
    let decision = &input.decision;
    let settings = &input.settings;
    let balance = &input.balance;
    let open_positions = &input.open_positions;

    let symbol_ok = decision.symbol == input.symbol;
    checks.check(
        "symbol",
        "Symbol allowed",
        symbol_ok,
        if symbol_ok {
            format!("{} matches the agent", input.symbol)
        } else {
            format!(
                "agent trades {}, AI asked for {}",
                input.symbol, decision.symbol
            )
        },
    );

    if decision.action == AiAction::Hold {
        return Ok(None);
    }

    let expiry = match &decision.expires_at {
        Some(text) => Some(parse_iso_datetime(text).ok_or_else(|| {
            Unevaluable::Invalid(format!("expiresAt is not an ISO date-time: {text}"))
        })?),
        None => None,
    };
    checks.check(
        "expiry",
        "Decision not expired",
        expiry.is_none_or(|ms| ms > input.now),
        match expiry {
            None => "no expiry given".to_string(),
            Some(ms) => format!("expires {}", iso_string(ms)),
        },
    );

    let existing = open_positions.iter().find(|p| p.symbol == input.symbol);
    if decision.action == AiAction::Close {
        checks.check(
            "has-position",
            "Position to close",
            existing.is_some(),
            match existing {
                Some(p) => format!("{} {}", side_text(p.side), p.quantity),
                None => format!("no open {} position", input.symbol),
            },
        );
        return Ok(None);
    }

    let quote = input.quote.as_ref();
    // No quote is an infinite age, which never satisfies the limit.
    let quote_age_sec = quote.map_or(f64::INFINITY, |q| (input.now - q.ts) as f64 / 1000.0);
    let fresh = checks.check(
        "freshness",
        "Market data fresh",
        quote_age_sec <= settings.max_quote_age_sec,
        match quote {
            Some(_) => format!(
                "quote age {}s, limit {}s",
                js_to_fixed(quote_age_sec, 1),
                js_number_string(settings.max_quote_age_sec)
            ),
            None => "no quote available".to_string(),
        },
    );

    let day_base = Dec::parse(&input.day_start_equity)?;
    let loss_limit = day_base
        .mul(&number(settings.max_daily_loss_pct))
        .div(&number(100.0))?;
    let daily_pnl = Dec::parse(&input.daily_pnl)?;
    checks.check(
        "daily-loss",
        "Daily loss limit",
        daily_pnl.gt(&loss_limit.neg()),
        format!(
            "today {} USDT, limit -{} USDT",
            daily_pnl.to_fixed_dp(2),
            loss_limit.to_fixed_dp(2)
        ),
    );

    checks.check(
        "max-positions",
        "Open position limit",
        (open_positions.len() as f64) < settings.max_open_positions,
        format!(
            "{} open, limit {}",
            open_positions.len(),
            js_number_string(settings.max_open_positions)
        ),
    );

    checks.check(
        "no-existing",
        "No existing position",
        existing.is_none(),
        match existing {
            Some(p) => format!(
                "already {}; CLOSE first (no pyramiding or flipping)",
                side_text(p.side)
            ),
            None => "flat on this symbol".to_string(),
        },
    );

    let quote = match quote {
        Some(q) if fresh => q,
        _ => return Ok(None),
    };

    let side = if decision.action == AiAction::Long {
        PositionSide::Long
    } else {
        PositionSide::Short
    };
    let reference = match (decision.order_type, decision.entry_price) {
        (EntryOrderType::Limit, Some(price)) => number(price),
        _ => Dec::parse(if side == PositionSide::Long {
            &quote.ask
        } else {
            &quote.bid
        })?,
    };

    match decision.stop_loss {
        None => {
            checks.check(
                "stop-loss",
                "Stop loss present",
                !settings.require_stop_loss,
                if settings.require_stop_loss {
                    "new positions must carry a stop loss".to_string()
                } else {
                    "not required by settings".to_string()
                },
            );
        }
        Some(stop) => {
            checks.check(
                "stop-loss",
                "Stop loss valid",
                stop_on_correct_side(side, &reference, stop),
                format!(
                    "stop {} vs entry {} for {}",
                    js_number_string(stop),
                    reference.to_fixed(),
                    side_text(side)
                ),
            );
        }
    }
    if let Some(target) = decision.take_profit {
        let tp = number(target);
        let valid = if side == PositionSide::Long {
            tp.gt(&reference)
        } else {
            tp.lt(&reference)
        };
        checks.check(
            "take-profit",
            "Take profit valid",
            valid,
            format!(
                "target {} vs entry {} for {}",
                js_number_string(target),
                reference.to_fixed(),
                side_text(side)
            ),
        );
    }

    if checks.any_failed() {
        return Ok(None);
    }

    let instrument = instruments
        .get(&input.symbol)
        .ok_or_else(|| Unevaluable::Invalid(format!("Unknown instrument {}", input.symbol)))?;
    let leverage = 1.0_f64.max(
        settings
            .max_leverage
            .min(f64::from(instrument.max_leverage))
            .floor(),
    );
    let leverage_dec = number(leverage);
    let equity = Dec::parse(&balance.equity)?;
    let risk_budget = equity
        .mul(&number(settings.max_risk_per_trade_pct))
        .div(&number(100.0))?;

    // Round-trip taker fees plus slippage count against the budget, not just the stop distance.
    let two = number(2.0);
    let slippage = Dec::parse(&input.fees.slippage_bps)?
        .mul(&two)
        .div(&number(10_000.0))?;
    let cost_per_unit = reference.mul(
        &Dec::parse(&input.fees.taker_fee_rate)?
            .mul(&two)
            .add(&slippage),
    );
    let stop_distance = match decision.stop_loss {
        None => reference.mul(&Dec::parse(ASSUMED_STOP_FRACTION)?),
        Some(stop) => abs(reference.sub(&number(stop))),
    };
    let risk_qty = risk_budget.div(&stop_distance.add(&cost_per_unit))?;
    let available = Dec::parse(&balance.available_balance)?;
    let margin_qty = available
        .mul(&Dec::parse(MARGIN_SAFETY)?)
        .mul(&leverage_dec)
        .div(&reference)?;
    let notional_qty = number(settings.max_notional).div(&reference)?;
    let leverage_qty = equity.mul(&leverage_dec).div(&reference)?;
    let quantity = risk_qty
        .min(&margin_qty)
        .min(&notional_qty.min(&leverage_qty))
        .floor_to_step(&Dec::parse(&instrument.quantity_step)?)?;

    let notional = perp::notional(&quantity, &reference);
    let margin = perp::initial_margin(&notional, leverage as u32)?;
    let sized = checks.check(
        "sizing",
        "Position sized",
        quantity.gte(&Dec::parse(&instrument.min_quantity)?),
        format!(
            "risk {} USDT over {} stop distance gives {} {} (min {})",
            risk_budget.to_fixed_dp(2),
            stop_distance.to_fixed(),
            quantity.to_fixed(),
            instrument.base_currency,
            instrument.min_quantity
        ),
    );
    if !sized {
        return Ok(None);
    }
    checks.check(
        "margin",
        "Sufficient margin",
        margin.lte(&available),
        format!(
            "needs {} USDT, available {} USDT",
            margin.to_fixed_dp(2),
            available.to_fixed_dp(2)
        ),
    );
    checks.check(
        "leverage",
        "Leverage limit",
        leverage <= settings.max_leverage,
        format!(
            "{}x, limit {}x",
            js_number_string(leverage),
            js_number_string(settings.max_leverage)
        ),
    );
    checks.check(
        "notional",
        "Notional limit",
        notional.lte(&number(settings.max_notional)),
        format!(
            "{} USDT, limit {} USDT",
            notional.to_fixed_dp(2),
            js_number_string(settings.max_notional)
        ),
    );

    let liquidation = perp::liquidation_price(
        side,
        &reference,
        &quantity,
        &margin,
        &Dec::parse(&instrument.maintenance_margin_rate)?,
    )?;
    if let Some(stop) = decision.stop_loss {
        let stop_dec = number(stop);
        let stop_before_liq = if side == PositionSide::Long {
            stop_dec.gt(&liquidation)
        } else {
            stop_dec.lt(&liquidation)
        };
        checks.check(
            "stop-before-liq",
            "Stop triggers before liquidation",
            stop_before_liq,
            format!(
                "stop {}, estimated liquidation {}",
                js_number_string(stop),
                liquidation.to_fixed_dp(2)
            ),
        );
    }

    Ok(Some(SizedTrade {
        quantity: quantity.to_fixed(),
        leverage: leverage as u32,
        notional: notional.to_fixed(),
        margin: margin.to_fixed(),
        risk_amount: risk_budget.to_fixed(),
        reference_price: reference.to_fixed(),
        estimated_liquidation_price: liquidation.to_fixed(),
    }))
}

/// `d(number)` in TypeScript: a JS number converted through its shortest decimal text.
fn number(x: f64) -> Dec {
    Dec::from_js_number(x)
}

/// `Dec` has no `abs`; a stop distance only needs it for one subtraction.
fn abs(x: Dec) -> Dec {
    if x.is_negative() {
        x.neg()
    } else {
        x
    }
}

fn side_text(side: PositionSide) -> &'static str {
    match side {
        PositionSide::Long => "LONG",
        PositionSide::Short => "SHORT",
    }
}

fn stop_on_correct_side(side: PositionSide, reference: &Dec, stop: f64) -> bool {
    let stop = number(stop);
    match side {
        PositionSide::Long => stop.lt(reference),
        PositionSide::Short => stop.gt(reference),
    }
}

// `Date.parse` and `Date#toISOString` for the one shape the decision schema allows
// (`z.iso.datetime({ offset: true })`). The workspace has no date crate and this is all we need.

/// Milliseconds since the Unix epoch for `YYYY-MM-DDTHH:MM:SS[.fff](Z|+HH:MM|-HH:MM)`.
/// Fractional digits beyond milliseconds are truncated, as V8 does.
fn parse_iso_datetime(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    let digits = |from: usize, len: usize| -> Option<i64> {
        let part = b.get(from..from + len)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    let expect = |at: usize, ch: u8| (b.get(at) == Some(&ch)).then_some(());

    let year = digits(0, 4)?;
    expect(4, b'-')?;
    let month = digits(5, 2)?;
    expect(7, b'-')?;
    let day = digits(8, 2)?;
    expect(10, b'T')?;
    let hour = digits(11, 2)?;
    expect(13, b':')?;
    let minute = digits(14, 2)?;
    expect(16, b':')?;
    let second = digits(17, 2)?;
    let mut at = 19;
    let mut millis = 0;
    if b.get(at) == Some(&b'.') {
        let start = at + 1;
        let mut end = start;
        while b.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end == start {
            return None;
        }
        for (i, digit) in b[start..end].iter().take(3).enumerate() {
            millis += i64::from(digit - b'0') * 10_i64.pow(2 - i as u32);
        }
        at = end;
    }
    let offset_minutes = match b.get(at)? {
        b'Z' => {
            at += 1;
            0
        }
        sign @ (b'+' | b'-') => {
            let hours = digits(at + 1, 2)?;
            expect(at + 3, b':')?;
            let minutes = digits(at + 4, 2)?;
            at += 6;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let total = hours * 60 + minutes;
            if *sign == b'+' {
                total
            } else {
                -total
            }
        }
        _ => return None,
    };
    if at != b.len() || !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1_000 + millis)
}

/// `new Date(ms).toISOString()`.
fn iso_string(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let in_day = ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let year_text = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{}{:06}", if year < 0 { '-' } else { '+' }, year.abs())
    };
    format!(
        "{year_text}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1_000 % 60,
        in_day % 1_000
    )
}

fn days_in_month(year: i64, month: i64) -> i64 {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_datetimes_round_trip_like_javascript() {
        assert_eq!(parse_iso_datetime("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso_datetime("1970-01-01T00:00:02.123+02:00"),
            Some(-7_197_877)
        );
        assert_eq!(
            parse_iso_datetime("2024-02-29T12:34:56.789Z"),
            Some(1_709_210_096_789)
        );
        assert_eq!(parse_iso_datetime("2023-02-29T00:00:00Z"), None);
        assert_eq!(parse_iso_datetime("2024-01-01T00:00:00"), None);
        assert_eq!(parse_iso_datetime("not a date"), None);
        assert_eq!(iso_string(1_709_210_096_789), "2024-02-29T12:34:56.789Z");
        assert_eq!(iso_string(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(
            iso_string(-62_198_755_200_000),
            "-000001-01-01T00:00:00.000Z"
        );
    }

    /// TypeScript throws on these; the gate must reject instead of panicking.
    #[test]
    fn unevaluable_input_fails_closed() {
        let text = include_str!("../../golden/risk.json");
        let cases: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        let instruments = Instruments::with_defaults();
        let mut input: RiskInput = serde_json::from_value(cases[0]["input"].clone()).unwrap();
        input.day_start_equity = "oops".to_string();
        let result = evaluate_risk(&input, &instruments);
        assert!(!result.approved);
        assert_eq!(result.checks.last().unwrap().id, "input");

        let mut input: RiskInput = serde_json::from_value(cases[0]["input"].clone()).unwrap();
        input.decision.expires_at = Some("tomorrow".to_string());
        assert!(!evaluate_risk(&input, &instruments).approved);

        let mut input: RiskInput = serde_json::from_value(cases[0]["input"].clone()).unwrap();
        input.quote.as_mut().unwrap().ask = "0".to_string();
        assert!(!evaluate_risk(&input, &instruments).approved);
    }
}
