//! Phone numbers: normalization to `+<digits>` and the masked receipt form (spec 2.5 §2, §10).

use std::fmt;
use std::str::FromStr;

use super::CustomerError;

/// The fewest digits a normalized phone number has (country code included).
pub const MIN_PHONE_DIGITS: usize = 8;
/// The most digits a normalized phone number has (E.164).
pub const MAX_PHONE_DIGITS: usize = 15;

/// Indonesia's country calling code: numbers typed without `+` belong to it.
const INDONESIA: &str = "62";

/// A normalized phone number, stored as `+` followed by 8–15 digits, e.g. `+628123456789`.
///
/// Parsing ([`Phone::parse`]) ignores surrounding spaces and the separators ` `, `-`, `.`, `(`
/// and `)`, then:
/// - `+…` keeps the digits as typed (another country code, e.g. `+65 9123 4567` →
///   `+6591234567`); a country code cannot start with `0`;
/// - `0…` is an Indonesian local number: the `0` becomes `+62` (`0812-3456-789` →
///   `+628123456789`);
/// - `62…` gets a `+` (`62812 3456 789` → `+628123456789`);
/// - any other digits are an Indonesian number typed without the trunk `0` (`812…` → `+62812…`).
///
/// Anything else (letters, a `+` that is not first) or a digit count outside 8–15 is
/// `invalid_phone`. Known limitation: the international prefix `00` is not recognised, so type
/// foreign numbers with `+`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Phone(String);

impl Phone {
    /// Normalizes a typed phone number (see the type's documentation).
    ///
    /// # Errors
    /// [`CustomerError::InvalidPhone`] when the input is not a phone number of 8–15 digits.
    pub fn parse(input: &str) -> Result<Self, CustomerError> {
        let invalid = || CustomerError::InvalidPhone {
            input: input.to_owned(),
        };
        let trimmed = input.trim();
        let (plus, rest) = match trimmed.strip_prefix('+') {
            Some(rest) => (true, rest),
            None => (false, trimmed),
        };
        let mut digits = String::with_capacity(rest.len());
        for c in rest.chars() {
            match c {
                '0'..='9' => digits.push(c),
                ' ' | '-' | '.' | '(' | ')' => {}
                _ => return Err(invalid()),
            }
        }
        let full = if plus {
            if digits.starts_with('0') {
                return Err(invalid());
            }
            digits
        } else if let Some(local) = digits.strip_prefix('0') {
            format!("{INDONESIA}{local}")
        } else if digits.starts_with(INDONESIA) {
            digits
        } else {
            format!("{INDONESIA}{digits}")
        };
        if !(MIN_PHONE_DIGITS..=MAX_PHONE_DIGITS).contains(&full.len()) {
            return Err(invalid());
        }
        Ok(Self(format!("+{full}")))
    }

    /// The stored form, e.g. `+628123456789`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the number is Indonesian (`+62…`).
    #[must_use]
    pub fn is_indonesian(&self) -> bool {
        self.0[1..].starts_with(INDONESIA)
    }

    /// The form people read: Indonesian numbers in the local `0…` form (`08123456789`), others
    /// as stored (`+6591234567`).
    #[must_use]
    pub fn local_form(&self) -> String {
        if self.is_indonesian() {
            format!("0{}", &self.0[1 + INDONESIA.len()..])
        } else {
            self.0.clone()
        }
    }

    /// The masked receipt form (spec 2.5 §10): the first 4 characters and the last 3 digits of
    /// the [local form](Self::local_form) are kept, and the hidden part is printed as one `*` per
    /// digit beyond those 7, so the result has as many characters as the number has digits:
    /// `+628123456789` → `0812****789`, `+6591234567` → `+659***567`.
    ///
    /// The shortest Indonesian numbers (7 digits in the local form) would show every digit that
    /// way, so they keep only their first 4 characters: `+62812345` → `0812***`.
    #[must_use]
    pub fn masked(&self) -> String {
        let local = self.local_form();
        let digits = local.bytes().filter(u8::is_ascii_digit).count();
        let (head, rest) = local.split_at(4);
        if digits > 7 {
            let tail = &rest[rest.len() - 3..];
            format!("{head}{}{tail}", "*".repeat(digits - 7))
        } else {
            format!("{head}{}", "*".repeat(rest.len()))
        }
    }
}

impl fmt::Display for Phone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Phone {
    type Err = CustomerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec 2.5 §15 case 1, plus the edge cases of the rules above.
    #[test]
    fn normalization() {
        let cases: &[(&str, Option<&str>)] = &[
            ("0812-3456-789", Some("+628123456789")),
            ("62812 3456 789", Some("+628123456789")),
            ("+62 812 3456 789", Some("+628123456789")),
            ("  +62 812 3456 789  ", Some("+628123456789")),
            ("812 3456 789", Some("+628123456789")),
            ("(021) 555.1234", Some("+62215551234")),
            ("+65 9123 4567", Some("+6591234567")),
            ("+1 (415) 555-0100", Some("+14155550100")),
            ("0812345", Some("+62812345")),
            ("+123456789012345", Some("+123456789012345")),
            // 7 digits.
            ("+1234567", None),
            ("081234", None),
            // 16 digits.
            ("+1234567890123456", None),
            // Letters and misplaced `+`.
            ("0812-3456-78a", None),
            ("abcdefghij", None),
            ("62+8123456789", None),
            ("++628123456789", None),
            // Country code starting with 0.
            ("+0812345678", None),
            ("", None),
            ("   ", None),
            ("+", None),
        ];
        for (input, want) in cases {
            let want = want
                .map(str::to_owned)
                .ok_or_else(|| CustomerError::InvalidPhone {
                    input: (*input).to_owned(),
                });
            assert_eq!(
                Phone::parse(input).map(|p| p.as_str().to_owned()),
                want,
                "{input}"
            );
        }
        assert_eq!(Phone::parse("x").unwrap_err().code(), "invalid_phone");
    }

    #[test]
    fn forms_and_masking() {
        // (input, local form, masked, Indonesian)
        let cases: &[(&str, &str, &str, bool)] = &[
            ("+628123456789", "08123456789", "0812****789", true),
            ("+6591234567", "+6591234567", "+659***567", false),
            ("+62812345", "0812345", "0812***", true),
            ("+628123456", "08123456", "0812*456", true),
            ("+12345678", "+12345678", "+123*678", false),
            ("+6281234567890", "081234567890", "0812*****890", true),
        ];
        for (input, local, masked, id) in cases {
            let p: Phone = input.parse().unwrap();
            assert_eq!(p.local_form(), *local, "{input}");
            assert_eq!(p.masked(), *masked, "{input}");
            assert_eq!(p.is_indonesian(), *id, "{input}");
            assert_eq!(p.to_string(), *input);
        }
    }
}
