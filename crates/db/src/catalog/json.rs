//! Stored-name mappings of the catalog enums and the JSON shapes of audit / outbox rows and of the
//! JSON columns (tags, scale layouts, unit list). Quantities are integer thousandths.

use serde_json::{Value, json};
use sultan_core::catalog::{
    Barcode, BundleComponent, Plu, ProductType, ScaleLayout, ScaleValueKind, Tracking, UnitKind,
};
use sultan_core::time::Timestamp;

use super::product::ProductDoc;
use crate::error::{DbError, Result};

pub(super) const fn unit_kind_str(kind: UnitKind) -> &'static str {
    match kind {
        UnitKind::Integer => "integer",
        UnitKind::Decimal => "decimal",
    }
}

pub(super) fn parse_unit_kind(s: &str) -> Result<UnitKind> {
    match s {
        "integer" => Ok(UnitKind::Integer),
        "decimal" => Ok(UnitKind::Decimal),
        other => Err(DbError::Corrupt(format!("unknown unit kind `{other}`"))),
    }
}

pub(super) fn parse_product_type(s: &str) -> Result<ProductType> {
    s.parse()
        .map_err(|_| DbError::Corrupt(format!("unknown product type `{s}`")))
}

pub(super) fn parse_tracking(s: &str) -> Result<Tracking> {
    [Tracking::None, Tracking::Batch, Tracking::Serial]
        .into_iter()
        .find(|t| t.as_str() == s)
        .ok_or_else(|| DbError::Corrupt(format!("unknown tracking `{s}`")))
}

const fn scale_kind_str(kind: ScaleValueKind) -> &'static str {
    match kind {
        ScaleValueKind::Weight => "weight",
        ScaleValueKind::Price => "price",
    }
}

/// The `scale_layouts` column value.
pub(super) fn layouts_json(layouts: &[ScaleLayout]) -> String {
    Value::Array(
        layouts
            .iter()
            .map(|l| {
                json!({
                    "prefix": l.prefix,
                    "plu_digits": l.plu_digits,
                    "value_digits": l.value_digits,
                    "kind": scale_kind_str(l.kind),
                })
            })
            .collect(),
    )
    .to_string()
}

/// Parses the `scale_layouts` column.
pub(super) fn parse_layouts(value: &Value) -> Result<Vec<ScaleLayout>> {
    let corrupt = || DbError::Corrupt("catalog_settings.scale_layouts is not a layout list".into());
    let digit = |v: &Value, key: &str| {
        v.get(key)
            .and_then(Value::as_u64)
            .and_then(|n| u8::try_from(n).ok())
            .ok_or_else(corrupt)
    };
    value
        .as_array()
        .ok_or_else(corrupt)?
        .iter()
        .map(|v| {
            let kind = match v.get("kind").and_then(Value::as_str) {
                Some("weight") => ScaleValueKind::Weight,
                Some("price") => ScaleValueKind::Price,
                _ => return Err(corrupt()),
            };
            Ok(ScaleLayout {
                prefix: digit(v, "prefix")?,
                plu_digits: digit(v, "plu_digits")?,
                value_digits: digit(v, "value_digits")?,
                kind,
            })
        })
        .collect()
}

/// A JSON array of strings (tags, unit list).
pub(super) fn strings_json(items: &[String]) -> String {
    Value::from(items.to_vec()).to_string()
}

/// Parses a JSON array of strings.
pub(super) fn parse_strings(value: &Value, col: &str) -> Result<Vec<String>> {
    let corrupt = || DbError::Corrupt(format!("column `{col}` is not a string list"));
    value
        .as_array()
        .ok_or_else(corrupt)?
        .iter()
        .map(|v| v.as_str().map(str::to_owned).ok_or_else(corrupt))
        .collect()
}

fn barcode_json(b: &Barcode) -> Value {
    json!({
        "id": b.id.to_string(),
        "variant_id": b.variant.to_string(),
        "unit_id": b.unit.to_string(),
        "code": b.code,
        "allow_invalid_check_digit": b.allow_invalid_check_digit,
    })
}

fn component_json(c: &BundleComponent) -> Value {
    json!({
        "variant_id": c.variant.to_string(),
        "unit_id": c.unit.to_string(),
        "qty": c.qty.thousandths(),
    })
}

/// The full product document: the audit before/after and the `catalog.product.put` payload.
pub(super) fn product_json(doc: &ProductDoc, archived_at: Option<Timestamp>) -> Value {
    let d = &doc.draft;
    let options: Vec<Value> = d
        .options
        .iter()
        .map(|o| {
            let values: Vec<Value> = o
                .values
                .iter()
                .map(|v| json!({ "id": v.id.to_string(), "name": v.name, "code": v.code }))
                .collect();
            json!({ "id": o.id.to_string(), "name": o.name, "values": values })
        })
        .collect();
    let units: Vec<Value> = d
        .units
        .units
        .iter()
        .map(|u| {
            json!({
                "id": u.id.to_string(),
                "name": u.name,
                "short_name": u.short_name,
                "kind": unit_kind_str(u.kind),
                "factor": u.factor.thousandths(),
                "sellable": u.sellable,
                "purchasable": u.purchasable,
                "archived": u.archived,
            })
        })
        .collect();
    let variants: Vec<Value> = d
        .variants
        .iter()
        .map(|v| {
            json!({
                "id": v.id.to_string(),
                "sku": v.sku,
                "values": v.values.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "plu": v.plu.map(Plu::value),
                "archived": v.archived,
            })
        })
        .collect();
    json!({
        "id": d.id.to_string(),
        "code": d.code,
        "name": d.name,
        "receipt_name": d.receipt_name,
        "type": d.product_type.as_str(),
        "tracking": d.tracking.as_str(),
        "category_id": doc.category.map(|c| c.to_string()),
        "brand_id": doc.brand.map(|b| b.to_string()),
        "tax_class_id": doc.tax_class.to_string(),
        "tags": d.tags,
        "sellable": d.sellable,
        "purchasable": d.purchasable,
        "open_price": d.open_price,
        "requires_note": d.requires_note,
        "base_unit_id": d.units.base.to_string(),
        "default_sale_unit_id": d.units.default_sale.to_string(),
        "default_purchase_unit_id": d.units.default_purchase.to_string(),
        "options": options,
        "units": units,
        "variants": variants,
        "barcodes": d.barcodes.iter().map(barcode_json).collect::<Vec<_>>(),
        "components": d.components.iter().map(component_json).collect::<Vec<_>>(),
        "archived": d.archived,
        "archived_at": archived_at.map(Timestamp::unix_millis),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_names_round_trip_and_reject_unknown() {
        for kind in [UnitKind::Integer, UnitKind::Decimal] {
            assert_eq!(parse_unit_kind(unit_kind_str(kind)).unwrap(), kind);
        }
        for t in [Tracking::None, Tracking::Batch, Tracking::Serial] {
            assert_eq!(parse_tracking(t.as_str()).unwrap(), t);
        }
        for t in ProductType::ALL {
            assert_eq!(parse_product_type(t.as_str()).unwrap(), t);
        }
        assert!(matches!(parse_unit_kind("x"), Err(DbError::Corrupt(_))));
        assert!(matches!(parse_tracking("x"), Err(DbError::Corrupt(_))));
        assert!(matches!(parse_product_type("x"), Err(DbError::Corrupt(_))));
    }

    #[test]
    fn layouts_and_lists_round_trip() {
        let layouts = ScaleLayout::default_layouts();
        let value: Value = serde_json::from_str(&layouts_json(&layouts)).unwrap();
        assert_eq!(parse_layouts(&value).unwrap(), layouts);
        for bad in [
            json!({}),
            json!([{"prefix": 21, "plu_digits": 5, "value_digits": 5, "kind": "volume"}]),
            json!([{"prefix": 300, "plu_digits": 5, "value_digits": 5, "kind": "weight"}]),
        ] {
            assert!(matches!(parse_layouts(&bad), Err(DbError::Corrupt(_))));
        }
        let list = vec!["pcs".to_owned(), "kg".to_owned()];
        let value: Value = serde_json::from_str(&strings_json(&list)).unwrap();
        assert_eq!(parse_strings(&value, "c").unwrap(), list);
        assert!(parse_strings(&json!([1]), "c").is_err());
        assert!(parse_strings(&json!("x"), "c").is_err());
    }
}
