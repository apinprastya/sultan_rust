//! Cash management (spec `docs/spec/2.7-cash-management.md`, phase K1): cash in / out / safe
//! drop, nota photos, the float between sessions, the X and close-report figures, the Z report of
//! a branch's business day, and their printable texts.
//!
//! Pure: no IO, no clock. The stored rows (movements, photos, Z snapshots) are the database's job
//! (K2); this module decides what is valid and computes what gets stored and printed.
//!
//! - **Movements** ([`CashMovement`], [`CashKind`], [`CashReason`]): §1, §2. Kind `in | out |
//!   drop`, amount > 0, a reason of the matching [`Direction`] for in / out and none for a drop,
//!   an active reason, a note trimmed to at most [`MAX_NOTE_CHARS`] and required when the reason
//!   says so. A reversal ([`CashMovement::reversal`], K10) names a movement of the same open
//!   session that is not a reversal and not reversed yet, takes its kind, amount and reason, has
//!   the opposite effect and needs a note. [`SEED_REASONS`] are the seeded reasons.
//! - **Permissions** ([`CashMovement::check`]): `cash.in`, `cash.out` (`MaxAmount` per movement,
//!   K4) and `cash.drop` through [`crate::access::PermissionSet::check`].
//! - **Nota photos** ([`is_missing_nota`], [`ensure_photo_slot`]): §2a, at most [`MAX_PHOTOS`],
//!   "missing nota" from [`DEFAULT_NOTA_FLAG_ABOVE`].
//! - **Session figures** ([`session_figures`], [`SessionFigures`]): §4. The sales block of spec
//!   2.6 §10 ([`crate::sale::session_report`]) with the netted cash movements as
//!   [`crate::sale::SessionEvent::CashIn`] / `CashOut` / `Drop`, plus the cash figures per kind and
//!   reason ([`CashFigures`]), drawer opens, X reads and missing notas. [`close_movements`] gives
//!   the 2.1 block's movements, whose `cash_out` = cash out + drops + payouts; both blocks agree on
//!   the expected cash.
//! - **Float** ([`float_figures`], [`opening_proposed`], [`proposed_float`]): §3, K20.
//! - **X and close** ([`XReport`], [`CloseFigures`]): §4, §5. An X has no counted cash and no
//!   over/short by type.
//! - **Z** ([`business_day`], [`check_z_run`], [`build_z`]): §6. Per-register subtotals and
//!   branch totals, cash to deposit, the tax summary per rate ([`tax_summary`]).
//! - **Texts** ([`build_slip`], [`build_x_report`], [`build_close_report`], [`build_z_report`]):
//!   fixed-width text like the receipts (32 / 42 / 48 columns), every label a [`CashLabel`] with
//!   Indonesian and English text.
//!
//! Numbers are in [`crate::doc_number`] ([`crate::doc_number::cash_movement_number`],
//! [`crate::doc_number::z_number`]). Readings of the spec made while building K1 are listed in
//! spec 2.7 §16.

mod figures;
mod movement;
mod text;
mod z;

pub use figures::{
    CashFigures, CloseFigures, FloatFigures, ReasonLine, SessionFigures, SessionInput, XReport,
    close_figures, close_movements, float_difference, float_figures, float_kept, opening_proposed,
    proposed_float, session_figures, x_report,
};
pub use movement::{
    CashKind, CashMovement, CashReason, DEFAULT_NOTA_FLAG_ABOVE, Direction, MAX_NOTE_CHARS,
    MAX_PHOTOS, SEED_REASONS, SeedReason, clean_note, ensure_photo_slot, is_missing_nota,
    is_reversed,
};
pub use text::{
    CashLabel, CloseReportInput, Printout, SessionHeader, SlipInput, SlipKind, TextOptions,
    XReportInput, ZReportInput, build_close_report, build_slip, build_x_report, build_z_report,
};
pub use z::{
    SaleRef, SessionSales, ZCandidate, ZReport, ZSession, ZSessionLine, ZTotals, build_z,
    business_day, check_z_run, tax_summary,
};

use crate::money::Money;

/// Errors of the cash rules. Each variant maps to a stable problem+json code through
/// [`CashError::code`] (spec 2.7 §11).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CashError {
    /// A field breaks a simple rule.
    #[error("invalid {field}: {reason}")]
    InvalidField {
        /// The field: `amount`, `reason`, `note`, `reverses`, `float_kept`, `day`.
        field: &'static str,
        /// Why it is invalid.
        reason: &'static str,
    },
    /// The movement was reversed already (K10).
    #[error("the cash movement is reversed already")]
    AlreadyReversed,
    /// A movement has [`MAX_PHOTOS`] photos already (§2a).
    #[error("a cash movement has at most {max} photos")]
    PhotoLimit {
        /// The limit.
        max: usize,
    },
    /// Sessions of the day are still open (K15).
    #[error("sessions of the day are still open: {}", numbers.join(", "))]
    SessionsOpen {
        /// Their numbers.
        numbers: Vec<String>,
    },
    /// No closed session of the day is left without a Z (§6).
    #[error("no closed session of the day is left without a Z")]
    NothingToClose,
    /// An amount does not fit in 64 bits.
    #[error("amount overflow")]
    AmountOverflow,
}

impl CashError {
    /// The stable problem+json code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidField { .. } => "invalid_field",
            Self::AlreadyReversed => "already_reversed",
            Self::PhotoLimit { .. } => "photo_limit",
            Self::SessionsOpen { .. } => "sessions_open",
            Self::NothingToClose => "nothing_to_close",
            Self::AmountOverflow => "amount_overflow",
        }
    }

    /// The field the error is about, when there is one (for problem+json's `field`).
    #[must_use]
    pub const fn field(&self) -> Option<&'static str> {
        match self {
            Self::InvalidField { field, .. } => Some(field),
            Self::AlreadyReversed => Some("reverses"),
            Self::PhotoLimit { .. }
            | Self::SessionsOpen { .. }
            | Self::NothingToClose
            | Self::AmountOverflow => None,
        }
    }
}

/// Shorthand for [`CashError::InvalidField`].
const fn invalid(field: &'static str, reason: &'static str) -> CashError {
    CashError::InvalidField { field, reason }
}

/// Adds money with overflow reported as [`CashError::AmountOverflow`].
fn add(a: Money, b: Money) -> Result<Money, CashError> {
    a.checked_add(b).ok_or(CashError::AmountOverflow)
}

/// Subtracts money with overflow reported as [`CashError::AmountOverflow`].
fn sub(a: Money, b: Money) -> Result<Money, CashError> {
    a.checked_sub(b).ok_or(CashError::AmountOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_fields_and_messages() {
        let cases: Vec<(CashError, &str, Option<&str>, &str)> = vec![
            (
                invalid("amount", "must be above 0"),
                "invalid_field",
                Some("amount"),
                "invalid amount: must be above 0",
            ),
            (
                CashError::AlreadyReversed,
                "already_reversed",
                Some("reverses"),
                "the cash movement is reversed already",
            ),
            (
                CashError::PhotoLimit { max: 3 },
                "photo_limit",
                None,
                "a cash movement has at most 3 photos",
            ),
            (
                CashError::SessionsOpen {
                    numbers: vec!["A-S-1".into(), "B-S-2".into()],
                },
                "sessions_open",
                None,
                "sessions of the day are still open: A-S-1, B-S-2",
            ),
            (
                CashError::NothingToClose,
                "nothing_to_close",
                None,
                "no closed session of the day is left without a Z",
            ),
            (
                CashError::AmountOverflow,
                "amount_overflow",
                None,
                "amount overflow",
            ),
        ];
        for (err, code, field, msg) in cases {
            assert_eq!(err.code(), code, "{err:?}");
            assert_eq!(err.field(), field, "{err:?}");
            assert_eq!(err.to_string(), msg);
        }
    }

    #[test]
    fn money_helpers_report_overflow() {
        let max = Money::from_minor(i64::MAX);
        let one = Money::from_minor(1);
        assert_eq!(add(max, one), Err(CashError::AmountOverflow));
        assert_eq!(
            sub(Money::from_minor(i64::MIN), one),
            Err(CashError::AmountOverflow)
        );
        assert_eq!(add(one, one), Ok(Money::from_minor(2)));
    }
}
