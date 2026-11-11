//! The customer line on a receipt (spec 2.5 §10), e.g. `Budi S. · 0812****789 · Gold`.
//!
//! A posted sale stores this line as a snapshot, so later edits or an erase never change an old
//! receipt. The full company name, tax id and tax address of a wholesale sale are printed by the
//! sale (2.6), not here.

use super::Phone;

/// The separator between the parts of the receipt line.
pub const RECEIPT_SEPARATOR: &str = " · ";

/// The printed name: the first word in full and the first letter of every other word followed
/// by a dot (`Budi Santoso Wijaya` → `Budi S. W.`, `Budi` → `Budi`). Words are separated by
/// any whitespace; the letters keep their case.
#[must_use]
pub fn short_name(name: &str) -> String {
    let mut words = name.split_whitespace();
    let mut out = words.next().unwrap_or_default().to_owned();
    for word in words {
        if let Some(initial) = word.chars().next() {
            out.push(' ');
            out.push(initial);
            out.push('.');
        }
    }
    out
}

/// What the receipt line is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiptCustomer<'a> {
    /// The customer name.
    pub name: &'a str,
    /// The company name; when it is not blank the customer is a company customer and this is
    /// printed in full instead of the shortened name.
    pub company_name: Option<&'a str>,
    /// The phone, printed masked.
    pub phone: Option<&'a Phone>,
    /// The tier name, only when the tier applies on the sale date (see
    /// [`super::tier_applies`]); otherwise `None`.
    pub tier_name: Option<&'a str>,
}

impl ReceiptCustomer<'_> {
    /// The printed name part: the company name in full, or else [`short_name`].
    #[must_use]
    pub fn name_part(&self) -> String {
        match self.company_name.map(str::trim) {
            Some(company) if !company.is_empty() => company.to_owned(),
            _ => short_name(self.name),
        }
    }

    /// The one-line form `Budi S. · 0812****789 · Gold`; missing or empty parts are left out.
    #[must_use]
    pub fn line(&self) -> String {
        let parts = [
            Some(self.name_part()),
            self.phone.map(Phone::masked),
            self.tier_name.map(|t| t.trim().to_owned()),
        ];
        parts
            .into_iter()
            .flatten()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(RECEIPT_SEPARATOR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec 2.5 §15 case 5 (names).
    #[test]
    fn short_names() {
        let cases = [
            ("Budi Santoso Wijaya", "Budi S. W."),
            ("Budi", "Budi"),
            ("  Budi   Santoso ", "Budi S."),
            ("budi santoso", "budi s."),
            ("Ölaf Ärnason", "Ölaf Ä."),
            ("", ""),
        ];
        for (name, want) in cases {
            assert_eq!(short_name(name), want, "{name}");
        }
    }

    /// Spec 2.5 §15 case 5 (the whole line).
    #[test]
    fn lines() {
        let local: Phone = "0812-3456-789".parse().unwrap();
        let foreign: Phone = "+65 9123 4567".parse().unwrap();
        let rc = |name, company, phone, tier| ReceiptCustomer {
            name,
            company_name: company,
            phone,
            tier_name: tier,
        };
        let cases = [
            (
                rc("Budi Santoso", None, Some(&local), Some("Gold")),
                "Budi S. · 0812****789 · Gold",
            ),
            (
                rc("Budi Santoso", None, None, Some("Gold")),
                "Budi S. · Gold",
            ),
            (
                rc("Budi Santoso", None, Some(&local), None),
                "Budi S. · 0812****789",
            ),
            (rc("Budi", None, None, None), "Budi"),
            (
                rc("Lim", None, Some(&foreign), Some("Silver")),
                "Lim · +659***567 · Silver",
            ),
            (
                rc("Budi Santoso", Some("PT Maju Jaya"), Some(&local), None),
                "PT Maju Jaya · 0812****789",
            ),
            (rc("Budi Santoso", Some("  "), None, Some(" ")), "Budi S."),
            (rc("", None, Some(&local), None), "0812****789"),
        ];
        for (customer, want) in cases {
            assert_eq!(customer.line(), want);
        }
    }
}
