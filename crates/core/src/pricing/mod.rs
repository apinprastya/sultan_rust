//! Pricing rules (spec 2.3): price lists, the resolution chain, quantity tiers, rules, unit
//! fallback, special lines, the manual-override limit check and cash rounding.
//!
//! Pure: the caller loads the lists, entries and cart and passes them in, together with the sale
//! date in the branch's time zone ([`sale_date`]). Nothing here reads a clock or a database.
//!
//! - **Model** ([`PriceList`], [`PriceEntry`], [`PriceTier`], [`Rule`]): a list of one
//!   [`PriceListKind`], an `active` flag, an optional list rule and dated entries per
//!   variant + unit with 1–10 quantity tiers. [`PriceList::validate`] and [`validate_entry`]
//!   enforce §2–§3 (`invalid_rule`, `price_overlap`, `invalid_field`).
//! - **Resolution** ([`PriceChain`], [`resolve_cart`], [`resolve_unit_price`]): spec §4. The chain
//!   is contract → tier (only when the membership applies on the date) → group → branch → default,
//!   skipping inactive or archived lists. Tiers compare the variant's counted quantity across the
//!   whole cart in base units; a list without a matching entry falls through; a rule is applied
//!   to the price from the lists after it; units without any entry fall back to base price ×
//!   factor. Money is rounded half up to Rp 1 (half away from zero for negative amounts).
//! - **Special lines** (§5): open price and overridden lines ([`LineMode`]); price-embedded scale
//!   labels ([`price_label`]).
//! - **Manual override** (§6): [`check_price_override`].
//! - **Cash rounding** (§8): [`CashRounding`].
//!
//! Known limitations and readings of the spec:
//! - A rule result ≤ 0 counts as "no price from this list", i.e. the list falls through to the
//!   price from the lists after it.
//! - Price-label lines are not part of the cart passed to [`resolve_cart`], so their quantity does
//!   not count towards other lines' tiers. Their per-unit price is the branch → default price for
//!   one unit (what a scale is programmed with), so tiers of those lists never apply to it.
//! - A rule in an entry of the `default` list is rejected like a rule on the list itself
//!   (`invalid_rule`): nothing comes after the default list, so it could never yield a price.
//! - Whether the user holds `price.override` at all is checked by
//!   [`crate::access::PermissionSet`]; [`check_price_override`] only applies its limit.

mod list;
mod override_;
mod resolve;
mod rounding;

#[cfg(test)]
mod tests;

pub use list::{
    MAX_TIERS, PriceEntry, PriceList, PriceListKind, PriceTier, Rule, TierPrice, validate_entry,
};
pub use override_::{OverrideDecision, check_price_override};
pub use resolve::{
    CartLine, CustomerLists, LabelLine, LineMode, PriceChain, PriceSource, PricedLine,
    ResolvedPrice, SaleItem, TierMembership, counted_quantities, price_label, resolve_cart,
    resolve_unit_price,
};
pub use rounding::{CashRounded, CashRounding, line_amount};

use std::fmt;

use chrono::NaiveDate;
use uuid::Uuid;

use crate::doc_number::{BranchTimeZone, LocalDay};
use crate::ids::{UnitId, VariantId};
use crate::time::Timestamp;

pub(crate) use rounding::div_round_half_up;

macro_rules! pricing_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a new time-ordered `UUIDv7` identifier.
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wraps an existing UUID.
            #[must_use]
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// Returns the underlying UUID.
            #[must_use]
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

pricing_id!(
    /// A price list (spec 2.3 §2).
    PriceListId
);
pricing_id!(
    /// An entry of a price list: one variant + unit over a date range (spec 2.3 §3).
    PriceEntryId
);

/// The local calendar date of a sale at `at` in the branch's time zone, the date that entry
/// validity, tier membership and tax rates are compared with (spec 2.3 §3, §7).
#[must_use]
pub fn sale_date(at: Timestamp, tz: BranchTimeZone) -> NaiveDate {
    let (y, m, d) = LocalDay::of(at, tz).ymd();
    // `LocalDay` always holds a real chrono date, so this cannot fail.
    NaiveDate::from_ymd_opt(y, m, d).unwrap_or(NaiveDate::MIN)
}

/// Errors of the pricing rules. Each variant maps to a stable problem+json code through
/// [`PricingError::code`] (spec 2.3 §10).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PricingError {
    /// A field breaks a simple rule (tier count, `min_qty`, negative price, date order).
    #[error("invalid {field}: {reason}")]
    InvalidField {
        /// The field, e.g. `tiers`, `min_qty`, `price`, `valid_to`.
        field: &'static str,
        /// Why it is invalid.
        reason: &'static str,
    },
    /// A rule where none is allowed, or one that can never give a price (spec 2.3 §2).
    #[error("invalid rule: {reason}")]
    InvalidRule {
        /// Why the rule is invalid.
        reason: &'static str,
    },
    /// Two entries of one list for the same variant + unit have overlapping dates (§3).
    #[error("entry dates overlap another entry for variant {variant}, unit {unit}")]
    PriceOverlap {
        /// The variant.
        variant: VariantId,
        /// The unit.
        unit: UnitId,
        /// The entry whose dates overlap.
        other: PriceEntryId,
    },
    /// No list in the chain prices the line (§4.5). For an open-price line without a typed price,
    /// `suggestion` is the resolved price, if any (§5).
    #[error("no price")]
    NoPrice {
        /// The resolved price shown as a suggestion on an open-price line.
        suggestion: Option<crate::money::Money>,
    },
    /// An amount or quantity does not fit in 64 bits.
    #[error("amount does not fit in 64 bits")]
    Overflow,
}

impl PricingError {
    /// The stable error code used in problem+json responses.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidField { .. } => "invalid_field",
            Self::InvalidRule { .. } => "invalid_rule",
            Self::PriceOverlap { .. } => "price_overlap",
            Self::NoPrice { .. } => "no_price",
            Self::Overflow => "amount_overflow",
        }
    }
}
