//! Opname (spec 2.8 §11, I9, I10): `counting → posted`, or `counting → cancelled`.
//!
//! [`start_opname`] takes the snapshot of on hand per variant in scope (a full count: every
//! stock variant plus anything with stock at the location; a cycle count: chosen variants and
//! categories) and a ledger mark, and locks (location, variant) so only one opname counts a
//! variant at a location (`409 opname_open`). Counters append entries ([`add_counts`]: a scan
//! adds, a typed quantity replaces; each entry is stored once by its ID). The variance is
//! counted − (snapshot + net movements since the mark, other than this opname's); selling goes
//! on meanwhile. [`post_opname`] writes the `count` movements (valued like `adjust`; a shortage
//! FEFO, an excess without a lot and, without a cost yet, without a value) and frees the locks.

use std::collections::HashMap;

use sultan_core::catalog::Qty;
use sultan_core::ids::{BranchId, CategoryId, LocationId, OpnameId, UnitId, UserId, VariantId};
use sultan_core::money::Money;
use sultan_core::stock::{
    CountEntry, DocKind, MovementKind, OpnameLine, Uncounted, check_note, counted, opname_moves,
    to_base, variance,
};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::json::{doc_actor, opname_json, record_doc};
use super::ledger::{
    DocRef, LotRule, Move, Valuation, location_on_hand, movements_since, post_moves, total_value,
};
use super::numbers::allocate;
use super::{StockDbError, StockResult, Written, doc_location, variant_info};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// What an opname counts (§11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpnameScope {
    /// Every stock product of the branch.
    Full,
    /// Chosen variants and / or the stock variants of chosen categories.
    Cycle {
        /// Variants.
        variants: Vec<VariantId>,
        /// Categories (their own products).
        categories: Vec<CategoryId>,
    },
}

/// An opname to start (`PUT /opnames/{id}`, `stock.count`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOpname {
    /// The client's `UUIDv7`.
    pub id: OpnameId,
    /// The branch.
    pub branch: BranchId,
    /// The location; `None` = the receiving default.
    pub location: Option<LocationId>,
    /// Full or cycle.
    pub scope: OpnameScope,
    /// A note.
    pub note: Option<String>,
    /// Who starts it.
    pub user: UserId,
}

/// One count entry (`POST /opnames/{id}/counts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountInput {
    /// The client's `UUIDv7` of the entry (a replayed entry is stored once).
    pub id: Uuid,
    /// The variant.
    pub variant: VariantId,
    /// Any unit of its product.
    pub unit: UnitId,
    /// The quantity in the unit (a scan adds it; a typed quantity replaces the line).
    pub qty: Qty,
    /// The entry replaces what was counted so far.
    pub replace: bool,
}

/// One line of an opname as the count screen shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpnameLineView {
    /// The variant.
    pub variant: VariantId,
    /// On hand at the start.
    pub snapshot: Qty,
    /// Net movements since the start (other than this opname's); 0 once posted.
    pub since: Qty,
    /// The counted quantity, `None` = not counted.
    pub counted: Option<Qty>,
    /// counted − (snapshot + since), for counted lines (the posted variance once posted).
    pub variance: Option<Qty>,
    /// The posted `count` movement's value.
    pub value: Option<Money>,
}

/// A stored opname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opname {
    /// The ID.
    pub id: OpnameId,
    /// `JKT01-OP-261126-01`.
    pub number: String,
    /// The branch.
    pub branch: BranchId,
    /// The location.
    pub location: LocationId,
    /// A full count (else cycle).
    pub full: bool,
    /// `counting`, `posted` or `cancelled`.
    pub status: String,
    /// The choice for uncounted variants when posted.
    pub uncounted: Option<Uncounted>,
    /// The note.
    pub note: Option<String>,
    /// Who started it.
    pub user: UserId,
    /// When it started (the snapshot).
    pub started_at: Timestamp,
    /// When it was posted.
    pub posted_at: Option<Timestamp>,
    /// The lines, by variant (empty in lists).
    pub lines: Vec<OpnameLineView>,
}

struct Head {
    opname: Opname,
    mark: Uuid,
}

const SELECT: &str = "SELECT id, number, branch_id, location_id, scope, status, snapshot_mark, \
                      uncounted, note, user_id, started_at, posted_at FROM opname";

fn map(row: &dyn Row) -> Result<Head> {
    let uncounted = row.opt_text("uncounted")?;
    Ok(Head {
        mark: sql::id(row, "snapshot_mark")?,
        opname: Opname {
            id: sql::id(row, "id")?,
            number: row.text("number")?,
            branch: sql::id(row, "branch_id")?,
            location: sql::id(row, "location_id")?,
            full: row.text("scope")? == "full",
            status: row.text("status")?,
            uncounted: uncounted.as_deref().and_then(Uncounted::from_key),
            note: row.opt_text("note")?,
            user: sql::id(row, "user_id")?,
            started_at: sql::ts(row, "started_at")?,
            posted_at: sql::opt_ts(row, "posted_at")?,
            lines: Vec::new(),
        },
    })
}

async fn head(conn: Conn<'_>, id: OpnameId) -> Result<Option<Head>> {
    let sql = format!("{SELECT} WHERE id = ?");
    sql::fetch_optional(conn, &sql, args![id], map).await
}

/// The lines with counts, movements since and variances.
async fn lines(mut conn: Conn<'_>, head: &Head) -> Result<Vec<OpnameLineView>> {
    let op = &head.opname;
    let stored = sql::fetch_all(
        conn.reborrow(),
        "SELECT variant_id, snapshot_qty, counted_qty, variance, value FROM opname_line \
         WHERE opname_id = ? ORDER BY variant_id",
        args![op.id],
        |row| {
            Ok((
                sql::id::<VariantId>(row, "variant_id")?,
                sql::qty(row, "snapshot_qty")?,
                sql::opt_qty(row, "counted_qty")?,
                sql::opt_qty(row, "variance")?,
                sql::opt_money(row, "value")?,
            ))
        },
    )
    .await?;
    if op.status != "counting" {
        return Ok(stored
            .into_iter()
            .map(
                |(variant, snapshot, counted, variance, value)| OpnameLineView {
                    variant,
                    snapshot,
                    since: Qty::ZERO,
                    counted,
                    variance,
                    value,
                },
            )
            .collect());
    }
    let entries = sql::fetch_all(
        conn.reborrow(),
        "SELECT variant_id, base_qty, replaces FROM opname_count WHERE opname_id = ? ORDER BY id",
        args![op.id],
        |row| {
            Ok((
                sql::id::<VariantId>(row, "variant_id")?,
                CountEntry {
                    base_qty: sql::qty(row, "base_qty")?,
                    replace: row.bool("replaces")?,
                },
            ))
        },
    )
    .await?;
    let mut by_variant: HashMap<VariantId, Vec<CountEntry>> = HashMap::new();
    for (v, e) in entries {
        by_variant.entry(v).or_default().push(e);
    }
    let since: HashMap<VariantId, Qty> = sql::fetch_all(
        conn,
        "SELECT variant_id, CAST(SUM(base_qty) AS SIGNED) AS qty FROM stock_movement \
         WHERE branch_id = ? AND location_id = ? AND id > ? AND doc_id <> ? GROUP BY variant_id",
        args![op.branch, op.location, head.mark, op.id],
        |row| Ok((sql::id(row, "variant_id")?, sql::qty(row, "qty")?)),
    )
    .await?
    .into_iter()
    .collect();
    let mut out = Vec::with_capacity(stored.len());
    for (variant, snapshot, _, _, _) in stored {
        let moved = since.get(&variant).copied().unwrap_or_default();
        let total = counted(by_variant.get(&variant).map_or(&[][..], Vec::as_slice))
            .map_err(|e| DbError::Corrupt(e.to_string()))?;
        let diff = total
            .map(|c| variance(c, snapshot, moved))
            .transpose()
            .map_err(|e| DbError::Corrupt(e.to_string()))?;
        out.push(OpnameLineView {
            variant,
            snapshot,
            since: moved,
            counted: total,
            variance: diff,
            value: None,
        });
    }
    Ok(out)
}

/// One opname with its lines: snapshot, movements since, counted and the live variance while
/// counting (§11); the posted figures once posted.
///
/// # Errors
/// The database error.
pub async fn get_opname<'c>(conn: impl Into<Conn<'c>>, id: OpnameId) -> Result<Option<Opname>> {
    let mut conn = conn.into();
    let Some(h) = head(conn.reborrow(), id).await? else {
        return Ok(None);
    };
    let lines = lines(conn, &h).await?;
    Ok(Some(Opname { lines, ..h.opname }))
}

/// The opnames of a branch, newest first, without their lines (at most 500).
///
/// # Errors
/// The database error.
pub async fn list_opnames<'c>(conn: impl Into<Conn<'c>>, branch: BranchId) -> Result<Vec<Opname>> {
    let sql = format!("{SELECT} WHERE branch_id = ? ORDER BY id DESC LIMIT 500");
    Ok(sql::fetch_all(conn.into(), &sql, args![branch], map)
        .await?
        .into_iter()
        .map(|h| h.opname)
        .collect())
}

/// The open opname holding (location, variant), by number.
async fn holder(
    tx: &mut Tx,
    location: LocationId,
    variants: &[VariantId],
) -> Result<Option<String>> {
    for chunk in variants.chunks(500) {
        let marks = vec!["?"; chunk.len()].join(", ");
        let mut a: Vec<Arg> = args![location];
        a.extend(chunk.iter().map(|v| Arg::from(*v)));
        let found = sql::fetch_optional(
            Conn::Tx(&mut *tx),
            &format!(
                "SELECT o.number FROM opname_lock l JOIN opname o ON o.id = l.opname_id \
                 WHERE l.location_id = ? AND l.variant_id IN ({marks}) LIMIT 1"
            ),
            a,
            |row| row.text("number"),
        )
        .await?;
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

async fn scope_variants(
    tx: &mut Tx,
    branch: BranchId,
    location: LocationId,
    scope: &OpnameScope,
) -> StockResult<Vec<VariantId>> {
    let stock = "p.type IN ('stock', 'weighed')";
    let mut out: Vec<VariantId> = match scope {
        OpnameScope::Full => {
            let mut v: Vec<VariantId> = sql::fetch_all(
                Conn::Tx(&mut *tx),
                &format!(
                    "SELECT v.id FROM variant v JOIN product p ON p.id = v.product_id \
                     WHERE {stock} AND v.archived = 0 AND p.archived_at IS NULL"
                ),
                args![],
                |row| sql::id(row, "id"),
            )
            .await?;
            v.extend(
                sql::fetch_all(
                    Conn::Tx(&mut *tx),
                    "SELECT variant_id FROM stock_balance WHERE branch_id = ? \
                     AND location_id = ? AND qty <> 0",
                    args![branch, location],
                    |row| sql::id::<VariantId>(row, "variant_id"),
                )
                .await?,
            );
            v
        }
        OpnameScope::Cycle {
            variants,
            categories,
        } => {
            if variants.is_empty() && categories.is_empty() {
                return Err(StockDbError::invalid(
                    "scope",
                    "a cycle count names variants or categories",
                ));
            }
            let mut v = Vec::new();
            for variant in variants {
                let found = sql::fetch_optional(
                    Conn::Tx(&mut *tx),
                    &format!(
                        "SELECT v.id FROM variant v JOIN product p ON p.id = v.product_id \
                         WHERE v.id = ? AND {stock}"
                    ),
                    args![*variant],
                    |row| sql::id::<VariantId>(row, "id"),
                )
                .await?;
                v.push(found.ok_or_else(|| {
                    StockDbError::invalid("scope", "names a variant without stock of its own")
                })?);
            }
            for category in categories {
                v.extend(
                    sql::fetch_all(
                        Conn::Tx(&mut *tx),
                        &format!(
                            "SELECT v.id FROM variant v JOIN product p ON p.id = v.product_id \
                             WHERE p.category_id = ? AND {stock} AND v.archived = 0 \
                             AND p.archived_at IS NULL"
                        ),
                        args![*category],
                        |row| sql::id::<VariantId>(row, "id"),
                    )
                    .await?,
                );
            }
            v
        }
    };
    out.sort();
    out.dedup();
    Ok(out)
}

async fn add_line(tx: &mut Tx, o: &Opname, variant: VariantId, snapshot: Qty) -> StockResult<()> {
    if let Some(number) = holder(tx, o.location, &[variant]).await? {
        return Err(StockDbError::OpnameOpen(number));
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO opname_line (opname_id, variant_id, snapshot_qty) VALUES (?, ?, ?)",
        args![o.id, variant, snapshot],
    )
    .await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO opname_lock (location_id, variant_id, opname_id) VALUES (?, ?, ?)",
        args![o.location, variant, o.id],
    )
    .await?;
    Ok(())
}

/// Starts an opname (`PUT /opnames/{id}`, `stock.count`): `-OP-`, the ledger mark, the
/// snapshot of on hand per variant in scope, and the locks. The same ID again returns the
/// stored opname unchanged.
///
/// # Errors
/// [`StockDbError::OpnameOpen`] naming the open opname that counts one of the variants at this
/// location; [`StockDbError::Invalid`] (`scope`); [`StockDbError::NotFound`] (`location`).
pub async fn start_opname(
    tx: &mut Tx,
    n: &NewOpname,
    at: Timestamp,
) -> StockResult<(Written, Opname)> {
    if let Some(stored) = get_opname(Conn::Tx(&mut *tx), n.id).await? {
        return Ok((Written::Unchanged, stored));
    }
    let location = doc_location(tx, n.branch, n.location).await?;
    let note = check_note(n.note.as_deref())?;
    let variants = scope_variants(tx, n.branch, location, &n.scope).await?;
    if let Some(number) = holder(tx, location, &variants).await? {
        return Err(StockDbError::OpnameOpen(number));
    }
    let mark = Uuid::now_v7();
    let balances: HashMap<VariantId, Qty> = sql::fetch_all(
        Conn::Tx(&mut *tx),
        "SELECT variant_id, qty FROM stock_balance WHERE branch_id = ? AND location_id = ?",
        args![n.branch, location],
        |row| Ok((sql::id(row, "variant_id")?, sql::qty(row, "qty")?)),
    )
    .await?
    .into_iter()
    .collect();
    let num = allocate(tx, DocKind::Opname, n.branch, at).await?;
    let full = matches!(n.scope, OpnameScope::Full);
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO opname (id, number, branch_id, location_id, scope, status, snapshot_mark, \
         note, local_day, seq, user_id, started_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, 'counting', ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            n.id,
            num.number.clone(),
            n.branch,
            location,
            if full { "full" } else { "cycle" },
            mark,
            note.clone(),
            num.day,
            num.seq,
            n.user,
            at,
            at,
            at,
        ],
    )
    .await?;
    if let OpnameScope::Cycle {
        variants: vs,
        categories,
    } = &n.scope
    {
        let refs = vs
            .iter()
            .map(|v| ("variant", *v.as_uuid()))
            .chain(categories.iter().map(|c| ("category", *c.as_uuid())));
        for (kind, id) in refs {
            sql::execute(
                Conn::Tx(&mut *tx),
                "INSERT INTO opname_scope (opname_id, kind, ref_id) VALUES (?, ?, ?) ",
                args![n.id, kind, id],
            )
            .await?;
        }
    }
    let o = Opname {
        id: n.id,
        number: num.number,
        branch: n.branch,
        location,
        full,
        status: "counting".into(),
        uncounted: None,
        note,
        user: n.user,
        started_at: at,
        posted_at: None,
        lines: Vec::new(),
    };
    for v in variants {
        let snapshot = balances.get(&v).copied().unwrap_or_default();
        add_line(tx, &o, v, snapshot).await?;
    }
    let stored = get_opname(Conn::Tx(&mut *tx), n.id)
        .await?
        .ok_or(DbError::NotFound("opname"))?;
    recorded(tx, n.user, "opname.start", &stored, at).await?;
    Ok((Written::Created, stored))
}

/// Records `action` on the opname (`opname.changed`) by `user` at its branch.
async fn recorded(
    tx: &mut Tx,
    user: UserId,
    action: &str,
    o: &Opname,
    at: Timestamp,
) -> Result<()> {
    record_doc(
        tx,
        &doc_actor(user, o.branch),
        at,
        (action, "opname.changed"),
        *o.id.as_uuid(),
        opname_json(o),
    )
    .await
}

async fn load_counting(tx: &mut Tx, id: OpnameId) -> StockResult<Option<Head>> {
    let h = head(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(StockDbError::NotFound("opname"))?;
    match h.opname.status.as_str() {
        "counting" => Ok(Some(h)),
        _ => Ok(None),
    }
}

/// Appends count entries (`POST /opnames/{id}/counts`, `stock.count`): several users may count
/// into one opname at once; an entry already stored (same ID) is skipped. In a full count a
/// variant without a line gets one (its snapshot is on hand at the mark); a cycle count refuses
/// variants outside its scope.
///
/// # Errors
/// [`StockDbError::State`] for a posted or cancelled opname; [`StockDbError::Invalid`]
/// (`variant` outside a cycle count's scope); [`StockDbError::OpnameOpen`];
/// [`StockDbError::NotFound`].
pub async fn add_counts(
    tx: &mut Tx,
    id: OpnameId,
    entries: &[CountInput],
    user: UserId,
    at: Timestamp,
) -> StockResult<Opname> {
    let Some(h) = load_counting(tx, id).await? else {
        let status = head(Conn::Tx(&mut *tx), id)
            .await?
            .map(|h| h.opname.status)
            .unwrap_or_default();
        return Err(StockDbError::State {
            doc: "opname",
            status,
        });
    };
    let o = &h.opname;
    let mut added = 0_usize;
    for e in entries {
        let exists = sql::fetch_optional(
            Conn::Tx(&mut *tx),
            "SELECT 1 AS x FROM opname_count WHERE id = ?",
            args![e.id],
            |_| Ok(()),
        )
        .await?;
        if exists.is_some() {
            continue;
        }
        added += 1;
        let info = variant_info(tx, e.variant, e.unit).await?;
        let base = to_base(e.qty, info.factor)?;
        let has_line = sql::fetch_optional(
            Conn::Tx(&mut *tx),
            "SELECT 1 AS x FROM opname_line WHERE opname_id = ? AND variant_id = ?",
            args![o.id, e.variant],
            |_| Ok(()),
        )
        .await?
        .is_some();
        if !has_line {
            if !o.full {
                return Err(StockDbError::invalid(
                    "variant",
                    "is not in this cycle count's scope",
                ));
            }
            let now =
                location_on_hand(Conn::Tx(&mut *tx), o.branch, Some(o.location), e.variant).await?;
            let since =
                movements_since(tx, o.branch, o.location, e.variant, h.mark, *o.id.as_uuid())
                    .await?;
            let snapshot = now
                .checked_sub(since)
                .ok_or(sultan_core::stock::StockError::Overflow)?;
            add_line(tx, o, e.variant, snapshot).await?;
        }
        let mut so_far = sql::fetch_all(
            Conn::Tx(&mut *tx),
            "SELECT base_qty, replaces FROM opname_count WHERE opname_id = ? AND variant_id = ? \
             ORDER BY id",
            args![o.id, e.variant],
            |row| {
                Ok(CountEntry {
                    base_qty: sql::qty(row, "base_qty")?,
                    replace: row.bool("replaces")?,
                })
            },
        )
        .await?;
        so_far.push(CountEntry {
            base_qty: base,
            replace: e.replace,
        });
        counted(&so_far)?;
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO opname_count (id, opname_id, variant_id, unit_id, factor, qty, base_qty, \
             replaces, user_id, counted_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            args![
                e.id,
                o.id,
                e.variant,
                e.unit,
                info.factor,
                e.qty,
                base,
                e.replace,
                user,
                at,
            ],
        )
        .await?;
    }
    let stored = get_opname(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(DbError::NotFound("opname"))?;
    if added > 0 {
        recorded(tx, user, "opname.count", &stored, at).await?;
    }
    Ok(stored)
}

async fn release(tx: &mut Tx, id: OpnameId) -> Result<()> {
    sql::execute(
        Conn::Tx(&mut *tx),
        "DELETE FROM opname_lock WHERE opname_id = ?",
        args![id],
    )
    .await?;
    Ok(())
}

/// Posts an opname (`POST /opnames/{id}/post`, `stock.count` and `stock.adjust`): a `count`
/// movement for every non-zero variance (a shortage at the average and FEFO, an excess at the
/// average or without a cost); in a full count, uncounted variants not at 0 go to 0 with
/// [`Uncounted::SetZero`] and stay with [`Uncounted::Leave`] (I10). Frees the locks. A posted
/// opname is returned unchanged.
///
/// # Errors
/// [`StockDbError::State`] for a cancelled opname; [`StockDbError::NotFound`].
pub async fn post_opname(
    tx: &mut Tx,
    id: OpnameId,
    choice: Uncounted,
    user: UserId,
    approved_by: Option<UserId>,
    at: Timestamp,
) -> StockResult<(Written, Opname)> {
    let Some(h) = load_counting(tx, id).await? else {
        let o = get_opname(Conn::Tx(&mut *tx), id)
            .await?
            .ok_or(StockDbError::NotFound("opname"))?;
        return if o.status == "posted" {
            Ok((Written::Unchanged, o))
        } else {
            Err(StockDbError::state("opname", &o.status))
        };
    };
    let view = lines(Conn::Tx(&mut *tx), &h).await?;
    let o = &h.opname;
    let input: Vec<OpnameLine<VariantId>> = view
        .iter()
        .map(|l| OpnameLine {
            variant: l.variant,
            snapshot: l.snapshot,
            since: l.since,
            counted: l.counted,
        })
        .collect();
    let moves = opname_moves(&input, o.full, choice)?;
    for (variant, delta) in moves {
        let increase = delta.is_positive();
        let posted = post_moves(
            tx,
            &[Move {
                branch: o.branch,
                location: Some(o.location),
                variant,
                kind: MovementKind::Count,
                qty: delta,
                valuation: if increase {
                    Valuation::Increase {
                        typed: None,
                        need_cost: false,
                    }
                } else {
                    Valuation::Average
                },
                lot: if increase {
                    LotRule::None
                } else {
                    LotRule::Fefo
                },
                doc: DocRef {
                    kind: DocKind::Opname.as_str(),
                    id: *id.as_uuid(),
                    line: None,
                },
                at,
            }],
        )
        .await?
        .remove(0);
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE opname_line SET variance = ?, value = ? WHERE opname_id = ? AND variant_id = ?",
            args![delta, total_value(&posted), id, variant],
        )
        .await?;
    }
    for l in &view {
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE opname_line SET counted_qty = ?, variance = COALESCE(variance, 0) \
             WHERE opname_id = ? AND variant_id = ?",
            args![l.counted, id, l.variant],
        )
        .await?;
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE opname SET status = 'posted', uncounted = ?, posted_at = ?, posted_by = ?, \
         approved_by = ?, updated_at = ? WHERE id = ?",
        args![choice.as_str(), at, user, approved_by, at, id],
    )
    .await?;
    release(tx, id).await?;
    let stored = get_opname(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(DbError::NotFound("opname"))?;
    recorded(tx, user, "opname.post", &stored, at).await?;
    Ok((Written::Updated, stored))
}

/// Cancels an opname that is still counting (`POST /opnames/{id}/cancel`) and frees its
/// locks. A cancelled one is returned unchanged.
///
/// # Errors
/// [`StockDbError::State`] for a posted opname; [`StockDbError::NotFound`].
pub async fn cancel_opname(
    tx: &mut Tx,
    id: OpnameId,
    user: UserId,
    at: Timestamp,
) -> StockResult<(Written, Opname)> {
    if load_counting(tx, id).await?.is_none() {
        let o = get_opname(Conn::Tx(&mut *tx), id)
            .await?
            .ok_or(StockDbError::NotFound("opname"))?;
        return if o.status == "cancelled" {
            Ok((Written::Unchanged, o))
        } else {
            Err(StockDbError::state("opname", &o.status))
        };
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE opname SET status = 'cancelled', cancelled_at = ?, cancelled_by = ?, \
         updated_at = ? WHERE id = ?",
        args![at, user, at, id],
    )
    .await?;
    release(tx, id).await?;
    let stored = get_opname(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(DbError::NotFound("opname"))?;
    recorded(tx, user, "opname.cancel", &stored, at).await?;
    Ok((Written::Updated, stored))
}
