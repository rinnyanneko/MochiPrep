//! Decimal numbers that reproduce decimal.js as the TypeScript accounting code configures it
//! (`packages/core/src/decimal.ts`): 50 significant digits, half-even rounding, and `toFixed()`
//! for storage. Stored money strings must match Node's byte for byte, so the rules here follow
//! decimal.js, not what would be tidiest.
//!
//! Only arithmetic rounds. Parsing and `neg` keep every digit they are given, as decimal.js does.

use num_bigint::{BigInt, BigUint, Sign};
use num_traits::Zero;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// Significant digits kept by every arithmetic result (`precision: 50` in decimal.ts).
const PRECISION: usize = 50;

/// Largest exponent `parse` accepts. decimal.js allows far more, but a stored `1e9000000000000000`
/// would make `to_fixed` try to build a string that long; no money value is anywhere near this.
const MAX_PARSED_EXPONENT: i64 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecError {
    #[error("not a decimal number")]
    Invalid,
    #[error("division by zero")]
    DivideByZero,
}

/// `(-1)^neg * mag * 10^exp`. Negative zero is representable because decimal.js has one and it
/// shows up in arithmetic (`-0 * 5`). The coefficient never ends in a zero digit, and a zero has
/// exponent 0, so equal values have one representation.
#[derive(Clone)]
pub struct Dec {
    neg: bool,
    mag: BigUint,
    exp: i64,
}

/// How a rounding step treats the digits it drops.
#[derive(Clone, Copy)]
enum Mode {
    HalfEven,
    Floor,
    Ceil,
}

/// What `drop_digits` removed from a coefficient.
struct Dropped {
    kept: BigUint,
    /// The dropped part compared with exactly one half of the last kept digit.
    half: Ordering,
    /// Whether anything non-zero was dropped.
    inexact: bool,
}

fn pow10(k: usize) -> BigUint {
    BigUint::from(10u32).pow(u32::try_from(k).expect("exponent fits in u32"))
}

fn digit_count(n: &BigUint) -> usize {
    if n.is_zero() {
        1
    } else {
        n.to_string().len()
    }
}

/// Removes the last `k` digits. `sticky` says the true value has a further non-zero tail below
/// them, which only matters to break a tie: an exact half plus a tail is above half.
fn drop_digits(mag: &BigUint, k: usize, sticky: bool) -> Dropped {
    let base = pow10(k);
    let kept = mag / &base;
    let rest = mag % &base;
    let mut half = (rest.clone() * 2u32).cmp(&base);
    if half == Ordering::Equal && sticky {
        half = Ordering::Greater;
    }
    Dropped {
        kept,
        half,
        inexact: !rest.is_zero() || sticky,
    }
}

/// Half-even: round up when above half, or at exactly half with an odd last kept digit.
fn round_half_even(dropped: Dropped) -> BigUint {
    let odd = dropped.kept.bit(0);
    let up = dropped.half == Ordering::Greater || (dropped.half == Ordering::Equal && odd);
    if up {
        dropped.kept + 1u32
    } else {
        dropped.kept
    }
}

impl Dec {
    /// Builds a value from raw parts, stripping trailing zeros so representations are unique.
    fn new(neg: bool, mag: BigUint, exp: i64) -> Dec {
        if mag.is_zero() {
            return Dec { neg, mag, exp: 0 };
        }
        let text = mag.to_string();
        let zeros = text.len() - text.trim_end_matches('0').len();
        if zeros == 0 {
            return Dec { neg, mag, exp };
        }
        Dec {
            neg,
            mag: mag / pow10(zeros),
            exp: exp + zeros as i64,
        }
    }

    /// Rounds an exact result (plus an optional sticky tail) to `PRECISION` significant digits.
    fn rounded(neg: bool, mag: BigUint, exp: i64, sticky: bool) -> Dec {
        let digits = digit_count(&mag);
        if digits <= PRECISION {
            return Dec::new(neg, mag, exp);
        }
        let extra = digits - PRECISION;
        let kept = round_half_even(drop_digits(&mag, extra, sticky));
        Dec::new(neg, kept, exp + extra as i64)
    }

    /// Rounds to a multiple of `10^target`. Values already that coarse are returned unchanged.
    /// The sign is kept even when the result is zero, as decimal.js does.
    fn round_to_exp(&self, target: i64, mode: Mode) -> Dec {
        if self.exp >= target || self.mag.is_zero() {
            return self.clone();
        }
        let dropped = drop_digits(&self.mag, (target - self.exp) as usize, false);
        // Floor grows a negative's magnitude, Ceil grows a positive's.
        let up = match mode {
            Mode::HalfEven => {
                dropped.half == Ordering::Greater
                    || (dropped.half == Ordering::Equal && dropped.kept.bit(0))
            }
            Mode::Floor => self.neg && dropped.inexact,
            Mode::Ceil => !self.neg && dropped.inexact,
        };
        let mag = if up {
            dropped.kept + 1u32
        } else {
            dropped.kept
        };
        Dec::new(self.neg, mag, target)
    }

    /// `new Decimal(s)`: an optional sign, digits with an optional fraction, an optional `e` exponent.
    pub fn parse(s: &str) -> Result<Dec, DecError> {
        let (neg, rest) = match s.as_bytes().first() {
            Some(b'-') => (true, &s[1..]),
            Some(b'+') => (false, &s[1..]),
            _ => (false, s),
        };
        let (number, exp_text) = match rest.find(['e', 'E']) {
            Some(at) => (&rest[..at], Some(&rest[at + 1..])),
            None => (rest, None),
        };
        let (int_part, frac_part) = number.split_once('.').unwrap_or((number, ""));
        let all_digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
        if int_part.len() + frac_part.len() == 0 || !all_digits(int_part) || !all_digits(frac_part)
        {
            return Err(DecError::Invalid);
        }
        let mut exp = match exp_text {
            None => 0,
            Some(t) => {
                let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
                if digits.is_empty() || !all_digits(digits) {
                    return Err(DecError::Invalid);
                }
                // Longer than 7 digits is already past the cap; avoid overflowing the parse.
                let value: i64 = if digits.len() > 7 {
                    MAX_PARSED_EXPONENT + 1
                } else {
                    digits.parse().unwrap()
                };
                if value > MAX_PARSED_EXPONENT {
                    return Err(DecError::Invalid);
                }
                if t.starts_with('-') {
                    -value
                } else {
                    value
                }
            }
        };
        exp -= frac_part.len() as i64;
        let mag =
            BigUint::from_str(&format!("{int_part}{frac_part}")).map_err(|_| DecError::Invalid)?;
        Ok(Dec::new(neg, mag, exp))
    }

    /// `new Decimal(x)` for a JS number: decimal.js reads the shortest round-trip text of the
    /// double, which is what Rust's `{}` prints (without an exponent). Non-finite input has no
    /// decimal value, so it is a caller bug.
    pub fn from_js_number(x: f64) -> Dec {
        assert!(x.is_finite(), "from_js_number needs a finite number");
        Dec::parse(&format!("{x}")).expect("a finite f64 prints as a decimal")
    }

    fn signed(&self) -> BigInt {
        let sign = if self.mag.is_zero() {
            Sign::NoSign
        } else if self.neg {
            Sign::Minus
        } else {
            Sign::Plus
        };
        BigInt::from_biguint(sign, self.mag.clone())
    }

    pub fn add(&self, other: &Dec) -> Dec {
        // Align both to the smaller exponent so the sum is exact, then round once.
        let exp = self.exp.min(other.exp);
        let a = self.signed() * BigInt::from(pow10((self.exp - exp) as usize));
        let b = other.signed() * BigInt::from(pow10((other.exp - exp) as usize));
        let sum = a + b;
        // An exact cancellation is +0; only -0 + -0 stays negative.
        let neg = if sum.is_zero() {
            self.neg && other.neg
        } else {
            sum.sign() == Sign::Minus
        };
        Dec::rounded(neg, sum.magnitude().clone(), exp, false)
    }

    pub fn sub(&self, other: &Dec) -> Dec {
        self.add(&other.neg())
    }

    pub fn mul(&self, other: &Dec) -> Dec {
        Dec::rounded(
            self.neg != other.neg,
            &self.mag * &other.mag,
            self.exp + other.exp,
            false,
        )
    }

    /// Correctly rounded quotient to 50 significant digits, half-even.
    pub fn div(&self, other: &Dec) -> Result<Dec, DecError> {
        if other.mag.is_zero() {
            return Err(DecError::DivideByZero);
        }
        let neg = self.neg != other.neg;
        if self.mag.is_zero() {
            return Ok(Dec::new(neg, BigUint::zero(), 0));
        }
        // Scale the numerator until the integer quotient has at least 52 digits: two more than
        // we keep, so the dropped part below always contains the quotient's own last digit.
        let shift =
            (PRECISION + 2 + digit_count(&other.mag)).saturating_sub(digit_count(&self.mag));
        let scaled = &self.mag * pow10(shift);
        let quotient = &scaled / &other.mag;
        let remainder = &scaled % &other.mag;
        // The remainder is the part of the true quotient below the integer one: a sticky bit.
        let exp = self.exp - other.exp - shift as i64;
        Ok(Dec::rounded(neg, quotient, exp, !remainder.is_zero()))
    }

    pub fn neg(&self) -> Dec {
        Dec {
            neg: !self.neg,
            mag: self.mag.clone(),
            exp: self.exp,
        }
    }

    pub fn floor(&self) -> Dec {
        self.round_to_exp(0, Mode::Floor)
    }

    pub fn ceil(&self) -> Dec {
        self.round_to_exp(0, Mode::Ceil)
    }

    /// Rounds to an integer, half-even (decimal.js `round()` uses the configured mode, not half-up).
    pub fn round(&self) -> Dec {
        self.round_to_exp(0, Mode::HalfEven)
    }

    pub fn to_decimal_places(&self, dp: u32) -> Dec {
        self.round_to_exp(-i64::from(dp), Mode::HalfEven)
    }

    fn sign_prefix(&self) -> &'static str {
        if self.neg && !self.mag.is_zero() {
            "-"
        } else {
            ""
        }
    }

    /// `toFixed()`: plain notation, no exponent, no trailing zeros. Zero, even `-0`, is "0".
    pub fn to_fixed(&self) -> String {
        let digits = if self.mag.is_zero() {
            "0".to_string()
        } else {
            self.mag.to_string()
        };
        let body = if self.exp >= 0 && !self.mag.is_zero() {
            format!("{digits}{}", "0".repeat(self.exp as usize))
        } else if self.exp < 0 {
            let frac_len = (-self.exp) as usize;
            if frac_len >= digits.len() {
                format!("0.{}{digits}", "0".repeat(frac_len - digits.len()))
            } else {
                let (int_part, frac_part) = digits.split_at(digits.len() - frac_len);
                format!("{int_part}.{frac_part}")
            }
        } else {
            digits
        };
        format!("{}{body}", self.sign_prefix())
    }

    /// `toFixed(dp)`: half-even to `dp` places, always showing `dp` places. The minus sign follows
    /// the value before rounding, so `-0.001` at 2 places is `-0.00`, as in decimal.js.
    pub fn to_fixed_dp(&self, dp: u32) -> String {
        let rounded = self.round_to_exp(-i64::from(dp), Mode::HalfEven);
        let dp = dp as usize;
        // The coefficient as an integer count of 10^-dp units.
        let units = &rounded.mag * pow10((rounded.exp + dp as i64) as usize);
        let mut digits = units.to_string();
        if digits.len() <= dp {
            digits = format!("{}{digits}", "0".repeat(dp + 1 - digits.len()));
        }
        let (int_part, frac_part) = digits.split_at(digits.len() - dp);
        let point = if dp == 0 { "" } else { "." };
        format!("{}{int_part}{point}{frac_part}", self.sign_prefix())
    }

    pub fn is_zero(&self) -> bool {
        self.mag.is_zero()
    }

    pub fn is_negative(&self) -> bool {
        self.neg
    }

    /// Numeric ordering: `-0` equals `0`.
    pub fn cmp_value(&self, other: &Dec) -> Ordering {
        let diff = self.sub_exact(other);
        diff.sign().cmp(&Sign::NoSign)
    }

    /// Exact difference, unrounded, for comparisons.
    fn sub_exact(&self, other: &Dec) -> BigInt {
        let exp = self.exp.min(other.exp);
        let a = self.signed() * BigInt::from(pow10((self.exp - exp) as usize));
        let b = other.signed() * BigInt::from(pow10((other.exp - exp) as usize));
        a - b
    }

    pub fn lt(&self, other: &Dec) -> bool {
        self.cmp_value(other) == Ordering::Less
    }

    pub fn lte(&self, other: &Dec) -> bool {
        self.cmp_value(other) != Ordering::Greater
    }

    pub fn gt(&self, other: &Dec) -> bool {
        self.cmp_value(other) == Ordering::Greater
    }

    pub fn gte(&self, other: &Dec) -> bool {
        self.cmp_value(other) != Ordering::Less
    }

    pub fn eq_value(&self, other: &Dec) -> bool {
        self.cmp_value(other) == Ordering::Equal
    }

    /// `min` in decimal.ts: `a.lt(b) ? a : b`, so equal values return `other`.
    pub fn min(&self, other: &Dec) -> Dec {
        if self.lt(other) {
            self.clone()
        } else {
            other.clone()
        }
    }

    /// `max` in decimal.ts: `a.gt(b) ? a : b`, so equal values return `other`.
    pub fn max(&self, other: &Dec) -> Dec {
        if self.gt(other) {
            self.clone()
        } else {
            other.clone()
        }
    }

    /// `v.div(step).floor().mul(step)`: each stage rounds to 50 digits as in decimal.ts.
    pub fn floor_to_step(&self, step: &Dec) -> Result<Dec, DecError> {
        Ok(self.div(step)?.floor().mul(step))
    }

    pub fn ceil_to_step(&self, step: &Dec) -> Result<Dec, DecError> {
        Ok(self.div(step)?.ceil().mul(step))
    }

    pub fn round_to_step(&self, step: &Dec) -> Result<Dec, DecError> {
        Ok(self.div(step)?.round().mul(step))
    }
}

impl fmt::Display for Dec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_fixed())
    }
}

impl fmt::Debug for Dec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Dec({})", self.to_fixed())
    }
}

impl FromStr for Dec {
    type Err = DecError;

    fn from_str(s: &str) -> Result<Dec, DecError> {
        Dec::parse(s)
    }
}

/// Stored and sent as a JSON string in `toFixed()` form, like every money value in the TS backend.
impl Serialize for Dec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Dec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Dec, D::Error> {
        let text = String::deserialize(deserializer)?;
        Dec::parse(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Dec {
        s.parse().unwrap()
    }

    #[test]
    fn rejects_what_decimal_js_rejects() {
        for bad in [
            "", "-", ".", "e5", "1e", "1e+", "1.2.3", "abc", "1 ", "0x10", "Infinity", "NaN",
        ] {
            assert_eq!(Dec::parse(bad).unwrap_err(), DecError::Invalid, "{bad:?}");
        }
        assert_eq!(d(".5").to_fixed(), "0.5");
        assert_eq!(d("5.").to_fixed(), "5");
        assert_eq!(d("+5E+3").to_fixed(), "5000");
    }

    #[test]
    fn division_by_zero_is_an_error() {
        assert_eq!(d("1").div(&d("0")).unwrap_err(), DecError::DivideByZero);
        assert_eq!(d("0").div(&d("-0")).unwrap_err(), DecError::DivideByZero);
    }

    #[test]
    fn negative_zero_survives_where_decimal_js_keeps_it() {
        assert!(d("-0").is_negative());
        assert!(d("-0").mul(&d("5")).is_negative());
        assert!(!d("5").sub(&d("5")).is_negative());
        assert!(d("-0").eq_value(&d("0")));
        assert_eq!(d("-0").to_fixed(), "0");
    }

    #[test]
    fn min_and_max_prefer_the_second_on_ties() {
        assert_eq!(d("1").min(&d("1.0")).to_fixed(), "1");
        assert_eq!(d("2").max(&d("3")).to_fixed(), "3");
        assert_eq!(d("2").min(&d("3")).to_fixed(), "2");
    }

    #[test]
    fn serde_round_trips_as_a_string() {
        let json = serde_json::to_string(&d("-12.50")).unwrap();
        assert_eq!(json, "\"-12.5\"");
        assert_eq!(
            serde_json::from_str::<Dec>(&json).unwrap().to_fixed(),
            "-12.5"
        );
        assert!(serde_json::from_str::<Dec>("12.5").is_err());
    }
}
