//! Rules for the customer's profile fields (spec 2.5 §2): name, email, tax id and birthday.
//! Phone and code have their own modules.

use std::fmt;
use std::str::FromStr;

use chrono::NaiveDate;

use super::CustomerError;

/// The most characters a customer name has after trimming.
pub const MAX_CUSTOMER_NAME_LEN: usize = 100;

/// A leap year, used to check a birthday given without a year (29 February is allowed then).
const LEAP_YEAR: i32 = 2000;

/// Trims a customer name and checks it has 1–100 characters (Unicode scalar values).
///
/// # Errors
/// [`CustomerError::InvalidField`] with field `name`.
pub fn normalize_name(name: &str) -> Result<String, CustomerError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(CustomerError::InvalidField {
            field: "name",
            reason: "required",
        });
    }
    if trimmed.chars().count() > MAX_CUSTOMER_NAME_LEN {
        return Err(CustomerError::InvalidField {
            field: "name",
            reason: "longer than 100 characters",
        });
    }
    Ok(trimmed.to_owned())
}

/// Trims and lower-cases an email address and checks its shape: exactly one `@`, something on
/// both sides of it, and no spaces. Deliverability is not checked. Emails are not unique.
///
/// # Errors
/// [`CustomerError::InvalidField`] with field `email`.
pub fn normalize_email(email: &str) -> Result<String, CustomerError> {
    let invalid = |reason| CustomerError::InvalidField {
        field: "email",
        reason,
    };
    let lower = email.trim().to_lowercase();
    if lower.chars().any(char::is_whitespace) {
        return Err(invalid("contains a space"));
    }
    let mut parts = lower.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(local), Some(domain), None) if !local.is_empty() && !domain.is_empty() => Ok(lower),
        _ => Err(invalid("needs exactly one @ with text on both sides")),
    }
}

/// A tax id (NPWP or NIK): 15 or 16 digits, stored as digits only.
///
/// The separators people type in a formatted NPWP (` `, `.`, `-`, e.g. `01.234.567.8-901.000`)
/// are dropped; any other character is invalid.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaxId(String);

impl TaxId {
    /// Parses a typed tax id.
    ///
    /// # Errors
    /// [`CustomerError::InvalidTaxId`] for another digit count or a character other than a digit
    /// or separator.
    pub fn parse(input: &str) -> Result<Self, CustomerError> {
        let invalid = || CustomerError::InvalidTaxId {
            input: input.to_owned(),
        };
        let mut digits = String::with_capacity(input.len());
        for c in input.trim().chars() {
            match c {
                '0'..='9' => digits.push(c),
                ' ' | '.' | '-' => {}
                _ => return Err(invalid()),
            }
        }
        if matches!(digits.len(), 15 | 16) {
            Ok(Self(digits))
        } else {
            Err(invalid())
        }
    }

    /// The stored digits.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TaxId {
    type Err = CustomerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// A birthday: month and day, with an optional year (stored as `birth_month`, `birth_day` and a
/// nullable `birth_year`). 29 February is allowed without a year, and with a year only when it
/// is a leap year. The year is not compared with today (core does not read the clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Birthday {
    month: u32,
    day: u32,
    year: Option<i32>,
}

impl Birthday {
    /// Checks month (1–12), day (a day of that month) and the optional year.
    ///
    /// # Errors
    /// [`CustomerError::InvalidField`] with field `birthday` when the date does not exist.
    pub fn new(month: u32, day: u32, year: Option<i32>) -> Result<Self, CustomerError> {
        if NaiveDate::from_ymd_opt(year.unwrap_or(LEAP_YEAR), month, day).is_none() {
            return Err(CustomerError::InvalidField {
                field: "birthday",
                reason: "not a calendar date",
            });
        }
        Ok(Self { month, day, year })
    }

    /// The month, 1–12.
    #[must_use]
    pub const fn month(self) -> u32 {
        self.month
    }

    /// The day of the month, 1–31.
    #[must_use]
    pub const fn day(self) -> u32 {
        self.day
    }

    /// The year, when known.
    #[must_use]
    pub const fn year(self) -> Option<i32> {
        self.year
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let long = "x".repeat(101);
        let max = "é".repeat(100);
        let cases: &[(&str, Result<&str, &str>)] = &[
            ("Budi", Ok("Budi")),
            ("  Budi Santoso \t", Ok("Budi Santoso")),
            (&max, Ok(&max)),
            ("", Err("required")),
            ("   ", Err("required")),
            (&long, Err("longer than 100 characters")),
        ];
        for (input, want) in cases {
            let got = normalize_name(input);
            match want {
                Ok(want) => assert_eq!(got.as_deref(), Ok(*want)),
                Err(reason) => assert_eq!(
                    got,
                    Err(CustomerError::InvalidField {
                        field: "name",
                        reason
                    })
                ),
            }
        }
    }

    #[test]
    fn emails() {
        let cases: &[(&str, Option<&str>)] = &[
            ("Budi@Example.COM", Some("budi@example.com")),
            ("  a@b ", Some("a@b")),
            ("budi.example.com", None),
            ("a@b@c", None),
            ("@example.com", None),
            ("budi@", None),
            ("budi @example.com", None),
            ("", None),
        ];
        for (input, want) in cases {
            let got = normalize_email(input);
            match want {
                Some(want) => assert_eq!(got.as_deref(), Ok(*want), "{input}"),
                None => assert_eq!(got.unwrap_err().field(), Some("email"), "{input}"),
            }
        }
    }

    /// Spec 2.5 §15 case 3.
    #[test]
    fn tax_ids() {
        let cases: &[(&str, Option<&str>)] = &[
            ("012345678901234", Some("012345678901234")),
            ("3171234567890001", Some("3171234567890001")),
            ("01.234.567.8-901.000", Some("012345678901000")),
            (" 3171 2345 6789 0001 ", Some("3171234567890001")),
            ("01234567890123", None),
            ("31712345678900012", None),
            ("01234567890123A", None),
            ("NPWPNPWPNPWPNPW", None),
            ("", None),
        ];
        for (input, want) in cases {
            let want = want
                .map(str::to_owned)
                .ok_or_else(|| CustomerError::InvalidTaxId {
                    input: (*input).to_owned(),
                });
            let got = input.parse::<TaxId>();
            assert_eq!(got.clone().map(|t| t.as_str().to_owned()), want, "{input}");
            assert_eq!(got.map(|t| t.to_string()), want, "{input}");
        }
        assert_eq!(TaxId::parse("x").unwrap_err().code(), "invalid_tax_id");
    }

    #[test]
    fn birthdays() {
        let cases: &[(u32, u32, Option<i32>, bool)] = &[
            (2, 29, None, true),
            (2, 29, Some(2024), true),
            (2, 29, Some(2000), true),
            (2, 29, Some(2023), false),
            (2, 29, Some(1900), false),
            (2, 30, None, false),
            (12, 31, Some(1990), true),
            (4, 31, None, false),
            (13, 1, None, false),
            (0, 1, None, false),
            (1, 0, None, false),
            (1, 1, None, true),
        ];
        for (m, d, y, ok) in cases {
            let got = Birthday::new(*m, *d, *y);
            if *ok {
                let b = got.unwrap();
                assert_eq!((b.month(), b.day(), b.year()), (*m, *d, *y));
            } else {
                assert_eq!(
                    got,
                    Err(CustomerError::InvalidField {
                        field: "birthday",
                        reason: "not a calendar date"
                    }),
                    "{m}-{d} {y:?}"
                );
            }
        }
    }
}
