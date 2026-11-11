//! The customer error type and its stable codes (spec 2.5 §13).

use crate::money::Money;

/// Errors of the customer rules. Each variant maps to a stable problem+json code through
/// [`CustomerError::code`]: the new codes of spec 2.5 §13 plus the existing `invalid_field`,
/// `invalid_code`, `archived` and `amount_overflow`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CustomerError {
    /// A field breaks a simple rule (name length, email, birthday, amount sign…).
    #[error("invalid {field}: {reason}")]
    InvalidField {
        /// The field, e.g. `name`, `email`, `birthday`, `amount`, `from`.
        field: &'static str,
        /// Why it is invalid.
        reason: &'static str,
    },
    /// A phone number that does not normalize to 8–15 digits (spec 2.5 §2).
    #[error("`{input}` is not a phone number with 8 to 15 digits")]
    InvalidPhone {
        /// The rejected input.
        input: String,
    },
    /// A tax id that is not 15 or 16 digits (spec 2.5 §2).
    #[error("`{input}` is not a tax id of 15 or 16 digits")]
    InvalidTaxId {
        /// The rejected input.
        input: String,
    },
    /// A customer code with the wrong length or a character outside the alphabet (spec 2.5 §2).
    #[error("`{input}` is not a customer code")]
    InvalidCode {
        /// The rejected input.
        input: String,
    },
    /// A merge into a customer that was itself merged, or from one already merged (spec 2.5 §8).
    #[error("the customer was merged into another customer")]
    Merged,
    /// A merge from an archived or erased customer (spec 2.5 §8).
    #[error("the customer is archived")]
    Archived,
    /// Saving more change than the limits allow (spec 2.5 §11). `max` is the most that can still
    /// be saved on this sale (0 when the feature is off or the balance is full).
    #[error("at most {max} can be saved to the balance")]
    BalanceLimit {
        /// The most that can still be saved.
        max: Money,
    },
    /// Paying more than the customer balance (spec 2.5 §11).
    #[error("the balance is only {balance}")]
    InsufficientBalance {
        /// The current balance.
        balance: Money,
    },
    /// Erasing a customer whose balance is not zero (spec 2.5 §11).
    #[error("the balance is {balance}, not 0")]
    BalanceNotZero {
        /// The current balance.
        balance: Money,
    },
    /// A balance sum or cash total does not fit in 64 bits.
    #[error("amount overflow")]
    AmountOverflow,
}

impl CustomerError {
    /// The stable problem+json code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidField { .. } => "invalid_field",
            Self::InvalidPhone { .. } => "invalid_phone",
            Self::InvalidTaxId { .. } => "invalid_tax_id",
            Self::InvalidCode { .. } => "invalid_code",
            Self::Merged => "merged",
            Self::Archived => "archived",
            Self::BalanceLimit { .. } => "balance_limit",
            Self::InsufficientBalance { .. } => "insufficient_balance",
            Self::BalanceNotZero { .. } => "balance_not_zero",
            Self::AmountOverflow => "amount_overflow",
        }
    }

    /// The field the error is about, when there is one (for problem+json's `field`).
    #[must_use]
    pub const fn field(&self) -> Option<&'static str> {
        match self {
            Self::InvalidField { field, .. } => Some(field),
            Self::InvalidPhone { .. } => Some("phone"),
            Self::InvalidTaxId { .. } => Some("tax_id"),
            Self::InvalidCode { .. } => Some("code"),
            Self::BalanceLimit { .. } | Self::InsufficientBalance { .. } => Some("amount"),
            Self::Merged | Self::Archived | Self::BalanceNotZero { .. } | Self::AmountOverflow => {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_fields_and_messages() {
        let m = Money::from_minor(5_000);
        let cases: Vec<(CustomerError, &str, Option<&str>, &str)> = vec![
            (
                CustomerError::InvalidField {
                    field: "name",
                    reason: "empty",
                },
                "invalid_field",
                Some("name"),
                "invalid name: empty",
            ),
            (
                CustomerError::InvalidPhone { input: "12".into() },
                "invalid_phone",
                Some("phone"),
                "`12` is not a phone number with 8 to 15 digits",
            ),
            (
                CustomerError::InvalidTaxId { input: "1".into() },
                "invalid_tax_id",
                Some("tax_id"),
                "`1` is not a tax id of 15 or 16 digits",
            ),
            (
                CustomerError::InvalidCode { input: "X".into() },
                "invalid_code",
                Some("code"),
                "`X` is not a customer code",
            ),
            (
                CustomerError::Merged,
                "merged",
                None,
                "the customer was merged into another customer",
            ),
            (
                CustomerError::Archived,
                "archived",
                None,
                "the customer is archived",
            ),
            (
                CustomerError::BalanceLimit { max: m },
                "balance_limit",
                Some("amount"),
                "at most 5000 can be saved to the balance",
            ),
            (
                CustomerError::InsufficientBalance { balance: m },
                "insufficient_balance",
                Some("amount"),
                "the balance is only 5000",
            ),
            (
                CustomerError::BalanceNotZero { balance: m },
                "balance_not_zero",
                None,
                "the balance is 5000, not 0",
            ),
            (
                CustomerError::AmountOverflow,
                "amount_overflow",
                None,
                "amount overflow",
            ),
        ];
        for (err, code, field, msg) in cases {
            assert_eq!(err.code(), code);
            assert_eq!(err.field(), field);
            assert_eq!(err.to_string(), msg);
        }
    }
}
