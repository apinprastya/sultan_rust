//! Brands (spec 2.2 §8): a unique name and an archived flag.

use super::CatalogError;
use super::search::search_key;
use crate::ids::BrandId;

/// Maximum length of a brand name.
pub const MAX_BRAND_NAME_LEN: usize = 120;

/// A brand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brand {
    /// Brand ID (client-generated).
    pub id: BrandId,
    /// Name, unique company-wide (case- and accent-insensitive).
    pub name: String,
    /// Archived brands are hidden from pickers; products keep them.
    pub archived: bool,
}

/// Validates creating or changing `brand` among `existing` (which may contain its stored
/// version): name 1–120 chars without surrounding spaces, and no other brand with the same
/// [`search_key`]. Archived brands count, so unarchiving never creates duplicates.
///
/// # Errors
/// [`CatalogError::InvalidField`] or [`CatalogError::DuplicateBrandName`].
pub fn validate_brand(existing: &[Brand], brand: &Brand) -> Result<(), CatalogError> {
    let len = brand.name.chars().count();
    if len == 0 || len > MAX_BRAND_NAME_LEN || brand.name.trim() != brand.name {
        return Err(CatalogError::InvalidField {
            field: "name",
            reason: "1–120 characters without surrounding spaces",
        });
    }
    let key = search_key(&brand.name);
    if existing
        .iter()
        .any(|b| b.id != brand.id && search_key(&b.name) == key)
    {
        return Err(CatalogError::DuplicateBrandName {
            name: brand.name.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn brand(n: u128, name: &str) -> Brand {
        Brand {
            id: BrandId::from_uuid(Uuid::from_u128(n)),
            name: name.to_owned(),
            archived: false,
        }
    }

    #[test]
    fn brand_rules() {
        let mut archived = brand(2, "Indomie");
        archived.archived = true;
        let existing = [brand(1, "Aqua"), archived];
        let name_err = Err(CatalogError::InvalidField {
            field: "name",
            reason: "1–120 characters without surrounding spaces",
        });
        let dup = |name: &str| {
            Err(CatalogError::DuplicateBrandName {
                name: name.to_owned(),
            })
        };
        let cases = [
            (brand(3, "Sosro"), Ok(())),
            (brand(1, "AQUA"), Ok(())),
            (brand(3, ""), name_err.clone()),
            (brand(3, " Sosro"), name_err.clone()),
            (brand(3, &"x".repeat(121)), name_err),
            (brand(3, "aqua"), dup("aqua")),
            (brand(3, "Indomíe"), dup("Indomíe")),
        ];
        for (b, expected) in cases {
            assert_eq!(validate_brand(&existing, &b), expected, "{}", b.name);
        }
    }
}
