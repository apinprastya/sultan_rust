//! Price-list entries with their quantity tiers (spec 2.3 §3), and the loader that gives a sale
//! its lists for `core::pricing` (§4).

use std::collections::HashMap;

use chrono::NaiveDate;
use serde_json::{Value, json};
use sultan_core::catalog::Qty;
use sultan_core::ids::{BranchId, ProductId, VariantId};
use sultan_core::money::Money;
use sultan_core::pricing::{
    CustomerLists, PriceChain, PriceEntry, PriceEntryId, PriceList, PriceListId, PriceListKind,
    PriceTier, TierMembership, TierPrice, validate_entry,
};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::lists::{DEFAULT_PRICE_LIST, LIST_SELECT, PriceListRecord, get_price_list, map_list};
use super::{
    Actor, Change, PricingDbError, PricingResult, Written, date_value, opt_date, placeholders,
    read_rule, record, rule_parts, tier_json, written,
};
use crate::catalog::mark_used;
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// The most entries [`list_price_entries`] returns per page.
pub const MAX_ENTRY_PAGE: u32 = 1_000;

/// An entry with the list it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceEntryRecord {
    /// The list.
    pub list: PriceListId,
    /// The entry with its tiers, ordered by `min_qty`.
    pub entry: PriceEntry,
}

/// Which entries [`list_price_entries`] returns. Every set field must match.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EntryFilter {
    /// Only entries of this list (`GET /price-lists/{id}/entries`).
    pub list: Option<PriceListId>,
    /// Only entries for this variant (`?variant=…`).
    pub variant: Option<VariantId>,
    /// Only entries for the variants of this product (`GET /products/{id}/prices`).
    pub product: Option<ProductId>,
    /// Only entries with a greater ID (the page cursor).
    pub after: Option<PriceEntryId>,
    /// Page size, 1..=[`MAX_ENTRY_PAGE`]; `0` means [`MAX_ENTRY_PAGE`].
    pub limit: u32,
}

const ENTRY_SELECT: &str = "SELECT e.id, e.list_id, e.variant_id, e.unit_id, e.valid_from, \
     e.valid_to FROM price_entry e";

/// Loads the entries matching `where_sql` (on alias `e`, ordered by ID) with their tiers.
async fn load_entries(
    mut conn: Conn<'_>,
    where_sql: &str,
    values: Vec<Arg>,
    limit: Option<i64>,
) -> Result<Vec<PriceEntryRecord>> {
    let mut sql = format!("{ENTRY_SELECT} WHERE {where_sql} ORDER BY e.id");
    let mut values = values;
    if let Some(limit) = limit {
        sql.push_str(" LIMIT ?");
        values.push(limit.into());
    }
    let mut records = sql::fetch_all(conn.reborrow(), &sql, values, |row| {
        Ok(PriceEntryRecord {
            list: PriceListId::from_uuid(sql::id(row, "list_id")?),
            entry: PriceEntry {
                id: PriceEntryId::from_uuid(sql::id(row, "id")?),
                variant: sql::id(row, "variant_id")?,
                unit: sql::id(row, "unit_id")?,
                valid_from: opt_date(row, "valid_from")?,
                valid_to: opt_date(row, "valid_to")?,
                tiers: Vec::new(),
            },
        })
    })
    .await?;
    if records.is_empty() {
        return Ok(records);
    }
    let mut tiers = load_tiers(conn, &records).await?;
    for r in &mut records {
        r.entry.tiers = tiers.remove(&r.entry.id).unwrap_or_default();
    }
    Ok(records)
}

/// The tiers of `records`' entries, by entry, each ordered by `min_qty`. Loaded in chunks so the
/// statement stays within the backends' placeholder limits.
async fn load_tiers(
    mut conn: Conn<'_>,
    records: &[PriceEntryRecord],
) -> Result<HashMap<PriceEntryId, Vec<PriceTier>>> {
    let mut tiers: HashMap<PriceEntryId, Vec<PriceTier>> = HashMap::new();
    for chunk in records.chunks(500) {
        let sql = format!(
            "SELECT entry_id, min_qty, price, rule_kind, rule_value FROM price_tier \
             WHERE entry_id IN ({}) ORDER BY entry_id, min_qty",
            placeholders(chunk.len())
        );
        let values = chunk.iter().map(|r| Arg::from(r.entry.id)).collect();
        let rows = sql::fetch_all(conn.reborrow(), &sql, values, map_tier).await?;
        for (entry, tier) in rows {
            tiers.entry(entry).or_default().push(tier);
        }
    }
    Ok(tiers)
}

fn map_tier(row: &dyn Row) -> Result<(PriceEntryId, PriceTier)> {
    let price = match (row.opt_i64("price")?, read_rule(row)?) {
        (Some(price), None) => TierPrice::Fixed(Money::from_minor(price)),
        (None, Some(rule)) => TierPrice::Rule(rule),
        _ => return Err(DbError::Corrupt("price tier without one price".into())),
    };
    Ok((
        PriceEntryId::from_uuid(sql::id(row, "entry_id")?),
        PriceTier {
            min_qty: Qty::from_thousandths(row.i64("min_qty")?),
            price,
        },
    ))
}

fn entry_json(list: PriceListId, e: &PriceEntry) -> Value {
    json!({
        "id": e.id.to_string(),
        "list_id": list.to_string(),
        "variant_id": e.variant.to_string(),
        "unit_id": e.unit.to_string(),
        "valid_from": e.valid_from.map(date_value),
        "valid_to": e.valid_to.map(date_value),
        "tiers": e.tiers.iter().map(tier_json).collect::<Vec<_>>(),
    })
}

/// One entry with its tiers.
///
/// # Errors
/// The database error.
pub async fn get_price_entry<'c>(
    conn: impl Into<Conn<'c>>,
    id: PriceEntryId,
) -> Result<Option<PriceEntryRecord>> {
    let found = load_entries(conn.into(), "e.id = ?", args![id], None).await?;
    Ok(found.into_iter().next())
}

/// Entries with their tiers, ordered by ID, one page at a time (`GET /price-lists/{id}/entries`,
/// `GET /products/{id}/prices`). The next page starts after the last entry's ID.
///
/// # Errors
/// The database error.
pub async fn list_price_entries<'c>(
    conn: impl Into<Conn<'c>>,
    filter: EntryFilter,
) -> Result<Vec<PriceEntryRecord>> {
    let mut clauses = vec!["1 = 1"];
    let mut values = args![];
    if let Some(list) = filter.list {
        clauses.push("e.list_id = ?");
        values.push(list.into());
    }
    if let Some(variant) = filter.variant {
        clauses.push("e.variant_id = ?");
        values.push(variant.into());
    }
    if let Some(product) = filter.product {
        clauses.push("e.variant_id IN (SELECT id FROM variant WHERE product_id = ?)");
        values.push(product.into());
    }
    if let Some(after) = filter.after {
        clauses.push("e.id > ?");
        values.push(after.into());
    }
    let limit = if filter.limit == 0 {
        MAX_ENTRY_PAGE
    } else {
        filter.limit.min(MAX_ENTRY_PAGE)
    };
    load_entries(
        conn.into(),
        &clauses.join(" AND "),
        values,
        Some(i64::from(limit)),
    )
    .await
}

/// Checks the variant exists and the unit is one of its product's units.
async fn check_item(
    tx: &mut Tx,
    variant: VariantId,
    unit: sultan_core::ids::UnitId,
) -> PricingResult<()> {
    let product: Option<Uuid> = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT product_id FROM variant WHERE id = ?",
        args![variant],
        |row| sql::id(row, "product_id"),
    )
    .await?;
    let product = product.ok_or(PricingDbError::InvalidReference("variant"))?;
    sql::fetch_optional(
        Conn::Tx(tx),
        "SELECT 1 FROM product_unit WHERE id = ? AND product_id = ?",
        args![unit, product],
        |_| Ok(()),
    )
    .await?
    .ok_or(PricingDbError::InvalidReference("unit"))
}

/// Creates or changes an entry of `list` with its tiers as one document
/// (`PUT /price-lists/{id}/entries/{entry_id}`). The tiers are stored ordered by `min_qty`. The
/// entry is validated with [`validate_entry`] against the list's other entries for the same
/// variant + unit (`price_overlap`). Marks the variant used (spec 2.2 §2). Audit and outbox
/// `price_entry.put` (old → new prices) unless nothing changed.
///
/// # Errors
/// - [`PricingDbError::NotFound`] (`price_list`).
/// - [`PricingDbError::Invalid`] (`list_id`): the entry ID belongs to another list.
/// - [`PricingDbError::InvalidReference`]: unknown `variant`, or a `unit` not of its product.
/// - [`PricingDbError::Rule`]: `invalid_field`, `invalid_rule`, `price_overlap`.
/// - The database error.
pub async fn put_price_entry(
    tx: &mut Tx,
    list: PriceListId,
    entry: &PriceEntry,
    actor: &Actor,
    now: Timestamp,
) -> PricingResult<Written> {
    let stored_list = get_price_list(Conn::Tx(&mut *tx), list)
        .await?
        .ok_or(PricingDbError::NotFound("price_list"))?;
    let old = get_price_entry(Conn::Tx(&mut *tx), entry.id).await?;
    if old.as_ref().is_some_and(|o| o.list != list) {
        return Err(PricingDbError::invalid(
            "list_id",
            "the entry belongs to another list",
        ));
    }
    let mut new = entry.clone();
    new.tiers.sort_by_key(|t| t.min_qty);
    let others = load_entries(
        Conn::Tx(&mut *tx),
        "e.list_id = ? AND e.variant_id = ? AND e.unit_id = ? AND e.id <> ?",
        args![list, new.variant, new.unit, new.id],
        None,
    )
    .await?;
    let others: Vec<PriceEntry> = others.into_iter().map(|r| r.entry).collect();
    validate_entry(stored_list.kind, &new, &others)?;
    if old.as_ref().is_some_and(|o| o.entry == new) {
        return Ok(Written::Unchanged);
    }
    check_item(tx, new.variant, new.unit).await?;
    let saved = sql::upsert_by_id(
        tx,
        "price_entry",
        new.id.into(),
        &["list_id", "variant_id", "unit_id", "valid_from", "valid_to"],
        args![
            list,
            new.variant,
            new.unit,
            new.valid_from.map(date_value),
            new.valid_to.map(date_value),
        ],
        now,
    )
    .await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "DELETE FROM price_tier WHERE entry_id = ?",
        args![new.id],
    )
    .await?;
    for tier in &new.tiers {
        let (price, rule) = match tier.price {
            TierPrice::Fixed(price) => (Some(price.minor()), None),
            TierPrice::Rule(rule) => (None, Some(rule)),
        };
        let (rule_kind, rule_value) = rule_parts(rule);
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO price_tier (entry_id, min_qty, price, rule_kind, rule_value) \
             VALUES (?, ?, ?, ?, ?)",
            args![
                new.id,
                tier.min_qty.thousandths(),
                price,
                rule_kind,
                rule_value
            ],
        )
        .await?;
    }
    mark_used(Conn::Tx(&mut *tx), &[new.variant], now).await?;
    let after = entry_json(list, &new);
    record(
        tx,
        actor,
        now,
        Change {
            action: "price_entry.put",
            target: format!("price_entry:{}", new.id),
            before: old.as_ref().map(|o| entry_json(o.list, &o.entry)),
            after: Some(after.clone()),
            payload: after,
        },
    )
    .await?;
    Ok(written(saved))
}

/// Deletes an entry of `list` with its tiers (`DELETE /price-lists/{id}/entries/{entry_id}`).
/// Audit and outbox `price_entry.delete`. The variant stays used.
///
/// Spec 2.3 §10 allows this only for an entry never used by a posted sale (otherwise it is ended
/// with `valid_to`). There are no sales yet (2.6), so every entry may be deleted.
///
/// # Errors
/// [`PricingDbError::NotFound`] (`price_entry`, also when it belongs to another list) or the
/// database error.
pub async fn delete_price_entry(
    tx: &mut Tx,
    list: PriceListId,
    id: PriceEntryId,
    actor: &Actor,
    now: Timestamp,
) -> PricingResult<()> {
    let old = get_price_entry(Conn::Tx(&mut *tx), id)
        .await?
        .filter(|o| o.list == list)
        .ok_or(PricingDbError::NotFound("price_entry"))?;
    // Hook for 2.6: refuse (`409 in_use`) an entry that a posted sale used, once sale lines
    // record their price source.
    sql::execute(
        Conn::Tx(&mut *tx),
        "DELETE FROM price_tier WHERE entry_id = ?",
        args![id],
    )
    .await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "DELETE FROM price_entry WHERE id = ?",
        args![id],
    )
    .await?;
    let before = entry_json(list, &old.entry);
    record(
        tx,
        actor,
        now,
        Change {
            action: "price_entry.delete",
            target: format!("price_entry:{id}"),
            before: Some(before.clone()),
            after: None,
            payload: before,
        },
    )
    .await?;
    Ok(())
}

/// Whose lists a sale is priced with (spec 2.3 §4.1). Absent targets are skipped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PriceTargets {
    /// The selling branch.
    pub branch: Option<BranchId>,
    /// The customer (contract list).
    pub customer: Option<Uuid>,
    /// The customer's group.
    pub group: Option<Uuid>,
    /// The customer's membership tier and whether it is archived / until when it holds; the
    /// chain uses the tier list only when the membership applies on the sale date.
    pub tier: Option<(Uuid, TierMembership)>,
}

/// The live lists a sale needs, each with only the entries for the requested variants that are
/// valid on the sale date (see [`load_for_resolution`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionLists {
    /// The `default` list.
    pub default: PriceList,
    /// The branch's list.
    pub branch: Option<PriceList>,
    /// The customer's contract list.
    pub contract: Option<PriceList>,
    /// The tier's list with the customer's membership.
    pub tier: Option<(PriceList, TierMembership)>,
    /// The group's list.
    pub group: Option<PriceList>,
}

impl ResolutionLists {
    /// The full chain contract → tier → group → branch → default for a sale on `date`, for
    /// [`sultan_core::pricing::resolve_cart`].
    #[must_use]
    pub fn chain(&self, date: NaiveDate) -> PriceChain<'_> {
        let customer = CustomerLists {
            contract: self.contract.as_ref(),
            tier: self.tier.as_ref().map(|(list, m)| (list, *m)),
            group: self.group.as_ref(),
        };
        PriceChain::build(Some(&customer), self.branch.as_ref(), &self.default, date)
    }

    /// The store chain branch → default (price-embedded labels, the lookup price).
    #[must_use]
    pub fn store_chain(&self, date: NaiveDate) -> PriceChain<'_> {
        PriceChain::store(self.branch.as_ref(), &self.default, date)
    }
}

/// The live (active, not archived) list of `kind` for `target`, if any.
async fn live_list(
    conn: Conn<'_>,
    kind: PriceListKind,
    target: Option<Uuid>,
) -> Result<Option<PriceListRecord>> {
    let sql = format!(
        "{LIST_SELECT} WHERE kind = ? AND target_id = ? AND active = ? AND archived_at IS NULL"
    );
    sql::fetch_optional(conn, &sql, args![kind.as_str(), target, true], map_list).await
}

/// Loads what a sale at a branch on `date` (the branch-local day) needs to be priced: the
/// `default` list and the live branch, contract, tier and group lists of `targets`, each with its
/// entries for `variants` that are valid on `date`. Every unit of a variant is loaded, so unit
/// fallback (spec 2.3 §4.4) works. Build the chain with [`ResolutionLists::chain`].
///
/// # Errors
/// [`DbError::Corrupt`] when the `default` list is missing, or the database error.
pub async fn load_for_resolution<'c>(
    conn: impl Into<Conn<'c>>,
    targets: &PriceTargets,
    variants: &[VariantId],
    date: NaiveDate,
) -> Result<ResolutionLists> {
    let mut conn = conn.into();
    let default = get_price_list(conn.reborrow(), DEFAULT_PRICE_LIST)
        .await?
        .ok_or_else(|| DbError::Corrupt("the default price list is missing".into()))?;
    let mut lists = vec![default];
    let wanted = [
        (PriceListKind::Branch, targets.branch.map(Uuid::from)),
        (PriceListKind::Customer, targets.customer),
        (PriceListKind::Tier, targets.tier.map(|(t, _)| t)),
        (PriceListKind::Group, targets.group),
    ];
    for (kind, target) in wanted {
        if target.is_some()
            && let Some(list) = live_list(conn.reborrow(), kind, target).await?
        {
            lists.push(list);
        }
    }
    let mut entries: HashMap<PriceListId, Vec<PriceEntry>> = HashMap::new();
    if !variants.is_empty() {
        let mut values: Vec<Arg> = lists.iter().map(|l| Arg::from(l.id)).collect();
        values.extend(variants.iter().map(|v| Arg::from(*v)));
        let day = date_value(date);
        values.extend([Arg::from(day), Arg::from(day)]);
        let where_sql = format!(
            "e.list_id IN ({}) AND e.variant_id IN ({}) \
             AND (e.valid_from IS NULL OR e.valid_from <= ?) \
             AND (e.valid_to IS NULL OR e.valid_to >= ?)",
            placeholders(lists.len()),
            placeholders(variants.len())
        );
        for r in load_entries(conn, &where_sql, values, None).await? {
            entries.entry(r.list).or_default().push(r.entry);
        }
    }
    let mut take = |kind: PriceListKind| {
        lists
            .iter()
            .find(|l| l.kind == kind)
            .map(|l| l.to_list(entries.remove(&l.id).unwrap_or_default()))
    };
    let default = take(PriceListKind::Default)
        .ok_or_else(|| DbError::Corrupt("the default price list is missing".into()))?;
    Ok(ResolutionLists {
        default,
        branch: take(PriceListKind::Branch),
        contract: take(PriceListKind::Customer),
        tier: take(PriceListKind::Tier).zip(targets.tier.map(|(_, m)| m)),
        group: take(PriceListKind::Group),
    })
}
