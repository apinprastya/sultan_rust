//! Hard delete of never-used products, categories and brands (spec 2.2 §9, §17 item 1) and the
//! keyset paging of the register search (§6, §17 item 2).

use sultan_core::catalog::{Brand, Category};
use sultan_core::ids::{BrandId, CategoryId, ProductId, VariantId};
use sultan_db::Db;
use sultan_db::catalog::{
    self, CatalogDbError, ProductImage, archive_product, delete_brand, delete_category,
    delete_product, get_product, mark_used, put_branch_blocks, put_product_image, search,
    search_page,
};

use crate::catalog_fixtures::{actor, bundle, component, kaos, put, simple, trail};
use crate::common::{Org, count, db_tests, org, t};

db_tests!(
    never_used_products_are_deleted_with_their_rows,
    used_products_are_in_use,
    categories_and_brands_delete_only_when_unused,
    search_pages_follow_the_search_order,
);

async fn del_product(db: &Db, o: &Org, id: ProductId) -> Result<(), CatalogDbError> {
    let mut tx = db.begin().await.unwrap();
    let result = delete_product(&mut tx, id, &actor(o), t(200)).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

async fn del_category(db: &Db, o: &Org, id: CategoryId) -> Result<(), CatalogDbError> {
    let mut tx = db.begin().await.unwrap();
    let result = delete_category(&mut tx, id, &actor(o), t(200)).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

async fn del_brand(db: &Db, o: &Org, id: BrandId) -> Result<(), CatalogDbError> {
    let mut tx = db.begin().await.unwrap();
    let result = delete_brand(&mut tx, id, &actor(o), t(200)).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

async fn never_used_products_are_deleted_with_their_rows(db: &Db) {
    let o = org(db).await;
    // A product with options, values, two units, four variants and barcodes, an image and a
    // branch block.
    let d = kaos("KAOS");
    let id = d.draft.id;
    put(db, &o, &d).await;
    let mut tx = db.begin().await.unwrap();
    let image = ProductImage {
        mime: "image/jpeg".into(),
        width: 1,
        height: 1,
        data: vec![1, 2, 3],
    };
    put_product_image(&mut tx, id, &image, &actor(&o), t(20))
        .await
        .unwrap();
    put_branch_blocks(&mut tx, id, &[o.bdg.id], &actor(&o), t(20))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Another product stays untouched.
    let other = simple("OTHER", "o-1");
    put(db, &o, &other).await;

    let before = trail(db).await;
    del_product(db, &o, id).await.unwrap();
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    assert!(get_product(db, id).await.unwrap().is_none());
    for table in [
        "variant",
        "barcode",
        "product_unit",
        "product_option",
        "product_option_value",
        "variant_option_value",
        "product_branch_block",
        "product_image",
    ] {
        let left = count(db, table).await;
        let expected = match table {
            "variant" | "barcode" | "product_unit" => 1,
            _ => 0,
        };
        assert_eq!(left, expected, "{table}");
    }
    assert!(get_product(db, other.draft.id).await.unwrap().is_some());

    // The audit row keeps the deleted document.
    let entries = sultan_db::audit::query(
        db,
        &sultan_db::audit::AuditFilter {
            action: Some("catalog.product.delete".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].target, Some(format!("product:{id}")));
    assert_eq!(entries[0].before.as_ref().unwrap()["code"], "KAOS");
    assert!(entries[0].after.is_none());

    // Gone: a second delete is `not_found`.
    let err = del_product(db, &o, id).await.unwrap_err();
    assert!(matches!(err, CatalogDbError::NotFound("product")));

    // An archived product that was never used may be deleted too.
    let mut tx = db.begin().await.unwrap();
    archive_product(&mut tx, other.draft.id, &actor(&o), t(30))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    del_product(db, &o, other.draft.id).await.unwrap();
    assert_eq!(count(db, "product").await, 0);
}

async fn used_products_are_in_use(db: &Db) {
    let o = org(db).await;
    let sold = simple("SOLD", "s-1");
    put(db, &o, &sold).await;
    mark_used(db, &[sold.draft.variants[0].id], t(40))
        .await
        .unwrap();
    let before = trail(db).await;
    let err = del_product(db, &o, sold.draft.id).await.unwrap_err();
    assert!(matches!(err, CatalogDbError::InUse("used")), "{err}");
    assert_eq!(err.code(), Some("in_use"));
    assert_eq!(trail(db).await, before);

    // A component of another bundle is in use; the bundle itself is not.
    let part = simple("PART", "p-1");
    put(db, &o, &part).await;
    let pack = bundle("PACK", vec![component(&part, 2_000)]);
    put(db, &o, &pack).await;
    let err = del_product(db, &o, part.draft.id).await.unwrap_err();
    assert!(matches!(err, CatalogDbError::InUse("bundle_component")));
    del_product(db, &o, pack.draft.id).await.unwrap();
    assert_eq!(count(db, "bundle_component").await, 0);
    del_product(db, &o, part.draft.id).await.unwrap();
    assert!(get_product(db, sold.draft.id).await.unwrap().is_some());
}

async fn categories_and_brands_delete_only_when_unused(db: &Db) {
    let o = org(db).await;
    let category = |name: &str, parent: Option<CategoryId>| Category {
        id: CategoryId::generate(),
        parent,
        name: name.to_owned(),
        code: String::new(),
        archived: false,
    };
    let food = category("Makanan", None);
    let snack = category("Snack", Some(food.id));
    let typo = category("Mkanan", None);
    let brand = Brand {
        id: BrandId::generate(),
        name: "Aqua".into(),
        archived: false,
    };
    let unused_brand = Brand {
        id: BrandId::generate(),
        name: "Aqau".into(),
        archived: true,
    };
    let mut tx = db.begin().await.unwrap();
    for c in [&food, &snack, &typo] {
        catalog::put_category(&mut tx, c, &actor(&o), t(10))
            .await
            .unwrap();
    }
    for b in [&brand, &unused_brand] {
        catalog::put_brand(&mut tx, b, &actor(&o), t(10))
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let mut d = simple("A600", "a-600");
    d.category = Some(snack.id);
    d.brand = Some(brand.id);
    put(db, &o, &d).await;
    // An archived product still counts.
    let mut tx = db.begin().await.unwrap();
    archive_product(&mut tx, d.draft.id, &actor(&o), t(20))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let cases = [
        (del_category(db, &o, food.id).await, "has_children"),
        (del_category(db, &o, snack.id).await, "has_products"),
        (del_brand(db, &o, brand.id).await, "has_products"),
    ];
    for (result, reason) in cases {
        let err = result.unwrap_err();
        assert!(
            matches!(err, CatalogDbError::InUse(r) if r == reason),
            "{err}"
        );
    }

    let before = trail(db).await;
    del_category(db, &o, typo.id).await.unwrap();
    del_brand(db, &o, unused_brand.id).await.unwrap();
    assert_eq!(trail(db).await, (before.0 + 2, before.1 + 2));
    assert!(catalog::get_category(db, typo.id).await.unwrap().is_none());
    assert!(
        catalog::get_brand(db, unused_brand.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        del_category(db, &o, typo.id).await.unwrap_err(),
        CatalogDbError::NotFound("category")
    ));
    assert!(matches!(
        del_brand(db, &o, unused_brand.id).await.unwrap_err(),
        CatalogDbError::NotFound("brand")
    ));

    // Once the product is gone, the snack category, then its parent, and the brand go too.
    del_product(db, &o, d.draft.id).await.unwrap();
    del_category(db, &o, snack.id).await.unwrap();
    del_category(db, &o, food.id).await.unwrap();
    del_brand(db, &o, brand.id).await.unwrap();
}

async fn search_pages_follow_the_search_order(db: &Db) {
    let o = org(db).await;
    // Two prefix matches (one with four variants), then contains-matches.
    for (code, name) in [
        ("P1", "Teh Botol"),
        ("P2", "Es Teh Manis"),
        ("P3", "Teh Celup"),
        ("P4", "Mie Teh"),
    ] {
        let mut d = simple(code, &format!("b-{code}"));
        d.draft.name = name.to_owned();
        d.draft.receipt_name = name.to_owned();
        put(db, &o, &d).await;
    }
    let mut shirt = kaos("TEH");
    shirt.draft.name = "Teh Kaos".into();
    shirt.draft.receipt_name = "Teh Kaos".into();
    put(db, &o, &shirt).await;

    let all = search(db, "teh", o.jkt.id, 50).await.unwrap();
    assert_eq!(all.len(), 8);
    for size in [1, 2, 3] {
        let mut paged = Vec::new();
        let mut after = None;
        loop {
            let page = search_page(db, "teh", o.jkt.id, size, after).await.unwrap();
            assert!(page.len() <= size as usize);
            let Some(last) = page.last() else { break };
            after = Some(last.variant);
            paged.extend(page);
        }
        assert_eq!(paged, all, "page size {size}");
    }
    // A cursor that names no variant ends the search.
    let gone = Some(VariantId::generate());
    assert!(
        search_page(db, "teh", o.jkt.id, 5, gone)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        search_page(db, " ", o.jkt.id, 5, None)
            .await
            .unwrap()
            .is_empty()
    );
}
