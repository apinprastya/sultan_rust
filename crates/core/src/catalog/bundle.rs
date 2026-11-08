//! Bundle (kit) components: validation and explosion into stock movements (spec 2.2 §5).

use super::unit::UnitKind;
use super::{CatalogError, ProductType, Qty};
use crate::ids::{ProductId, UnitId, VariantId};

/// Most components per bundle.
pub const MAX_BUNDLE_COMPONENTS: usize = 50;

/// One component line of a bundle: `qty` of `variant` in `unit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BundleComponent {
    /// The component variant (of another product).
    pub variant: VariantId,
    /// The unit of `qty`.
    pub unit: UnitId,
    /// Quantity per bundle, > 0.
    pub qty: Qty,
}

/// What [`validate_bundle_components`] needs to know about a component's variant + unit, looked
/// up by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentInfo {
    /// The product the variant belongs to.
    pub product: ProductId,
    /// That product's type.
    pub product_type: ProductType,
    /// Whether the variant (or its product) is archived.
    pub archived: bool,
    /// The kind of the component unit.
    pub unit_kind: UnitKind,
    /// The factor of the component unit to its base unit.
    pub factor: Qty,
}

/// Checks the component lines on their own: 1–[`MAX_BUNDLE_COMPONENTS`] lines, each `qty > 0`,
/// no variant + unit twice.
///
/// # Errors
/// [`CatalogError::BundleComponentCount`], [`CatalogError::NonPositive`] or
/// [`CatalogError::DuplicateBundleComponent`].
pub fn check_component_lines(components: &[BundleComponent]) -> Result<(), CatalogError> {
    if components.is_empty() || components.len() > MAX_BUNDLE_COMPONENTS {
        return Err(CatalogError::BundleComponentCount {
            count: components.len(),
            max: MAX_BUNDLE_COMPONENTS,
        });
    }
    for (i, c) in components.iter().enumerate() {
        if !c.qty.is_positive() {
            return Err(CatalogError::NonPositive {
                field: "components.qty",
            });
        }
        if components[..i]
            .iter()
            .any(|e| e.variant == c.variant && e.unit == c.unit)
        {
            return Err(CatalogError::DuplicateBundleComponent { variant: c.variant });
        }
    }
    Ok(())
}

/// Validates the components of bundle product `bundle`: the line rules of
/// [`check_component_lines`], plus for each component (resolved with `resolve`): it exists, it
/// is not part of the bundle itself, it is not a bundle (`bundle_nesting`), it is not archived,
/// and its quantity fits its unit (whole numbers for integer units).
///
/// # Errors
/// The first broken rule.
pub fn validate_bundle_components(
    bundle: ProductId,
    components: &[BundleComponent],
    resolve: impl Fn(VariantId, UnitId) -> Option<ComponentInfo>,
) -> Result<(), CatalogError> {
    check_component_lines(components)?;
    for c in components {
        let info = resolve(c.variant, c.unit)
            .ok_or(CatalogError::UnknownBundleComponent { variant: c.variant })?;
        if info.product == bundle {
            return Err(CatalogError::BundleComponentSelf { variant: c.variant });
        }
        if info.product_type == ProductType::Bundle {
            return Err(CatalogError::BundleNesting { variant: c.variant });
        }
        if info.archived {
            return Err(CatalogError::BundleComponentArchived { variant: c.variant });
        }
        if info.unit_kind == UnitKind::Integer && !c.qty.is_whole() {
            return Err(CatalogError::FractionalQty {
                unit: "component unit".to_owned(),
            });
        }
    }
    Ok(())
}

/// A component as the sale line snapshots it at sale time, so later bundle edits never change
/// past sales (spec 2.2 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentSnapshot {
    /// The component variant.
    pub variant: VariantId,
    /// Its product type (service components move no stock).
    pub product_type: ProductType,
    /// The component unit.
    pub unit: UnitId,
    /// Quantity per bundle in `unit`.
    pub qty: Qty,
    /// Factor of `unit` to the base unit.
    pub factor: Qty,
}

impl ComponentSnapshot {
    /// The base quantity per bundle: `qty × factor`.
    fn base_per_bundle(&self) -> Option<Qty> {
        self.qty.times_factor(self.factor)
    }
}

/// The base quantity of one component variant consumed by a bundle sale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentMovement {
    /// The component variant.
    pub variant: VariantId,
    /// Base units consumed (a sale posts the negative, a return the positive).
    pub base_qty: Qty,
}

/// Explodes `bundles` sold bundles into the base quantity consumed per component variant:
/// `bundles × qty × factor`, merged per variant in first-seen order. Service components are
/// skipped (no stock). E.g. 2 × (1 pcs A + 0.5 kg B + 1 box(6) C) → A 2, B 1.000, C 12.
///
/// # Errors
/// [`CatalogError::QtyOverflow`].
pub fn explode_bundle(
    bundles: Qty,
    components: &[ComponentSnapshot],
) -> Result<Vec<ComponentMovement>, CatalogError> {
    let mut movements: Vec<ComponentMovement> = Vec::new();
    for c in components.iter().filter(|c| c.product_type.has_stock()) {
        let base_qty = c
            .base_per_bundle()
            .and_then(|per| per.times_factor(bundles))
            .ok_or(CatalogError::QtyOverflow)?;
        match movements.iter_mut().find(|m| m.variant == c.variant) {
            Some(m) => {
                m.base_qty = m
                    .base_qty
                    .checked_add(base_qty)
                    .ok_or(CatalogError::QtyOverflow)?;
            }
            None => movements.push(ComponentMovement {
                variant: c.variant,
                base_qty,
            }),
        }
    }
    Ok(movements)
}

/// How many bundles the stock covers: the minimum over stocked components of
/// ⌊stock / required⌋, informational only (spec 2.2 §5). `None` when no component is stocked.
#[must_use]
pub fn available_bundles(
    components: &[ComponentSnapshot],
    stock: impl Fn(VariantId) -> Qty,
) -> Option<i64> {
    let per_bundle = explode_bundle(Qty::ONE, components).ok()?;
    per_bundle
        .iter()
        .filter_map(|m| stock(m.variant).whole_times(m.base_qty))
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn vid(n: u128) -> VariantId {
        VariantId::from_uuid(Uuid::from_u128(n))
    }
    fn uid(n: u128) -> UnitId {
        UnitId::from_uuid(Uuid::from_u128(n))
    }
    fn pid(n: u128) -> ProductId {
        ProductId::from_uuid(Uuid::from_u128(n))
    }
    fn q(t: i64) -> Qty {
        Qty::from_thousandths(t)
    }
    fn line(v: u128, u: u128, qty: i64) -> BundleComponent {
        BundleComponent {
            variant: vid(v),
            unit: uid(u),
            qty: q(qty),
        }
    }

    /// Variant n belongs to product n; 3 is a bundle, 4 archived, 5 a kg item, 6 a service.
    fn resolve(v: VariantId, _u: UnitId) -> Option<ComponentInfo> {
        let n = v.as_uuid().as_u128();
        (n < 10).then(|| ComponentInfo {
            product: pid(n),
            product_type: match n {
                3 => ProductType::Bundle,
                5 => ProductType::Weighed,
                6 => ProductType::Service,
                _ => ProductType::Stock,
            },
            archived: n == 4,
            unit_kind: if n == 5 {
                UnitKind::Decimal
            } else {
                UnitKind::Integer
            },
            factor: Qty::ONE,
        })
    }

    #[test]
    fn component_validation() {
        let bundle = pid(9);
        let cases = [
            (
                vec![line(1, 1, 1_000), line(5, 1, 500), line(6, 1, 1_000)],
                Ok(()),
            ),
            (vec![line(1, 1, 1_000), line(1, 2, 1_000)], Ok(())),
            (
                vec![],
                Err(CatalogError::BundleComponentCount { count: 0, max: 50 }),
            ),
            (
                vec![line(3, 1, 1_000)],
                Err(CatalogError::BundleNesting { variant: vid(3) }),
            ),
            (
                vec![line(4, 1, 1_000)],
                Err(CatalogError::BundleComponentArchived { variant: vid(4) }),
            ),
            (
                vec![line(9, 1, 1_000)],
                Err(CatalogError::BundleComponentSelf { variant: vid(9) }),
            ),
            (
                vec![line(42, 1, 1_000)],
                Err(CatalogError::UnknownBundleComponent { variant: vid(42) }),
            ),
            (
                vec![line(1, 1, 0)],
                Err(CatalogError::NonPositive {
                    field: "components.qty",
                }),
            ),
            (
                vec![line(1, 1, -1_000)],
                Err(CatalogError::NonPositive {
                    field: "components.qty",
                }),
            ),
            (
                vec![line(1, 1, 1_000), line(1, 1, 2_000)],
                Err(CatalogError::DuplicateBundleComponent { variant: vid(1) }),
            ),
            (
                vec![line(1, 1, 1_500)],
                Err(CatalogError::FractionalQty {
                    unit: "component unit".into(),
                }),
            ),
        ];
        for (components, expected) in cases {
            assert_eq!(
                validate_bundle_components(bundle, &components, resolve),
                expected,
                "{components:?}"
            );
        }
        let too_many: Vec<BundleComponent> = (0..51).map(|i| line(1, 100 + i, 1_000)).collect();
        assert_eq!(
            check_component_lines(&too_many),
            Err(CatalogError::BundleComponentCount { count: 51, max: 50 })
        );
        assert_eq!(check_component_lines(&too_many[..50]), Ok(()));
    }

    fn snap(v: u128, product_type: ProductType, qty: i64, factor: i64) -> ComponentSnapshot {
        ComponentSnapshot {
            variant: vid(v),
            product_type,
            unit: uid(v),
            qty: q(qty),
            factor: q(factor),
        }
    }

    #[test]
    fn explosion_of_two_bundles() {
        // 1 pcs A + 0.5 kg B + 1 box(6) C, plus a service that moves no stock.
        let kit = [
            snap(1, ProductType::Stock, 1_000, 1_000),
            snap(2, ProductType::Weighed, 500, 1_000),
            snap(3, ProductType::Stock, 1_000, 6_000),
            snap(4, ProductType::Service, 1_000, 1_000),
        ];
        let movements = explode_bundle(q(2_000), &kit).unwrap();
        let sale: Vec<(VariantId, String)> = movements
            .iter()
            .map(|m| (m.variant, m.base_qty.checked_neg().unwrap().to_string()))
            .collect();
        assert_eq!(
            sale,
            [
                (vid(1), "-2.000".to_owned()),
                (vid(2), "-1.000".to_owned()),
                (vid(3), "-12.000".to_owned())
            ]
        );
    }

    #[test]
    fn explosion_merges_units_of_one_variant_and_reports_overflow() {
        let kit = [
            snap(1, ProductType::Stock, 1_000, 1_000),
            snap(1, ProductType::Stock, 1_000, 6_000),
        ];
        assert_eq!(
            explode_bundle(q(1_000), &kit),
            Ok(vec![ComponentMovement {
                variant: vid(1),
                base_qty: q(7_000)
            }])
        );
        let huge = [snap(1, ProductType::Stock, i64::MAX, 1_000)];
        assert_eq!(
            explode_bundle(q(2_000), &huge),
            Err(CatalogError::QtyOverflow)
        );
        let sum_overflow = [
            snap(1, ProductType::Stock, i64::MAX, 1_000),
            snap(1, ProductType::Stock, 1_000, 1_000),
        ];
        assert_eq!(
            explode_bundle(q(1_000), &sum_overflow),
            Err(CatalogError::QtyOverflow)
        );
    }

    #[test]
    fn availability() {
        let kit = [
            snap(1, ProductType::Stock, 1_000, 1_000),
            snap(2, ProductType::Stock, 1_000, 6_000),
        ];
        let stock = |v: VariantId| if v == vid(1) { q(10_000) } else { q(30_000) };
        assert_eq!(available_bundles(&kit, stock), Some(5));
        let services = [snap(1, ProductType::Service, 1_000, 1_000)];
        assert_eq!(available_bundles(&services, stock), None);
        let huge = [snap(1, ProductType::Stock, i64::MAX, 2_000)];
        assert_eq!(available_bundles(&huge, stock), None);
    }
}
