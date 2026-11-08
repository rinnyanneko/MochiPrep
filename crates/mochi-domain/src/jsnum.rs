//! JavaScript-exact number behaviour, so the Rust replay matches the TypeScript one bit for bit.

/// `Math.round`: ties go toward +infinity, so -2.5 becomes -2 and 2.5 becomes 3.
/// Rust's `f64::round` rounds ties away from zero, which differs for negatives.
///
/// Like JS it returns -0 for -0.5 <= x <= -0. No output can show that sign (JSON.stringify,
/// toFixed and String all print 0), but keeping it means the value is bit-identical to Node's.
pub fn js_round(x: f64) -> f64 {
    let f = x.floor();
    // `x - f` can round for tiny negatives (e.g. -0.49999999999999994), but rounding is
    // monotonic and 0.5 is exactly representable, so the comparison with 0.5 is still right
    // (unlike `(x + 0.5).floor()`, which is wrong for 0.49999999999999994).
    let rounded = if x - f >= 0.5 { f + 1.0 } else { f };
    // `-1.0 + 1.0` is +0, but Math.round keeps the sign of a negative input that rounds to zero.
    if rounded == 0.0 && x.is_sign_negative() {
        -0.0
    } else {
        rounded
    }
}

/// `Number.prototype.toFixed` for `digits <= 20` (JS throws a RangeError above that).
pub fn js_to_fixed(x: f64, digits: usize) -> String {
    debug_assert!(digits <= 20, "toFixed digits must be 0..=20, got {digits}");
    // JS returns "NaN", "Infinity" and "-Infinity" for non-finite values, and for |x| >= 1e21
    // it returns String(x) (e.g. "1e+21"), which is the JSON.stringify text for those numbers.
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if x.abs() >= 1e21 {
        return js_number_string(x);
    }
    let magnitude = x.abs();
    // Rust formats an f64 with its exact decimal expansion and breaks ties to even; JS toFixed
    // takes the larger magnitude on an exact tie. A tie is exact only when the expansion ends
    // right after a 5 in position digits+1, so check that and round the magnitude up by hand.
    // An f64 has at most 1074 fraction digits, so 1100 digits is always the exact expansion.
    let exact = format!("{magnitude:.1100}");
    let (int_part, frac) = exact
        .split_once('.')
        .expect("a fixed-precision format always has a decimal point");
    let is_tie = frac.as_bytes()[digits] == b'5' && frac[digits + 1..].bytes().all(|b| b == b'0');
    let mut text = if is_tie {
        add_one_in_last_place(int_part, &frac[..digits])
    } else {
        format!("{magnitude:.digits$}")
    };
    // JS puts the sign on whenever x < 0, so -1e-7 gives "-0.000000" while -0 gives "0.00".
    if x < 0.0 {
        text.insert(0, '-');
    }
    text
}

/// Adds one unit in the last place to the decimal number `int_part.frac`, carrying through 9s.
/// With an empty `frac` the result has no decimal point, as `toFixed(0)` has none.
fn add_one_in_last_place(int_part: &str, frac: &str) -> String {
    let mut digits: Vec<u8> = int_part.bytes().chain(frac.bytes()).collect();
    let mut position = digits.len();
    loop {
        if position == 0 {
            // Every digit was a 9 (e.g. 99.5 -> 100): the carry adds a new leading digit.
            digits.insert(0, b'1');
            break;
        }
        position -= 1;
        if digits[position] == b'9' {
            digits[position] = b'0';
        } else {
            digits[position] += 1;
            break;
        }
    }
    let split = digits.len() - frac.len();
    let (whole, fraction) = digits.split_at(split);
    let whole = String::from_utf8_lossy(whole);
    if fraction.is_empty() {
        whole.into_owned()
    } else {
        format!("{whole}.{}", String::from_utf8_lossy(fraction))
    }
}

/// `JSON.stringify(x)` for a number: the shortest round-trip digits laid out by the
/// `Number::toString` rules. Rust's own formatting differs (serde_json writes `3.0` and `1e-6`),
/// so the text is built here from the shortest digits of `{:e}`.
pub fn js_number_string(x: f64) -> String {
    if !x.is_finite() {
        return "null".to_string();
    }
    if x == 0.0 {
        // Covers -0 as well: JSON.stringify(-0) is "0".
        return "0".to_string();
    }
    // `{:e}` gives the shortest digits that round-trip, e.g. "1.5e-6" or "1e21".
    let scientific = format!("{:e}", x.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("`{:e}` output always contains an exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let exponent: i32 = exponent.parse().expect("`{:e}` exponent is an integer");
    // `n` is the position of the decimal point relative to the digits, as in the ECMAScript spec.
    let n = exponent + 1;
    let k = digits.len() as i32;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let sign = if n - 1 < 0 { '-' } else { '+' };
        let power = (n - 1).abs();
        if k == 1 {
            format!("{digits}e{sign}{power}")
        } else {
            format!("{}.{}e{sign}{power}", &digits[..1], &digits[1..])
        }
    };
    if x < 0.0 {
        format!("-{body}")
    } else {
        body
    }
}

/// The same number as raw JSON text. serde_json would re-format an f64 (`1.5e-6`, `3.0`), but a
/// `RawValue` is written through unchanged, which is the simplest way to keep JS formatting.
pub fn js_number_raw(x: f64) -> Box<serde_json::value::RawValue> {
    serde_json::value::RawValue::from_string(js_number_string(x))
        .expect("a JS number string is valid JSON")
}

/// The characters JS `String.prototype.trim` removes (WhiteSpace and LineTerminator). Rust's
/// `trim` uses a different set: it keeps U+FEFF and removes U+0085.
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// JS `String.prototype.trim`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// JS `Number(string)`: trimmed; empty is 0; `Infinity`; `0x`/`0o`/`0b` integers; otherwise a
/// decimal literal with optional sign, fraction and exponent, or NaN.
pub fn js_string_to_number(s: &str) -> f64 {
    let text = js_trim(s);
    if text.is_empty() {
        return 0.0;
    }
    match text {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = match text.get(..2) {
        Some("0x" | "0X") => Some(16),
        Some("0o" | "0O") => Some(8),
        Some("0b" | "0B") => Some(2),
        _ => None,
    };
    if let Some(radix) = radix {
        let digits = &text[2..];
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return f64::NAN;
        }
        // Exact while it fits u128; beyond that the value is far past every bound checked here.
        return match u128::from_str_radix(digits, radix) {
            Ok(n) => n as f64,
            Err(_) => digits.chars().fold(0.0, |acc, c| {
                acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            }),
        };
    }
    if is_js_decimal_literal(text) {
        // Rust parses this grammar with the same correct rounding (and overflow to infinity).
        text.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// `[+-]? (digits [. digits?] | . digits) ([eE] [+-]? digits)?`. Checked first because Rust's
/// parser also accepts words like "inf" and "nan", which JS does not.
fn is_js_decimal_literal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let count_digits = |from: usize| {
        bytes[from..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    let whole = count_digits(i);
    i += whole;
    let mut fraction = 0;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        fraction = count_digits(i);
        i += fraction;
    }
    if whole == 0 && fraction == 0 {
        return false;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exponent = count_digits(i);
        if exponent == 0 {
            return false;
        }
        i += exponent;
    }
    i == bytes.len()
}
