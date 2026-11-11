//! The limit check of a manual price override (spec 2.3 §6).

use super::PricingError;
use crate::access::{FULL_PCT_BASIS_POINTS, Limit, Requested};
use crate::money::Money;

/// The outcome of [`check_price_override`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverrideDecision {
    /// The user may set the price with their own `price.override`.
    Allowed,
    /// The price is further below the resolved price than the user's limit: a manager override
    /// (spec 2.1 §6) is needed. `requested` is how far below, in basis points rounded up, for
    /// checking the approver's limit.
    NeedsManager {
        /// `(resolved − new) / resolved` in basis points, rounded up.
        requested: Requested,
    },
}

/// Checks a manual unit price `new` against the `resolved` price and the user's `price.override`
/// limit (spec 2.3 §6). `limit` is the user's effective limit: `None` = held without a limit.
///
/// - No resolved price (`no_price`): any price ≥ 0 is allowed; the limit does not apply.
/// - Equal or higher than the resolved price: allowed.
/// - Lower: allowed when `(resolved − new) / resolved ≤ limit` (exactly, inclusive), otherwise
///   [`OverrideDecision::NeedsManager`]. A limit of the wrong kind fails closed.
///
/// Whether the user holds `price.override` at all is [`crate::access::PermissionSet::check`]'s
/// job; this only applies the limit.
///
/// # Errors
/// [`PricingError::InvalidField`] (`price`) when `new` is negative.
pub fn check_price_override(
    resolved: Option<Money>,
    new: Money,
    limit: Option<Limit>,
) -> Result<OverrideDecision, PricingError> {
    if new.is_negative() {
        return Err(PricingError::InvalidField {
            field: "price",
            reason: "must not be negative",
        });
    }
    let Some(resolved) = resolved.filter(|r| new < *r) else {
        return Ok(OverrideDecision::Allowed);
    };
    // 0 ≤ new < resolved, so resolved > 0 and the drop is in (0, resolved].
    let drop = i128::from(resolved.minor()) - i128::from(new.minor());
    let full = i128::from(FULL_PCT_BASIS_POINTS);
    let within = match limit {
        None => true,
        Some(Limit::MaxPct(bp)) => drop * full <= i128::from(bp) * i128::from(resolved.minor()),
        Some(Limit::MaxAmount(_)) => false,
    };
    if within {
        return Ok(OverrideDecision::Allowed);
    }
    // Ceiling of drop × 10 000 / resolved, at most 10 000, so it fits a u16.
    let bp = (drop * full + i128::from(resolved.minor()) - 1) / i128::from(resolved.minor());
    let bp = u16::try_from(bp).unwrap_or(FULL_PCT_BASIS_POINTS);
    Ok(OverrideDecision::NeedsManager {
        requested: Requested::Pct(bp),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    /// §12.13 and edges.
    #[test]
    fn override_limits() {
        let manager = Some(Limit::MaxPct(2_000));
        let needs = |bp| {
            Ok(OverrideDecision::NeedsManager {
                requested: Requested::Pct(bp),
            })
        };
        #[rustfmt::skip]
        let cases = [
            (Some(10_000), 8_000, manager, Ok(OverrideDecision::Allowed)),
            (Some(10_000), 7_999, manager, needs(2_001)),
            (Some(10_000), 12_000, manager, Ok(OverrideDecision::Allowed)),
            (Some(10_000), 10_000, manager, Ok(OverrideDecision::Allowed)),
            (Some(3), 2, manager, needs(3_334)),
            (Some(10_000), 0, manager, needs(10_000)),
            (Some(10_000), 0, None, Ok(OverrideDecision::Allowed)),
            (Some(10_000), 9_999, Some(Limit::MaxAmount(m(5_000))), needs(1)),
            (None, 0, manager, Ok(OverrideDecision::Allowed)),
            (None, 123, Some(Limit::MaxPct(0)), Ok(OverrideDecision::Allowed)),
            (Some(i64::MAX), 0, manager, needs(10_000)),
            (Some(10_000), -1, None, Err(PricingError::InvalidField {
                field: "price", reason: "must not be negative" })),
        ];
        for (resolved, new, limit, expected) in cases {
            assert_eq!(
                check_price_override(resolved.map(m), m(new), limit),
                expected,
                "{resolved:?} → {new} with {limit:?}"
            );
        }
    }
}
