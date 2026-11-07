//! JavaScript-exact number behaviour, so the Rust replay matches the TypeScript one bit for bit.

/// `Math.round`: ties go toward +infinity, so -2.5 becomes -2 and 2.5 becomes 3.
/// Rust's `f64::round` rounds ties away from zero, which differs for negatives.
pub fn js_round(x: f64) -> f64 {
    let f = x.floor();
    // `x - f` can round for tiny negatives (e.g. -0.49999999999999994), but rounding is
    // monotonic and 0.5 is exactly representable, so the comparison with 0.5 is still right
    // (unlike `(x + 0.5).floor()`, which is wrong for 0.49999999999999994).
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// `Number.prototype.toFixed` for `|x| < 1e21` and `digits <= 20`.
pub fn js_to_fixed(x: f64, digits: usize) -> String {
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
