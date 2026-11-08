//! Register lookup and search through the database index (spec 2.2 §6, §10; §14 items 18, 19 at
//! repo level), branch blocks, and byte-wise comparison of barcodes on both backends.

use sultan_core::catalog::{CatalogError, CatalogIndex, MatchSource, Plu, ProductType, Qty};
use sultan_core::ids::BranchId;
use sultan_core::money::Money;
use sultan_db::Db;
use sultan_db::catalog::{
    CatalogDbError, Written, archive_product, get_branch_blocks, get_catalog_settings, load_index,
    lookup, mark_used, put_branch_blocks, search,
};

use crate::catalog_fixtures::{actor, barcode, cheese, ean13, kaos, put, simple, trail};
use crate::common::{Org, db_tests, org, t};

db_tests!(
    lookup_order_through_the_db_index,
    scale_barcodes_through_the_db_index,
    branch_blocks_in_lookup_and_search,
    archived_items_in_lookup_and_search,
    search_ranks_and_limits,
    barcodes_and_skus_compare_byte_wise,
);

fn lookup_error(err: CatalogDbError) -> CatalogError {
    match err {
        CatalogDbError::Rule(mut errors) => errors.remove(0),
        other => panic!("expected a lookup error, got {other:?}"),
    }
}

async fn blocks(
    db: &Db,
    o: &Org,
    product: sultan_core::ids::ProductId,
    set: &[BranchId],
) -> Written {
    let mut tx = db.begin().await.unwrap();
    let written = put_branch_blocks(&mut tx, product, set, &actor(o), t(60))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    written
}

/// §14 item 10 through the DB: exact barcode, UPC-A ↔ EAN-13, SKU (any case), the barcode's
/// unit, the default sale unit for a SKU, display names.
async fn lookup_order_through_the_db_index(db: &Db) {
    let o = org(db).await;
    let upc = ean13("001234567890");
    let mut aqua = simple("AQUA600", &upc);
    aqua.draft.open_price = true;
    put(db, &o, &aqua).await;
    let k = kaos("KAOS01");
    let box_unit = k.draft.units.units[1].id;
    let mut k = k;
    k.draft
        .barcodes
        .push(barcode(k.draft.variants[1].id, box_unit, "BOX-1"));
    put(db, &o, &k).await;

    let item = lookup(db, &upc, o.jkt.id).await.unwrap();
    assert_eq!(item.source, MatchSource::Barcode);
    assert_eq!(item.variant, aqua.draft.variants[0].id);
    assert_eq!(item.qty, Some(Qty::ONE));
    assert!(item.open_price);
    assert!(!item.requires_note);
    assert_eq!(item.product_type, ProductType::Stock);
    assert_eq!(item.name, "Produk AQUA600");

    let item = lookup(db, &upc[1..], o.jkt.id).await.unwrap();
    assert_eq!(item.source, MatchSource::NormalizedBarcode);
    assert_eq!(item.variant, aqua.draft.variants[0].id);

    let item = lookup(db, " BOX-1 ", o.jkt.id).await.unwrap();
    assert_eq!(
        (item.variant, item.unit),
        (k.draft.variants[1].id, box_unit)
    );
    assert_eq!(item.name, "Produk KAOS01 – Small / Blue");

    let item = lookup(db, "kaos01-m-red", o.jkt.id).await.unwrap();
    assert_eq!(item.source, MatchSource::Sku);
    assert_eq!(item.variant, k.draft.variants[2].id);
    assert_eq!(item.unit, k.draft.units.default_sale);
    assert_eq!(item.sku, "KAOS01-M-RED");
    assert_eq!(item.name, "Produk KAOS01 – Medium / Red");

    for missing in ["", "NOPE", "999999"] {
        let err = lookup(db, missing, o.jkt.id).await.unwrap_err();
        assert_eq!(lookup_error(err), CatalogError::NotFound, "{missing:?}");
    }
}

/// Scale codes resolve through the DB PLU index (default layouts 21 weight / 22 price); a typed
/// PLU falls back to the weighed variant; an exact barcode beats the scale parse.
async fn scale_barcodes_through_the_db_index(db: &Db) {
    let o = org(db).await;
    let keju = cheese("KEJU", 123);
    put(db, &o, &keju).await;
    let variant = keju.draft.variants[0].id;

    let weight = lookup(db, &ean13("210012301250"), o.jkt.id).await.unwrap();
    assert_eq!(weight.source, MatchSource::ScaleBarcode);
    assert_eq!(weight.variant, variant);
    assert_eq!(weight.unit, keju.draft.units.base);
    assert_eq!(weight.qty, Some(Qty::from_thousandths(1_250)));
    assert_eq!(weight.product_type, ProductType::Weighed);

    let price = lookup(db, &ean13("220012315000"), o.jkt.id).await.unwrap();
    assert_eq!(price.qty, None);
    assert_eq!(price.amount, Some(Money::from_minor(15_000)));

    let typed = lookup(db, "123", o.jkt.id).await.unwrap();
    assert_eq!(typed.source, MatchSource::Plu);
    assert_eq!(typed.variant, variant);

    let zero = lookup(db, &ean13("210012300000"), o.jkt.id)
        .await
        .unwrap_err();
    assert_eq!(lookup_error(zero), CatalogError::ScaleValueZero);
    let bad_check = format!("{}0", &ean13("210012301250")[..12]);
    if bad_check != ean13("210012301250") {
        let err = lookup(db, &bad_check, o.jkt.id).await.unwrap_err();
        assert_eq!(lookup_error(err), CatalogError::NotFound);
    }

    // An imported code that happens to start with a scale prefix wins as a barcode.
    let imported = ean13("210012300500");
    put(db, &o, &simple("OLD", &imported)).await;
    let item = lookup(db, &imported, o.jkt.id).await.unwrap();
    assert_eq!(item.source, MatchSource::Barcode);

    // The prefetched index itself.
    let layouts = get_catalog_settings(db).await.unwrap().scale_layouts;
    let index = load_index(db, &ean13("210012301250"), &layouts)
        .await
        .unwrap();
    assert_eq!(
        index.by_plu(Plu::new(123).unwrap()).unwrap().variant,
        variant
    );
    assert!(index.by_plu(Plu::new(124).unwrap()).is_none());
    assert!(
        load_index(db, "  ", &layouts)
            .await
            .unwrap()
            .by_sku("")
            .is_none()
    );
}

/// §14 item 18 at repo level: blocked → `not_sold_here` and hidden from search at that branch
/// only; the block set is replaced and replays are no-ops.
async fn branch_blocks_in_lookup_and_search(db: &Db) {
    let o = org(db).await;
    let aqua = simple("AQUA600", "8991");
    put(db, &o, &aqua).await;
    let id = aqua.draft.id;
    let before = trail(db).await;
    assert_eq!(
        blocks(db, &o, id, &[o.bdg.id, o.bdg.id]).await,
        Written::Created
    );
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    assert_eq!(blocks(db, &o, id, &[o.bdg.id]).await, Written::Unchanged);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    assert_eq!(get_branch_blocks(db, id).await.unwrap(), [o.bdg.id]);

    let err = lookup(db, "8991", o.bdg.id).await.unwrap_err();
    assert_eq!(lookup_error(err), CatalogError::NotSoldHere);
    assert_eq!(lookup(db, "8991", o.jkt.id).await.unwrap().product, id);
    assert!(search(db, "aqua", o.bdg.id, 10).await.unwrap().is_empty());
    assert_eq!(search(db, "aqua", o.jkt.id, 10).await.unwrap().len(), 1);

    let mut both = vec![o.jkt.id, o.bdg.id];
    both.sort();
    assert_eq!(
        blocks(db, &o, id, &[o.jkt.id, o.bdg.id]).await,
        Written::Updated
    );
    assert_eq!(get_branch_blocks(db, id).await.unwrap(), both);
    assert_eq!(blocks(db, &o, id, &[]).await, Written::Updated);
    assert!(get_branch_blocks(db, id).await.unwrap().is_empty());
    assert_eq!(lookup(db, "8991", o.bdg.id).await.unwrap().product, id);

    let mut tx = db.begin().await.unwrap();
    let unknown = put_branch_blocks(&mut tx, id, &[BranchId::generate()], &actor(&o), t(1))
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), Some("invalid_field"));
    let missing = put_branch_blocks(
        &mut tx,
        sultan_core::ids::ProductId::generate(),
        &[],
        &actor(&o),
        t(1),
    )
    .await
    .unwrap_err();
    assert!(matches!(missing, CatalogDbError::NotFound("product")));
}

/// §14 item 19 at repo level: archived product → `archived` on lookup, hidden from search; an
/// archived barcode of a used variant still resolves to `archived`; not sellable → hidden from
/// search and `not_sellable` on lookup.
async fn archived_items_in_lookup_and_search(db: &Db) {
    let o = org(db).await;
    let aqua = simple("AQUA600", "8991");
    put(db, &o, &aqua).await;
    let mut tx = db.begin().await.unwrap();
    archive_product(&mut tx, aqua.draft.id, &actor(&o), t(5))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for code in ["8991", "aqua600"] {
        let err = lookup(db, code, o.jkt.id).await.unwrap_err();
        assert_eq!(lookup_error(err), CatalogError::Archived, "{code}");
    }
    assert!(search(db, "aqua", o.jkt.id, 10).await.unwrap().is_empty());

    // A used variant's removed barcode stays resolvable as archived.
    let k = kaos("KAOS01");
    let (_, stored) = put(db, &o, &k).await;
    mark_used(db, &[stored.draft.variants[0].id], t(6))
        .await
        .unwrap();
    let mut edit = stored.clone();
    let removed = edit.draft.barcodes.remove(0);
    put(db, &o, &edit).await;
    let err = lookup(db, &removed.code, o.jkt.id).await.unwrap_err();
    assert_eq!(lookup_error(err), CatalogError::Archived);
    // An archived variant is hidden from search but its siblings are found.
    let mut edit = get_product_doc(db, k.draft.id).await;
    edit.draft.variants[1].archived = true;
    put(db, &o, &edit).await;
    let hits = search(db, "kaos", o.jkt.id, 10).await.unwrap();
    assert_eq!(hits.len(), 3);
    assert!(hits.iter().all(|h| h.variant != edit.draft.variants[1].id));

    let mut hidden = simple("BAG", "bag-1");
    hidden.draft.sellable = false;
    put(db, &o, &hidden).await;
    assert!(search(db, "bag", o.jkt.id, 10).await.unwrap().is_empty());
    let err = lookup(db, "bag-1", o.jkt.id).await.unwrap_err();
    assert_eq!(lookup_error(err), CatalogError::NotSellable);
}

async fn get_product_doc(
    db: &Db,
    id: sultan_core::ids::ProductId,
) -> sultan_db::catalog::ProductDoc {
    sultan_db::catalog::get_product(db, id)
        .await
        .unwrap()
        .unwrap()
        .doc
}

async fn search_ranks_and_limits(db: &Db) {
    let o = org(db).await;
    for (code, name) in [
        ("P1", "Teh Botol Sosro"),
        ("P2", "Es Teh Manis"),
        ("P3", "Café Latte"),
        ("P4", "Kopi 100%"),
    ] {
        let mut d = simple(code, &format!("b-{code}"));
        d.draft.name = name.to_owned();
        d.draft.receipt_name = name.to_owned();
        put(db, &o, &d).await;
    }
    let names = |hits: Vec<sultan_db::catalog::SearchHit>| -> Vec<String> {
        hits.into_iter().map(|h| h.name).collect()
    };
    assert_eq!(
        names(search(db, "TEH", o.jkt.id, 0).await.unwrap()),
        ["Teh Botol Sosro", "Es Teh Manis"],
        "prefix matches first"
    );
    assert_eq!(
        names(search(db, "cafe", o.jkt.id, 10).await.unwrap()),
        ["Café Latte"],
        "accents folded"
    );
    assert_eq!(
        names(search(db, "100%", o.jkt.id, 10).await.unwrap()),
        ["Kopi 100%"]
    );
    assert!(search(db, "0%x", o.jkt.id, 10).await.unwrap().is_empty());
    assert!(search(db, "   ", o.jkt.id, 10).await.unwrap().is_empty());
    assert_eq!(search(db, "e", o.jkt.id, 2).await.unwrap().len(), 2);
    let hit = &search(db, "latte", o.jkt.id, 10).await.unwrap()[0];
    assert_eq!(hit.sku, "P3");
    assert_eq!(hit.product_type, ProductType::Stock);
    assert!(!hit.open_price && !hit.requires_note);
}

/// Barcodes are stored as scanned and compare byte-wise on both backends (`utf8mb4_0900_bin` on
/// `MySQL`): `abc-1` and `ABC-1` are different codes on different products.
async fn barcodes_and_skus_compare_byte_wise(db: &Db) {
    let o = org(db).await;
    let lower = simple("LOWER", "abc-1");
    let upper = simple("UPPER", "ABC-1");
    put(db, &o, &lower).await;
    assert_eq!(put(db, &o, &upper).await.0, Written::Created);
    let accent = simple("ACCENT", "aBc-1");
    let plain = simple("PLAIN", "abc-1x");
    put(db, &o, &accent).await;
    put(db, &o, &plain).await;
    for (code, product) in [
        ("abc-1", lower.draft.id),
        ("ABC-1", upper.draft.id),
        ("aBc-1", accent.draft.id),
        ("abc-1x", plain.draft.id),
    ] {
        assert_eq!(
            lookup(db, code, o.jkt.id).await.unwrap().product,
            product,
            "{code}"
        );
    }
    // Trailing spaces count (NO PAD) — but lookup trims input, so "abc-1 " is "abc-1".
    assert_eq!(
        lookup(db, "abc-1 ", o.jkt.id).await.unwrap().product,
        lower.draft.id
    );
    let err = lookup(db, "Abc-1", o.jkt.id).await.unwrap_err();
    assert_eq!(lookup_error(err), CatalogError::NotFound);
    // SKUs are upper case by rule; a lower-case typed SKU resolves through upper-casing only.
    assert_eq!(
        lookup(db, "lower", o.jkt.id).await.unwrap().product,
        lower.draft.id
    );
}
