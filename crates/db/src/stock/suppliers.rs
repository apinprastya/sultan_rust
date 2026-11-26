//! Suppliers and supplier items (spec 2.8 §3, I15): HQ master data. `PUT /suppliers/{id}`
//! creates or overwrites one ([`put_supplier`]), `DELETE` archives it ([`archive_supplier`]);
//! a supplier is never hard-deleted. A supplier item is (supplier, variant) with the supplier's
//! item code, the last price per base unit (set by every posted receipt) and the preferred flag
//! (at most one supplier per variant).

use sultan_core::ids::{SupplierId, UnitId, VariantId};
use sultan_core::money::Money;
use sultan_core::stock::{SupplierInput, check_supplier};
use sultan_core::time::Timestamp;

use super::{StockDbError, StockResult, Written};
use crate::conn::{Conn, Tx};
use crate::error::Result;
use crate::sql::{self, Arg, Row, args};

/// A supplier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supplier {
    /// `UUIDv7`.
    pub id: SupplierId,
    /// 1–20 characters, stored upper case, unique.
    pub code: String,
    /// 1–120 characters.
    pub name: String,
    /// Address.
    pub address: Option<String>,
    /// Phone.
    pub phone: Option<String>,
    /// E-mail.
    pub email: Option<String>,
    /// NPWP.
    pub tax_id: Option<String>,
    /// The supplier is PKP (informational).
    pub pkp: bool,
    /// Payment terms in days (0 = cash).
    pub terms_days: u32,
    /// Lead time in days (§10).
    pub lead_days: u32,
    /// Notes.
    pub notes: Option<String>,
    /// Offered for new documents.
    pub active: bool,
    /// When it was archived; `None` while it exists.
    pub archived_at: Option<Timestamp>,
}

const SELECT: &str = "SELECT id, code, name, address, phone, email, tax_id, pkp, terms_days, \
                      lead_days, notes, active, archived_at FROM supplier";

fn days(row: &dyn Row, col: &str) -> Result<u32> {
    let v = row.i64(col)?;
    u32::try_from(v).map_err(|_| crate::DbError::Corrupt(format!("`{col}` {v} out of range")))
}

fn map(row: &dyn Row) -> Result<Supplier> {
    Ok(Supplier {
        id: sql::id(row, "id")?,
        code: row.text("code")?,
        name: row.text("name")?,
        address: row.opt_text("address")?,
        phone: row.opt_text("phone")?,
        email: row.opt_text("email")?,
        tax_id: row.opt_text("tax_id")?,
        pkp: row.bool("pkp")?,
        terms_days: days(row, "terms_days")?,
        lead_days: days(row, "lead_days")?,
        notes: row.opt_text("notes")?,
        active: row.bool("active")?,
        archived_at: sql::opt_ts(row, "archived_at")?,
    })
}

fn clean(s: Option<&String>) -> Option<String> {
    s.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
}

/// One supplier, archived or not.
///
/// # Errors
/// The database error.
pub async fn get_supplier<'c>(
    conn: impl Into<Conn<'c>>,
    id: SupplierId,
) -> Result<Option<Supplier>> {
    let sql = format!("{SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map).await
}

/// The suppliers by code: the non-archived ones, or with `archived` every one.
///
/// # Errors
/// The database error.
pub async fn list_suppliers<'c>(
    conn: impl Into<Conn<'c>>,
    archived: bool,
) -> Result<Vec<Supplier>> {
    let filter = if archived {
        ""
    } else {
        " WHERE archived_at IS NULL"
    };
    let sql = format!("{SELECT}{filter} ORDER BY code");
    sql::fetch_all(conn.into(), &sql, args![], map).await
}

/// Creates or overwrites a supplier (`PUT /suppliers/{id}`). The code is cleaned by
/// `core::stock::check_supplier`; an archived supplier cannot be edited. Returns what is stored.
///
/// # Errors
/// [`StockDbError::Rule`] for a field; [`StockDbError::Archived`]; [`StockDbError::Db`] with a
/// [`crate::DbError::Conflict`] for a duplicate code.
pub async fn put_supplier(
    tx: &mut Tx,
    s: &Supplier,
    now: Timestamp,
) -> StockResult<(Written, Supplier)> {
    let (code, name) = check_supplier(&SupplierInput {
        code: &s.code,
        name: &s.name,
        terms_days: s.terms_days,
        lead_days: s.lead_days,
    })?;
    let before = get_supplier(Conn::Tx(&mut *tx), s.id).await?;
    if before.as_ref().is_some_and(|b| b.archived_at.is_some()) {
        return Err(StockDbError::Archived("supplier"));
    }
    let after = Supplier {
        code,
        name,
        address: clean(s.address.as_ref()),
        phone: clean(s.phone.as_ref()),
        email: clean(s.email.as_ref()),
        tax_id: clean(s.tax_id.as_ref()),
        notes: clean(s.notes.as_ref()),
        archived_at: None,
        ..s.clone()
    };
    if before.as_ref() == Some(&after) {
        return Ok((Written::Unchanged, after));
    }
    let saved = sql::upsert_by_id(
        tx,
        "supplier",
        after.id.into(),
        &[
            "code",
            "name",
            "address",
            "phone",
            "email",
            "tax_id",
            "pkp",
            "terms_days",
            "lead_days",
            "notes",
            "active",
            "archived_at",
        ],
        vec![
            after.code.clone().into(),
            after.name.clone().into(),
            after.address.clone().into(),
            after.phone.clone().into(),
            after.email.clone().into(),
            after.tax_id.clone().into(),
            after.pkp.into(),
            i64::from(after.terms_days).into(),
            i64::from(after.lead_days).into(),
            after.notes.clone().into(),
            after.active.into(),
            Option::<i64>::None.into(),
        ],
        now,
    )
    .await?;
    Ok((
        match saved {
            crate::Saved::Created => Written::Created,
            crate::Saved::Updated => Written::Updated,
        },
        after,
    ))
}

/// Archives a supplier (`DELETE /suppliers/{id}`); its documents keep pointing at it.
///
/// # Errors
/// [`StockDbError::NotFound`]; the database error.
pub async fn archive_supplier(
    tx: &mut Tx,
    id: SupplierId,
    now: Timestamp,
) -> StockResult<(Written, Supplier)> {
    let before = get_supplier(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(StockDbError::NotFound("supplier"))?;
    if before.archived_at.is_some() {
        return Ok((Written::Unchanged, before));
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE supplier SET archived_at = ?, active = 0, updated_at = ? WHERE id = ?",
        args![now, now, id],
    )
    .await?;
    Ok((
        Written::Updated,
        Supplier {
            archived_at: Some(now),
            active: false,
            ..before
        },
    ))
}

/// A supplier item (§3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupplierItem {
    /// The supplier.
    pub supplier: SupplierId,
    /// The variant.
    pub variant: VariantId,
    /// The supplier's item code (unique per supplier).
    pub supplier_code: Option<String>,
    /// The last price per base unit (set by posted receipts).
    pub last_price: Option<Money>,
    /// The unit of the last receipt line (the purchase unit of suggestions).
    pub last_unit: Option<UnitId>,
    /// The preferred supplier of the variant.
    pub preferred: bool,
}

const ITEM_SELECT: &str = "SELECT supplier_id, variant_id, supplier_code, last_price, \
                           last_unit_id, preferred FROM supplier_item";

fn map_item(row: &dyn Row) -> Result<SupplierItem> {
    Ok(SupplierItem {
        supplier: sql::id(row, "supplier_id")?,
        variant: sql::id(row, "variant_id")?,
        supplier_code: row.opt_text("supplier_code")?,
        last_price: sql::opt_money(row, "last_price")?,
        last_unit: sql::opt_id(row, "last_unit_id")?,
        preferred: row.bool("preferred")?,
    })
}

/// One supplier item.
///
/// # Errors
/// The database error.
pub async fn get_supplier_item<'c>(
    conn: impl Into<Conn<'c>>,
    supplier: SupplierId,
    variant: VariantId,
) -> Result<Option<SupplierItem>> {
    let sql = format!("{ITEM_SELECT} WHERE supplier_id = ? AND variant_id = ?");
    sql::fetch_optional(conn.into(), &sql, args![supplier, variant], map_item).await
}

/// The items of a supplier, or of a variant (its suppliers), by variant then supplier.
///
/// # Errors
/// The database error.
pub async fn list_supplier_items<'c>(
    conn: impl Into<Conn<'c>>,
    supplier: Option<SupplierId>,
    variant: Option<VariantId>,
) -> Result<Vec<SupplierItem>> {
    let mut sql = format!("{ITEM_SELECT} WHERE 1 = 1");
    let mut a: Vec<Arg> = Vec::new();
    if let Some(s) = supplier {
        sql.push_str(" AND supplier_id = ?");
        a.push(s.into());
    }
    if let Some(v) = variant {
        sql.push_str(" AND variant_id = ?");
        a.push(v.into());
    }
    sql.push_str(" ORDER BY variant_id, supplier_id");
    sql::fetch_all(conn.into(), &sql, a, map_item).await
}

/// Creates or overwrites a supplier item (`PUT /suppliers/{id}/items/{variant_id}`): the code
/// and the preferred flag. Making it preferred takes the flag from the variant's other supplier.
/// The last price and unit are kept (only receipts set them).
///
/// # Errors
/// [`StockDbError::NotFound`] (`supplier`); [`StockDbError::Invalid`] (`supplier_code`, at most
/// 40 characters); [`crate::DbError::Conflict`] for a duplicate code of the supplier.
pub async fn put_supplier_item(
    tx: &mut Tx,
    item: &SupplierItem,
    now: Timestamp,
) -> StockResult<(Written, SupplierItem)> {
    if get_supplier(Conn::Tx(&mut *tx), item.supplier)
        .await?
        .is_none()
    {
        return Err(StockDbError::NotFound("supplier"));
    }
    let code = item
        .supplier_code
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    if code.as_ref().is_some_and(|c| c.chars().count() > 40) {
        return Err(StockDbError::invalid(
            "supplier_code",
            "is at most 40 characters",
        ));
    }
    let before = get_supplier_item(Conn::Tx(&mut *tx), item.supplier, item.variant).await?;
    let after = SupplierItem {
        supplier_code: code,
        last_price: before.as_ref().and_then(|b| b.last_price),
        last_unit: before.as_ref().and_then(|b| b.last_unit),
        ..item.clone()
    };
    if before.as_ref() == Some(&after) {
        return Ok((Written::Unchanged, after));
    }
    if after.preferred {
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE supplier_item SET preferred = 0, preferred_variant = NULL, updated_at = ? \
             WHERE variant_id = ? AND supplier_id <> ? AND preferred = 1",
            args![now, item.variant, item.supplier],
        )
        .await?;
    }
    let preferred_variant = after.preferred.then_some(after.variant);
    if before.is_some() {
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE supplier_item SET supplier_code = ?, preferred = ?, preferred_variant = ?, \
             updated_at = ? WHERE supplier_id = ? AND variant_id = ?",
            args![
                after.supplier_code.clone(),
                after.preferred,
                preferred_variant,
                now,
                after.supplier,
                after.variant,
            ],
        )
        .await?;
    } else {
        insert_item(tx, &after, now).await?;
    }
    Ok((
        if before.is_some() {
            Written::Updated
        } else {
            Written::Created
        },
        after,
    ))
}

async fn insert_item(tx: &mut Tx, item: &SupplierItem, now: Timestamp) -> Result<()> {
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO supplier_item (supplier_id, variant_id, supplier_code, last_price, \
         last_unit_id, preferred, preferred_variant, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            item.supplier,
            item.variant,
            item.supplier_code.clone(),
            item.last_price,
            item.last_unit,
            item.preferred,
            item.preferred.then_some(item.variant),
            now,
            now,
        ],
    )
    .await?;
    Ok(())
}

/// Records a posted receipt line's price per base unit and unit on the supplier item, creating
/// the item when the variant has none for this supplier (§3).
pub(crate) async fn record_last_price(
    tx: &mut Tx,
    supplier: SupplierId,
    variant: VariantId,
    price: Money,
    unit: UnitId,
    now: Timestamp,
) -> Result<()> {
    let done = sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE supplier_item SET last_price = ?, last_unit_id = ?, updated_at = ? \
         WHERE supplier_id = ? AND variant_id = ?",
        args![price, unit, now, supplier, variant],
    )
    .await?;
    if done == 0 {
        insert_item(
            tx,
            &SupplierItem {
                supplier,
                variant,
                supplier_code: None,
                last_price: Some(price),
                last_unit: Some(unit),
                preferred: false,
            },
            now,
        )
        .await?;
    }
    Ok(())
}
