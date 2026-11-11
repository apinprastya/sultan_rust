//! Customer groups and membership tiers (spec 2.5 §3, §4). Both are company-wide master data,
//! never deleted: archiving keeps the links of the customers that use them.

use serde_json::{Value, json};
use sultan_core::time::Timestamp;

use super::{
    Actor, Change, CustomerDbError, CustomerGroupId, CustomerResult, MembershipTierId, Written,
    clean_name, record, written,
};
use crate::conn::{Conn, Tx};
use crate::error::Result;
use crate::sql::{self, Row, args};

/// A customer group, e.g. Wholesale (spec 2.5 §3). Name unique (byte-wise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerGroup {
    /// ID.
    pub id: CustomerGroupId,
    /// Name, 1–100 characters (trimmed when saved).
    pub name: String,
    /// When it was archived; `None` = active. A `PUT` with `Some` archives it (the stored time
    /// is kept when it already was archived), `None` unarchives it.
    pub archived_at: Option<Timestamp>,
}

/// A membership tier, e.g. Gold (spec 2.5 §4). Name and rank unique.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipTier {
    /// ID.
    pub id: MembershipTierId,
    /// Name, 1–100 characters (trimmed when saved).
    pub name: String,
    /// Higher = better.
    pub rank: i64,
    /// Like [`CustomerGroup::archived_at`].
    pub archived_at: Option<Timestamp>,
}

const GROUP_SELECT: &str = "SELECT id, name, archived_at FROM customer_group";
const TIER_SELECT: &str = "SELECT id, name, tier_rank, archived_at FROM membership_tier";

fn map_group(row: &dyn Row) -> Result<CustomerGroup> {
    Ok(CustomerGroup {
        id: sql::id(row, "id")?,
        name: row.text("name")?,
        archived_at: sql::opt_ts(row, "archived_at")?,
    })
}

fn map_tier(row: &dyn Row) -> Result<MembershipTier> {
    Ok(MembershipTier {
        id: sql::id(row, "id")?,
        name: row.text("name")?,
        rank: row.i64("tier_rank")?,
        archived_at: sql::opt_ts(row, "archived_at")?,
    })
}

fn group_json(g: &CustomerGroup) -> Value {
    json!({
        "id": g.id.to_string(),
        "name": g.name,
        "archived_at": g.archived_at.map(Timestamp::unix_millis),
    })
}

fn tier_json(t: &MembershipTier) -> Value {
    json!({
        "id": t.id.to_string(),
        "name": t.name,
        "rank": t.rank,
        "archived_at": t.archived_at.map(Timestamp::unix_millis),
    })
}

/// One customer group.
///
/// # Errors
/// The database error.
pub async fn get_group<'c>(
    conn: impl Into<Conn<'c>>,
    id: CustomerGroupId,
) -> Result<Option<CustomerGroup>> {
    let sql = format!("{GROUP_SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map_group).await
}

/// Customer groups ordered by name; archived ones only with `include_archived`.
///
/// # Errors
/// The database error.
pub async fn list_groups<'c>(
    conn: impl Into<Conn<'c>>,
    include_archived: bool,
) -> Result<Vec<CustomerGroup>> {
    let filter = if include_archived {
        ""
    } else {
        " WHERE archived_at IS NULL"
    };
    let sql = format!("{GROUP_SELECT}{filter} ORDER BY name, id");
    sql::fetch_all(conn.into(), &sql, args![], map_group).await
}

/// Keeps the stored archive time when the record stays archived.
fn archived_at(
    requested: Option<Timestamp>,
    stored: Option<Timestamp>,
    now: Timestamp,
) -> Option<Timestamp> {
    match (requested, stored) {
        (None, _) => None,
        (Some(_), Some(kept)) => Some(kept),
        (Some(_), None) => Some(now),
    }
}

/// Creates or changes a customer group (`PUT /customer-groups/{id}`): the name is trimmed and
/// must be unique. Audit and outbox `customer_group.put` unless nothing changed.
///
/// # Errors
/// [`CustomerDbError::Invalid`] (name), [`CustomerDbError::Duplicate`] (name) or the database
/// error.
pub async fn put_group(
    tx: &mut Tx,
    group: &CustomerGroup,
    actor: &Actor,
    now: Timestamp,
) -> CustomerResult<Written> {
    let old = get_group(Conn::Tx(&mut *tx), group.id).await?;
    let new = CustomerGroup {
        id: group.id,
        name: clean_name(&group.name)?,
        archived_at: archived_at(
            group.archived_at,
            old.as_ref().and_then(|o| o.archived_at),
            now,
        ),
    };
    if old.as_ref() == Some(&new) {
        return Ok(Written::Unchanged);
    }
    let clash = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT 1 FROM customer_group WHERE name = ? AND id <> ?",
        args![new.name.as_str(), new.id],
        |_| Ok(()),
    )
    .await?;
    if clash.is_some() {
        return Err(CustomerDbError::Duplicate {
            field: "name",
            value: new.name,
        });
    }
    let saved = sql::upsert_by_id(
        tx,
        "customer_group",
        new.id.into(),
        &["name", "archived_at"],
        args![new.name.as_str(), new.archived_at],
        now,
    )
    .await?;
    let after = group_json(&new);
    record(
        tx,
        actor,
        now,
        Change {
            action: "customer_group.put",
            target: format!("customer_group:{}", new.id),
            before: old.as_ref().map(group_json),
            after: Some(after.clone()),
            outbox_kind: "customer_group.put",
            payload: after,
        },
    )
    .await?;
    Ok(written(saved))
}

/// Archives a customer group. Customers keep the link (spec 2.5 §3).
///
/// # Errors
/// [`CustomerDbError::NotFound`] or the database error.
pub async fn archive_group(
    tx: &mut Tx,
    id: CustomerGroupId,
    actor: &Actor,
    now: Timestamp,
) -> CustomerResult<Written> {
    let mut group = get_group(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(CustomerDbError::NotFound("customer_group"))?;
    group.archived_at = Some(now);
    put_group(tx, &group, actor, now).await
}

/// One membership tier.
///
/// # Errors
/// The database error.
pub async fn get_tier<'c>(
    conn: impl Into<Conn<'c>>,
    id: MembershipTierId,
) -> Result<Option<MembershipTier>> {
    let sql = format!("{TIER_SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map_tier).await
}

/// Membership tiers, best (highest rank) first; archived ones only with `include_archived`.
///
/// # Errors
/// The database error.
pub async fn list_tiers<'c>(
    conn: impl Into<Conn<'c>>,
    include_archived: bool,
) -> Result<Vec<MembershipTier>> {
    let filter = if include_archived {
        ""
    } else {
        " WHERE archived_at IS NULL"
    };
    let sql = format!("{TIER_SELECT}{filter} ORDER BY tier_rank DESC");
    sql::fetch_all(conn.into(), &sql, args![], map_tier).await
}

/// Creates or changes a membership tier (`PUT /membership-tiers/{id}`): the name is trimmed;
/// name and rank must be unique (archived tiers included). Audit and outbox
/// `membership_tier.put` unless nothing changed.
///
/// # Errors
/// [`CustomerDbError::Invalid`] (name), [`CustomerDbError::Duplicate`] (name or rank) or the
/// database error.
pub async fn put_tier(
    tx: &mut Tx,
    tier: &MembershipTier,
    actor: &Actor,
    now: Timestamp,
) -> CustomerResult<Written> {
    let old = get_tier(Conn::Tx(&mut *tx), tier.id).await?;
    let new = MembershipTier {
        id: tier.id,
        name: clean_name(&tier.name)?,
        rank: tier.rank,
        archived_at: archived_at(
            tier.archived_at,
            old.as_ref().and_then(|o| o.archived_at),
            now,
        ),
    };
    if old.as_ref() == Some(&new) {
        return Ok(Written::Unchanged);
    }
    let others = sql::fetch_all(
        Conn::Tx(&mut *tx),
        "SELECT name, tier_rank FROM membership_tier WHERE (name = ? OR tier_rank = ?) AND id <> ?",
        args![new.name.as_str(), new.rank, new.id],
        |row| Ok((row.text("name")?, row.i64("tier_rank")?)),
    )
    .await?;
    if others.iter().any(|(name, _)| *name == new.name) {
        return Err(CustomerDbError::Duplicate {
            field: "name",
            value: new.name,
        });
    }
    if !others.is_empty() {
        return Err(CustomerDbError::Duplicate {
            field: "rank",
            value: new.rank.to_string(),
        });
    }
    let saved = sql::upsert_by_id(
        tx,
        "membership_tier",
        new.id.into(),
        &["name", "tier_rank", "archived_at"],
        args![new.name.as_str(), new.rank, new.archived_at],
        now,
    )
    .await?;
    let after = tier_json(&new);
    record(
        tx,
        actor,
        now,
        Change {
            action: "membership_tier.put",
            target: format!("membership_tier:{}", new.id),
            before: old.as_ref().map(tier_json),
            after: Some(after.clone()),
            outbox_kind: "membership_tier.put",
            payload: after,
        },
    )
    .await?;
    Ok(written(saved))
}

/// Archives a membership tier: it stops applying to its customers (spec 2.5 §4).
///
/// # Errors
/// [`CustomerDbError::NotFound`] or the database error.
pub async fn archive_tier(
    tx: &mut Tx,
    id: MembershipTierId,
    actor: &Actor,
    now: Timestamp,
) -> CustomerResult<Written> {
    let mut tier = get_tier(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(CustomerDbError::NotFound("membership_tier"))?;
    tier.archived_at = Some(now);
    put_tier(tx, &tier, actor, now).await
}
