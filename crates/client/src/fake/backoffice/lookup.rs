//! The fake's `GET /catalog/lookup` (phase C5): the core resolution order
//! (`sultan_core::catalog::resolve_lookup`) over an index of the in-memory catalog, then the
//! server's checks in its order (archived → `422 archived`, blocked at the register's branch →
//! `422 not_sold_here`, not sellable → `422 not_sellable`; nothing → `404 not_found`).
//!
//! A register PIN session looks up at [`BRANCH_ID`]; a back-office session applies no
//! assortment. Like the server's index, an active barcode, SKU or PLU wins over an archived one
//! with the same code, and a removed (archived) barcode still resolves, to `archived`.

use std::collections::HashMap;

use sultan_core::catalog::{
    CatalogError, CatalogIndex, ItemRef, ItemStatus, MatchSource, Plu, ScaleLayout, ScaleValueKind,
    check_sellable, matched_code, resolve_lookup,
};

use super::{BRANCH_ID, State, rules};
use crate::catalog::{self, Lookup, Product};
use crate::{ClientError, Problem};

/// The catalog as lookup indexes: code → (variant + unit, archived).
#[derive(Default)]
struct Index {
    barcodes: HashMap<String, (ItemRef, bool)>,
    skus: HashMap<String, (ItemRef, bool)>,
    plus: HashMap<u32, (ItemRef, bool)>,
}

/// Keeps an active entry over an archived one with the same key.
fn keep<K: std::hash::Hash + Eq>(
    map: &mut HashMap<K, (ItemRef, bool)>,
    key: K,
    item: ItemRef,
    archived: bool,
) {
    match map.get(&key) {
        Some((_, false)) if archived => {}
        _ => {
            map.insert(key, (item, archived));
        }
    }
}

impl Index {
    fn add(&mut self, p: &Product) {
        let item = |variant: &str, unit: &str| -> Option<ItemRef> {
            Some(ItemRef {
                variant: variant.parse().ok()?,
                unit: unit.parse().ok()?,
            })
        };
        let removed = p.archived_barcodes.iter().map(|b| (b, true));
        for (b, archived) in p.barcodes.iter().map(|b| (b, false)).chain(removed) {
            if let Some(i) = item(&b.variant_id, &b.unit_id) {
                keep(&mut self.barcodes, b.code.clone(), i, archived);
            }
        }
        for v in &p.variants {
            let archived = p.archived || v.archived;
            if let Some(i) = item(&v.id, &p.default_sale_unit_id) {
                keep(&mut self.skus, v.sku.clone(), i, archived);
            }
            if p.product_type == "weighed"
                && let (Some(plu), Some(i)) = (v.plu, item(&v.id, &p.base_unit_id))
            {
                keep(&mut self.plus, plu, i, archived);
            }
        }
    }

    fn barcode_archived(&self, code: &str) -> bool {
        self.barcodes
            .get(code)
            .is_some_and(|(_, archived)| *archived)
    }
}

impl CatalogIndex for Index {
    fn by_barcode(&self, code: &str) -> Option<ItemRef> {
        self.barcodes.get(code).map(|(i, _)| *i)
    }

    fn by_sku(&self, sku: &str) -> Option<ItemRef> {
        self.skus.get(sku).map(|(i, _)| *i)
    }

    fn by_plu(&self, plu: Plu) -> Option<ItemRef> {
        self.plus.get(&plu.value()).map(|(i, _)| *i)
    }
}

/// The server's answer for a lookup rule: `404 not_found`, else `422` with `errors`.
fn lookup_error(error: CatalogError) -> ClientError {
    if error == CatalogError::NotFound {
        let item = serde_json::json!({ "code": error.code(), "message": error.to_string() });
        return Problem::new(404, error.code(), &error.to_string())
            .with("errors", vec![item])
            .into();
    }
    rules(&[error])
}

/// The wire name of a match source.
const fn source_str(source: MatchSource) -> &'static str {
    match source {
        MatchSource::Barcode => "barcode",
        MatchSource::NormalizedBarcode => "normalized_barcode",
        MatchSource::ScaleBarcode => "scale_barcode",
        MatchSource::Sku => "sku",
        MatchSource::Plu => "plu",
    }
}

/// The settings' layouts in core form (unknown kinds are skipped).
fn layouts(settings: &[catalog::ScaleLayout]) -> Vec<ScaleLayout> {
    settings
        .iter()
        .filter_map(|l| {
            let kind = match l.kind.as_str() {
                "weight" => ScaleValueKind::Weight,
                "price" => ScaleValueKind::Price,
                _ => return None,
            };
            Some(ScaleLayout {
                prefix: l.prefix,
                plu_digits: l.plu_digits,
                value_digits: l.value_digits,
                kind,
            })
        })
        .collect()
}

impl State {
    /// `GET /catalog/lookup?code=` (see the module docs).
    pub(in crate::fake) fn lookup(&self, token: &str, code: &str) -> Result<Lookup, ClientError> {
        let branch = if self.caller(token).is_ok() {
            Some(BRANCH_ID)
        } else {
            self.bo_user(token)?;
            None
        };
        let mut index = Index::default();
        for stored in self.catalog.products.values() {
            index.add(&stored.product);
        }
        let layouts = layouts(&self.catalog.settings.scale_layouts);
        let found = resolve_lookup(code, &layouts, &index).map_err(lookup_error)?;
        let code = code.trim();
        let barcode_archived = match found.source {
            MatchSource::Barcode => index.barcode_archived(code),
            MatchSource::NormalizedBarcode => {
                index.barcode_archived(&matched_code(code, found.source))
            }
            _ => false,
        };
        let (variant_id, unit_id) = (found.item.variant.to_string(), found.item.unit.to_string());
        let (p, v, u) = self
            .catalog
            .products
            .values()
            .find_map(|s| {
                let p = &s.product;
                let v = p.variants.iter().find(|v| v.id == variant_id)?;
                let u = p.units.iter().find(|u| u.id == unit_id)?;
                Some((p, v, u))
            })
            .ok_or_else(|| lookup_error(CatalogError::NotFound))?;
        let blocked = branch.is_some_and(|b| {
            self.catalog
                .blocks
                .get(&p.id)
                .is_some_and(|blocks| blocks.iter().any(|x| x == b))
        });
        check_sellable(ItemStatus {
            product_archived: p.archived,
            variant_archived: v.archived || u.archived || barcode_archived,
            blocked_at_branch: blocked,
            sellable: p.sellable,
        })
        .map_err(lookup_error)?;
        let values: Vec<String> = v
            .option_value_ids
            .iter()
            .filter_map(|id| {
                p.options
                    .iter()
                    .flat_map(|o| &o.values)
                    .find(|x| x.id == *id)
                    .map(|x| x.name.clone())
            })
            .collect();
        Ok(Lookup {
            product_id: p.id.clone(),
            variant_id,
            unit_id,
            name: super::variant_name(p, v),
            product_name: p.name.clone(),
            variant_label: values.join(" / "),
            unit_name: u.name.clone(),
            sku: v.sku.clone(),
            plu: v.plu,
            product_type: p.product_type.clone(),
            open_price: p.open_price,
            requires_note: p.requires_note,
            source: source_str(found.source).to_owned(),
            matched_code: matched_code(code, found.source),
            qty_thousandths: found.qty.map(sultan_core::catalog::Qty::thousandths),
            amount: found.amount.map(sultan_core::money::Money::minor),
            branch_id: branch.map(str::to_owned),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_entries_win_over_archived_ones() {
        use sultan_core::ids::{UnitId, VariantId};
        let items: Vec<ItemRef> = (0..3)
            .map(|_| ItemRef {
                variant: VariantId::generate(),
                unit: UnitId::generate(),
            })
            .collect();
        let mut map = HashMap::new();
        keep(&mut map, "A", items[0], false);
        keep(&mut map, "A", items[1], true);
        assert_eq!(map["A"], (items[0], false));
        keep(&mut map, "B", items[1], true);
        keep(&mut map, "B", items[2], false);
        assert_eq!(map["B"], (items[2], false));
    }

    #[test]
    fn layouts_skip_unknown_kinds() {
        let layout = |kind: &str| catalog::ScaleLayout {
            prefix: 21,
            plu_digits: 5,
            value_digits: 5,
            kind: kind.to_owned(),
        };
        let got = layouts(&[layout("weight"), layout("price"), layout("volume")]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].kind, ScaleValueKind::Price);
    }
}
