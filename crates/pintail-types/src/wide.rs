//! Exact integers past 128 bits, for decimals wider than `i128` holds.
//!
//! `MySQL` computes DECIMAL results up to 65 digits. The executor keeps
//! scaled `i128` units for everything that fits them and moves here when an
//! operation would overflow. The magnitude is held under 2^511, room for
//! 153 digits, so the product or the scaled dividend of two 65-digit values
//! fits before it is rounded back to a result scale.

use std::cmp::Ordering;
use std::fmt::Write as _;

const LIMBS: usize = 8;

/// Ten to the nineteenth: the largest power of ten in one 64-bit limb, the
/// step formatting divides by.
const CHUNK: u64 = 10_000_000_000_000_000_000;

type Magnitude = [u64; LIMBS];

/// A signed integer with a magnitude under 2^511.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WideInt {
    negative: bool,
    /// Little-endian 64-bit limbs.
    magnitude: Magnitude,
}

fn low(value: u128) -> u64 {
    u64::try_from(value & u128::from(u64::MAX)).expect("masked to one limb")
}

fn high(value: u128) -> u64 {
    u64::try_from(value >> 64).expect("shifted to one limb")
}

fn is_zero(magnitude: &Magnitude) -> bool {
    magnitude.iter().all(|limb| *limb == 0)
}

fn compare(left: &Magnitude, right: &Magnitude) -> Ordering {
    left.iter().rev().cmp(right.iter().rev())
}

/// The magnitude, when it stays under 2^511.
fn bounded(magnitude: Magnitude) -> Option<Magnitude> {
    (magnitude[LIMBS - 1] >> 63 == 0).then_some(magnitude)
}

fn add(left: &Magnitude, right: &Magnitude) -> Option<Magnitude> {
    let mut sum = [0; LIMBS];
    let mut carry = 0_u128;
    for ((out, left), right) in sum.iter_mut().zip(left).zip(right) {
        let total = u128::from(*left) + u128::from(*right) + carry;
        *out = low(total);
        carry = total >> 64;
    }
    if carry == 0 { bounded(sum) } else { None }
}

/// `left - right`, for `left >= right`.
fn subtract(left: &Magnitude, right: &Magnitude) -> Magnitude {
    let mut difference = [0; LIMBS];
    let mut borrow = false;
    for ((out, left), right) in difference.iter_mut().zip(left).zip(right) {
        let (step, first) = left.overflowing_sub(*right);
        let (step, second) = step.overflowing_sub(u64::from(borrow));
        *out = step;
        borrow = first || second;
    }
    difference
}

fn multiply(left: &Magnitude, right: &Magnitude) -> Option<Magnitude> {
    let mut product = [0_u64; 2 * LIMBS];
    for (i, left) in left.iter().enumerate() {
        if *left == 0 {
            continue;
        }
        let mut carry = 0_u128;
        for (j, right) in right.iter().enumerate() {
            let total = u128::from(*left) * u128::from(*right) + u128::from(product[i + j]) + carry;
            product[i + j] = low(total);
            carry = total >> 64;
        }
        let mut k = i + LIMBS;
        while carry != 0 {
            let total = u128::from(product[k]) + carry;
            product[k] = low(total);
            carry = total >> 64;
            k += 1;
        }
    }
    if product[LIMBS..].iter().any(|limb| *limb != 0) {
        return None;
    }
    let mut result = [0; LIMBS];
    result.copy_from_slice(&product[..LIMBS]);
    bounded(result)
}

/// `magnitude * factor + addend`.
fn multiply_add_small(magnitude: &Magnitude, factor: u64, addend: u64) -> Option<Magnitude> {
    let mut result = [0; LIMBS];
    let mut carry = u128::from(addend);
    for (out, limb) in result.iter_mut().zip(magnitude) {
        let total = u128::from(*limb) * u128::from(factor) + carry;
        *out = low(total);
        carry = total >> 64;
    }
    if carry == 0 { bounded(result) } else { None }
}

/// The quotient and remainder of a division by one limb.
fn divide_small(magnitude: &Magnitude, divisor: u64) -> (Magnitude, u64) {
    let mut quotient = [0; LIMBS];
    let mut remainder = 0_u128;
    for (out, limb) in quotient.iter_mut().zip(magnitude).rev() {
        let current = (remainder << 64) | u128::from(*limb);
        *out = low(current / u128::from(divisor));
        remainder = current % u128::from(divisor);
    }
    (quotient, low(remainder))
}

/// The quotient and remainder of a long division, `None` for a zero divisor.
/// Both magnitudes are under 2^511, so doubling the running remainder
/// never overflows.
fn divide(dividend: &Magnitude, divisor: &Magnitude) -> Option<(Magnitude, Magnitude)> {
    if is_zero(divisor) {
        return None;
    }
    let mut quotient = [0; LIMBS];
    let mut remainder = [0; LIMBS];
    for bit in (0..LIMBS * 64).rev() {
        let mut carry = (dividend[bit / 64] >> (bit % 64)) & 1;
        for limb in &mut remainder {
            let next = *limb >> 63;
            *limb = (*limb << 1) | carry;
            carry = next;
        }
        if compare(&remainder, divisor) != Ordering::Less {
            remainder = subtract(&remainder, divisor);
            quotient[bit / 64] |= 1 << (bit % 64);
        }
    }
    Some((quotient, remainder))
}

impl WideInt {
    /// Zero.
    pub const ZERO: Self = Self {
        negative: false,
        magnitude: [0; LIMBS],
    };

    /// A zero magnitude is never negative, so equal values compare equal.
    fn new(negative: bool, magnitude: Magnitude) -> Self {
        Self {
            negative: negative && !is_zero(&magnitude),
            magnitude,
        }
    }

    /// Widens an `i128`.
    #[must_use]
    pub fn from_i128(value: i128) -> Self {
        let magnitude = value.unsigned_abs();
        let mut limbs = [0; LIMBS];
        limbs[0] = low(magnitude);
        limbs[1] = high(magnitude);
        Self::new(value < 0, limbs)
    }

    /// Narrows back to an `i128` when the value fits one.
    #[must_use]
    pub fn to_i128(&self) -> Option<i128> {
        if self.magnitude[2..].iter().any(|limb| *limb != 0) {
            return None;
        }
        let magnitude = u128::from(self.magnitude[0]) | (u128::from(self.magnitude[1]) << 64);
        if self.negative {
            0_i128.checked_sub_unsigned(magnitude)
        } else {
            i128::try_from(magnitude).ok()
        }
    }

    /// Whether the value is zero.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        is_zero(&self.magnitude)
    }

    /// Whether the value is below zero.
    #[must_use]
    pub const fn is_negative(&self) -> bool {
        self.negative
    }

    /// The value with its sign flipped.
    #[must_use]
    pub fn negated(self) -> Self {
        Self::new(!self.negative, self.magnitude)
    }

    /// `self + other`, `None` past the magnitude bound.
    #[must_use]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        if self.negative == other.negative {
            return add(&self.magnitude, &other.magnitude).map(|sum| Self::new(self.negative, sum));
        }
        Some(match compare(&self.magnitude, &other.magnitude) {
            Ordering::Less => {
                Self::new(other.negative, subtract(&other.magnitude, &self.magnitude))
            }
            _ => Self::new(self.negative, subtract(&self.magnitude, &other.magnitude)),
        })
    }

    /// `self - other`, `None` past the magnitude bound.
    #[must_use]
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.checked_add(other.negated())
    }

    /// `self * other`, `None` past the magnitude bound.
    #[must_use]
    pub fn checked_mul(self, other: Self) -> Option<Self> {
        multiply(&self.magnitude, &other.magnitude)
            .map(|product| Self::new(self.negative != other.negative, product))
    }

    /// Ten to `exponent`, `None` past the magnitude bound.
    #[must_use]
    pub fn pow10(exponent: u32) -> Option<Self> {
        let mut magnitude = [0; LIMBS];
        magnitude[0] = 1;
        for _ in 0..exponent {
            magnitude = multiply_add_small(&magnitude, 10, 0)?;
        }
        Some(Self::new(false, magnitude))
    }

    /// `self / divisor` rounded half away from zero, the way `MySQL` rounds a
    /// decimal quotient; `None` for a zero divisor.
    #[must_use]
    pub fn div_round_half_up(self, divisor: Self) -> Option<Self> {
        let (quotient, remainder) = divide(&self.magnitude, &divisor.magnitude)?;
        let doubled = add(&remainder, &remainder)?;
        let quotient = if compare(&doubled, &divisor.magnitude) == Ordering::Less {
            quotient
        } else {
            multiply_add_small(&quotient, 1, 1)?
        };
        Some(Self::new(self.negative != divisor.negative, quotient))
    }

    /// The remainder of `self / divisor`, carrying the dividend's sign as
    /// `MOD` does; `None` for a zero divisor.
    #[must_use]
    pub fn checked_rem(self, divisor: Self) -> Option<Self> {
        let (_, remainder) = divide(&self.magnitude, &divisor.magnitude)?;
        Some(Self::new(self.negative, remainder))
    }

    /// The number of decimal digits in the magnitude; zero has one.
    #[must_use]
    pub fn digits(&self) -> usize {
        self.magnitude_text().len()
    }

    fn magnitude_text(&self) -> String {
        let mut chunks = Vec::new();
        let mut rest = self.magnitude;
        while !is_zero(&rest) {
            let (quotient, remainder) = divide_small(&rest, CHUNK);
            chunks.push(remainder);
            rest = quotient;
        }
        let Some((first, rest)) = chunks.split_last() else {
            return "0".to_owned();
        };
        let mut text = first.to_string();
        for chunk in rest.iter().rev() {
            write!(text, "{chunk:019}").expect("writing to a String cannot fail");
        }
        text
    }
}

impl Ord for WideInt {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => compare(&self.magnitude, &other.magnitude),
            (true, true) => compare(&other.magnitude, &self.magnitude),
        }
    }
}

impl PartialOrd for WideInt {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn parse(text: &str, scale: u8, round: bool) -> Option<WideInt> {
    let bytes = text.as_bytes();
    let (negative, rest) = match bytes.first()? {
        b'-' => (true, &bytes[1..]),
        b'+' => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let mut magnitude = [0; LIMBS];
    let mut fraction_digits: u8 = 0;
    let mut seen_dot = false;
    let mut seen_digit = false;
    let mut round_up = false;
    for &byte in rest {
        match byte {
            b'0'..=b'9' => {
                seen_digit = true;
                let digit = u64::from(byte - b'0');
                if seen_dot && fraction_digits >= scale {
                    if !round && digit != 0 {
                        return None;
                    }
                    if round && fraction_digits == scale {
                        round_up = digit >= 5;
                    }
                    fraction_digits = fraction_digits.checked_add(1)?;
                    continue;
                }
                magnitude = multiply_add_small(&magnitude, 10, digit)?;
                if seen_dot {
                    fraction_digits += 1;
                }
            }
            b'.' if !seen_dot => seen_dot = true,
            _ => return None,
        }
    }
    if !seen_digit {
        return None;
    }
    while fraction_digits < scale {
        magnitude = multiply_add_small(&magnitude, 10, 0)?;
        fraction_digits += 1;
    }
    if round_up {
        magnitude = multiply_add_small(&magnitude, 1, 1)?;
    }
    Some(WideInt::new(negative, magnitude))
}

/// Parses canonical decimal text (`[+-]digits[.digits]`) into units of
/// `10^-scale`: `None` on malformed text, or on a nonzero digit past the
/// scale.
#[must_use]
pub fn parse_decimal_wide(text: &str, scale: u8) -> Option<WideInt> {
    parse(text, scale, false)
}

/// Parses decimal text into units of `10^-scale`, rounding digits past the
/// scale half away from zero, as a cast to a narrower DECIMAL does.
#[must_use]
pub fn parse_decimal_wide_rounded(text: &str, scale: u8) -> Option<WideInt> {
    parse(text, scale, true)
}

/// Formats units of `10^-scale` as canonical decimal text: exactly `scale`
/// fraction digits and one integer digit at least, the inverse of
/// [`parse_decimal_wide`].
#[must_use]
pub fn format_decimal_wide(value: &WideInt, scale: u8) -> String {
    let scale = usize::from(scale);
    let mut digits = value.magnitude_text();
    if digits.len() <= scale {
        digits.insert_str(0, &"0".repeat(scale + 1 - digits.len()));
    }
    let (integer, fraction) = digits.split_at(digits.len() - scale);
    let sign = if value.is_negative() { "-" } else { "" };
    if scale == 0 {
        format!("{sign}{integer}")
    } else {
        format!("{sign}{integer}.{fraction}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str, scale: u8) -> WideInt {
        parse_decimal_wide(text, scale).unwrap_or_else(|| panic!("{text} parses"))
    }

    #[test]
    fn text_round_trips_past_i128() {
        for (text, scale) in [
            ("0", 0),
            ("-0.000", 3),
            (
                "99999999999999999999999999999999999999999999999999999999999999999",
                0,
            ),
            (
                "-9999999999999999999999999999999999999999999999999999999999.9999999",
                7,
            ),
            ("170141183460469231731687303715884105728", 0),
            ("0.005", 3),
        ] {
            let value = wide(text, scale);
            let expected = if text == "-0.000" { "0.000" } else { text };
            assert_eq!(format_decimal_wide(&value, scale), expected);
        }
        assert_eq!(parse_decimal_wide("1.25", 1), None);
        assert_eq!(parse_decimal_wide("1.20", 1), Some(wide("1.2", 1)));
        assert_eq!(
            parse_decimal_wide_rounded("-1.25", 1),
            Some(wide("-1.3", 1))
        );
        assert_eq!(parse_decimal_wide("1e5", 0), None);
        assert_eq!(parse_decimal_wide("", 0), None);
    }

    #[test]
    fn arithmetic_agrees_with_i128_where_both_fit() {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            i128::from(seed >> 1) - (1 << 62)
        };
        for _ in 0..2_000 {
            let (left, right) = (next(), next() / 1_000 + 1);
            let (a, b) = (WideInt::from_i128(left), WideInt::from_i128(right));
            assert_eq!(
                a.checked_add(b).and_then(|v| v.to_i128()),
                Some(left + right)
            );
            assert_eq!(
                a.checked_sub(b).and_then(|v| v.to_i128()),
                Some(left - right)
            );
            assert_eq!(
                a.checked_mul(b).and_then(|v| v.to_i128()),
                Some(left * right)
            );
            assert_eq!(
                a.div_round_half_up(b).and_then(|v| v.to_i128()),
                crate::div_decimal_round_half_up(left, right)
            );
            assert_eq!(
                a.checked_rem(b).and_then(|v| v.to_i128()),
                Some(left % right)
            );
            assert_eq!(a.cmp(&b), left.cmp(&right));
        }
        assert_eq!(WideInt::from_i128(i128::MIN).to_i128(), Some(i128::MIN));
        assert_eq!(WideInt::from_i128(i128::MAX).to_i128(), Some(i128::MAX));
    }

    #[test]
    fn wide_values_multiply_and_divide_exactly() {
        let nines = wide("99999999999999999999999999999999999999", 0);
        let product = nines
            .checked_mul(WideInt::pow10(3).expect("1000"))
            .expect("41 digits");
        assert_eq!(
            format_decimal_wide(&product, 3),
            "99999999999999999999999999999999999999.000"
        );
        assert_eq!(product.digits(), 41);
        let third = wide("10", 0)
            .checked_mul(WideInt::pow10(40).expect("scale"))
            .expect("scaled")
            .div_round_half_up(wide("3", 0))
            .expect("quotient");
        assert_eq!(
            format_decimal_wide(&third, 40),
            "3.3333333333333333333333333333333333333333"
        );
        let two_thirds = wide("-20", 0)
            .checked_mul(WideInt::pow10(2).expect("scale"))
            .and_then(|value| value.div_round_half_up(wide("3", 0)))
            .expect("quotient");
        assert_eq!(format_decimal_wide(&two_thirds, 2), "-6.67");
        assert_eq!(wide("1", 0).div_round_half_up(WideInt::ZERO), None);
        assert!(WideInt::pow10(153).is_some());
        assert!(WideInt::pow10(154).is_none());
    }
}
