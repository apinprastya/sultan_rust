//! Tax classes, the category tree and brands (spec 2.2 §2, §8, §9). Archiving hides a category or
//! brand from pickers and products keep it. Only a never-used one may be deleted
//! ([`delete_category`], [`delete_brand`]): a category without products or child categories, a
//! brand without products (archived ones count).

use serde_json::{Value, json};
use sultan_core::catalog::{
    Brand, Category, product_search_key, search_key, validate_brand, validate_category,
};
use sultan_core::ids::{BrandId, CategoryId, ProductId, TaxClassId};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::{Actor, CatalogDbError, CatalogResult, Change, Written, record};
use crate::conn::{Conn, Tx};
use crate::error::Result;
use crate::sql::{self, Row, args};

/// The seeded `Standard` tax class (fixed ID on every node).
pub const STANDARD_TAX_CLASS: TaxClassId =
    TaxClassId::from_uuid(Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_0101));
/// The seeded `Exempt` tax class (fixed ID on every node).
pub const EXEMPT_TAX_CLASS: TaxClassId =
    TaxClassId::from_uuid(Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_0102));

/// A tax class. Rates and inclusive/exclusive rules arrive with spec 2.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxClass {
    /// ID.
    pub id: TaxClassId,
    /// Name.
    pub name: String,
    /// Archived.
    pub archived: bool,
}

/// All tax classes, ordered by name.
///
/// # Errors
/// The database error.
pub async fn list_tax_classes<'c>(conn: impl Into<Conn<'c>>) -> Result<Vec<TaxClass>> {
    sql::fetch_all(
        conn.into(),
        "SELECT id, name, archived FROM tax_class ORDER BY name",
        args![],
        |row| {
            Ok(TaxClass {
                id: sql::id(row, "id")?,
                name: row.text("name")?,
                archived: row.bool("archived")?,
            })
        },
    )
    .await
}

/// A category with its children (`GET /categories`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryNode {
    /// The category.
    pub category: Category,
    /// Its children, ordered by name.
    pub children: Vec<CategoryNode>,
}

const CATEGORY_SELECT: &str = "SELECT id, parent_id, name, code, archived FROM category";

fn map_category(row: &dyn Row) -> Result<Category> {
    Ok(Category {
        id: sql::id(row, "id")?,
        parent: sql::opt_id(row, "parent_id")?,
        name: row.text("name")?,
        code: row.text("code")?,
        archived: row.bool("archived")?,
    })
}

async fn all_categories(conn: Conn<'_>) -> Result<Vec<Category>> {
    let sql = format!("{CATEGORY_SELECT} ORDER BY name, id");
    sql::fetch_all(conn, &sql, args![], map_category).await
}

/// One category.
///
/// # Errors
/// The database error.
pub async fn get_category<'c>(
    conn: impl Into<Conn<'c>>,
    id: CategoryId,
) -> Result<Option<Category>> {
    let sql = format!("{CATEGORY_SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map_category).await
}

fn category_json(c: &Category) -> Value {
    json!({
        "id": c.id.to_string(),
        "parent_id": c.parent.map(|p| p.to_string()),
        "name": c.name,
        "code": c.code,
        "archived": c.archived,
    })
}

/// Creates or changes (renames, moves, archives) a category (`PUT /categories/{id}`), validated
/// with `core::validate_category` against the whole tree (name, code, parent exists, no cycle,
/// depth ≤ 4, unique sibling names). The name is trimmed and the code upper-cased first. Audit
/// and outbox `catalog.category.put` unless nothing changed.
///
/// # Errors
/// [`CatalogDbError::Rule`] or the database error.
pub async fn put_category(
    tx: &mut Tx,
    category: &Category,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<Written> {
    let mut new = category.clone();
    new.name = new.name.trim().to_owned();
    new.code = new.code.trim().to_ascii_uppercase();
    let existing = all_categories(Conn::Tx(&mut *tx)).await?;
    let old = existing.iter().find(|c| c.id == new.id).cloned();
    if old.as_ref() == Some(&new) {
        return Ok(Written::Unchanged);
    }
    validate_category(&existing, &new).map_err(CatalogDbError::rule)?;
    let saved = sql::upsert_by_id(
        tx,
        "category",
        new.id.into(),
        &["parent_id", "name", "code", "archived"],
        args![
            new.parent,
            new.name.as_str(),
            new.code.as_str(),
            new.archived
        ],
        now,
    )
    .await?;
    let after = category_json(&new);
    record(
        tx,
        actor,
        now,
        Change {
            action: "catalog.category.put",
            target: format!("category:{}", new.id),
            before: old.as_ref().map(category_json),
            after: Some(after.clone()),
            outbox_kind: "catalog.category.put",
            payload: after,
        },
    )
    .await?;
    Ok(written(saved))
}

/// Whether `sql` (one `?` for `id`) finds a row.
async fn any_row(tx: &mut Tx, sql: &str, id: crate::sql::Arg) -> Result<bool> {
    Ok(
        sql::fetch_optional(Conn::Tx(&mut *tx), sql, vec![id], |_| Ok(()))
            .await?
            .is_some(),
    )
}

/// Hard-deletes a never-used category (`DELETE /categories/{id}`, spec 2.2 §9). Audit and outbox
/// `catalog.category.delete` in `tx`.
///
/// # Errors
/// [`CatalogDbError::NotFound`]; [`CatalogDbError::InUse`] (`has_products`, archived products
/// included, or `has_children`); or the database error.
pub async fn delete_category(
    tx: &mut Tx,
    id: CategoryId,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<()> {
    let old = get_category(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(CatalogDbError::NotFound("category"))?;
    if any_row(
        tx,
        "SELECT 1 FROM product WHERE category_id = ? LIMIT 1",
        id.into(),
    )
    .await?
    {
        return Err(CatalogDbError::InUse("has_products"));
    }
    if any_row(
        tx,
        "SELECT 1 FROM category WHERE parent_id = ? LIMIT 1",
        id.into(),
    )
    .await?
    {
        return Err(CatalogDbError::InUse("has_children"));
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "DELETE FROM category WHERE id = ?",
        args![id],
    )
    .await?;
    record(
        tx,
        actor,
        now,
        Change {
            action: "catalog.category.delete",
            target: format!("category:{id}"),
            before: Some(category_json(&old)),
            after: None,
            outbox_kind: "catalog.category.delete",
            payload: json!({ "id": id.to_string() }),
        },
    )
    .await
}

/// Hard-deletes a never-used brand (`DELETE /brands/{id}`, spec 2.2 §9). Audit and outbox
/// `catalog.brand.delete` in `tx`.
///
/// # Errors
/// [`CatalogDbError::NotFound`]; [`CatalogDbError::InUse`] (`has_products`, archived products
/// included); or the database error.
pub async fn delete_brand(
    tx: &mut Tx,
    id: BrandId,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<()> {
    let old = get_brand(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(CatalogDbError::NotFound("brand"))?;
    if any_row(
        tx,
        "SELECT 1 FROM product WHERE brand_id = ? LIMIT 1",
        id.into(),
    )
    .await?
    {
        return Err(CatalogDbError::InUse("has_products"));
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "DELETE FROM brand WHERE id = ?",
        args![id],
    )
    .await?;
    record(
        tx,
        actor,
        now,
        Change {
            action: "catalog.brand.delete",
            target: format!("brand:{id}"),
            before: Some(brand_json(&old)),
            after: None,
            outbox_kind: "catalog.brand.delete",
            payload: json!({ "id": id.to_string() }),
        },
    )
    .await
}

fn written(saved: crate::Saved) -> Written {
    match saved {
        crate::Saved::Created => Written::Created,
        crate::Saved::Updated => Written::Updated,
    }
}

/// The category tree: roots and children ordered by name. Archived categories are left out
/// unless `include_archived` (an archived category hides its subtree).
///
/// # Errors
/// The database error.
pub async fn list_categories<'c>(
    conn: impl Into<Conn<'c>>,
    include_archived: bool,
) -> Result<Vec<CategoryNode>> {
    let all: Vec<Category> = all_categories(conn.into())
        .await?
        .into_iter()
        .filter(|c| include_archived || !c.archived)
        .collect();
    Ok(children_of(&all, None))
}

fn children_of(all: &[Category], parent: Option<CategoryId>) -> Vec<CategoryNode> {
    all.iter()
        .filter(|c| c.parent == parent)
        .map(|c| CategoryNode {
            category: c.clone(),
            children: children_of(all, Some(c.id)),
        })
        .collect()
}

const BRAND_SELECT: &str = "SELECT id, name, archived FROM brand";

fn map_brand(row: &dyn Row) -> Result<Brand> {
    Ok(Brand {
        id: sql::id(row, "id")?,
        name: row.text("name")?,
        archived: row.bool("archived")?,
    })
}

/// One brand.
///
/// # Errors
/// The database error.
pub async fn get_brand<'c>(conn: impl Into<Conn<'c>>, id: BrandId) -> Result<Option<Brand>> {
    let sql = format!("{BRAND_SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map_brand).await
}

/// Brands ordered by name; archived ones only with `include_archived`.
///
/// # Errors
/// The database error.
pub async fn list_brands<'c>(
    conn: impl Into<Conn<'c>>,
    include_archived: bool,
) -> Result<Vec<Brand>> {
    let filter = if include_archived {
        ""
    } else {
        " WHERE archived = 0"
    };
    let sql = format!("{BRAND_SELECT}{filter} ORDER BY name, id");
    sql::fetch_all(conn.into(), &sql, args![], map_brand).await
}

fn brand_json(b: &Brand) -> Value {
    json!({ "id": b.id.to_string(), "name": b.name, "archived": b.archived })
}

/// Creates or changes a brand (`PUT /brands/{id}`), validated with `core::validate_brand` (name
/// unique ignoring case and accents; the `name_key` unique index is the safety net). A rename
/// refreshes the search key of the brand's products. Audit and outbox `catalog.brand.put` unless
/// nothing changed.
///
/// # Errors
/// [`CatalogDbError::Rule`] or the database error.
pub async fn put_brand(
    tx: &mut Tx,
    brand: &Brand,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<Written> {
    let mut new = brand.clone();
    new.name = new.name.trim().to_owned();
    let existing = list_brands(Conn::Tx(&mut *tx), true).await?;
    let old = existing.iter().find(|b| b.id == new.id).cloned();
    if old.as_ref() == Some(&new) {
        return Ok(Written::Unchanged);
    }
    validate_brand(&existing, &new).map_err(CatalogDbError::rule)?;
    let saved = sql::upsert_by_id(
        tx,
        "brand",
        new.id.into(),
        &["name", "name_key", "archived"],
        args![new.name.as_str(), search_key(&new.name), new.archived],
        now,
    )
    .await?;
    if old.as_ref().is_some_and(|o| o.name != new.name) {
        refresh_search_keys(tx, &new).await?;
    }
    let after = brand_json(&new);
    record(
        tx,
        actor,
        now,
        Change {
            action: "catalog.brand.put",
            target: format!("brand:{}", new.id),
            before: old.as_ref().map(brand_json),
            after: Some(after.clone()),
            outbox_kind: "catalog.brand.put",
            payload: after,
        },
    )
    .await?;
    Ok(written(saved))
}

/// Recomputes `product.search_key` of the products of `brand` after a rename.
async fn refresh_search_keys(tx: &mut Tx, brand: &Brand) -> CatalogResult<()> {
    let rows = sql::fetch_all(
        Conn::Tx(&mut *tx),
        "SELECT id, name, receipt_name, code FROM product WHERE brand_id = ?",
        args![brand.id],
        |row| {
            let key = product_search_key(
                &row.text("name")?,
                &row.text("receipt_name")?,
                &row.text("code")?,
                Some(&brand.name),
            );
            Ok((sql::id::<ProductId>(row, "id")?, key))
        },
    )
    .await?;
    for (id, key) in rows {
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE product SET search_key = ? WHERE id = ?",
            args![key, id],
        )
        .await?;
    }
    Ok(())
}
