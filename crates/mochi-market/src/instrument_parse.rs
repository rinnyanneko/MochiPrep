//! `parseOkxInstrument` (packages/market-data/src/okx.ts): which OKX SWAP contracts MochiPrep
//! may trade, and the spec each one trades under. Checked against `crates/golden/okx-public.json`.

use mochi_decimal::Dec;
use mochi_domain::jsnum::js_string_to_number;
use mochi_domain::InstrumentSpecification;
use serde_json::Value;

/// Inferred from OKX data, not documentation: 1 is crypto, 3 stocks, 4 commodities.
const CRYPTO_CATEGORY: &str = "1";

/// A contract value or lot size decimal.js could not read. Node throws this out of
/// `listInstruments`, so one bad row fails the whole list rather than being guessed around.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("[DecimalError] Invalid argument: {0}")]
pub struct BadDecimal(pub String);

/// A text field of an OKX row. OKX sends every field as a string; anything else counts as absent.
fn field<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

fn dec(text: &str) -> Result<Dec, BadDecimal> {
    Dec::parse(text).map_err(|_| BadDecimal(text.to_string()))
}

/// Turns an OKX SWAP row into a spec, or `None` if MochiPrep must not trade it. Only live, linear,
/// USDT-settled crypto contracts qualify: pre-market, stock and commodity contracts have trading
/// rules the paper engine and risk checks do not model.
pub fn parse_okx_instrument(row: &Value) -> Result<Option<InstrumentSpecification>, BadDecimal> {
    // /^([A-Z0-9]+)-USDT-SWAP$/
    let Some(base) = field(row, "instId").and_then(|id| id.strip_suffix("-USDT-SWAP")) else {
        return Ok(None);
    };
    if base.is_empty()
        || !base
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return Ok(None);
    }
    let is = |key: &str, want: &str| field(row, key) == Some(want);
    let eligible = is("settleCcy", "USDT")
        && is("ctType", "linear")
        && is("state", "live")
        && is("ruleType", "normal")
        && is("instCategory", CRYPTO_CATEGORY)
        && is("ctValCcy", base);
    // `!row.ctVal`: missing and empty are both refused.
    let present = |key: &str| field(row, key).filter(|text| !text.is_empty());
    let (Some(ct_val), Some(lot_sz), Some(min_sz), Some(tick_sz)) = (
        present("ctVal"),
        present("lotSz"),
        present("minSz"),
        present("tickSz"),
    ) else {
        return Ok(None);
    };
    if !eligible {
        return Ok(None);
    }
    // Node's field order, so the same argument is named when two are bad.
    let quantity_step = dec(ct_val)?.mul(&dec(lot_sz)?).to_fixed();
    let min_quantity = dec(ct_val)?.mul(&dec(min_sz)?).to_fixed();
    let Some(max_leverage) = max_leverage(field(row, "lever")) else {
        return Ok(None);
    };
    Ok(Some(InstrumentSpecification {
        symbol: format!("{base}USDT"),
        market_type: "CRYPTO_PERP".to_string(),
        base_currency: base.to_string(),
        quote_currency: "USDT".to_string(),
        settlement_currency: "USDT".to_string(),
        contract_size: "1".to_string(),
        quantity_step,
        min_quantity,
        price_tick: tick_sz.to_string(),
        max_leverage,
        maintenance_margin_rate: "0.005".to_string(),
        funding_interval_hours: 8,
        venue_contract_value: ct_val.to_string(),
        venue_lot_size: lot_sz.to_string(),
    }))
}

/// `Number(row.lever) || 1`. OKX sends whole numbers; a fractional, negative or huge value, which
/// Node would pass through as is, has no `u32` form, so the contract is refused instead (`None`).
fn max_leverage(lever: Option<&str>) -> Option<u32> {
    let value = lever.map_or(f64::NAN, js_string_to_number);
    if value.is_nan() || value == 0.0 {
        return Some(1);
    }
    if value.fract() == 0.0 && value >= 1.0 && value <= f64::from(u32::MAX) {
        Some(value as u32)
    } else {
        None
    }
}

/// `listInstruments`: every tradable contract in OKX's list, sorted by symbol. Symbols are
/// upper-case letters and digits, where JS `localeCompare` and byte order agree.
pub fn parse_okx_instruments(rows: &[Value]) -> Result<Vec<InstrumentSpecification>, BadDecimal> {
    let mut specs = Vec::new();
    for row in rows {
        if let Some(spec) = parse_okx_instrument(row)? {
            specs.push(spec);
        }
    }
    specs.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    Ok(specs)
}
