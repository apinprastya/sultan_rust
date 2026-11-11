//! Merging duplicate customers at HQ (spec 2.5 §8): which merges are allowed and the profile A
//! ends up with. Moving B's balance is [`super::merge_balance`].

use chrono::NaiveDate;
use uuid::Uuid;

use super::{Birthday, CustomerError, Phone, TaxId};

/// A customer's tier with its optional last valid date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierAssignment {
    /// The membership tier.
    pub tier_id: Uuid,
    /// The last day it applies, if any.
    pub valid_until: Option<NaiveDate>,
}

/// The fields of a customer that a merge combines. Empty text (`None` or blank) counts as empty.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CustomerProfile {
    /// The name (required on a real customer).
    pub name: String,
    /// The normalized phone.
    pub phone: Option<Phone>,
    /// The lower-case email.
    pub email: Option<String>,
    /// The birthday.
    pub birthday: Option<Birthday>,
    /// The address.
    pub address: Option<String>,
    /// The company name.
    pub company_name: Option<String>,
    /// The tax id.
    pub tax_id: Option<TaxId>,
    /// The tax address.
    pub tax_address: Option<String>,
    /// Staff notes.
    pub notes: Option<String>,
    /// The customer group.
    pub group_id: Option<Uuid>,
    /// The membership tier.
    pub tier: Option<TierAssignment>,
    /// "Manual tier": the automatic selection never changes this customer.
    pub manual_tier: bool,
}

/// The lifecycle state of a merge party.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeParty {
    /// The customer.
    pub id: Uuid,
    /// `archived_at` is set.
    pub archived: bool,
    /// `erased_at` is set.
    pub erased: bool,
    /// `merged_into_id` is set.
    pub merged: bool,
}

/// Checks that B (`from`) may be merged into A (`into`), in this order:
/// A = B → `invalid_field` (field `from`); A or B already merged → `merged` (chains are never
/// created); B archived or erased → `archived`. A may itself be archived: the spec only
/// restricts B.
///
/// # Errors
/// As listed above.
pub fn check_merge(into: &MergeParty, from: &MergeParty) -> Result<(), CustomerError> {
    if into.id == from.id {
        return Err(CustomerError::InvalidField {
            field: "from",
            reason: "a customer cannot be merged into itself",
        });
    }
    if into.merged || from.merged {
        return Err(CustomerError::Merged);
    }
    if from.archived || from.erased {
        return Err(CustomerError::Archived);
    }
    Ok(())
}

fn is_blank(s: Option<&String>) -> bool {
    s.is_none_or(|s| s.trim().is_empty())
}

fn fill_text(a: Option<&String>, b: Option<&String>) -> Option<String> {
    if is_blank(a) && !is_blank(b) {
        b.cloned()
    } else {
        a.cloned()
    }
}

/// The profile A has after merging B into it: A's non-empty fields win and each empty field of
/// A is filled from B (the phone included). Group and tier are taken from B only when A has
/// none; a tier comes with its `valid_until`. A keeps its own manual-tier flag.
#[must_use]
pub fn merge_profiles(a: &CustomerProfile, b: &CustomerProfile) -> CustomerProfile {
    let name = if a.name.trim().is_empty() {
        b.name.clone()
    } else {
        a.name.clone()
    };
    CustomerProfile {
        name,
        phone: a.phone.clone().or_else(|| b.phone.clone()),
        email: fill_text(a.email.as_ref(), b.email.as_ref()),
        birthday: a.birthday.or(b.birthday),
        address: fill_text(a.address.as_ref(), b.address.as_ref()),
        company_name: fill_text(a.company_name.as_ref(), b.company_name.as_ref()),
        tax_id: a.tax_id.clone().or_else(|| b.tax_id.clone()),
        tax_address: fill_text(a.tax_address.as_ref(), b.tax_address.as_ref()),
        notes: fill_text(a.notes.as_ref(), b.notes.as_ref()),
        group_id: a.group_id.or(b.group_id),
        tier: a.tier.or(b.tier),
        manual_tier: a.manual_tier,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn party(n: u128, archived: bool, erased: bool, merged: bool) -> MergeParty {
        MergeParty {
            id: Uuid::from_u128(n),
            archived,
            erased,
            merged,
        }
    }

    #[test]
    fn merge_checks() {
        let ok = party(1, false, false, false);
        let invalid_from = CustomerError::InvalidField {
            field: "from",
            reason: "a customer cannot be merged into itself",
        };
        let cases = [
            (ok, party(2, false, false, false), Ok(())),
            (
                party(1, true, false, false),
                party(2, false, false, false),
                Ok(()),
            ),
            (ok, ok, Err(invalid_from)),
            (
                party(1, true, false, true),
                party(2, false, false, false),
                Err(CustomerError::Merged),
            ),
            (ok, party(2, true, false, true), Err(CustomerError::Merged)),
            (
                ok,
                party(2, true, false, false),
                Err(CustomerError::Archived),
            ),
            (
                ok,
                party(2, true, true, false),
                Err(CustomerError::Archived),
            ),
            (
                ok,
                party(2, false, true, false),
                Err(CustomerError::Archived),
            ),
        ];
        for (a, b, want) in cases {
            assert_eq!(check_merge(&a, &b), want, "{a:?} {b:?}");
        }
    }

    fn tier(n: u128, until: Option<&str>) -> TierAssignment {
        TierAssignment {
            tier_id: Uuid::from_u128(n),
            valid_until: until.map(|s| s.parse().unwrap()),
        }
    }

    fn full(name: &str, phone: &str, email: &str, tag: &str) -> CustomerProfile {
        CustomerProfile {
            name: name.into(),
            phone: Some(phone.parse().unwrap()),
            email: Some(email.into()),
            birthday: Some(Birthday::new(1, 2, None).unwrap()),
            address: Some(format!("{tag} street")),
            company_name: Some(format!("PT {tag}")),
            tax_id: Some("1".repeat(15).parse().unwrap()),
            tax_address: Some(format!("{tag} tax")),
            notes: Some(format!("{tag} notes")),
            group_id: Some(Uuid::from_u128(10)),
            tier: Some(tier(20, Some("2027-01-01"))),
            manual_tier: false,
        }
    }

    /// Spec 2.5 §15 case 6.
    #[test]
    fn merge_field_rules() {
        let a_full = full("Budi", "081234567890", "a@x", "A");
        let mut b_full = full("Budi S", "081299999999", "b@x", "B");
        b_full.birthday = Some(Birthday::new(3, 4, Some(1990)).unwrap());
        b_full.tax_id = Some("2".repeat(16).parse().unwrap());
        b_full.group_id = Some(Uuid::from_u128(11));
        b_full.tier = Some(tier(21, None));
        b_full.manual_tier = true;

        // A's fields win.
        assert_eq!(merge_profiles(&a_full, &b_full), a_full);

        // Empty A fields are filled from B, including the phone, group and tier (with its date).
        let a_empty = CustomerProfile {
            name: "Budi".into(),
            email: Some("  ".into()),
            notes: Some(String::new()),
            ..CustomerProfile::default()
        };
        let merged = merge_profiles(&a_empty, &b_full);
        assert_eq!(
            merged,
            CustomerProfile {
                name: "Budi".into(),
                manual_tier: false,
                ..b_full.clone()
            }
        );

        // A without a tier takes B's tier, keeps its own group.
        let mut a_no_tier = a_full.clone();
        a_no_tier.tier = None;
        let merged = merge_profiles(&a_no_tier, &b_full);
        assert_eq!(merged.tier, Some(tier(21, None)));
        assert_eq!(merged.group_id, Some(Uuid::from_u128(10)));

        // Both empty stays empty; a blank A name takes B's.
        let blank = CustomerProfile {
            name: " ".into(),
            ..CustomerProfile::default()
        };
        let merged = merge_profiles(&blank, &CustomerProfile::default());
        assert_eq!(merged.name, "");
        assert_eq!(merged.email, None);
        let merged = merge_profiles(
            &blank,
            &CustomerProfile {
                name: "Budi".into(),
                ..CustomerProfile::default()
            },
        );
        assert_eq!(merged.name, "Budi");
    }
}
