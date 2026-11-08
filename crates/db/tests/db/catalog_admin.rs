//! Catalog settings, code generators (incl. concurrency), categories, brands, images and the
//! catalog permissions in the seeded roles (fresh and migrated databases).

use std::borrow::Cow;
use std::path::Path;

use sultan_core::access::Permission;
use sultan_core::catalog::{
    Brand, CatalogError, Category, CheckDigitStatus, ScaleLayout, ScaleValueKind,
    check_digit_status,
};
use sultan_core::ids::{BrandId, CategoryId, ProductId};
use sultan_db::catalog::{
    self, CatalogSettings, ProductImage, Written, generate_internal_barcode, generate_product_code,
    get_catalog_settings, get_product_image, put_catalog_settings,
};
use sultan_db::seed::{self, CASHIER_ROLE, MANAGER_ROLE, OWNER_ROLE, STOCK_CLERK_ROLE};
use sultan_db::{Db, roles};

use crate::catalog_fixtures::{actor, ean13, put, simple, trail};
use crate::common::{MySqlTestDb, Org, count, db_tests, org, raw, t};

db_tests!(
    settings_validate_and_replay,
    generators_skip_taken_values,
    put_product_generates_an_empty_code,
    concurrent_code_generation_hands_out_distinct_codes,
    categories_form_a_validated_tree,
    brands_are_unique_and_refresh_search,
    images_round_trip,
    catalog_permissions_on_a_fresh_database,
);

type SettingsEdit = fn(&mut CatalogSettings);

async fn save_settings(
    db: &Db,
    o: &Org,
    s: &CatalogSettings,
) -> Result<Written, catalog::CatalogDbError> {
    let mut tx = db.begin().await.unwrap();
    let result = put_catalog_settings(&mut tx, s, &actor(o), t(70)).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

async fn settings_validate_and_replay(db: &Db) {
    let o = org(db).await;
    let defaults = get_catalog_settings(db).await.unwrap();
    assert_eq!(defaults.product_code_prefix, "P");
    assert_eq!(defaults.internal_barcode_prefix, "29");
    assert_eq!(defaults.scale_layouts, ScaleLayout::default_layouts());
    assert!(defaults.unit_names.contains(&"renteng".to_owned()));
    assert_eq!(
        save_settings(db, &o, &defaults).await.unwrap(),
        Written::Unchanged
    );

    let before = trail(db).await;
    let mut s = defaults.clone();
    s.product_code_prefix = " brg ".into();
    s.scale_layouts.push(ScaleLayout {
        prefix: 23,
        plu_digits: 4,
        value_digits: 6,
        kind: ScaleValueKind::Price,
    });
    s.unit_names.push("karung".into());
    assert_eq!(save_settings(db, &o, &s).await.unwrap(), Written::Updated);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    let stored = get_catalog_settings(db).await.unwrap();
    assert_eq!(stored.product_code_prefix, "BRG");
    assert_eq!(stored.scale_layouts.len(), 3);
    assert_eq!(stored.unit_names.last().unwrap(), "karung");

    let cases: Vec<(SettingsEdit, &str)> = vec![
        (
            |s| s.internal_barcode_prefix = "21".into(),
            "scale_prefix_overlap",
        ),
        (|s| s.scale_layouts[0].prefix = 22, "scale_prefix_overlap"),
        (
            |s| s.scale_layouts[0].plu_digits = 3,
            "invalid_scale_layout",
        ),
        (|s| s.product_code_prefix = "A B".into(), "invalid_code"),
        (|s| s.internal_barcode_prefix = "2".into(), "invalid_code"),
        (|s| s.unit_names.push("PCS".into()), "duplicate_unit_name"),
    ];
    for (edit, code) in cases {
        let mut bad = stored.clone();
        edit(&mut bad);
        let err = save_settings(db, &o, &bad).await.unwrap_err();
        assert_eq!(err.code(), Some(code), "{bad:?}");
    }
}

async fn generate_code(db: &Db, o: &Org) -> String {
    generate_product_code(db, &actor(o), t(80)).await.unwrap()
}

/// §14 item 14 through the DB: counters advance, taken values are skipped, internal EAN-13s have
/// a valid check digit and use the configured prefix.
async fn generators_skip_taken_values(db: &Db) {
    let o = org(db).await;
    let before = trail(db).await;
    assert_eq!(generate_code(db, &o).await, "P000001");
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    put(db, &o, &simple("P000002", "x")).await;
    put(db, &o, &simple("P000003", "y")).await;
    assert_eq!(generate_code(db, &o).await, "P000004");

    let barcode = || async {
        generate_internal_barcode(db, &actor(&o), t(81))
            .await
            .unwrap()
    };
    let first = barcode().await;
    assert_eq!(first, ean13("290000000001"));
    assert!(matches!(
        check_digit_status(&first),
        CheckDigitStatus::Valid(_)
    ));
    assert_eq!(barcode().await, ean13("290000000002"));
    put(db, &o, &simple("TAKEN", &ean13("290000000003"))).await;
    assert_eq!(
        barcode().await,
        ean13("290000000004"),
        "taken value skipped"
    );

    // A prefix that a scale layout now uses → scale_prefix_overlap (settings are validated, so
    // force it in the table to check the generator's own guard).
    raw(
        db,
        "UPDATE catalog_settings SET internal_barcode_prefix = '21'",
    )
    .await
    .unwrap();
    let err = generate_internal_barcode(db, &actor(&o), t(82))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("scale_prefix_overlap"));
    raw(
        db,
        "UPDATE catalog_counter SET next_value = 999999999 WHERE name = 'product_code'",
    )
    .await
    .unwrap();
    raw(
        db,
        "UPDATE catalog_settings SET product_code_prefix = 'ABCDEFGHIJKLMNOPQ'",
    )
    .await
    .unwrap();
    let err = generate_product_code(db, &actor(&o), t(82))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("generator_exhausted"));
}

/// An empty product code is generated for a new product and kept on replay.
async fn put_product_generates_an_empty_code(db: &Db) {
    let o = org(db).await;
    let mut d = simple("", "g-1");
    d.draft.variants[0].sku = "GEN-1".into();
    let (written, stored) = put(db, &o, &d).await;
    assert_eq!(written, Written::Created);
    assert_eq!(stored.draft.code, "P000001");
    assert_eq!(put(db, &o, &d).await.0, Written::Unchanged);
    assert_eq!(generate_code(db, &o).await, "P000002");
}

/// Generators are safe under concurrency: every racing call gets its own code.
async fn concurrent_code_generation_hands_out_distinct_codes(db: &Db) {
    let o = org(db).await;
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let db = db.clone();
            let actor = actor(&o);
            tokio::spawn(async move { generate_product_code(&db, &actor, t(90)).await })
        })
        .collect();
    let mut codes = Vec::new();
    for task in tasks {
        codes.push(task.await.unwrap().unwrap());
    }
    codes.sort();
    let expected: Vec<String> = (1..=8).map(|n| format!("P{n:06}")).collect();
    assert_eq!(codes, expected);
    assert_eq!(generate_code(db, &o).await, "P000009");
}

fn category(name: &str, parent: Option<CategoryId>) -> Category {
    Category {
        id: CategoryId::generate(),
        parent,
        name: name.to_owned(),
        code: String::new(),
        archived: false,
    }
}

async fn put_cat(db: &Db, o: &Org, c: &Category) -> Result<Written, catalog::CatalogDbError> {
    let mut tx = db.begin().await.unwrap();
    let result = catalog::put_category(&mut tx, c, &actor(o), t(100)).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

async fn categories_form_a_validated_tree(db: &Db) {
    let o = org(db).await;
    let food = category("Makanan", None);
    let snack = category("Snack", Some(food.id));
    let chips = category("Keripik", Some(snack.id));
    let drinks = category(" Minuman ", None);
    for c in [&food, &snack, &chips, &drinks] {
        assert_eq!(put_cat(db, &o, c).await.unwrap(), Written::Created);
    }
    let stored = catalog::get_category(db, drinks.id).await.unwrap().unwrap();
    assert_eq!(stored.name, "Minuman");
    let before = trail(db).await;
    assert_eq!(put_cat(db, &o, &stored).await.unwrap(), Written::Unchanged);
    assert_eq!(trail(db).await, before);

    let mut cycle = food.clone();
    cycle.parent = Some(chips.id);
    assert_eq!(
        put_cat(db, &o, &cycle).await.unwrap_err().code(),
        Some("category_cycle")
    );
    let deep = category("Kentang", Some(chips.id));
    put_cat(db, &o, &deep).await.unwrap();
    let too_deep = category("Pedas", Some(deep.id));
    assert_eq!(
        put_cat(db, &o, &too_deep).await.unwrap_err().code(),
        Some("category_too_deep")
    );
    let twin = category("snack", Some(food.id));
    assert_eq!(
        put_cat(db, &o, &twin).await.unwrap_err().code(),
        Some("duplicate_category_name")
    );
    let orphan = category("Orphan", Some(CategoryId::generate()));
    assert_eq!(
        put_cat(db, &o, &orphan).await.unwrap_err().code(),
        Some("unknown_category")
    );

    // Move Snack under Minuman and archive Keripik.
    let mut moved = snack.clone();
    moved.parent = Some(drinks.id);
    assert_eq!(put_cat(db, &o, &moved).await.unwrap(), Written::Updated);
    let mut archived = chips.clone();
    archived.archived = true;
    put_cat(db, &o, &archived).await.unwrap();

    let tree = catalog::list_categories(db, false).await.unwrap();
    let roots: Vec<&str> = tree.iter().map(|n| n.category.name.as_str()).collect();
    assert_eq!(roots, ["Makanan", "Minuman"]);
    assert!(tree[0].children.is_empty());
    assert_eq!(tree[1].children[0].category.name, "Snack");
    assert!(tree[1].children[0].children.is_empty(), "archived hidden");
    let full = catalog::list_categories(db, true).await.unwrap();
    assert_eq!(full[1].children[0].children[0].category.name, "Keripik");
    assert_eq!(
        full[1].children[0].children[0].children[0].category.name,
        "Kentang"
    );
}

async fn put_brand(db: &Db, o: &Org, b: &Brand) -> Result<Written, catalog::CatalogDbError> {
    let mut tx = db.begin().await.unwrap();
    let result = catalog::put_brand(&mut tx, b, &actor(o), t(110)).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

async fn brands_are_unique_and_refresh_search(db: &Db) {
    let o = org(db).await;
    let aqua = Brand {
        id: BrandId::generate(),
        name: "Aqua".into(),
        archived: false,
    };
    assert_eq!(put_brand(db, &o, &aqua).await.unwrap(), Written::Created);
    assert_eq!(put_brand(db, &o, &aqua).await.unwrap(), Written::Unchanged);
    let twin = Brand {
        id: BrandId::generate(),
        name: "AQUA".into(),
        archived: false,
    };
    let err = put_brand(db, &o, &twin).await.unwrap_err();
    assert_eq!(err.code(), Some("duplicate_brand_name"));
    assert!(matches!(
        err,
        catalog::CatalogDbError::Rule(ref e) if matches!(e[..], [CatalogError::DuplicateBrandName { .. }])
    ));

    let mut d = simple("A600", "a-600");
    d.brand = Some(aqua.id);
    put(db, &o, &d).await;
    let found =
        |q: &'static str| async move { catalog::search(db, q, o.jkt.id, 10).await.unwrap().len() };
    assert_eq!(found("aqua").await, 1);
    let mut renamed = aqua.clone();
    renamed.name = "Aqua Danone".into();
    assert_eq!(put_brand(db, &o, &renamed).await.unwrap(), Written::Updated);
    assert_eq!(found("danone").await, 1, "search key refreshed");

    let mut archived = renamed.clone();
    archived.archived = true;
    put_brand(db, &o, &archived).await.unwrap();
    assert!(catalog::list_brands(db, false).await.unwrap().is_empty());
    assert_eq!(
        catalog::list_brands(db, true).await.unwrap(),
        [archived.clone()]
    );
    assert_eq!(
        catalog::get_brand(db, aqua.id).await.unwrap(),
        Some(archived)
    );
}

async fn images_round_trip(db: &Db) {
    let o = org(db).await;
    let d = simple("IMG", "i-1");
    put(db, &o, &d).await;
    let id = d.draft.id;
    let image = ProductImage {
        mime: "image/webp".into(),
        width: 512,
        height: 384,
        // Larger than a MySQL BLOB (64 KiB) to prove MEDIUMBLOB; bytes stored exactly.
        data: (0..300_000u32).map(|i| (i % 251) as u8).collect(),
    };
    let mut tx = db.begin().await.unwrap();
    let a = actor(&o);
    assert_eq!(
        catalog::put_product_image(&mut tx, id, &image, &a, t(1))
            .await
            .unwrap(),
        Written::Created
    );
    assert_eq!(
        catalog::put_product_image(&mut tx, id, &image, &a, t(2))
            .await
            .unwrap(),
        Written::Unchanged
    );
    let mut small = image.clone();
    small.data = vec![1, 2, 3];
    assert_eq!(
        catalog::put_product_image(&mut tx, id, &small, &a, t(3))
            .await
            .unwrap(),
        Written::Updated
    );
    tx.commit().await.unwrap();
    assert_eq!(
        get_product_image(db, id).await.unwrap(),
        Some(small.clone())
    );

    let mut tx = db.begin().await.unwrap();
    let mut big = image.clone();
    big.width = 1;
    catalog::put_product_image(&mut tx, id, &big, &a, t(4))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(get_product_image(db, id).await.unwrap(), Some(big));

    let before = trail(db).await;
    let mut tx = db.begin().await.unwrap();
    let missing = catalog::put_product_image(&mut tx, ProductId::generate(), &small, &a, t(5))
        .await
        .unwrap_err();
    assert!(matches!(
        missing,
        catalog::CatalogDbError::NotFound("product")
    ));
    let mut empty = small.clone();
    empty.data.clear();
    let err = catalog::put_product_image(&mut tx, id, &empty, &a, t(5))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("invalid_field"));
    assert!(
        catalog::delete_product_image(&mut tx, id, &a, t(6))
            .await
            .unwrap()
    );
    assert!(
        !catalog::delete_product_image(&mut tx, id, &a, t(6))
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    assert_eq!(get_product_image(db, id).await.unwrap(), None);
    assert_eq!(count(db, "product_image").await, 0);
}

fn grants(role: &sultan_core::access::Role) -> Vec<Permission> {
    role.grants
        .iter()
        .map(sultan_core::access::Grant::permission)
        .collect()
}

async fn assert_catalog_grants(db: &Db) {
    let role = |id| async move { roles::get_role(db, id).await.unwrap().unwrap() };
    let owner = grants(&role(OWNER_ROLE).await);
    assert!(owner.contains(&Permission::CatalogManage));
    assert!(owner.contains(&Permission::CatalogViewArchived));
    let manager = grants(&role(MANAGER_ROLE).await);
    assert!(manager.contains(&Permission::CatalogManage));
    assert!(manager.contains(&Permission::CatalogViewArchived));
    let clerk = grants(&role(STOCK_CLERK_ROLE).await);
    assert!(clerk.contains(&Permission::CatalogManage));
    assert!(!clerk.contains(&Permission::CatalogViewArchived));
    let cashier = grants(&role(CASHIER_ROLE).await);
    assert!(!cashier.contains(&Permission::CatalogManage));
}

async fn catalog_permissions_on_a_fresh_database(db: &Db) {
    seed::seed_defaults(db, t(1)).await.unwrap();
    assert_catalog_grants(db).await;
    // Re-running the migration's grant step is harmless: the seed is idempotent too.
    seed::seed_defaults(db, t(2)).await.unwrap();
    assert_catalog_grants(db).await;
}

const CATALOG_MIGRATION: i64 = 20_261_109_000_001;

/// The migrations before the catalog, as an older release shipped them.
async fn old_migrator(backend: &str) -> sqlx::migrate::Migrator {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(backend);
    let mut migrator = sqlx::migrate::Migrator::new(dir).await.unwrap();
    let old: Vec<_> = migrator
        .migrations
        .iter()
        .filter(|m| m.version < CATALOG_MIGRATION)
        .cloned()
        .collect();
    migrator.migrations = Cow::Owned(old);
    migrator
}

/// Seeds the roles as a pre-catalog release did (without the catalog grants).
async fn seed_like_an_old_release(db: &Db) {
    seed::seed_defaults(db, t(1)).await.unwrap();
    raw(
        db,
        "DELETE FROM role_permission WHERE perm IN ('catalog.manage', 'catalog.view_archived')",
    )
    .await
    .unwrap();
}

/// An install migrated from before 2.2: the catalog migration adds the new grants to the
/// existing seeded roles, and keeps a role a user edited otherwise intact.
async fn upgrade_adds_catalog_grants(db: &Db) {
    seed_like_an_old_release(db).await;
    let manager_before = roles::get_role(db, MANAGER_ROLE).await.unwrap().unwrap();
    assert!(!grants(&manager_before).contains(&Permission::CatalogManage));
    db.migrate().await.unwrap();
    let manager = roles::get_role(db, MANAGER_ROLE).await.unwrap().unwrap();
    assert_eq!(manager.grants.len(), manager_before.grants.len() + 2);
    let clerk = roles::get_role(db, STOCK_CLERK_ROLE)
        .await
        .unwrap()
        .unwrap();
    assert!(grants(&clerk).contains(&Permission::CatalogManage));
    seed::seed_defaults(db, t(3)).await.unwrap();
    assert_catalog_grants(db).await;
    assert_eq!(count(db, "tax_class").await, 2);
}

#[tokio::test]
async fn sqlite_upgrade_adds_catalog_grants() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let Db::Sqlite(pool) = &db else {
        unreachable!()
    };
    old_migrator("sqlite").await.run(pool).await.unwrap();
    upgrade_adds_catalog_grants(&db).await;
}

#[tokio::test]
async fn mysql_upgrade_adds_catalog_grants() {
    let Some(test_db) = MySqlTestDb::create_empty().await else {
        return;
    };
    let Db::MySql(pool) = &test_db.db else {
        unreachable!()
    };
    old_migrator("mysql").await.run(pool).await.unwrap();
    upgrade_adds_catalog_grants(&test_db.db).await;
    test_db.drop().await;
}
