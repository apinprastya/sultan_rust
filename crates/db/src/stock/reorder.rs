//! Reorder (spec 2.8 §10, I13): min / max rules per (branch, variant), the low-stock list and
//! suggested purchase order lines from sales velocity.

use sultan_core::catalog::Qty;
use sultan_core::ids::{BranchId, SupplierId, UnitId, VariantId};
use sultan_core::money::Money;
use sultan_core::stock::{ReorderInput, check_rule, is_low, net_sold, suggest};
use sultan_core::time::Timestamp;

use super::json::{Change, record, rule_json};
use super::ledger::branch_on_hand;
use super::purchase::on_order;
use super::suppliers::{get_supplier, list_supplier_items};
use super::{Actor, StockDbError, StockResult, Written, get_inventory_settings};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

/// One day in milliseconds.
const DAY_MS: i64 = 86_400_000;

/// A reorder rule (base units).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReorderRule {
    /// The branch.
    pub branch: BranchId,
    /// The variant.
    pub variant: VariantId,
    /// Reorder at or below this.
    pub min: Qty,
    /// Order up to this.
    pub max: Qty,
}

fn map(row: &dyn Row) -> Result<ReorderRule> {
    Ok(ReorderRule {
        branch: sql::id(row, "branch_id")?,
        variant: sql::id(row, "variant_id")?,
        min: sql::qty(row, "min_qty")?,
        max: sql::qty(row, "max_qty")?,
    })
}

/// One reorder rule.
///
/// # Errors
/// The database error.
pub async fn get_reorder_rule<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
    variant: VariantId,
) -> Result<Option<ReorderRule>> {
    sql::fetch_optional(
        conn.into(),
        "SELECT branch_id, variant_id, min_qty, max_qty FROM reorder_rule \
         WHERE branch_id = ? AND variant_id = ?",
        args![branch, variant],
        map,
    )
    .await
}

/// The reorder rules of a branch, by variant.
///
/// # Errors
/// The database error.
pub async fn list_reorder_rules<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
) -> Result<Vec<ReorderRule>> {
    sql::fetch_all(
        conn.into(),
        "SELECT branch_id, variant_id, min_qty, max_qty FROM reorder_rule \
         WHERE branch_id = ? ORDER BY variant_id",
        args![branch],
        map,
    )
    .await
}

/// Creates or overwrites a rule (`PUT /reorder-rules/{branch_id}/{variant_id}`,
/// `purchase.order`): `0 ≤ min ≤ max`.
///
/// # Errors
/// [`StockDbError::Rule`] (`min`, `max`); the database error.
pub async fn put_reorder_rule(
    tx: &mut Tx,
    rule: &ReorderRule,
    actor: &Actor,
    now: Timestamp,
) -> StockResult<Written> {
    check_rule(rule.min, rule.max)?;
    let before = get_reorder_rule(Conn::Tx(&mut *tx), rule.branch, rule.variant).await?;
    if before.as_ref() == Some(rule) {
        return Ok(Written::Unchanged);
    }
    record(
        tx,
        actor,
        now,
        Change {
            action: "reorder_rule.put",
            target: format!("reorder_rule:{}:{}", rule.branch, rule.variant),
            before: before.as_ref().map(rule_json),
            after: rule_json(rule),
            event: "reorder_rule.changed",
        },
    )
    .await?;
    if before.is_some() {
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE reorder_rule SET min_qty = ?, max_qty = ?, updated_at = ? \
             WHERE branch_id = ? AND variant_id = ?",
            args![rule.min, rule.max, now, rule.branch, rule.variant],
        )
        .await?;
        Ok(Written::Updated)
    } else {
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO reorder_rule (branch_id, variant_id, min_qty, max_qty, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
            args![rule.branch, rule.variant, rule.min, rule.max, now],
        )
        .await?;
        Ok(Written::Created)
    }
}

/// Removes a rule (the variant then falls back to the velocity suggestion). Whether one
/// existed.
///
/// # Errors
/// The database error.
pub async fn delete_reorder_rule(
    tx: &mut Tx,
    branch: BranchId,
    variant: VariantId,
    actor: &Actor,
    now: Timestamp,
) -> Result<bool> {
    let Some(before) = get_reorder_rule(Conn::Tx(&mut *tx), branch, variant).await? else {
        return Ok(false);
    };
    sql::execute(
        Conn::Tx(&mut *tx),
        "DELETE FROM reorder_rule WHERE branch_id = ? AND variant_id = ?",
        args![branch, variant],
    )
    .await?;
    record(
        tx,
        actor,
        now,
        Change {
            action: "reorder_rule.delete",
            target: format!("reorder_rule:{branch}:{variant}"),
            before: Some(rule_json(&before)),
            after: serde_json::json!({
                "branch_id": branch.to_string(),
                "variant_id": variant.to_string(),
                "deleted": true,
            }),
            event: "reorder_rule.changed",
        },
    )
    .await?;
    Ok(true)
}

/// A line of the low-stock list or a suggested purchase order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuggestedLine {
    /// The variant.
    pub variant: VariantId,
    /// Branch on hand.
    pub on_hand: Qty,
    /// On order.
    pub on_order: Qty,
    /// The rule, when there is one.
    pub rule: Option<(Qty, Qty)>,
    /// The purchase unit (the supplier item's last unit, else the base unit); `None` in the
    /// low-stock list.
    pub unit: Option<UnitId>,
    /// The suggested whole purchase units (0 in the low-stock list).
    pub units: i64,
    /// The same in base units.
    pub base_qty: Qty,
    /// The supplier's last price per purchase unit, for the draft PO.
    pub price: Option<Money>,
}

/// The low-stock list of a branch (§10): variants with a rule whose on hand + on order ≤ min,
/// and every variant with negative on hand.
///
/// # Errors
/// The database error.
pub async fn low_stock<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
) -> Result<Vec<SuggestedLine>> {
    let mut conn = conn.into();
    let mut variants: Vec<VariantId> = sql::fetch_all(
        conn.reborrow(),
        "SELECT variant_id FROM reorder_rule WHERE branch_id = ? \
         UNION SELECT variant_id FROM stock_balance WHERE branch_id = ? AND qty < 0",
        args![branch, branch],
        |row| sql::id(row, "variant_id"),
    )
    .await?;
    variants.sort();
    variants.dedup();
    let mut out = Vec::new();
    for variant in variants {
        let on_hand = branch_on_hand(conn.reborrow(), branch, variant).await?;
        let ordered = on_order(conn.reborrow(), branch, variant).await?;
        let rule = get_reorder_rule(conn.reborrow(), branch, variant)
            .await?
            .map(|r| (r.min, r.max));
        if is_low(on_hand, ordered, rule.map(|r| r.0)) {
            out.push(SuggestedLine {
                variant,
                on_hand,
                on_order: ordered,
                rule,
                unit: None,
                units: 0,
                base_qty: Qty::ZERO,
                price: None,
            });
        }
    }
    Ok(out)
}

/// The base unit and the factor of `unit` (when given and a unit of the variant's product).
async fn purchase_unit(
    mut conn: Conn<'_>,
    variant: VariantId,
    unit: Option<UnitId>,
) -> Result<Option<(UnitId, Qty)>> {
    if let Some(u) = unit {
        let found = sql::fetch_optional(
            conn.reborrow(),
            "SELECT u.id, u.factor FROM product_unit u JOIN variant v ON v.product_id = \
             u.product_id WHERE v.id = ? AND u.id = ?",
            args![variant, u],
            |row| Ok((sql::id(row, "id")?, sql::qty(row, "factor")?)),
        )
        .await?;
        if found.is_some() {
            return Ok(found);
        }
    }
    sql::fetch_optional(
        conn,
        "SELECT u.id, u.factor FROM variant v JOIN product p ON p.id = v.product_id \
         JOIN product_unit u ON u.id = p.base_unit_id WHERE v.id = ?",
        args![variant],
        |row| Ok((sql::id(row, "id")?, sql::qty(row, "factor")?)),
    )
    .await
}

/// The suggested purchase order of `supplier` for `branch` at `now` (§10,
/// `GET /reorder/suggestions`): over its preferred variants (or, when it is preferred for
/// none, every variant it supplied), with a rule max − (on hand + on order) once at or below
/// min, else velocity × (lead + cover days) − (on hand + on order); velocity = (sold − returned)
/// over the last `velocity_days` / `velocity_days`. Rounded up to whole purchase units (the
/// supplier item's last unit, else the base unit). Variants with nothing to order are left out.
///
/// # Errors
/// [`StockDbError::NotFound`] (`supplier`); the database error.
pub async fn suggest_purchase_order<'c>(
    conn: impl Into<Conn<'c>>,
    supplier: SupplierId,
    branch: BranchId,
    now: Timestamp,
) -> StockResult<Vec<SuggestedLine>> {
    let mut conn = conn.into();
    let s = get_supplier(conn.reborrow(), supplier)
        .await?
        .ok_or(StockDbError::NotFound("supplier"))?;
    let settings = get_inventory_settings(conn.reborrow()).await?;
    let items = list_supplier_items(conn.reborrow(), Some(supplier), None).await?;
    let preferred: Vec<_> = items.iter().filter(|i| i.preferred).cloned().collect();
    let items = if preferred.is_empty() {
        items
    } else {
        preferred
    };
    let since = now.saturating_add_millis(-i64::from(settings.velocity_days) * DAY_MS);
    let mut out = Vec::new();
    for item in items {
        let on_hand = branch_on_hand(conn.reborrow(), branch, item.variant).await?;
        let ordered = on_order(conn.reborrow(), branch, item.variant).await?;
        let rule = get_reorder_rule(conn.reborrow(), branch, item.variant)
            .await?
            .map(|r| (r.min, r.max));
        let sold = sql::fetch_optional(
            conn.reborrow(),
            "SELECT CAST(COALESCE(SUM(base_qty), 0) AS SIGNED) AS qty FROM stock_movement \
             WHERE branch_id = ? AND variant_id = ? AND occurred_at >= ? \
             AND kind IN ('sale', 'sale_void', 'return')",
            args![branch, item.variant, since],
            |row| sql::qty(row, "qty"),
        )
        .await?
        .unwrap_or_default();
        let Some((unit, factor)) =
            purchase_unit(conn.reborrow(), item.variant, item.last_unit).await?
        else {
            continue;
        };
        let suggestion = suggest(&ReorderInput {
            on_hand,
            on_order: ordered,
            rule,
            net_sold: net_sold(sold)?,
            velocity_days: settings.velocity_days,
            lead_days: s.lead_days,
            cover_days: settings.cover_days,
            purchase_factor: factor,
        })?;
        if let Some(sg) = suggestion {
            let price = item
                .last_price
                .map(|p| sultan_core::stock::line_value(factor, p))
                .transpose()
                .map_err(|e| DbError::Corrupt(e.to_string()))?;
            out.push(SuggestedLine {
                variant: item.variant,
                on_hand,
                on_order: ordered,
                rule,
                unit: Some(unit),
                units: sg.units,
                base_qty: sg.base_qty,
                price,
            });
        }
    }
    Ok(out)
}
