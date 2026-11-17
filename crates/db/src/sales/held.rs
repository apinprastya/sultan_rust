//! Held carts (spec 2.6 §4): a cart put aside on the branch server and resumed on any register
//! of the branch. Branch-local (not synced, no outbox), not documents, never dropped
//! automatically. Taking one is a guarded `UPDATE`, so of two registers taking it at once exactly
//! one wins and the other gets `cart_taken`.

use serde_json::{Value, json};
use sultan_core::ids::{BranchId, HeldCartId, RegisterId, UserId};
use sultan_core::money::Money;
use sultan_core::sale::SaleError;
use sultan_core::time::Timestamp;

use super::{
    Actor, Change, CustomerId, SaleDbError, SaleResult, Written, clean_text, count, record,
};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

/// A held cart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldCart {
    /// The register's `UUIDv7`.
    pub id: HeldCartId,
    /// The branch.
    pub branch: BranchId,
    /// The register that held it.
    pub register: RegisterId,
    /// The cashier who held it.
    pub user: UserId,
    /// The attached customer.
    pub customer: Option<CustomerId>,
    /// An optional note (at most 200 characters, trimmed).
    pub note: Option<String>,
    /// The cart as the register had it (opaque JSON).
    pub payload: Value,
    /// The item count shown in the resume list.
    pub item_count: u32,
    /// The total shown in the resume list.
    pub total: Money,
    /// When it was held.
    pub held_at: Timestamp,
    /// Who resumed it (`None` while it waits).
    pub taken_by: Option<UserId>,
    /// The register that resumed it.
    pub taken_register: Option<RegisterId>,
    /// When it was resumed.
    pub taken_at: Option<Timestamp>,
}

const SELECT: &str = "SELECT id, branch_id, register_id, user_id, customer_id, note, payload, \
     item_count, total, held_at, taken_by, taken_register_id, taken_at FROM held_cart";

fn map(row: &dyn Row) -> Result<HeldCart> {
    Ok(HeldCart {
        id: sql::id(row, "id")?,
        branch: sql::id(row, "branch_id")?,
        register: sql::id(row, "register_id")?,
        user: sql::id(row, "user_id")?,
        customer: sql::opt_id(row, "customer_id")?,
        note: row.opt_text("note")?,
        payload: sql::opt_json(row, "payload")?
            .ok_or_else(|| DbError::Corrupt("held cart without payload".into()))?,
        item_count: count(row, "item_count")?,
        total: sql::money(row, "total")?,
        held_at: sql::ts(row, "held_at")?,
        taken_by: sql::opt_id(row, "taken_by")?,
        taken_register: sql::opt_id(row, "taken_register_id")?,
        taken_at: sql::opt_ts(row, "taken_at")?,
    })
}

fn summary(c: &HeldCart) -> Value {
    json!({
        "id": c.id.to_string(),
        "register_id": c.register.to_string(),
        "user_id": c.user.to_string(),
        "customer_id": c.customer.map(|c| c.to_string()),
        "note": c.note,
        "item_count": c.item_count,
        "total": c.total.minor(),
        "held_at": c.held_at.unix_millis(),
        "payload": c.payload,
    })
}

/// One held cart, taken or not.
///
/// # Errors
/// The database error.
pub async fn get_held_cart<'c>(
    conn: impl Into<Conn<'c>>,
    id: HeldCartId,
) -> Result<Option<HeldCart>> {
    let sql = format!("{SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map).await
}

/// The branch's held carts that are still waiting, newest first (`GET /held-carts`).
///
/// # Errors
/// The database error.
pub async fn list_held_carts<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
) -> Result<Vec<HeldCart>> {
    let sql =
        format!("{SELECT} WHERE branch_id = ? AND taken_at IS NULL ORDER BY held_at DESC, id DESC");
    sql::fetch_all(conn.into(), &sql, args![branch], map).await
}

/// Holds a cart (`PUT /held-carts/{id}`, `sale.create`): creates it, or overwrites it while it
/// still waits. The `taken_*` fields are ignored. Replaying the same cart answers
/// [`Written::Unchanged`]. No audit, no outbox (branch-local, spec 2.6 §4).
///
/// # Errors
/// [`SaleDbError::Rule`] `cart_taken` when it was resumed already; [`SaleDbError::Invalid`]
/// (`note`); or the database error.
pub async fn put_held_cart(tx: &mut Tx, cart: &HeldCart) -> SaleResult<Written> {
    let cart = HeldCart {
        note: clean_text("note", cart.note.as_deref(), 200)?,
        taken_by: None,
        taken_register: None,
        taken_at: None,
        ..cart.clone()
    };
    let old = get_held_cart(Conn::Tx(&mut *tx), cart.id).await?;
    match old {
        Some(old) if old.taken_at.is_some() => Err(SaleError::CartTaken.into()),
        Some(old) if old == cart => Ok(Written::Unchanged),
        Some(_) => {
            sql::execute(
                Conn::Tx(&mut *tx),
                "UPDATE held_cart SET branch_id = ?, register_id = ?, user_id = ?, \
                 customer_id = ?, note = ?, payload = ?, item_count = ?, total = ?, held_at = ? \
                 WHERE id = ? AND taken_at IS NULL",
                args![
                    cart.branch,
                    cart.register,
                    cart.user,
                    cart.customer,
                    cart.note.as_deref(),
                    cart.payload.to_string(),
                    i64::from(cart.item_count),
                    cart.total,
                    cart.held_at,
                    cart.id,
                ],
            )
            .await?;
            Ok(Written::Updated)
        }
        None => {
            sql::execute(
                Conn::Tx(&mut *tx),
                "INSERT INTO held_cart (id, branch_id, register_id, user_id, customer_id, note, \
                 payload, item_count, total, held_at, taken_by, taken_register_id, taken_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL)",
                args![
                    cart.id,
                    cart.branch,
                    cart.register,
                    cart.user,
                    cart.customer,
                    cart.note.as_deref(),
                    cart.payload.to_string(),
                    i64::from(cart.item_count),
                    cart.total,
                    cart.held_at,
                ],
            )
            .await?;
            Ok(Written::Created)
        }
    }
}

/// Takes (resumes) a held cart (`POST /held-carts/{id}/take`): it leaves the list and the cart
/// is returned for the register to re-quote. Only one taker wins.
///
/// # Errors
/// [`SaleDbError::NotFound`]; [`SaleDbError::Rule`] `cart_taken` when another register took it
/// first; or the database error.
pub async fn take_held_cart(
    tx: &mut Tx,
    id: HeldCartId,
    user: UserId,
    register: RegisterId,
    now: Timestamp,
) -> SaleResult<HeldCart> {
    let changed = sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE held_cart SET taken_by = ?, taken_register_id = ?, taken_at = ? \
         WHERE id = ? AND taken_at IS NULL",
        args![user, register, now, id],
    )
    .await?;
    let cart = get_held_cart(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(SaleDbError::NotFound("held cart"))?;
    if changed == 0 {
        return Err(SaleError::CartTaken.into());
    }
    Ok(cart)
}

/// Drops a held cart without resuming it (`DELETE /held-carts/{id}`, `sale.void`): the row is
/// deleted and the audit row `held_cart.delete` keeps the cart. Returns the deleted cart.
///
/// # Errors
/// [`SaleDbError::NotFound`]; [`SaleDbError::Rule`] `cart_taken` when it was resumed already;
/// or the database error.
pub async fn delete_held_cart(
    tx: &mut Tx,
    id: HeldCartId,
    actor: &Actor,
    now: Timestamp,
) -> SaleResult<HeldCart> {
    let cart = get_held_cart(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(SaleDbError::NotFound("held cart"))?;
    let deleted = sql::execute(
        Conn::Tx(&mut *tx),
        "DELETE FROM held_cart WHERE id = ? AND taken_at IS NULL",
        args![id],
    )
    .await?;
    if deleted == 0 {
        return Err(SaleError::CartTaken.into());
    }
    record(
        tx,
        actor,
        now,
        Change {
            action: "held_cart.delete",
            target: format!("held_cart:{id}"),
            before: Some(summary(&cart)),
            after: None,
            outbox: None,
        },
    )
    .await?;
    Ok(cart)
}
