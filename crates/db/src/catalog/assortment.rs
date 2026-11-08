//! Per-branch assortment (spec 2.2 §10) and product images (§8).

use serde_json::json;
use sultan_core::catalog::CatalogError;
use sultan_core::ids::{BranchId, ProductId};
use sultan_core::time::Timestamp;

use super::{Actor, CatalogDbError, CatalogResult, Change, Written, record};
use crate::conn::{Conn, Tx};
use crate::error::Result;
use crate::sql::{self, Arg, args};

async fn product_exists(tx: &mut Tx, product: ProductId) -> Result<bool> {
    Ok(sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT 1 FROM product WHERE id = ?",
        args![product],
        |_| Ok(()),
    )
    .await?
    .is_some())
}

/// The branches where `product` is NOT sold, ordered by ID.
///
/// # Errors
/// The database error.
pub async fn get_branch_blocks<'c>(
    conn: impl Into<Conn<'c>>,
    product: ProductId,
) -> Result<Vec<BranchId>> {
    sql::fetch_all(
        conn.into(),
        "SELECT branch_id FROM product_branch_block WHERE product_id = ? ORDER BY branch_id",
        args![product],
        |row| sql::id(row, "branch_id"),
    )
    .await
}

/// Replaces the set of branches where `product` is not sold
/// (`PUT /products/{id}/branch-blocks`); duplicates are ignored. Audit and outbox
/// `catalog.product.branch_blocks.put` unless the set is unchanged.
///
/// # Errors
/// [`CatalogDbError::NotFound`] for an unknown product, [`CatalogDbError::Rule`]
/// (`invalid_field` `branch_ids`) for an unknown branch, or the database error.
pub async fn put_branch_blocks(
    tx: &mut Tx,
    product: ProductId,
    branches: &[BranchId],
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<Written> {
    if !product_exists(tx, product).await? {
        return Err(CatalogDbError::NotFound("product"));
    }
    let mut new: Vec<BranchId> = branches.to_vec();
    new.sort_unstable();
    new.dedup();
    if !new.is_empty() {
        let sql = format!(
            "SELECT id FROM branch WHERE id IN ({})",
            vec!["?"; new.len()].join(", ")
        );
        let found = sql::fetch_all(
            Conn::Tx(&mut *tx),
            &sql,
            new.iter().map(|b| Arg::from(*b)).collect(),
            |_| Ok(()),
        )
        .await?;
        if found.len() != new.len() {
            return Err(CatalogDbError::rule(CatalogError::InvalidField {
                field: "branch_ids",
                reason: "unknown branch",
            }));
        }
    }
    let old = get_branch_blocks(Conn::Tx(&mut *tx), product).await?;
    if old == new {
        return Ok(Written::Unchanged);
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "DELETE FROM product_branch_block WHERE product_id = ?",
        args![product],
    )
    .await?;
    for branch in &new {
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO product_branch_block (product_id, branch_id) VALUES (?, ?)",
            args![product, *branch],
        )
        .await?;
    }
    let ids = |list: &[BranchId]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
    let after = json!({ "product_id": product.to_string(), "branch_ids": ids(&new) });
    record(
        tx,
        actor,
        now,
        Change {
            action: "catalog.product.branch_blocks.put",
            target: format!("product:{product}"),
            before: Some(json!({ "product_id": product.to_string(), "branch_ids": ids(&old) })),
            after: Some(after.clone()),
            outbox_kind: "catalog.product.branch_blocks.put",
            payload: after,
        },
    )
    .await?;
    Ok(if old.is_empty() {
        Written::Created
    } else {
        Written::Updated
    })
}

/// A product image as stored (spec 2.2 §8). The server resizes and re-encodes before storing;
/// this layer keeps the bytes exactly as given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductImage {
    /// Media type, e.g. `image/webp`.
    pub mime: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The encoded image.
    pub data: Vec<u8>,
}

/// The image of `product`, if any.
///
/// # Errors
/// The database error.
pub async fn get_product_image<'c>(
    conn: impl Into<Conn<'c>>,
    product: ProductId,
) -> Result<Option<ProductImage>> {
    sql::fetch_optional(
        conn.into(),
        "SELECT mime, width, height, data FROM product_image WHERE product_id = ?",
        args![product],
        |row| {
            let dim = |col: &str| -> Result<u32> {
                u32::try_from(row.i64(col)?).map_err(|_| {
                    crate::DbError::Corrupt(format!("product_image.{col} out of range"))
                })
            };
            Ok(ProductImage {
                mime: row.text("mime")?,
                width: dim("width")?,
                height: dim("height")?,
                data: row.bytes("data")?,
            })
        },
    )
    .await
}

fn image_json(product: ProductId, image: &ProductImage) -> serde_json::Value {
    json!({
        "product_id": product.to_string(),
        "mime": image.mime,
        "width": image.width,
        "height": image.height,
        "bytes": image.data.len(),
    })
}

/// Sets (or replaces) the image of `product` (`PUT /products/{id}/image`). Audit and outbox
/// `catalog.product.image.put` with the metadata (the sync worker reads the bytes from the table)
/// unless the image is unchanged.
///
/// # Errors
/// [`CatalogDbError::NotFound`] for an unknown product, [`CatalogDbError::Rule`] for an empty
/// image or media type, or the database error.
pub async fn put_product_image(
    tx: &mut Tx,
    product: ProductId,
    image: &ProductImage,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<Written> {
    if !product_exists(tx, product).await? {
        return Err(CatalogDbError::NotFound("product"));
    }
    if image.data.is_empty() || image.mime.trim().is_empty() {
        return Err(CatalogDbError::rule(CatalogError::InvalidField {
            field: "image",
            reason: "empty image or media type",
        }));
    }
    let old = get_product_image(Conn::Tx(&mut *tx), product).await?;
    if old.as_ref() == Some(image) {
        return Ok(Written::Unchanged);
    }
    if old.is_some() {
        sql::execute(
            Conn::Tx(&mut *tx),
            "DELETE FROM product_image WHERE product_id = ?",
            args![product],
        )
        .await?;
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO product_image (product_id, mime, width, height, byte_size, data, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        args![
            product,
            image.mime.as_str(),
            i64::from(image.width),
            i64::from(image.height),
            i64::try_from(image.data.len()).unwrap_or(i64::MAX),
            image.data.clone(),
            now,
        ],
    )
    .await?;
    let after = image_json(product, image);
    record(
        tx,
        actor,
        now,
        Change {
            action: "catalog.product.image.put",
            target: format!("product:{product}"),
            before: old.as_ref().map(|o| image_json(product, o)),
            after: Some(after.clone()),
            outbox_kind: "catalog.product.image.put",
            payload: after,
        },
    )
    .await?;
    Ok(if old.is_some() {
        Written::Updated
    } else {
        Written::Created
    })
}

/// Removes the image of `product`. Returns whether there was one (audit and outbox
/// `catalog.product.image.delete` only then).
///
/// # Errors
/// The database error.
pub async fn delete_product_image(
    tx: &mut Tx,
    product: ProductId,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<bool> {
    let Some(old) = get_product_image(Conn::Tx(&mut *tx), product).await? else {
        return Ok(false);
    };
    sql::execute(
        Conn::Tx(&mut *tx),
        "DELETE FROM product_image WHERE product_id = ?",
        args![product],
    )
    .await?;
    record(
        tx,
        actor,
        now,
        Change {
            action: "catalog.product.image.delete",
            target: format!("product:{product}"),
            before: Some(image_json(product, &old)),
            after: None,
            outbox_kind: "catalog.product.image.delete",
            payload: json!({ "product_id": product.to_string() }),
        },
    )
    .await?;
    Ok(true)
}
