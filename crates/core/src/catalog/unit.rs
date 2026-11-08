//! Units of measure, defined per product and shared by its variants (spec 2.2 §4).

use super::search::search_key;
use super::{CatalogError, Qty};
use crate::ids::UnitId;

/// Most units a product may have besides its base unit *(proposed in the spec)*.
pub const MAX_EXTRA_UNITS: usize = 5;
/// Maximum length of a unit name.
pub const MAX_UNIT_NAME_LEN: usize = 24;
/// Maximum length of a unit's short receipt name.
pub const MAX_UNIT_SHORT_NAME_LEN: usize = 8;
/// Most entries in the company unit list (autocomplete, spec 2.2 §4).
pub const MAX_UNIT_LIST_LEN: usize = 100;

/// Validates the company unit list used for autocomplete (spec 2.2 §4): at most
/// [`MAX_UNIT_LIST_LEN`] names, each 1–24 chars without surrounding spaces, unique
/// (case- and accent-insensitive). Products keep their own copies, so the list is free to change.
///
/// # Errors
/// [`CatalogError::InvalidField`] or [`CatalogError::DuplicateUnitName`].
pub fn validate_unit_list(names: &[String]) -> Result<(), CatalogError> {
    if names.len() > MAX_UNIT_LIST_LEN {
        return Err(CatalogError::InvalidField {
            field: "unit_names",
            reason: "at most 100 names",
        });
    }
    let mut keys: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        let len = name.chars().count();
        if len == 0 || len > MAX_UNIT_NAME_LEN || name.trim() != name {
            return Err(CatalogError::InvalidField {
                field: "unit_names",
                reason: "1–24 characters without surrounding spaces",
            });
        }
        let key = search_key(name);
        if keys.contains(&key) {
            return Err(CatalogError::DuplicateUnitName { name: name.clone() });
        }
        keys.push(key);
    }
    Ok(())
}

/// How quantities of a unit are entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnitKind {
    /// Whole numbers only (`pcs`, `box`).
    Integer,
    /// Up to 3 decimal places (`kg`, `liter`).
    Decimal,
}

/// One unit of a product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// Unit ID (client-generated).
    pub id: UnitId,
    /// Name, e.g. `box`; unique (case-insensitive) among the product's active units.
    pub name: String,
    /// Short display name for the receipt, ≤8 chars; empty = use `name`.
    pub short_name: String,
    /// Integer or decimal quantities.
    pub kind: UnitKind,
    /// How many base units one of this unit is (`box` = 24). Exactly 1 for the base unit.
    pub factor: Qty,
    /// Can be sold in this unit.
    pub sellable: bool,
    /// Can be bought in this unit.
    pub purchasable: bool,
    /// Archived units stay for history but are not offered any more (spec 2.2 §4, §9).
    pub archived: bool,
}

impl Unit {
    /// Checks that `qty` can be entered in this unit: an integer unit takes whole numbers only
    /// (decimal units take any [`Qty`], which already has at most 3 decimal places).
    ///
    /// # Errors
    /// [`CatalogError::FractionalQty`].
    pub fn check_qty(&self, qty: Qty) -> Result<(), CatalogError> {
        if self.kind == UnitKind::Integer && !qty.is_whole() {
            return Err(CatalogError::FractionalQty {
                unit: self.name.clone(),
            });
        }
        Ok(())
    }

    /// Converts `qty` of this unit to base units: `qty × factor` (spec 2.2 §4), after
    /// [`Unit::check_qty`]. A decimal factor on a decimal unit is rounded half away from zero
    /// to 3 decimal places.
    ///
    /// # Errors
    /// [`CatalogError::FractionalQty`] or [`CatalogError::QtyOverflow`].
    pub fn to_base(&self, qty: Qty) -> Result<Qty, CatalogError> {
        self.check_qty(qty)?;
        qty.times_factor(self.factor)
            .ok_or(CatalogError::QtyOverflow)
    }
}

/// A product's units: the base (stock) unit plus up to [`MAX_EXTRA_UNITS`] others, and the
/// default sale and purchase units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductUnits {
    /// The base (stock) unit; must be in `units`.
    pub base: UnitId,
    /// All units, including the base unit and archived ones.
    pub units: Vec<Unit>,
    /// The unit the register offers first.
    pub default_sale: UnitId,
    /// The unit purchasing offers first.
    pub default_purchase: UnitId,
}

impl ProductUnits {
    /// A single base unit that is also the default sale and purchase unit.
    #[must_use]
    pub fn single(base: Unit) -> Self {
        Self {
            base: base.id,
            default_sale: base.id,
            default_purchase: base.id,
            units: vec![base],
        }
    }

    /// The unit with this ID.
    #[must_use]
    pub fn get(&self, id: UnitId) -> Option<&Unit> {
        self.units.iter().find(|u| u.id == id)
    }

    /// The base unit, if `base` points at one of `units`.
    #[must_use]
    pub fn base_unit(&self) -> Option<&Unit> {
        self.get(self.base)
    }

    /// Converts `qty` of `unit` to base units.
    ///
    /// # Errors
    /// [`CatalogError::UnknownUnit`], or see [`Unit::to_base`].
    pub fn to_base(&self, unit: UnitId, qty: Qty) -> Result<Qty, CatalogError> {
        self.get(unit)
            .ok_or(CatalogError::UnknownUnit)?
            .to_base(qty)
    }

    /// Validates the unit rules of spec 2.2 §4:
    /// - IDs are unique; the base unit exists, is active and has factor 1;
    /// - names are 1–24 chars (trimmed), short names ≤8 chars, active names unique
    ///   (case- and accent-insensitive);
    /// - at most [`MAX_EXTRA_UNITS`] active units besides the base unit;
    /// - non-base factors are > 1, and whole when the base unit is integer; when the base unit
    ///   is integer, every unit is integer (a decimal quantity would give a fractional stock
    ///   quantity);
    /// - the default sale / purchase unit exists, is active and is sellable / purchasable.
    ///
    /// # Errors
    /// The first broken rule.
    pub fn validate(&self) -> Result<(), CatalogError> {
        for (i, unit) in self.units.iter().enumerate() {
            if self.units[..i].iter().any(|u| u.id == unit.id) {
                return Err(CatalogError::DuplicateId { what: "unit" });
            }
        }
        let base = self.base_unit().ok_or(CatalogError::UnknownUnit)?;
        if base.archived {
            return Err(CatalogError::InvalidField {
                field: "base_unit",
                reason: "the base unit cannot be archived",
            });
        }
        if base.factor != Qty::ONE {
            return Err(CatalogError::InvalidUnitFactor {
                unit: base.name.clone(),
                reason: "the base unit factor must be 1",
            });
        }
        let mut names: Vec<String> = Vec::new();
        for unit in &self.units {
            let len = unit.name.trim().chars().count();
            if len == 0 || len > MAX_UNIT_NAME_LEN || unit.name.trim() != unit.name {
                return Err(CatalogError::InvalidField {
                    field: "units.name",
                    reason: "1–24 characters without surrounding spaces",
                });
            }
            if unit.short_name.chars().count() > MAX_UNIT_SHORT_NAME_LEN {
                return Err(CatalogError::InvalidField {
                    field: "units.short_name",
                    reason: "at most 8 characters",
                });
            }
            if unit.archived {
                continue;
            }
            let key = search_key(&unit.name);
            if names.contains(&key) {
                return Err(CatalogError::DuplicateUnitName {
                    name: unit.name.clone(),
                });
            }
            names.push(key);
            if unit.id != base.id {
                check_factor(base, unit)?;
            }
        }
        if names.len() - 1 > MAX_EXTRA_UNITS {
            return Err(CatalogError::TooManyUnits {
                max: MAX_EXTRA_UNITS,
            });
        }
        self.check_default("sale", self.default_sale, |u| u.sellable, "not sellable")?;
        self.check_default(
            "purchase",
            self.default_purchase,
            |u| u.purchasable,
            "not purchasable",
        )
    }

    fn check_default(
        &self,
        which: &'static str,
        id: UnitId,
        flag: fn(&Unit) -> bool,
        flag_reason: &'static str,
    ) -> Result<(), CatalogError> {
        let invalid = |reason| CatalogError::InvalidDefaultUnit { which, reason };
        let unit = self.get(id).ok_or_else(|| invalid("unknown unit"))?;
        if unit.archived {
            return Err(invalid("archived"));
        }
        if !flag(unit) {
            return Err(invalid(flag_reason));
        }
        Ok(())
    }
}

fn check_factor(base: &Unit, unit: &Unit) -> Result<(), CatalogError> {
    let invalid = |reason| CatalogError::InvalidUnitFactor {
        unit: unit.name.clone(),
        reason,
    };
    if unit.factor <= Qty::ONE {
        return Err(invalid("must be greater than 1"));
    }
    if base.kind == UnitKind::Integer {
        if !unit.factor.is_whole() {
            return Err(invalid(
                "must be a whole number when the base unit is integer",
            ));
        }
        if unit.kind == UnitKind::Decimal {
            return Err(invalid("a decimal unit needs a decimal base unit"));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A test unit with a deterministic ID.
    pub(crate) fn unit(n: u128, name: &str, kind: UnitKind, factor: i64) -> Unit {
        Unit {
            id: UnitId::from_uuid(uuid::Uuid::from_u128(n)),
            name: name.to_owned(),
            short_name: String::new(),
            kind,
            factor: Qty::from_thousandths(factor),
            sellable: true,
            purchasable: true,
            archived: false,
        }
    }

    pub(crate) fn pcs() -> Unit {
        unit(1, "pcs", UnitKind::Integer, 1_000)
    }

    pub(crate) fn kg() -> Unit {
        unit(1, "kg", UnitKind::Decimal, 1_000)
    }

    fn with(base: Unit, others: Vec<Unit>) -> ProductUnits {
        let mut units = ProductUnits::single(base);
        units.units.extend(others);
        units
    }

    fn factor_err(unit: &str, reason: &'static str) -> Result<(), CatalogError> {
        Err(CatalogError::InvalidUnitFactor {
            unit: unit.to_owned(),
            reason,
        })
    }

    #[test]
    fn unit_list_rules() {
        let list = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        assert_eq!(validate_unit_list(&list(&["pcs", "box", "kg"])), Ok(()));
        assert_eq!(validate_unit_list(&[]), Ok(()));
        assert_eq!(
            validate_unit_list(&list(&["pcs", "PCS"])),
            Err(CatalogError::DuplicateUnitName { name: "PCS".into() })
        );
        let field = |reason| {
            Err(CatalogError::InvalidField {
                field: "unit_names",
                reason,
            })
        };
        let bad = "1–24 characters without surrounding spaces";
        assert_eq!(validate_unit_list(&list(&[""])), field(bad));
        assert_eq!(validate_unit_list(&list(&[" pcs"])), field(bad));
        assert_eq!(validate_unit_list(&["x".repeat(25)]), field(bad));
        let many: Vec<String> = (0..101).map(|i| format!("u{i}")).collect();
        assert_eq!(validate_unit_list(&many), field("at most 100 names"));
    }

    #[test]
    fn factor_rules() {
        use UnitKind::{Decimal, Integer};
        let cases = [
            (pcs(), unit(2, "box", Integer, 24_000), Ok(())),
            (
                pcs(),
                unit(2, "box", Integer, 2_500),
                factor_err(
                    "box",
                    "must be a whole number when the base unit is integer",
                ),
            ),
            (kg(), unit(2, "sack", Decimal, 2_500), Ok(())),
            (kg(), unit(2, "sack", Integer, 25_000), Ok(())),
            (
                pcs(),
                unit(2, "box", Integer, 1_000),
                factor_err("box", "must be greater than 1"),
            ),
            (
                kg(),
                unit(2, "gram", Decimal, 1),
                factor_err("gram", "must be greater than 1"),
            ),
            (
                pcs(),
                unit(2, "kg", Decimal, 2_000),
                factor_err("kg", "a decimal unit needs a decimal base unit"),
            ),
        ];
        for (base, other, expected) in cases {
            assert_eq!(
                with(base, vec![other.clone()]).validate(),
                expected,
                "{other:?}"
            );
        }
    }

    #[test]
    fn structural_rules() {
        use UnitKind::Integer;
        let mut base_factor = with(pcs(), vec![]);
        base_factor.units[0].factor = Qty::from_thousandths(2_000);
        assert_eq!(
            base_factor.validate(),
            factor_err("pcs", "the base unit factor must be 1")
        );

        let mut unknown_base = with(pcs(), vec![]);
        unknown_base.base = UnitId::from_uuid(uuid::Uuid::from_u128(99));
        assert_eq!(unknown_base.validate(), Err(CatalogError::UnknownUnit));

        let mut archived_base = with(pcs(), vec![]);
        archived_base.units[0].archived = true;
        assert!(matches!(
            archived_base.validate(),
            Err(CatalogError::InvalidField {
                field: "base_unit",
                ..
            })
        ));

        let dup_id = with(pcs(), vec![unit(1, "box", Integer, 6_000)]);
        assert_eq!(
            dup_id.validate(),
            Err(CatalogError::DuplicateId { what: "unit" })
        );

        let dup_name = with(
            pcs(),
            vec![
                unit(2, "Box", Integer, 6_000),
                unit(3, "BOX", Integer, 12_000),
            ],
        );
        assert_eq!(
            dup_name.validate(),
            Err(CatalogError::DuplicateUnitName {
                name: "BOX".to_owned()
            })
        );
        // An archived unit frees its name (`box(24)` replaced by `box(20)`).
        let mut replaced = with(
            pcs(),
            vec![
                unit(2, "box", Integer, 24_000),
                unit(3, "box", Integer, 20_000),
            ],
        );
        replaced.units[1].archived = true;
        assert_eq!(replaced.validate(), Ok(()));

        for name in ["", " box", &"x".repeat(25)] {
            let bad = with(pcs(), vec![unit(2, name, Integer, 6_000)]);
            assert!(
                matches!(
                    bad.validate(),
                    Err(CatalogError::InvalidField {
                        field: "units.name",
                        ..
                    })
                ),
                "{name:?}"
            );
        }
        let mut short = with(pcs(), vec![]);
        short.units[0].short_name = "123456789".to_owned();
        assert!(matches!(
            short.validate(),
            Err(CatalogError::InvalidField {
                field: "units.short_name",
                ..
            })
        ));
    }

    #[test]
    fn at_most_five_extra_units() {
        let extras = |n: u128| {
            (0..n)
                .map(|i| unit(10 + i, &format!("u{i}"), UnitKind::Integer, 2_000))
                .collect()
        };
        assert_eq!(with(pcs(), extras(5)).validate(), Ok(()));
        assert_eq!(
            with(pcs(), extras(6)).validate(),
            Err(CatalogError::TooManyUnits {
                max: MAX_EXTRA_UNITS
            })
        );
        let mut archived_sixth = with(pcs(), extras(6));
        archived_sixth.units[6].archived = true;
        assert_eq!(archived_sixth.validate(), Ok(()));
    }

    #[test]
    fn default_units() {
        let box_ = unit(2, "box", UnitKind::Integer, 24_000);
        let box_id = box_.id;
        let invalid = |which, reason| Err(CatalogError::InvalidDefaultUnit { which, reason });

        let mut buy_box = with(pcs(), vec![box_.clone()]);
        buy_box.default_purchase = box_id;
        assert_eq!(buy_box.validate(), Ok(()));

        let mut unknown = with(pcs(), vec![]);
        unknown.default_sale = box_id;
        assert_eq!(unknown.validate(), invalid("sale", "unknown unit"));

        let mut archived = with(pcs(), vec![box_.clone()]);
        archived.units[1].archived = true;
        archived.default_purchase = box_id;
        assert_eq!(archived.validate(), invalid("purchase", "archived"));

        let mut not_sellable = with(pcs(), vec![]);
        not_sellable.units[0].sellable = false;
        assert_eq!(not_sellable.validate(), invalid("sale", "not sellable"));

        let mut not_purchasable = with(pcs(), vec![]);
        not_purchasable.units[0].purchasable = false;
        assert_eq!(
            not_purchasable.validate(),
            invalid("purchase", "not purchasable")
        );
    }

    #[test]
    fn qty_and_conversion() {
        let box24 = unit(2, "box", UnitKind::Integer, 24_000);
        let units = with(pcs(), vec![box24.clone()]);
        let q = Qty::from_thousandths;
        assert_eq!(units.to_base(box24.id, q(2_000)), Ok(q(48_000)));
        assert_eq!(
            units.to_base(box24.id, q(1_500)),
            Err(CatalogError::FractionalQty {
                unit: "box".to_owned()
            })
        );
        assert_eq!(kg().to_base(q(250)), Ok(q(250)));
        assert_eq!(kg().check_qty(q(125)), Ok(()));
        assert_eq!(
            units.to_base(UnitId::from_uuid(uuid::Uuid::from_u128(99)), q(1_000)),
            Err(CatalogError::UnknownUnit)
        );
        let huge = unit(3, "pallet", UnitKind::Integer, 1_000_000_000);
        assert_eq!(
            huge.to_base(q(i64::MAX / 1_000 * 1_000)),
            Err(CatalogError::QtyOverflow)
        );
        assert_eq!(units.base_unit().map(|u| u.name.as_str()), Some("pcs"));
    }
}
