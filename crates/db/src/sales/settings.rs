//! The sales settings (spec 2.6 §8, §9; phase S6, deferred from S3): the company's return
//! window `return_days` (default 30, `0` = no limit) and the receipt width of each branch (32,
//! 42 or 48 columns, default 32), and since spec 2.7 §2a the company's `nota_flag_above` (Rp,
//! default 100 000, `0` = off: a cash out of at least this much without a nota photo is flagged
//! "missing nota"). All are HQ data: [`put_sales_settings`] writes them with one
//! audit row and one outbox row (`sales_settings.put`), nothing when nothing changed.

use serde_json::{Value, json};
use sultan_core::ids::BranchId;
use sultan_core::money::Money;
use sultan_core::time::Timestamp;

use super::{Actor, Change, SaleDbError, SaleResult, Written, count, record};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, args};

/// The return window when nothing else is stored (spec 2.6 §8).
pub const DEFAULT_RETURN_DAYS: u32 = 30;
/// The longest return window that can be stored (10 years).
pub const MAX_RETURN_DAYS: u32 = 3_650;
/// The receipt width of a branch without a stored one.
pub const DEFAULT_RECEIPT_WIDTH: u16 = 32;
/// The receipt widths a branch may have.
pub const RECEIPT_WIDTHS: [u16; 3] = [32, 42, 48];

/// The sales settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SalesSettings {
    /// The return window in days; `0` = no limit.
    pub return_days: u32,
    /// The stored receipt widths per branch, by branch ID; a branch that is not listed prints
    /// [`DEFAULT_RECEIPT_WIDTH`] columns.
    pub receipt_widths: Vec<(BranchId, u16)>,
    /// A cash out of at least this much without a nota photo is "missing nota" (spec 2.7 §2a,
    /// default `core::cash::DEFAULT_NOTA_FLAG_ABOVE`); `0` = off.
    pub nota_flag_above: Money,
}

fn settings_json(s: &SalesSettings) -> Value {
    json!({
        "return_days": s.return_days,
        "receipt_widths": s
            .receipt_widths
            .iter()
            .map(|(b, w)| json!({"branch_id": b.to_string(), "receipt_width": w}))
            .collect::<Vec<_>>(),
        "nota_flag_above": s.nota_flag_above.minor(),
    })
}

/// The stored settings (`GET /settings/sales`); widths by branch ID.
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] when the company row is missing.
pub async fn get_sales_settings<'c>(conn: impl Into<Conn<'c>>) -> Result<SalesSettings> {
    let mut conn = conn.into();
    let (return_days, nota_flag_above) = sql::fetch_optional(
        conn.reborrow(),
        "SELECT return_days, nota_flag_above FROM sales_settings WHERE id = 1",
        args![],
        |row| {
            Ok((
                count(row, "return_days")?,
                sql::money(row, "nota_flag_above")?,
            ))
        },
    )
    .await?
    .ok_or_else(|| DbError::Corrupt("sales_settings row is missing".into()))?;
    let mut receipt_widths = sql::fetch_all(
        conn,
        "SELECT branch_id, receipt_width FROM branch_sales_settings",
        args![],
        |row| {
            let width = count(row, "receipt_width")?;
            Ok((
                sql::id::<BranchId>(row, "branch_id")?,
                u16::try_from(width).unwrap_or(DEFAULT_RECEIPT_WIDTH),
            ))
        },
    )
    .await?;
    receipt_widths.sort_by_key(|(b, _)| b.to_string());
    Ok(SalesSettings {
        return_days,
        receipt_widths,
        nota_flag_above,
    })
}

/// The return window in days (`0` = no limit).
///
/// # Errors
/// As [`get_sales_settings`].
pub async fn return_days<'c>(conn: impl Into<Conn<'c>>) -> Result<u32> {
    sql::fetch_optional(
        conn.into(),
        "SELECT return_days FROM sales_settings WHERE id = 1",
        args![],
        |row| count(row, "return_days"),
    )
    .await?
    .ok_or_else(|| DbError::Corrupt("sales_settings row is missing".into()))
}

/// The receipt width of `branch` ([`DEFAULT_RECEIPT_WIDTH`] when none is stored).
///
/// # Errors
/// The database error.
pub async fn receipt_width<'c>(conn: impl Into<Conn<'c>>, branch: BranchId) -> Result<u16> {
    let width = sql::fetch_optional(
        conn.into(),
        "SELECT receipt_width FROM branch_sales_settings WHERE branch_id = ?",
        args![branch],
        |row| count(row, "receipt_width"),
    )
    .await?;
    Ok(width
        .and_then(|w| u16::try_from(w).ok())
        .unwrap_or(DEFAULT_RECEIPT_WIDTH))
}

/// Replaces the return window and the nota threshold and sets the receipt width of every listed branch (`PUT
/// /settings/sales`); branches that are not listed keep theirs. One audit row and one outbox row
/// `sales_settings.put` with the whole settings before and after, nothing when nothing changed.
///
/// # Errors
/// [`SaleDbError::Invalid`] (`return_days` above [`MAX_RETURN_DAYS`], a `receipt_width` that is
/// not 32, 42 or 48, a negative `nota_flag_above`), [`SaleDbError::NotFound`] (`branch`), or the
/// database error.
pub async fn put_sales_settings(
    tx: &mut Tx,
    settings: &SalesSettings,
    actor: &Actor,
    now: Timestamp,
) -> SaleResult<Written> {
    if settings.return_days > MAX_RETURN_DAYS {
        return Err(SaleDbError::Invalid {
            field: "return_days",
            message: format!("must be 0 to {MAX_RETURN_DAYS}"),
        });
    }
    if settings.nota_flag_above.is_negative() {
        return Err(SaleDbError::Invalid {
            field: "nota_flag_above",
            message: "must not be negative".into(),
        });
    }
    let mut seen = std::collections::HashSet::new();
    for (branch, width) in &settings.receipt_widths {
        if !seen.insert(*branch) {
            return Err(SaleDbError::Invalid {
                field: "receipt_width",
                message: "lists a branch twice".into(),
            });
        }
        if !RECEIPT_WIDTHS.contains(width) {
            return Err(SaleDbError::Invalid {
                field: "receipt_width",
                message: "must be 32, 42 or 48".into(),
            });
        }
        if crate::org::get_branch(Conn::Tx(&mut *tx), *branch)
            .await?
            .is_none()
        {
            return Err(SaleDbError::NotFound("branch"));
        }
    }
    let old = get_sales_settings(Conn::Tx(&mut *tx)).await?;
    let mut changed = false;
    if old.return_days != settings.return_days {
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE sales_settings SET return_days = ?, updated_at = ? WHERE id = 1",
            args![i64::from(settings.return_days), now],
        )
        .await?;
        changed = true;
    }
    if old.nota_flag_above != settings.nota_flag_above {
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE sales_settings SET nota_flag_above = ?, updated_at = ? WHERE id = 1",
            args![settings.nota_flag_above, now],
        )
        .await?;
        changed = true;
    }
    for (branch, width) in &settings.receipt_widths {
        let stored = old.receipt_widths.iter().find(|(b, _)| b == branch);
        match stored {
            Some((_, w)) if w == width => continue,
            Some(_) => {
                sql::execute(
                    Conn::Tx(&mut *tx),
                    "UPDATE branch_sales_settings SET receipt_width = ?, updated_at = ? \
                     WHERE branch_id = ?",
                    args![i64::from(*width), now, *branch],
                )
                .await?;
            }
            None => {
                sql::execute(
                    Conn::Tx(&mut *tx),
                    "INSERT INTO branch_sales_settings (branch_id, receipt_width, updated_at) \
                     VALUES (?, ?, ?)",
                    args![*branch, i64::from(*width), now],
                )
                .await?;
            }
        }
        changed = true;
    }
    if !changed {
        return Ok(Written::Unchanged);
    }
    let new = get_sales_settings(Conn::Tx(&mut *tx)).await?;
    let after = settings_json(&new);
    record(
        tx,
        actor,
        now,
        Change {
            action: "sales_settings.put",
            target: "sales_settings".to_owned(),
            before: Some(settings_json(&old)),
            after: Some(after.clone()),
            outbox: Some(("sales_settings.put", &after)),
        },
    )
    .await?;
    Ok(Written::Updated)
}
