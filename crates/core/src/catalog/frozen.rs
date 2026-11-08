//! "Frozen after first use" rules, checked by diffing the stored product document against the
//! proposed one (spec 2.2 §3, §4, §5, §12).

use super::{CatalogError, ProductDraft};

fn frozen(field: &'static str) -> CatalogError {
    CatalogError::FrozenAfterUse { field }
}

/// Checks a product update. When the product has been `used` (any sale, stock movement or
/// price-list entry references one of its variants) these changes are rejected with
/// [`CatalogError::FrozenAfterUse`]:
///
/// - `type`: the product type changes;
/// - `base_unit`: another base unit, or the base unit's kind changes;
/// - `units`: a unit is removed (archive it instead);
/// - `units.factor` / `units.kind`: a unit's factor or kind changes (archive it and add a new one);
/// - `options`: an option is added or removed (create a new product instead);
/// - `options.values`: a value is removed or moved to another option (archive the variants).
///
/// Renames (product, options, values, units), new option values and new units stay allowed.
/// When the product has not been used, every change is allowed.
///
/// # Errors
/// The first frozen field that changed.
pub fn check_frozen(
    stored: &ProductDraft,
    proposed: &ProductDraft,
    used: bool,
) -> Result<(), CatalogError> {
    if !used {
        return Ok(());
    }
    if stored.product_type != proposed.product_type {
        return Err(frozen("type"));
    }
    let old_base = stored.units.base_unit();
    let new_base = proposed.units.base_unit();
    if stored.units.base != proposed.units.base
        || old_base.map(|u| u.kind) != new_base.map(|u| u.kind)
    {
        return Err(frozen("base_unit"));
    }
    for old in &stored.units.units {
        let Some(new) = proposed.units.get(old.id) else {
            return Err(frozen("units"));
        };
        if new.factor != old.factor {
            return Err(frozen("units.factor"));
        }
        if new.kind != old.kind {
            return Err(frozen("units.kind"));
        }
    }
    let same_option_set = stored.options.len() == proposed.options.len()
        && stored
            .options
            .iter()
            .all(|o| proposed.options.iter().any(|p| p.id == o.id));
    if !same_option_set {
        return Err(frozen("options"));
    }
    for old in &stored.options {
        let new = proposed.options.iter().find(|p| p.id == old.id);
        let all_kept = new.is_some_and(|new| old.values.iter().all(|v| new.value(v.id).is_some()));
        if !all_kept {
            return Err(frozen("options.values"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::product::tests::kaos;
    use crate::catalog::unit::UnitKind;
    use crate::catalog::unit::tests::unit;
    use crate::catalog::variant::tests::{option, value};
    use crate::catalog::{ProductType, Qty};
    use crate::ids::UnitId;

    /// A named edit of the stored document and the expected result once used.
    type Case = (
        &'static str,
        fn(&mut ProductDraft),
        Result<(), CatalogError>,
    );

    #[test]
    #[allow(clippy::too_many_lines)] // one table row per frozen rule
    fn frozen_changes_after_use() {
        let cases: Vec<Case> = vec![
            ("rename product", |d| d.name = "Kaos Oblong".into(), Ok(())),
            (
                "rename option and value",
                |d| {
                    d.options[0].name = "Ukuran".into();
                    d.options[0].values[1].name = "Sedang".into();
                },
                Ok(()),
            ),
            (
                "rename unit",
                |d| d.units.units[1].name = "dus".into(),
                Ok(()),
            ),
            (
                "add value",
                |d| d.options[0].values.push(value(14, "XL", "XL")),
                Ok(()),
            ),
            (
                "add unit",
                |d| {
                    d.units
                        .units
                        .push(unit(3, "pack", UnitKind::Integer, 6_000));
                },
                Ok(()),
            ),
            ("archive unit", |d| d.units.units[1].archived = true, Ok(())),
            ("reorder options", |d| d.options.reverse(), Ok(())),
            (
                "remove value",
                |d| {
                    d.options[0].values.pop();
                },
                Err(frozen("options.values")),
            ),
            (
                "move value",
                |d| {
                    let v = d.options[0].values.pop().unwrap();
                    d.options[1].values.push(v);
                },
                Err(frozen("options.values")),
            ),
            (
                "add option",
                |d| {
                    d.options
                        .push(option(3, "Material", vec![value(31, "Cotton", "C")]));
                },
                Err(frozen("options")),
            ),
            (
                "remove option",
                |d| {
                    d.options.pop();
                },
                Err(frozen("options")),
            ),
            (
                "replace option",
                |d| d.options[1] = option(3, "Material", vec![value(31, "Cotton", "C")]),
                Err(frozen("options")),
            ),
            (
                "change factor",
                |d| d.units.units[1].factor = Qty::from_thousandths(20_000),
                Err(frozen("units.factor")),
            ),
            (
                "change unit kind",
                |d| d.units.units[1].kind = UnitKind::Decimal,
                Err(frozen("units.kind")),
            ),
            (
                "remove unit",
                |d| {
                    d.units.units.pop();
                },
                Err(frozen("units")),
            ),
            (
                "change type",
                |d| d.product_type = ProductType::Service,
                Err(frozen("type")),
            ),
            (
                "change base unit",
                |d| d.units.base = d.units.units[1].id,
                Err(frozen("base_unit")),
            ),
            (
                "change base kind",
                |d| d.units.units[0].kind = UnitKind::Decimal,
                Err(frozen("base_unit")),
            ),
            (
                "dangling base",
                |d| d.units.base = UnitId::from_uuid(uuid::Uuid::from_u128(99)),
                Err(frozen("base_unit")),
            ),
        ];
        let stored = kaos();
        for (name, edit, expected) in cases {
            let mut proposed = kaos();
            edit(&mut proposed);
            assert_eq!(check_frozen(&stored, &proposed, true), expected, "{name}");
            assert_eq!(
                check_frozen(&stored, &proposed, false),
                Ok(()),
                "{name} (unused)"
            );
        }
    }
}
