//! Rupiah amounts on screen and keypad-style amount entry (`docs/plan.md` §1: `i64` minor units,
//! Rupiah minor unit = 1).
//!
//! - [`rupiah`] formats `1250000` as `Rp 1.250.000` (Indonesian grouping with `.`, no decimals);
//!   negative amounts as `-Rp 5.000`.
//! - [`Digits`] is what the cashier types: digits only, no leading zeros, at most
//!   [`Digits::MAX`] of them, so the value always fits in `i64`. Formatting is applied live by the
//!   screen, so there is no free-text parsing.

/// `1250000` → `1.250.000`.
#[must_use]
pub fn group(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c);
    }
    out
}

/// `1250000` → `Rp 1.250.000`; `-5000` → `-Rp 5.000`.
#[must_use]
pub fn rupiah(minor: i64) -> String {
    let sign = if minor < 0 { "-" } else { "" };
    format!("{sign}Rp {}", group(minor.unsigned_abs()))
}

/// `+Rp 10.000` / `-Rp 5.000` / `Rp 0`: an over/short amount with its sign.
#[must_use]
pub fn signed_rupiah(minor: i64) -> String {
    if minor > 0 {
        format!("+{}", rupiah(minor))
    } else {
        rupiah(minor)
    }
}

/// `1500` basis points → `15%`; `1250` → `12.5%`.
#[must_use]
pub fn percent(basis_points: i64) -> String {
    let whole = basis_points / 100;
    let rest = (basis_points % 100).abs();
    if rest == 0 {
        format!("{whole}%")
    } else if rest % 10 == 0 {
        format!("{whole}.{}%", rest / 10)
    } else {
        format!("{whole}.{rest:02}%")
    }
}

/// Typed digits of a non-negative amount or quantity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Digits {
    text: String,
    max: usize,
}

impl Digits {
    /// Most digits of an amount: 13, i.e. up to Rp 9.999.999.999.999.
    pub const MAX: usize = 13;

    /// Empty, for amounts.
    #[must_use]
    pub const fn amount() -> Self {
        Self::with_max(Self::MAX)
    }

    /// Empty, accepting at most `max` digits.
    #[must_use]
    pub const fn with_max(max: usize) -> Self {
        Self {
            text: String::new(),
            max,
        }
    }

    /// Appends a digit (0–9). Leading zeros are dropped; beyond the maximum the digit is
    /// refused. Returns whether the value changed.
    pub fn push(&mut self, digit: u8) -> bool {
        if digit > 9 || self.text.len() >= self.max || (self.text.is_empty() && digit == 0) {
            return false;
        }
        self.text.push(char::from(b'0' + digit));
        true
    }

    /// Removes the last digit. Returns whether the value changed.
    pub fn pop(&mut self) -> bool {
        self.text.pop().is_some()
    }

    /// Clears the value.
    pub fn clear(&mut self) {
        self.text.clear();
    }

    /// Whether nothing was typed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The value (0 when empty).
    #[must_use]
    pub fn value(&self) -> i64 {
        self.text.parse().unwrap_or(0)
    }

    /// The value as a quantity.
    #[must_use]
    pub fn quantity(&self) -> u32 {
        self.text.parse().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        #[rustfmt::skip]
        let cases = [
            (0, "Rp 0", "Rp 0"),
            (500, "Rp 500", "+Rp 500"),
            (1_000, "Rp 1.000", "+Rp 1.000"),
            (1_250_000, "Rp 1.250.000", "+Rp 1.250.000"),
            (-5_000, "-Rp 5.000", "-Rp 5.000"),
            (i64::MIN, "-Rp 9.223.372.036.854.775.808", "-Rp 9.223.372.036.854.775.808"),
        ];
        for (minor, plain, signed) in cases {
            assert_eq!(rupiah(minor), plain);
            assert_eq!(signed_rupiah(minor), signed);
        }
        let pct = [
            (1_500, "15%"),
            (1_250, "12.5%"),
            (1_205, "12.05%"),
            (0, "0%"),
        ];
        for (bp, text) in pct {
            assert_eq!(percent(bp), text);
        }
    }

    #[test]
    fn typing_digits() {
        let mut d = Digits::amount();
        assert!(!d.push(0), "no leading zero");
        assert!(!d.pop());
        for digit in [5, 0, 0, 0, 0, 0] {
            assert!(d.push(digit));
        }
        assert_eq!(d.value(), 500_000);
        assert!(!d.push(10), "not a digit");
        assert!(d.pop());
        assert_eq!(d.value(), 50_000);
        d.clear();
        assert!(d.is_empty());
        assert_eq!(d.value(), 0);

        let mut d = Digits::amount();
        for _ in 0..Digits::MAX {
            assert!(d.push(9));
        }
        assert!(!d.push(9), "at most 13 digits");
        assert_eq!(d.value(), 9_999_999_999_999);

        let mut q = Digits::with_max(2);
        assert!(q.push(1) && q.push(2) && !q.push(3));
        assert_eq!(q.quantity(), 12);
    }
}
