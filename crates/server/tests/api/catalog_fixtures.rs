//! Product documents and helpers for the catalog API tests (spec 2.2, phase C3).

use axum::http::Method;
use serde_json::{Value, json};
use sultan_core::catalog::gtin_check_digit;

use crate::common::{Harness, Response, new_id};

/// The seeded `Standard` tax class.
pub const STANDARD: &str = "00000000-0000-7000-8000-000000000101";

/// An EAN-13 with a valid check digit for a 12-digit body.
pub fn ean13(body: &str) -> String {
    format!("{body}{}", gtin_check_digit(body).unwrap())
}

/// A product body and the IDs a test needs.
pub struct Doc {
    pub id: String,
    pub body: Value,
    pub base_unit: String,
    pub variants: Vec<String>,
}

/// A stock product without options: one variant (SKU from the code), unit pcs, one barcode.
pub fn simple(code: &str, barcode: &str) -> Doc {
    let (id, pcs, variant) = (new_id(), new_id(), new_id());
    let body = json!({
        "code": code,
        "name": format!("Produk {code}"),
        "type": "stock",
        "tax_class_id": STANDARD,
        "units": [{ "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
        "base_unit_id": pcs,
        "variants": [{ "id": variant }],
        "barcodes": [{ "id": new_id(), "variant_id": variant, "unit_id": pcs, "code": barcode }],
    });
    Doc {
        id,
        body,
        base_unit: pcs,
        variants: vec![variant],
    }
}

/// `Kaos Polos`: Size (S, M) × Color (RED, BLUE) → 4 variants with default SKUs, units pcs and
/// box(24), an EAN-13 per variant on pcs (`<prefix>00000<i>` + check digit) and one box barcode.
pub fn kaos(code: &str, prefix: &str) -> Doc {
    let (id, pcs, box_) = (new_id(), new_id(), new_id());
    let sizes = [(new_id(), "Small", "S"), (new_id(), "Medium", "M")];
    let colors = [(new_id(), "Red", "RED"), (new_id(), "Blue", "BLUE")];
    let mut variants = Vec::new();
    let mut variant_rows = Vec::new();
    for s in &sizes {
        for c in &colors {
            let v = new_id();
            variant_rows.push(json!({ "id": v, "option_value_ids": [s.0, c.0] }));
            variants.push(v);
        }
    }
    let mut barcodes: Vec<Value> = variants
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json!({
                "id": new_id(), "variant_id": v, "unit_id": pcs,
                "code": ean13(&format!("{prefix}{i:06}")),
            })
        })
        .collect();
    barcodes.push(json!({
        "id": new_id(), "variant_id": variants[0], "unit_id": box_,
        "code": format!("BOX-{code}"),
    }));
    let values = |list: &[(String, &str, &str); 2]| -> Vec<Value> {
        list.iter()
            .map(|(id, name, code)| json!({ "id": id, "name": name, "code": code }))
            .collect()
    };
    let body = json!({
        "code": code,
        "name": "Kaos Polos",
        "type": "stock",
        "tax_class_id": STANDARD,
        "tags": ["Promo"],
        "options": [
            { "id": new_id(), "name": "Size", "values": values(&sizes) },
            { "id": new_id(), "name": "Color", "values": values(&colors) },
        ],
        "units": [
            { "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 },
            { "id": box_, "name": "box", "kind": "integer", "factor_thousandths": 24000 },
        ],
        "base_unit_id": pcs,
        "default_purchase_unit_id": box_,
        "variants": variant_rows,
        "barcodes": barcodes,
    });
    Doc {
        id,
        body,
        base_unit: pcs,
        variants,
    }
}

/// `PUT /products/{id}` as the holder of `token`.
pub async fn put_product(h: &Harness, token: &str, id: &str, body: &Value) -> Response {
    h.call(
        token,
        Method::PUT,
        &format!("/api/v1/products/{id}"),
        Some(body.clone()),
    )
    .await
}

/// Creates `doc` as `token`; asserts `201`.
pub async fn create(h: &Harness, token: &str, doc: &Doc) -> Value {
    let r = put_product(h, token, &doc.id, &doc.body).await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    r.body
}

/// `GET {uri}` with only the device token.
pub async fn as_device(h: &Harness, device: &str, uri: &str) -> Response {
    h.send(Method::GET, uri, &[("x-register-token", device)], None)
        .await
}

/// `PUT /categories/{id}`.
pub async fn put_category(h: &Harness, token: &str, id: &str, body: Value) -> Response {
    h.call(
        token,
        Method::PUT,
        &format!("/api/v1/categories/{id}"),
        Some(body),
    )
    .await
}

/// `PUT /brands/{id}`.
pub async fn put_brand(h: &Harness, token: &str, id: &str, name: &str) -> Response {
    h.call(
        token,
        Method::PUT,
        &format!("/api/v1/brands/{id}"),
        Some(json!({ "name": name })),
    )
    .await
}
