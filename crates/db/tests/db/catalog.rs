//! Catalog products through the repository (spec 2.2 §14 items 15, 16, 19, 20 at repo level;
//! frozen after use; archive vs hard delete of removed children; bundles).

use sultan_core::catalog::{CatalogError, ProductType, Qty, Variant};
use sultan_core::ids::{BrandId, CategoryId, ProductId, TaxClassId, VariantId};
use sultan_db::Db;
use sultan_db::catalog::{
    self, CatalogDbError, Duplicate, EXEMPT_TAX_CLASS, ProductFilter, Written, archive_product,
    get_product, list_products, mark_used, put_product, unarchive_product,
};
use sultan_db::outbox;

use crate::catalog_fixtures::{
    actor, barcode, bundle, cheese, component, kaos, put, simple, trail,
};
use crate::common::{count, db_tests, org, t};

db_tests!(
    create_replay_update,
    duplicates_across_products_name_the_variant,
    failed_write_leaves_nothing,
    frozen_after_use_through_the_repo,
    removed_children_are_deleted_or_archived,
    archive_and_unarchive,
    list_products_filters_and_pages,
    references_are_checked,
    bundles_validate_against_the_database,
    mark_used_is_idempotent,
    empty_skus_get_the_default,
    variant_count_and_labels,
);

type Edit = fn(&mut catalog::ProductDoc);
type Steal = fn(&mut catalog::ProductDoc, &catalog::ProductDoc);

fn rule(err: CatalogDbError) -> Vec<CatalogError> {
    match err {
        CatalogDbError::Rule(errors) => errors,
        other => panic!("expected a rule error, got {other:?}"),
    }
}

fn dup(err: CatalogDbError) -> Duplicate {
    match err {
        CatalogDbError::Duplicate(d) => d,
        other => panic!("expected a duplicate, got {other:?}"),
    }
}

async fn try_put(
    db: &Db,
    o: &crate::common::Org,
    doc: &catalog::ProductDoc,
) -> Result<(Written, catalog::ProductDoc), CatalogDbError> {
    let mut tx = db.begin().await.unwrap();
    let result = put_product(&mut tx, doc, &actor(o), t(20)).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

/// §14 item 15 + 20: create → replay → no duplicates, no extra audit/outbox; update applied
/// atomically with exactly one audit and one outbox row.
async fn create_replay_update(db: &Db) {
    let o = org(db).await;
    let doc = kaos("KAOS01");
    let before = trail(db).await;
    let (written, stored) = put(db, &o, &doc).await;
    assert_eq!(written, Written::Created);
    assert_eq!(stored.draft.receipt_name, "Produk KAOS01", "normalised");
    assert_eq!(stored.draft.tags, ["promo"]);
    let after_create = trail(db).await;
    assert_eq!(after_create, (before.0 + 1, before.1 + 1));
    let events = outbox::list_unsynced(db, 100).await.unwrap();
    let event = events.last().unwrap();
    assert_eq!(event.kind, "catalog.product.put");
    assert_eq!(event.payload["code"], "KAOS01");
    assert_eq!(event.payload["variants"].as_array().unwrap().len(), 4);

    let record = get_product(db, doc.draft.id).await.unwrap().unwrap();
    assert_eq!(record.doc, stored);
    assert!(!record.used);
    assert_eq!(record.archived_at, None);
    assert_eq!(record.created_at, t(10));

    // Replay: the raw input and the stored document both change nothing.
    for replay in [&doc, &stored] {
        let (written, _) = put(db, &o, replay).await;
        assert_eq!(written, Written::Unchanged);
    }
    assert_eq!(trail(db).await, after_create);
    assert_eq!(count(db, "variant").await, 4);
    assert_eq!(count(db, "barcode").await, 4);
    assert_eq!(count(db, "product_option_value").await, 4);
    assert_eq!(count(db, "variant_option_value").await, 8);

    // Update: rename, a new value + variant, swap two SKUs, move a barcode code to a new id.
    let mut edit = stored.clone();
    edit.draft.name = "Kaos Polos".into();
    edit.draft.receipt_name = String::new();
    edit.draft.options[0]
        .values
        .push(sultan_core::catalog::OptionValue {
            id: sultan_core::ids::OptionValueId::generate(),
            name: "Large".into(),
            code: "L".into(),
        });
    let large = edit.draft.options[0].values[2].id;
    let red = edit.draft.options[1].values[0].id;
    edit.draft.variants.push(Variant {
        id: VariantId::generate(),
        sku: "KAOS01-L-RED".into(),
        values: vec![large, red],
        plu: None,
        archived: false,
    });
    let (a, b) = (
        edit.draft.variants[0].sku.clone(),
        edit.draft.variants[1].sku.clone(),
    );
    edit.draft.variants[0].sku = b;
    edit.draft.variants[1].sku = a;
    let moved = edit.draft.barcodes.remove(0);
    edit.draft
        .barcodes
        .push(barcode(moved.variant, moved.unit, &moved.code));
    let (written, updated) = put(db, &o, &edit).await;
    assert_eq!(written, Written::Updated);
    assert_eq!(updated.draft.receipt_name, "Kaos Polos");
    assert_eq!(trail(db).await, (after_create.0 + 1, after_create.1 + 1));
    let record = get_product(db, doc.draft.id).await.unwrap().unwrap();
    assert_eq!(record.doc, updated);
    assert_eq!(count(db, "variant").await, 5);
    assert_eq!(count(db, "barcode").await, 4, "unused barcode hard-deleted");
    assert!(record.archived_barcodes.is_empty());
    let entries = sultan_db::audit::query(
        db,
        &sultan_db::audit::AuditFilter {
            action: Some("catalog.product.put".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].user, Some(o.manager.id));
    assert_eq!(entries[0].branch, Some(o.jkt.id));
    assert_eq!(entries[0].target, Some(format!("product:{}", doc.draft.id)));
    assert_eq!(entries[0].before.as_ref().unwrap()["name"], "Produk KAOS01");
    assert_eq!(entries[0].after.as_ref().unwrap()["name"], "Kaos Polos");
    assert!(entries[1].before.is_none());
}

/// §14 item 16: SKU, barcode, PLU and code already used by another product → typed duplicate
/// with the conflicting product and variant.
async fn duplicates_across_products_name_the_variant(db: &Db) {
    let o = org(db).await;
    let a = simple("AQUA600", "8991234567890");
    put(db, &o, &a).await;
    let a_variant = a.draft.variants[0].id;
    let keju = cheese("KEJU", 123);
    put(db, &o, &keju).await;

    let mut sku = simple("AQUA1500", "8991234567891");
    sku.draft.variants[0].sku = "aqua600".into();
    let d = dup(try_put(db, &o, &sku).await.unwrap_err());
    assert_eq!(
        d.error,
        CatalogError::DuplicateSku {
            sku: "AQUA600".into()
        }
    );
    assert_eq!((d.product, d.variant), (a.draft.id, Some(a_variant)));

    let bar = simple("AQUA1500", "8991234567890");
    let err = try_put(db, &o, &bar).await.unwrap_err();
    assert_eq!(err.code(), Some("duplicate_barcode"));
    let d = dup(err);
    assert_eq!((d.product, d.variant), (a.draft.id, Some(a_variant)));

    let code = simple("AQUA600", "1");
    let mut code = code;
    code.draft.variants[0].sku = "OTHER".into();
    let d = dup(try_put(db, &o, &code).await.unwrap_err());
    assert_eq!(
        d.error,
        CatalogError::DuplicateCode {
            code: "AQUA600".into()
        }
    );
    assert_eq!((d.product, d.variant), (a.draft.id, None));

    let plu = cheese("KEJU2", 123);
    let d = dup(try_put(db, &o, &plu).await.unwrap_err());
    assert_eq!(d.error, CatalogError::DuplicatePlu { plu: 123 });
    assert_eq!(d.variant, Some(keju.draft.variants[0].id));

    // Duplicates inside one document are core rule errors.
    let mut twice = simple("TWICE", "777");
    twice.draft.barcodes.push(barcode(
        twice.draft.variants[0].id,
        twice.draft.units.base,
        "777",
    ));
    let errors = rule(try_put(db, &o, &twice).await.unwrap_err());
    assert_eq!(
        errors,
        [CatalogError::DuplicateBarcode { code: "777".into() }]
    );
}

/// A rejected write leaves no rows, no audit and no outbox entry.
async fn failed_write_leaves_nothing(db: &Db) {
    let o = org(db).await;
    put(db, &o, &simple("A1", "100")).await;
    let before = (trail(db).await, count(db, "product").await);
    let mut bad = kaos("K2");
    bad.draft.barcodes[0].code = "100".into();
    let mut tx = db.begin().await.unwrap();
    put_product(&mut tx, &bad, &actor(&o), t(5))
        .await
        .unwrap_err();
    drop(tx);
    assert_eq!((trail(db).await, count(db, "product").await), before);
    assert_eq!(count(db, "variant").await, 1);
}

/// "Frozen after use" through the repository: once a variant is used, removing a value,
/// adding an option, changing a factor, the type or the base unit is rejected; renames and new
/// values are fine.
async fn frozen_after_use_through_the_repo(db: &Db) {
    let o = org(db).await;
    let (_, stored) = put(db, &o, &kaos("KAOS01")).await;
    // Before use everything may change: drop the box unit.
    let mut free = stored.clone();
    free.draft.units.units.pop();
    let (written, stored) = put(db, &o, &free).await;
    assert_eq!(written, Written::Updated);
    put(db, &o, &{
        let mut d = stored.clone();
        d.draft.units.units.push(crate::catalog_fixtures::unit(
            "box",
            sultan_core::catalog::UnitKind::Integer,
            24_000,
        ));
        d
    })
    .await;
    let stored = get_product(db, stored.draft.id).await.unwrap().unwrap().doc;

    assert_eq!(
        mark_used(db, &[stored.draft.variants[0].id], t(30))
            .await
            .unwrap(),
        1
    );
    assert!(
        get_product(db, stored.draft.id)
            .await
            .unwrap()
            .unwrap()
            .used
    );

    let frozen: Vec<(Edit, &str)> = vec![
        (
            |d| {
                d.draft.options[0].values.remove(0);
                d.draft
                    .variants
                    .retain(|v| v.values[0] == d.draft.options[0].values[0].id);
                let keep: Vec<VariantId> = d.draft.variants.iter().map(|v| v.id).collect();
                d.draft.barcodes.retain(|b| keep.contains(&b.variant));
            },
            "options.values",
        ),
        (|d| d.draft.product_type = ProductType::Service, "type"),
        (
            |d| d.draft.units.units[1].factor = Qty::from_thousandths(12_000),
            "units.factor",
        ),
        (
            |d| {
                let unit = d.draft.units.units[1].id;
                d.draft.units.base = unit;
            },
            "base_unit",
        ),
        (
            |d| {
                d.draft.options.pop();
                for v in &mut d.draft.variants {
                    v.values.pop();
                }
            },
            "options",
        ),
    ];
    for (edit, field) in frozen {
        let mut d = stored.clone();
        edit(&mut d);
        let errors = rule(try_put(db, &o, &d).await.unwrap_err());
        assert_eq!(errors, [CatalogError::FrozenAfterUse { field }], "{field}");
    }
    let mut rename = stored.clone();
    rename.draft.options[0].values[0].name = "Kecil".into();
    rename.draft.units.units[1].name = "dus".into();
    assert_eq!(try_put(db, &o, &rename).await.unwrap().0, Written::Updated);
}

/// Removed never-used children are deleted; a removed used variant is archived (and kept in the
/// document) and its removed barcode archived, freeing the code.
async fn removed_children_are_deleted_or_archived(db: &Db) {
    let o = org(db).await;
    let (_, stored) = put(db, &o, &kaos("KAOS01")).await;
    // Never used: drop the last variant and its barcode → rows gone.
    let mut edit = stored.clone();
    let gone = edit.draft.variants.pop().unwrap();
    edit.draft.barcodes.retain(|b| b.variant != gone.id);
    let (_, stored) = put(db, &o, &edit).await;
    assert_eq!(count(db, "variant").await, 3);
    assert_eq!(count(db, "barcode").await, 3);
    assert_eq!(count(db, "variant_option_value").await, 6);

    // Use variant 0, then remove it and its barcode from the document.
    let used = stored.draft.variants[0].clone();
    mark_used(db, &[used.id], t(30)).await.unwrap();
    let mut edit = stored.clone();
    edit.draft.variants.remove(0);
    let removed_barcode = edit.draft.barcodes.remove(0);
    assert_eq!(removed_barcode.variant, used.id);
    let (written, result) = put(db, &o, &edit).await;
    assert_eq!(written, Written::Updated);
    let kept = result.draft.variants.last().unwrap();
    assert_eq!(kept.id, used.id);
    assert!(kept.archived, "used variant archived, not deleted");
    assert_eq!(count(db, "variant").await, 3);
    let record = get_product(db, stored.draft.id).await.unwrap().unwrap();
    assert_eq!(record.doc, result);
    assert_eq!(
        record.archived_barcodes,
        std::slice::from_ref(&removed_barcode)
    );
    assert_eq!(count(db, "barcode").await, 3, "archived, not deleted");

    // Replaying the edit is a no-op; the archived barcode's code is free for another product.
    assert_eq!(put(db, &o, &edit).await.0, Written::Unchanged);
    let other = simple("OTHER", &removed_barcode.code);
    assert_eq!(put(db, &o, &other).await.0, Written::Created);
    // The archived SKU is free too.
    let mut sku = simple("OTHER2", "x-2");
    sku.draft.variants[0].sku.clone_from(&used.sku);
    assert_eq!(put(db, &o, &sku).await.0, Written::Created);
}

/// §14 item 19 (repo level): archived products leave `list_products` (unless `archived`), free
/// their SKUs and PLUs, and unarchive fails while another product holds them.
async fn archive_and_unarchive(db: &Db) {
    let o = org(db).await;
    let a = simple("AQUA600", "899100");
    put(db, &o, &a).await;
    let before = trail(db).await;
    let mut tx = db.begin().await.unwrap();
    let written = archive_product(&mut tx, a.draft.id, &actor(&o), t(40))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(written, Written::Updated);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    let record = get_product(db, a.draft.id).await.unwrap().unwrap();
    assert_eq!(record.archived_at, Some(t(40)));
    assert!(record.doc.draft.archived);

    let mut tx = db.begin().await.unwrap();
    let again = archive_product(&mut tx, a.draft.id, &actor(&o), t(41))
        .await
        .unwrap();
    assert_eq!(again, Written::Unchanged);
    let missing = archive_product(&mut tx, ProductId::generate(), &actor(&o), t(41))
        .await
        .unwrap_err();
    assert!(matches!(missing, CatalogDbError::NotFound("product")));
    drop(tx);

    let active = list_products(db, &ProductFilter::default()).await.unwrap();
    assert!(active.iter().all(|p| p.id != a.draft.id));
    let archived = list_products(
        db,
        &ProductFilter {
            archived: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].archived_at, Some(t(40)));

    // Another product takes the SKU (the barcode stays reserved by the archived product).
    let mut taker = simple("TAKER", "899200");
    taker.draft.variants[0].sku = "AQUA600".into();
    put(db, &o, &taker).await;
    let mut tx = db.begin().await.unwrap();
    let err = unarchive_product(&mut tx, a.draft.id, &actor(&o), t(42))
        .await
        .unwrap_err();
    drop(tx);
    let d = dup(err);
    assert_eq!(d.variant, Some(taker.draft.variants[0].id));
    let barcode_taken = simple("TAKER2", "899100");
    assert_eq!(
        try_put(db, &o, &barcode_taken).await.unwrap_err().code(),
        Some("duplicate_barcode")
    );

    // Free the SKU, then unarchive works.
    let mut taker = get_product(db, taker.draft.id).await.unwrap().unwrap().doc;
    taker.draft.variants[0].sku = "TAKER".into();
    put(db, &o, &taker).await;
    let mut tx = db.begin().await.unwrap();
    let written = unarchive_product(&mut tx, a.draft.id, &actor(&o), t(43))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(written, Written::Updated);
    let record = get_product(db, a.draft.id).await.unwrap().unwrap();
    assert_eq!(record.archived_at, None);
    let audits = sultan_db::audit::query(
        db,
        &sultan_db::audit::AuditFilter {
            target: Some(format!("product:{}", a.draft.id)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let actions: Vec<&str> = audits.iter().map(|e| e.action.as_str()).collect();
    assert_eq!(
        actions,
        [
            "catalog.product.unarchive",
            "catalog.product.archive",
            "catalog.product.put"
        ]
    );
}

async fn list_products_filters_and_pages(db: &Db) {
    let o = org(db).await;
    let category = CategoryId::generate();
    let brand = BrandId::generate();
    let mut tx = db.begin().await.unwrap();
    catalog::put_category(
        &mut tx,
        &sultan_core::catalog::Category {
            id: category,
            parent: None,
            name: "Minuman".into(),
            code: String::new(),
            archived: false,
        },
        &actor(&o),
        t(1),
    )
    .await
    .unwrap();
    catalog::put_brand(
        &mut tx,
        &sultan_core::catalog::Brand {
            id: brand,
            name: "Dånone".into(),
            archived: false,
        },
        &actor(&o),
        t(1),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut ids = Vec::new();
    for i in 0..5 {
        let mut d = simple(&format!("P{i}"), &format!("B{i}"));
        d.draft.name = format!("Air Mineral 50{i}");
        if i % 2 == 0 {
            d.category = Some(category);
            d.brand = Some(brand);
        }
        put(db, &o, &d).await;
        ids.push(d.draft.id);
    }
    put(db, &o, &cheese("KEJU", 5)).await;

    let list = |f: ProductFilter| async move { list_products(db, &f).await.unwrap() };
    let all = list(ProductFilter::default()).await;
    assert_eq!(all.len(), 6);
    assert!(all.windows(2).all(|w| w[0].id > w[1].id), "newest first");
    let by_brand_name = list(ProductFilter {
        q: Some("DANONE".into()),
        ..Default::default()
    })
    .await;
    assert_eq!(by_brand_name.len(), 3, "brand folded into the search key");
    let by_category = list(ProductFilter {
        category: Some(category),
        ..Default::default()
    })
    .await;
    assert_eq!(by_category.len(), 3);
    let by_brand = list(ProductFilter {
        brand: Some(brand),
        q: Some("502".into()),
        ..Default::default()
    })
    .await;
    assert_eq!(by_brand.len(), 1);
    assert_eq!(by_brand[0].id, ids[2]);
    let weighed = list(ProductFilter {
        product_type: Some(ProductType::Weighed),
        ..Default::default()
    })
    .await;
    assert_eq!(weighed.len(), 1);
    assert_eq!(weighed[0].code, "KEJU");
    let wildcard = list(ProductFilter {
        q: Some("%".into()),
        ..Default::default()
    })
    .await;
    assert!(wildcard.is_empty(), "LIKE wildcards are escaped");

    let page1 = list(ProductFilter {
        limit: 4,
        ..Default::default()
    })
    .await;
    assert_eq!(page1.len(), 4);
    let page2 = list(ProductFilter {
        limit: 4,
        before_id: Some(page1[3].id),
        ..Default::default()
    })
    .await;
    assert_eq!(page2.len(), 2);
    assert!(page2[0].id < page1[3].id);
}

async fn references_are_checked(db: &Db) {
    let o = org(db).await;
    let cases: Vec<(Edit, &str)> = vec![
        (|d| d.tax_class = TaxClassId::generate(), "tax_class_id"),
        (|d| d.category = Some(CategoryId::generate()), "category_id"),
        (|d| d.brand = Some(BrandId::generate()), "brand_id"),
    ];
    for (edit, field) in cases {
        let mut d = simple("REF", "r1");
        edit(&mut d);
        let errors = rule(try_put(db, &o, &d).await.unwrap_err());
        assert!(
            matches!(errors[..], [CatalogError::InvalidField { field: f, .. }] if f == field),
            "{errors:?}"
        );
    }
    let mut exempt = simple("REF", "r1");
    exempt.tax_class = EXEMPT_TAX_CLASS;
    put(db, &o, &exempt).await;
    let tax = catalog::list_tax_classes(db).await.unwrap();
    let names: Vec<&str> = tax.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Exempt", "Standard"]);

    // Child IDs of another product are rejected (no cross-product overwrite).
    let other = kaos("OTHER");
    put(db, &o, &other).await;
    let steals: Vec<(Steal, &str)> = vec![
        (
            |d, o| {
                d.draft.variants[0].id = o.draft.variants[0].id;
                d.draft.barcodes[0].variant = o.draft.variants[0].id;
            },
            "variants.id",
        ),
        (
            |d, o| {
                let unit = o.draft.units.base;
                d.draft.units.units[0].id = unit;
                d.draft.units.base = unit;
                d.draft.units.default_sale = unit;
                d.draft.units.default_purchase = unit;
                d.draft.barcodes[0].unit = unit;
            },
            "units.id",
        ),
        (
            |d, o| d.draft.barcodes[0].id = o.draft.barcodes[0].id,
            "barcodes.id",
        ),
        (
            |d, o| {
                d.draft.options = vec![o.draft.options[0].clone()];
                d.draft.variants[0].values = vec![o.draft.options[0].values[0].id];
            },
            "options.id",
        ),
        (
            |d, o| {
                let mut option = o.draft.options[0].clone();
                option.id = sultan_core::ids::OptionId::generate();
                d.draft.variants[0].values = vec![option.values[0].id];
                d.draft.options = vec![option];
            },
            "options.values.id",
        ),
    ];
    for (steal, field) in steals {
        let mut d = simple("THIEF", "t1");
        steal(&mut d, &other);
        let errors = rule(try_put(db, &o, &d).await.unwrap_err());
        assert!(
            matches!(errors[..], [CatalogError::InvalidField { field: f, reason: "belongs to another product" }] if f == field),
            "{field}: {errors:?}"
        );
    }
}

/// Bundles: components resolved from the database; nesting, archived and self components are
/// rejected; a component makes its product "used".
async fn bundles_validate_against_the_database(db: &Db) {
    let o = org(db).await;
    let a = simple("A", "a1");
    let b = cheese("B", 7);
    put(db, &o, &a).await;
    put(db, &o, &b).await;
    let kit = bundle("KIT", vec![component(&a, 1_000), component(&b, 500)]);
    assert_eq!(put(db, &o, &kit).await.0, Written::Created);
    let stored = get_product(db, kit.draft.id).await.unwrap().unwrap();
    assert_eq!(stored.doc.draft.components, kit.draft.components);
    assert!(!stored.used);

    let nested = bundle("KIT2", vec![component(&kit, 1_000)]);
    assert_eq!(
        rule(try_put(db, &o, &nested).await.unwrap_err()),
        [CatalogError::BundleNesting {
            variant: kit.draft.variants[0].id
        }]
    );
    let fractional = bundle("KIT3", vec![component(&a, 500)]);
    assert_eq!(
        rule(try_put(db, &o, &fractional).await.unwrap_err())[0].code(),
        "invalid_qty"
    );
    let unknown = bundle(
        "KIT4",
        vec![sultan_core::catalog::BundleComponent {
            variant: VariantId::generate(),
            unit: a.draft.units.base,
            qty: Qty::ONE,
        }],
    );
    assert_eq!(
        rule(try_put(db, &o, &unknown).await.unwrap_err())[0].code(),
        "invalid_bundle"
    );
    let mut own = bundle("KIT5", vec![]);
    own.draft.components = vec![component(&own, 1_000)];
    assert!(matches!(
        rule(try_put(db, &o, &own).await.unwrap_err())[..],
        [CatalogError::BundleComponentSelf { .. }]
    ));

    // A is a component of KIT: it counts as used (type frozen), and removing its variant
    // archives it instead of deleting it.
    let a_record = get_product(db, a.draft.id).await.unwrap().unwrap();
    assert!(a_record.used);
    let mut retype = a_record.doc.clone();
    retype.draft.product_type = ProductType::Service;
    assert_eq!(
        rule(try_put(db, &o, &retype).await.unwrap_err()),
        [CatalogError::FrozenAfterUse { field: "type" }]
    );

    // Archive B: a new bundle may not use it.
    let mut tx = db.begin().await.unwrap();
    archive_product(&mut tx, b.draft.id, &actor(&o), t(50))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let with_archived = bundle("KIT6", vec![component(&b, 1_000)]);
    assert_eq!(
        rule(try_put(db, &o, &with_archived).await.unwrap_err())[0].code(),
        "component_archived"
    );
    // Replaying KIT unchanged still succeeds (nothing to validate).
    assert_eq!(put(db, &o, &kit).await.0, Written::Unchanged);
}

async fn mark_used_is_idempotent(db: &Db) {
    let o = org(db).await;
    let d = kaos("K");
    put(db, &o, &d).await;
    let ids: Vec<VariantId> = d.draft.variants.iter().map(|v| v.id).collect();
    assert_eq!(mark_used(db, &[], t(1)).await.unwrap(), 0);
    assert_eq!(mark_used(db, &ids[..2], t(1)).await.unwrap(), 2);
    assert_eq!(mark_used(db, &ids, t(2)).await.unwrap(), 2);
    assert_eq!(mark_used(db, &ids, t(3)).await.unwrap(), 0);
}

/// An empty SKU becomes `<product code>-<value codes>` (spec 2.2 §3), also with a generated code.
async fn empty_skus_get_the_default(db: &Db) {
    let o = org(db).await;
    let mut doc = kaos("KSKU");
    for v in &mut doc.draft.variants {
        v.sku.clear();
    }
    let (_, stored) = put(db, &o, &doc).await;
    let skus: Vec<&str> = stored
        .draft
        .variants
        .iter()
        .map(|v| v.sku.as_str())
        .collect();
    assert_eq!(
        skus,
        ["KSKU-S-RED", "KSKU-S-BLUE", "KSKU-M-RED", "KSKU-M-BLUE"]
    );

    let mut doc = simple("", "SKU-GEN-1");
    doc.draft.variants[0].sku.clear();
    let (_, stored) = put(db, &o, &doc).await;
    assert!(!stored.draft.code.is_empty());
    assert_eq!(stored.draft.variants[0].sku, stored.draft.code);
}

/// `ProductSummary::variant_count` and `variant_labels` (back office, phase C4).
async fn variant_count_and_labels(db: &Db) {
    let o = org(db).await;
    // Active variants are counted; labels name a variant of another product.
    let shirt = kaos("KAOS");
    put(db, &o, &shirt).await;
    let mut archived = shirt.clone();
    archived.draft.variants[3].archived = true;
    put(db, &o, &archived).await;
    let counted = list_products(
        db,
        &ProductFilter {
            q: Some("KAOS".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(counted[0].variant_count, 3);
    let cheese = cheese("KEJU", 5);
    put(db, &o, &cheese).await;
    let all = list_products(db, &ProductFilter::default()).await.unwrap();
    assert_eq!(all[0].variant_count, 1);
    let red = shirt.draft.variants[0].id;
    let labels = catalog::variant_labels(db, &[red, VariantId::generate()])
        .await
        .unwrap();
    assert_eq!(labels.len(), 1, "unknown variants are left out");
    let label = &labels[&red];
    assert_eq!(label.product, shirt.draft.id);
    assert_eq!(label.sku, "KAOS-S-RED");
    assert_eq!(label.name, format!("{} – Small / Red", shirt.draft.name));
    let units: Vec<&str> = label.units.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(units, ["pcs", "box"]);
    assert!(catalog::variant_labels(db, &[]).await.unwrap().is_empty());
}
