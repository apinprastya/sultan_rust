//! Price lists (spec 2.3 §2): one seeded `default` list, and at most one live list per branch,
//! customer group, membership tier or customer. Lists are never deleted: archiving or switching
//! one off removes it from resolution at once.

use serde_json::{Value, json};
use sultan_core::pricing::{PriceEntry, PriceList, PriceListId, PriceListKind, Rule};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::{
    Actor, Change, PricingDbError, PricingResult, Written, clean_name, read_rule, record,
    rule_json, rule_parts, written,
};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

/// The seeded `default` list (fixed ID on every node).
pub const DEFAULT_PRICE_LIST: PriceListId =
    PriceListId::from_uuid(Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_0201));

/// A price list as stored, without its entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceListRecord {
    /// ID.
    pub id: PriceListId,
    /// Its slot in the chain. Fixed once created.
    pub kind: PriceListKind,
    /// The branch, customer group, membership tier or customer; `None` for the `default` list.
    /// Fixed once created.
    pub target: Option<Uuid>,
    /// Name, 1–100 characters (trimmed when saved).
    pub name: String,
    /// Switched on. The `default` list is always on.
    pub active: bool,
    /// The list rule for items without an entry (never on the `default` list).
    pub rule: Option<Rule>,
    /// When it was archived; `None` = not archived. A `PUT` with `Some` archives it (the stored
    /// time is kept when it already was archived), `None` unarchives it.
    pub archived_at: Option<Timestamp>,
}

impl PriceListRecord {
    /// The `core` list with `entries`, for validation and resolution.
    #[must_use]
    pub fn to_list(&self, entries: Vec<PriceEntry>) -> PriceList {
        PriceList {
            id: self.id,
            kind: self.kind,
            active: self.active,
            archived: self.archived_at.is_some(),
            rule: self.rule,
            entries,
        }
    }

    /// Active and not archived.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.active && self.archived_at.is_none()
    }
}

/// Which lists [`list_price_lists`] returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListFilter {
    /// Only lists of this kind.
    pub kind: Option<PriceListKind>,
    /// Only lists for this target.
    pub target: Option<Uuid>,
    /// Include archived lists.
    pub include_archived: bool,
}

pub(super) const LIST_SELECT: &str = "SELECT id, kind, target_id, name, active, rule_kind, \
     rule_value, archived_at FROM price_list";

pub(super) fn map_list(row: &dyn Row) -> Result<PriceListRecord> {
    let kind = row.text("kind")?;
    Ok(PriceListRecord {
        id: PriceListId::from_uuid(sql::id(row, "id")?),
        kind: kind
            .parse()
            .map_err(|_| DbError::Corrupt(format!("price list kind `{kind}`")))?,
        target: sql::opt_id(row, "target_id")?,
        name: row.text("name")?,
        active: row.bool("active")?,
        rule: read_rule(row)?,
        archived_at: sql::opt_ts(row, "archived_at")?,
    })
}

fn list_json(l: &PriceListRecord) -> Value {
    json!({
        "id": l.id.to_string(),
        "kind": l.kind.as_str(),
        "target_id": l.target.map(|t| t.to_string()),
        "name": l.name,
        "active": l.active,
        "rule": rule_json(l.rule),
        "archived_at": l.archived_at.map(Timestamp::unix_millis),
    })
}

/// One price list.
///
/// # Errors
/// The database error.
pub async fn get_price_list<'c>(
    conn: impl Into<Conn<'c>>,
    id: PriceListId,
) -> Result<Option<PriceListRecord>> {
    let sql = format!("{LIST_SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map_list).await
}

/// Price lists ordered by kind and name (`GET /price-lists`).
///
/// # Errors
/// The database error.
pub async fn list_price_lists<'c>(
    conn: impl Into<Conn<'c>>,
    filter: ListFilter,
) -> Result<Vec<PriceListRecord>> {
    let mut sql = format!("{LIST_SELECT} WHERE 1 = 1");
    let mut values = args![];
    if let Some(kind) = filter.kind {
        sql.push_str(" AND kind = ?");
        values.push(kind.as_str().into());
    }
    if let Some(target) = filter.target {
        sql.push_str(" AND target_id = ?");
        values.push(target.into());
    }
    if !filter.include_archived {
        sql.push_str(" AND archived_at IS NULL");
    }
    sql.push_str(" ORDER BY kind, name, id");
    sql::fetch_all(conn.into(), &sql, values, map_list).await
}

/// The table a target of `kind` lives in.
const fn target_table(kind: PriceListKind) -> Option<&'static str> {
    match kind {
        PriceListKind::Default => None,
        PriceListKind::Branch => Some("branch"),
        PriceListKind::Group => Some("customer_group"),
        PriceListKind::Tier => Some("membership_tier"),
        PriceListKind::Customer => Some("customer"),
    }
}

/// Checks the target fits the kind: none for `default`, an existing row otherwise.
async fn check_target(tx: &mut Tx, kind: PriceListKind, target: Option<Uuid>) -> PricingResult<()> {
    match (target_table(kind), target) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(PricingDbError::invalid(
            "target_id",
            "the default list has no target",
        )),
        (Some(_), None) => Err(PricingDbError::invalid(
            "target_id",
            "required for this kind of list",
        )),
        (Some(table), Some(id)) => {
            let found = sql::fetch_optional(
                Conn::Tx(tx),
                &format!("SELECT 1 FROM {table} WHERE id = ?"),
                args![id],
                |_| Ok(()),
            )
            .await?;
            found.ok_or(PricingDbError::InvalidReference("target"))
        }
    }
}

/// Creates or changes a price list (`PUT /price-lists/{id}`). The name is trimmed; the kind and
/// target are fixed once created; the list is validated with `core` (no rule on the `default`
/// list, which can be neither switched off nor archived). A live list must be the only live one
/// for its target. Audit and outbox `price_list.put` unless nothing changed.
///
/// # Errors
/// - [`PricingDbError::Invalid`]: name; a changed kind or target; a new `default` list; a target
///   on the `default` list or none on another.
/// - [`PricingDbError::InvalidReference`] (`target`): the branch, group, tier or customer does
///   not exist.
/// - [`PricingDbError::Rule`]: `invalid_rule` or `invalid_field` (`active`) from `core`.
/// - [`PricingDbError::ListConflict`], or the database error.
pub async fn put_price_list(
    tx: &mut Tx,
    list: &PriceListRecord,
    actor: &Actor,
    now: Timestamp,
) -> PricingResult<Written> {
    let old = get_price_list(Conn::Tx(&mut *tx), list.id).await?;
    match &old {
        Some(o) if o.kind != list.kind => {
            return Err(PricingDbError::invalid("kind", "cannot be changed"));
        }
        Some(o) if o.target != list.target => {
            return Err(PricingDbError::invalid("target_id", "cannot be changed"));
        }
        None if list.kind == PriceListKind::Default => {
            return Err(PricingDbError::invalid(
                "kind",
                "there is only one default list",
            ));
        }
        _ => {}
    }
    let new = PriceListRecord {
        id: list.id,
        kind: list.kind,
        target: list.target,
        name: clean_name(&list.name)?,
        active: list.active,
        rule: list.rule,
        archived_at: match (list.archived_at, old.as_ref().and_then(|o| o.archived_at)) {
            (None, _) => None,
            (Some(_), Some(kept)) => Some(kept),
            (Some(_), None) => Some(now),
        },
    };
    new.to_list(Vec::new()).validate()?;
    if old.as_ref() == Some(&new) {
        return Ok(Written::Unchanged);
    }
    check_target(tx, new.kind, new.target).await?;
    if new.is_live() && new.kind != PriceListKind::Default {
        let existing = sql::fetch_optional(
            Conn::Tx(&mut *tx),
            "SELECT id FROM price_list WHERE kind = ? AND target_id = ? AND active = ? \
             AND archived_at IS NULL AND id <> ?",
            args![new.kind.as_str(), new.target, true, new.id],
            |row| sql::id(row, "id").map(PriceListId::from_uuid),
        )
        .await?;
        if let Some(existing) = existing {
            return Err(PricingDbError::ListConflict { existing });
        }
    }
    let (rule_kind, rule_value) = rule_parts(new.rule);
    let saved = sql::upsert_by_id(
        tx,
        "price_list",
        new.id.into(),
        &[
            "kind",
            "target_id",
            "name",
            "active",
            "rule_kind",
            "rule_value",
            "archived_at",
        ],
        args![
            new.kind.as_str(),
            new.target,
            new.name.as_str(),
            new.active,
            rule_kind,
            rule_value,
            new.archived_at,
        ],
        now,
    )
    .await?;
    let after = list_json(&new);
    record(
        tx,
        actor,
        now,
        Change {
            action: "price_list.put",
            target: format!("price_list:{}", new.id),
            before: old.as_ref().map(list_json),
            after: Some(after.clone()),
            payload: after,
        },
    )
    .await?;
    Ok(written(saved))
}

/// Archives a price list (`POST /price-lists/{id}/archive`). The `default` list cannot be
/// archived (`invalid_field`).
///
/// # Errors
/// [`PricingDbError::NotFound`], [`PricingDbError::Rule`] or the database error.
pub async fn archive_price_list(
    tx: &mut Tx,
    id: PriceListId,
    actor: &Actor,
    now: Timestamp,
) -> PricingResult<Written> {
    let mut list = get_price_list(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(PricingDbError::NotFound("price_list"))?;
    list.archived_at = Some(now);
    put_price_list(tx, &list, actor, now).await
}
