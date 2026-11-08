//! Product documents for the catalog tests.

use sultan_core::catalog::{
    Barcode, BundleComponent, OptionValue, Plu, ProductDraft, ProductOption, ProductType,
    ProductUnits, Qty, Tracking, Unit, UnitKind, Variant, gtin_check_digit,
};
use sultan_core::ids::{BarcodeId, OptionId, OptionValueId, ProductId, UnitId, VariantId};
use sultan_db::Db;
use sultan_db::catalog::{Actor, ProductDoc, STANDARD_TAX_CLASS, Written, put_product};

use crate::common::{Org, count};

pub fn actor(o: &Org) -> Actor {
    Actor {
        user: Some(o.manager.id),
        branch: Some(o.jkt.id),
        register: None,
    }
}

pub fn unit(name: &str, kind: UnitKind, factor: i64) -> Unit {
    Unit {
        id: UnitId::generate(),
        name: name.to_owned(),
        short_name: String::new(),
        kind,
        factor: Qty::from_thousandths(factor),
        sellable: true,
        purchasable: true,
        archived: false,
    }
}

pub fn variant(sku: &str, values: Vec<OptionValueId>) -> Variant {
    Variant {
        id: VariantId::generate(),
        sku: sku.to_owned(),
        values,
        plu: None,
        archived: false,
    }
}

pub fn barcode(variant: VariantId, unit: UnitId, code: &str) -> Barcode {
    Barcode {
        id: BarcodeId::generate(),
        variant,
        unit,
        code: code.to_owned(),
        allow_invalid_check_digit: true,
    }
}

fn draft(code: &str, product_type: ProductType, units: ProductUnits) -> ProductDraft {
    ProductDraft {
        id: ProductId::generate(),
        code: code.to_owned(),
        name: format!("Produk {code}"),
        receipt_name: String::new(),
        product_type,
        tracking: Tracking::None,
        tags: vec![" Promo ".to_owned()],
        sellable: true,
        purchasable: product_type.default_purchasable(),
        open_price: false,
        requires_note: false,
        options: vec![],
        units,
        variants: vec![],
        barcodes: vec![],
        components: vec![],
        archived: false,
    }
}

pub fn doc(draft: ProductDraft) -> ProductDoc {
    ProductDoc {
        draft,
        category: None,
        brand: None,
        tax_class: STANDARD_TAX_CLASS,
    }
}

/// A stock product without options: one variant (SKU = code), unit pcs, one barcode.
pub fn simple(code: &str, barcode_code: &str) -> ProductDoc {
    let mut d = draft(
        code,
        ProductType::Stock,
        ProductUnits::single(unit("pcs", UnitKind::Integer, 1_000)),
    );
    let v = variant(code, vec![]);
    d.barcodes = vec![barcode(v.id, d.units.base, barcode_code)];
    d.variants = vec![v];
    doc(d)
}

/// A stock product with Size (S, M) × Color (RED, BLUE): 4 variants `<code>-<size>-<color>`,
/// units pcs + box(24), a pcs barcode `<code>-<i>` on each variant.
pub fn kaos(code: &str) -> ProductDoc {
    let pcs = unit("pcs", UnitKind::Integer, 1_000);
    let box_ = unit("box", UnitKind::Integer, 24_000);
    let mut d = draft(code, ProductType::Stock, ProductUnits::single(pcs));
    d.units.units.push(box_);
    let value = |name: &str, code: &str| OptionValue {
        id: OptionValueId::generate(),
        name: name.to_owned(),
        code: code.to_owned(),
    };
    d.options = vec![
        ProductOption {
            id: OptionId::generate(),
            name: "Size".into(),
            values: vec![value("Small", "S"), value("Medium", "M")],
        },
        ProductOption {
            id: OptionId::generate(),
            name: "Color".into(),
            values: vec![value("Red", "RED"), value("Blue", "BLUE")],
        },
    ];
    for size in &d.options[0].values {
        for color in &d.options[1].values {
            d.variants.push(variant(
                &format!("{code}-{}-{}", size.code, color.code),
                vec![size.id, color.id],
            ));
        }
    }
    d.barcodes = d
        .variants
        .iter()
        .enumerate()
        .map(|(i, v)| barcode(v.id, d.units.base, &format!("{code}-{i}")))
        .collect();
    doc(d)
}

/// A weighed product (kg) with one variant and `plu`.
pub fn cheese(code: &str, plu: u32) -> ProductDoc {
    let mut d = draft(
        code,
        ProductType::Weighed,
        ProductUnits::single(unit("kg", UnitKind::Decimal, 1_000)),
    );
    let mut v = variant(code, vec![]);
    v.plu = Plu::new(plu);
    d.variants = vec![v];
    doc(d)
}

/// A bundle of `components`.
pub fn bundle(code: &str, components: Vec<BundleComponent>) -> ProductDoc {
    let mut d = draft(
        code,
        ProductType::Bundle,
        ProductUnits::single(unit("pcs", UnitKind::Integer, 1_000)),
    );
    d.variants = vec![variant(code, vec![])];
    d.components = components;
    doc(d)
}

/// One component line: `qty` thousandths of the first variant of `of`, in its base unit.
pub fn component(of: &ProductDoc, qty: i64) -> BundleComponent {
    BundleComponent {
        variant: of.draft.variants[0].id,
        unit: of.draft.units.base,
        qty: Qty::from_thousandths(qty),
    }
}

/// An EAN-13 with a valid check digit for a 12-digit body.
pub fn ean13(body: &str) -> String {
    format!("{body}{}", gtin_check_digit(body).unwrap())
}

/// Writes and commits a product; returns the stored document.
pub async fn put(db: &Db, o: &Org, doc: &ProductDoc) -> (Written, ProductDoc) {
    let mut tx = db.begin().await.unwrap();
    let out = put_product(&mut tx, doc, &actor(o), crate::common::t(10))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    out
}

/// `(audit rows, outbox rows)`.
pub async fn trail(db: &Db) -> (i64, i64) {
    (count(db, "audit_log").await, count(db, "outbox").await)
}
