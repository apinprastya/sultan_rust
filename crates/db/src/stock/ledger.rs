//! The stock ledger and its caches (spec 2.8 §2, §4, §5, §9): [`post_moves`] appends movements
//! and keeps `stock_balance`, `stock_cost` and `stock_lot` in step, in the caller's transaction.
//! The reads: on hand, the stock card, lots, a document's movements and the cost state.

use sultan_core::catalog::Qty;
use sultan_core::doc_number::LocalDay;
use sultan_core::ids::{BranchId, BrandId, CategoryId, LocationId, LotId, ProductId, VariantId};
use sultan_core::money::Money;
use sultan_core::stock::{CostState, LotStock, MovementKind, RefCost, fefo, running_balance};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::{StockResult, clamp_limit, day};
use crate::conn::{Backend, Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// The location ID under which a movement without a location is balanced (a branch with no
/// location at all, §4).
pub const NO_LOCATION: [u8; 16] = [0; 16];

/// How a movement is valued (§2, §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Valuation {
    /// At the average: out through `CostState::issue`, in through `CostState::restore`.
    Average,
    /// Incoming at a cost (receipt, opening, transfer in): `CostState::receive`.
    Receive(Money),
    /// Incoming at a recorded value, else the average (a void, a restock return).
    PutBack(Option<Money>),
    /// An `adjust` / `count` increase: the average, else the typed value, else (when not
    /// required) no cost.
    Increase {
        /// The line's value from its typed unit cost.
        typed: Option<Money>,
        /// Refuse an increase without a cost (`invalid_field`, `unit_cost`).
        need_cost: bool,
    },
    /// Takes back a receipt at its value (`CostState::reverse`).
    Reverse(Money),
    /// A value fixed elsewhere, signed (`CostState::add_fixed`).
    Fixed(Option<Money>),
}

/// Which lot a movement uses (§9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LotRule {
    /// No lot.
    None,
    /// Out: first-expiry-first-out from the lots of a tracked product, the rest from "no lot".
    /// In: no lot.
    Fefo,
    /// In: into the lot (batch, expiry), created when new.
    Into(String, LocalDay),
    /// In or out: this lot.
    Lot(LotId),
}

/// The document a movement belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocRef {
    /// `sale`, `return`, `goods_receipt`, … (`stock_movement.doc_kind`).
    pub kind: &'static str,
    /// The document.
    pub id: Uuid,
    /// Its line, when there is one.
    pub line: Option<u32>,
}

/// One movement to post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    /// The branch.
    pub branch: BranchId,
    /// The location (`None` only for a branch without any location).
    pub location: Option<LocationId>,
    /// The variant.
    pub variant: VariantId,
    /// The kind; `qty` must have its sign.
    pub kind: MovementKind,
    /// Base units, signed.
    pub qty: Qty,
    /// How it is valued.
    pub valuation: Valuation,
    /// Which lot.
    pub lot: LotRule,
    /// The document.
    pub doc: DocRef,
    /// When it happened.
    pub at: Timestamp,
}

/// One stored ledger row of a posting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posted {
    /// The row's ID.
    pub id: Uuid,
    /// Its lot.
    pub lot: Option<LotId>,
    /// Base units, signed.
    pub qty: Qty,
    /// Its value, signed; `None` = no cost.
    pub value: Option<Money>,
}

/// The sum of the values of `posted`, `None` when any has no cost.
pub(crate) fn total_value(posted: &[Posted]) -> Option<Money> {
    posted
        .iter()
        .try_fold(Money::ZERO, |acc, p| acc.checked_add(p.value?))
}

fn location_arg(location: Option<LocationId>) -> Arg {
    match location {
        Some(l) => l.into(),
        None => Arg::from(NO_LOCATION.to_vec()),
    }
}

const fn for_update(backend: Backend) -> &'static str {
    match backend {
        Backend::Sqlite => "",
        Backend::MySql => " FOR UPDATE",
    }
}

/// Loads (and on `MySQL` locks) the cost state of (branch, variant), creating it.
async fn load_cost(tx: &mut Tx, branch: BranchId, variant: VariantId) -> Result<CostState> {
    let ensure = match tx.backend() {
        Backend::Sqlite => {
            "INSERT INTO stock_cost (branch_id, variant_id, qty, value, ref_value, ref_qty, \
             updated_at) VALUES (?, ?, 0, 0, NULL, NULL, 0) ON CONFLICT DO NOTHING"
        }
        Backend::MySql => {
            "INSERT INTO stock_cost (branch_id, variant_id, qty, value, ref_value, ref_qty, \
             updated_at) VALUES (?, ?, 0, 0, NULL, NULL, 0) AS new \
             ON DUPLICATE KEY UPDATE qty = stock_cost.qty"
        }
    };
    sql::execute(Conn::Tx(&mut *tx), ensure, args![branch, variant]).await?;
    let lock = for_update(tx.backend());
    let sql = format!(
        "SELECT qty, value, ref_value, ref_qty FROM stock_cost \
         WHERE branch_id = ? AND variant_id = ?{lock}"
    );
    sql::fetch_optional(Conn::Tx(&mut *tx), &sql, args![branch, variant], map_cost)
        .await?
        .ok_or(DbError::NotFound("stock cost"))
}

fn map_cost(row: &dyn Row) -> Result<CostState> {
    let reference = match (
        sql::opt_money(row, "ref_value")?,
        sql::opt_qty(row, "ref_qty")?,
    ) {
        (Some(value), Some(qty)) => Some(RefCost { value, qty }),
        _ => None,
    };
    Ok(CostState {
        qty: sql::qty(row, "qty")?,
        value: sql::money(row, "value")?,
        reference,
    })
}

async fn save_cost(
    tx: &mut Tx,
    branch: BranchId,
    variant: VariantId,
    s: &CostState,
    at: Timestamp,
) -> Result<()> {
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE stock_cost SET qty = ?, value = ?, ref_value = ?, ref_qty = ?, updated_at = ? \
         WHERE branch_id = ? AND variant_id = ?",
        args![
            s.qty,
            s.value,
            s.reference.map(|r| r.value),
            s.reference.map(|r| r.qty),
            at,
            branch,
            variant,
        ],
    )
    .await?;
    Ok(())
}

async fn add_balance(tx: &mut Tx, m: &Move, qty: Qty) -> Result<()> {
    let sql = match tx.backend() {
        Backend::Sqlite => {
            "INSERT INTO stock_balance (branch_id, location_id, variant_id, qty, updated_at) \
             VALUES (?, ?, ?, ?, ?) ON CONFLICT (branch_id, location_id, variant_id) \
             DO UPDATE SET qty = qty + excluded.qty, updated_at = excluded.updated_at"
        }
        Backend::MySql => {
            "INSERT INTO stock_balance (branch_id, location_id, variant_id, qty, updated_at) \
             VALUES (?, ?, ?, ?, ?) AS new ON DUPLICATE KEY UPDATE \
             qty = stock_balance.qty + new.qty, updated_at = new.updated_at"
        }
    };
    sql::execute(
        Conn::Tx(&mut *tx),
        sql,
        vec![
            m.branch.into(),
            location_arg(m.location),
            m.variant.into(),
            qty.into(),
            m.at.into(),
        ],
    )
    .await?;
    Ok(())
}

/// The lot (branch, location, variant, batch, expiry), created when new.
async fn lot_into(tx: &mut Tx, m: &Move, batch: &str, expiry: LocalDay) -> Result<LotId> {
    let key = vec![
        m.branch.into(),
        location_arg(m.location),
        m.variant.into(),
        batch.into(),
        expiry.key().into(),
    ];
    let find = "SELECT id FROM stock_lot WHERE branch_id = ? AND location_id = ? \
                AND variant_id = ? AND batch = ? AND expiry = ?";
    if let Some(id) =
        sql::fetch_optional(Conn::Tx(&mut *tx), find, key.clone(), |r| sql::id(r, "id")).await?
    {
        return Ok(id);
    }
    let id = LotId::generate();
    let mut values = vec![Arg::from(id)];
    values.extend(key);
    values.push(m.at.into());
    values.push(m.at.into());
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO stock_lot (id, branch_id, location_id, variant_id, batch, expiry, qty, \
         created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?)",
        values,
    )
    .await?;
    Ok(id)
}

/// The lots of a tracked product at the movement's place with stock, oldest first.
async fn tracked_lots(tx: &mut Tx, m: &Move) -> Result<Vec<LotStock<LotId>>> {
    let lock = for_update(tx.backend());
    let sql = format!(
        "SELECT l.id, l.expiry, l.qty FROM stock_lot l \
         JOIN variant v ON v.id = l.variant_id JOIN product p ON p.id = v.product_id \
         WHERE l.branch_id = ? AND l.location_id = ? AND l.variant_id = ? AND l.qty > 0 \
         AND p.track_expiry = 1 ORDER BY l.created_at, l.id{lock}"
    );
    sql::fetch_all(
        Conn::Tx(&mut *tx),
        &sql,
        vec![m.branch.into(), location_arg(m.location), m.variant.into()],
        |row| {
            Ok(LotStock {
                lot: sql::id(row, "id")?,
                expiry: day(row, "expiry")?,
                qty: sql::qty(row, "qty")?,
            })
        },
    )
    .await
}

fn value_of(state: &mut CostState, v: Valuation, qty: Qty) -> StockResult<Option<Money>> {
    let positive = qty.is_positive();
    let abs = if positive {
        qty
    } else {
        qty.checked_neg()
            .ok_or(sultan_core::stock::StockError::Overflow)?
    };
    Ok(match v {
        Valuation::Average if positive => state.restore(abs)?,
        Valuation::Average => state.issue(abs)?,
        Valuation::Receive(value) => Some(state.receive(abs, value)?),
        Valuation::PutBack(value) => state.put_back(abs, value)?,
        Valuation::Increase { typed, need_cost } => state.increase(abs, typed, need_cost)?,
        Valuation::Reverse(value) => Some(state.reverse(abs, value)?),
        Valuation::Fixed(value) => {
            state.add_fixed(qty, value)?;
            value
        }
    })
}

/// Posts `moves` in order (spec 2.8 §2): for each, checks the kind's sign, splits an outgoing
/// FEFO movement into one row per lot taken (plus "no lot"), values each part from the cost
/// state, appends the rows and updates the balance, the cost and the lots. Returns the rows of
/// each move.
///
/// # Errors
/// [`StockDbError::Rule`] for a wrong sign or a cost rule (`unit_cost`), or the database error.
pub async fn post_moves(tx: &mut Tx, moves: &[Move]) -> StockResult<Vec<Vec<Posted>>> {
    let mut out = Vec::with_capacity(moves.len());
    for m in moves {
        out.push(post_move(tx, m).await?);
    }
    Ok(out)
}

async fn post_move(tx: &mut Tx, m: &Move) -> StockResult<Vec<Posted>> {
    m.kind.check_qty(m.qty)?;
    let mut state = load_cost(tx, m.branch, m.variant).await?;
    let parts: Vec<(Option<LotId>, Qty)> = match &m.lot {
        LotRule::None => vec![(None, m.qty)],
        LotRule::Lot(lot) => vec![(Some(*lot), m.qty)],
        LotRule::Into(batch, expiry) => vec![(Some(lot_into(tx, m, batch, *expiry).await?), m.qty)],
        LotRule::Fefo if m.qty.is_positive() => vec![(None, m.qty)],
        LotRule::Fefo => {
            let lots = tracked_lots(tx, m).await?;
            let abs = m.qty.checked_neg().ok_or(DbError::Corrupt("qty".into()))?;
            fefo(&lots, abs)?
                .into_iter()
                .map(|t| (t.lot, t.qty.checked_neg().unwrap_or(Qty::ZERO)))
                .collect()
        }
    };
    let mut posted = Vec::with_capacity(parts.len());
    for (lot, qty) in parts {
        let value = value_of(&mut state, m.valuation, qty)?;
        let id = Uuid::now_v7();
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO stock_movement (id, branch_id, location_id, variant_id, kind, base_qty, \
             value, lot_id, doc_kind, doc_id, doc_line, occurred_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            args![
                id,
                m.branch,
                m.location,
                m.variant,
                m.kind.as_str(),
                qty,
                value,
                lot,
                m.doc.kind,
                m.doc.id,
                m.doc.line.map(i64::from),
                m.at,
            ],
        )
        .await?;
        if let Some(lot) = lot {
            sql::execute(
                Conn::Tx(&mut *tx),
                "UPDATE stock_lot SET qty = qty + ?, updated_at = ? WHERE id = ?",
                args![qty, m.at, lot],
            )
            .await?;
        }
        add_balance(tx, m, qty).await?;
        posted.push(Posted {
            id,
            lot,
            qty,
            value,
        });
    }
    save_cost(tx, m.branch, m.variant, &state, m.at).await?;
    Ok(posted)
}

/// The sign of a kind as stored, for reads.
fn parse_kind(key: &str) -> Result<MovementKind> {
    MovementKind::from_key(key)
        .ok_or_else(|| DbError::Corrupt(format!("unknown stock movement kind `{key}`")))
}

/// A stored ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovementRecord {
    /// `UUIDv7`, in posting order.
    pub id: Uuid,
    /// The branch.
    pub branch: BranchId,
    /// The location.
    pub location: Option<LocationId>,
    /// The variant.
    pub variant: VariantId,
    /// The kind.
    pub kind: MovementKind,
    /// Base units, signed.
    pub qty: Qty,
    /// Signed Rupiah; `None` = no cost (and every row from before 2.8).
    pub value: Option<Money>,
    /// The lot.
    pub lot: Option<LotId>,
    /// The document kind.
    pub doc_kind: String,
    /// The document.
    pub doc_id: Uuid,
    /// The document line.
    pub doc_line: Option<u32>,
    /// When it happened.
    pub occurred_at: Timestamp,
}

const MOVEMENT_COLUMNS: &str = "id, branch_id, location_id, variant_id, kind, base_qty, value, \
                                lot_id, doc_kind, doc_id, doc_line, occurred_at";

pub(crate) fn map_movement(row: &dyn Row) -> Result<MovementRecord> {
    Ok(MovementRecord {
        id: sql::id(row, "id")?,
        branch: sql::id(row, "branch_id")?,
        location: sql::opt_id(row, "location_id")?,
        variant: sql::id(row, "variant_id")?,
        kind: parse_kind(&row.text("kind")?)?,
        qty: sql::qty(row, "base_qty")?,
        value: sql::opt_money(row, "value")?,
        lot: sql::opt_id(row, "lot_id")?,
        doc_kind: row.text("doc_kind")?,
        doc_id: sql::id(row, "doc_id")?,
        doc_line: row
            .opt_i64("doc_line")?
            .map(|v| u32::try_from(v).map_err(|_| DbError::Corrupt(format!("doc_line {v}"))))
            .transpose()?,
        occurred_at: sql::ts(row, "occurred_at")?,
    })
}

/// The movements of a document, in posting order.
///
/// # Errors
/// The database error.
pub async fn document_movements<'c>(
    conn: impl Into<Conn<'c>>,
    doc_id: Uuid,
) -> Result<Vec<MovementRecord>> {
    let sql = format!("SELECT {MOVEMENT_COLUMNS} FROM stock_movement WHERE doc_id = ? ORDER BY id");
    sql::fetch_all(conn.into(), &sql, args![doc_id], map_movement).await
}

/// The cost state of (branch, variant): quantity over all locations, value and reference cost
/// (spec 2.8 §5); the default (nothing, no cost) when it never moved.
///
/// # Errors
/// The database error.
pub async fn cost_state<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
    variant: VariantId,
) -> Result<CostState> {
    sql::fetch_optional(
        conn.into(),
        "SELECT qty, value, ref_value, ref_qty FROM stock_cost \
         WHERE branch_id = ? AND variant_id = ?",
        args![branch, variant],
        map_cost,
    )
    .await
    .map(Option::unwrap_or_default)
}

/// On hand of `variant` at `location` (spec 2.8 §4), from the cache.
///
/// # Errors
/// The database error.
pub async fn location_on_hand<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
    location: Option<LocationId>,
    variant: VariantId,
) -> Result<Qty> {
    sql::fetch_optional(
        conn.into(),
        "SELECT qty FROM stock_balance WHERE branch_id = ? AND location_id = ? \
         AND variant_id = ?",
        vec![branch.into(), location_arg(location), variant.into()],
        |row| sql::qty(row, "qty"),
    )
    .await
    .map(Option::unwrap_or_default)
}

/// On hand of `variant` at `branch` over all its locations (spec 2.8 §4, the till's lookup,
/// I18).
///
/// # Errors
/// The database error.
pub async fn branch_on_hand<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
    variant: VariantId,
) -> Result<Qty> {
    sql::fetch_optional(
        conn.into(),
        "SELECT CAST(COALESCE(SUM(qty), 0) AS SIGNED) AS qty FROM stock_balance \
         WHERE branch_id = ? AND variant_id = ?",
        args![branch, variant],
        |row| sql::qty(row, "qty"),
    )
    .await
    .map(Option::unwrap_or_default)
}

/// Filters of the on-hand list (spec 2.8 §14, §16).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OnHandFilter {
    /// The branch.
    pub branch: Option<BranchId>,
    /// One location (else all of the branch, one row per location and variant).
    pub location: Option<LocationId>,
    /// One variant.
    pub variant: Option<VariantId>,
    /// Products of this category (its own products, like a cycle count).
    pub category: Option<CategoryId>,
    /// Products of this brand.
    pub brand: Option<BrandId>,
    /// Text search: contained in the product's search key (name, brand, tags), or exactly its
    /// code, the variant's SKU or one of its barcodes.
    pub q: Option<String>,
    /// Only negative balances (I7).
    pub negative: bool,
    /// Only "low" variants (§10): negative here, or a reorder rule with branch on hand + on
    /// order at or below its min.
    pub low: bool,
    /// Only rows with a lot (qty > 0) expiring within the product's warn days (else the
    /// company's) of this day, expired lots included (§9).
    pub expiring: Option<LocalDay>,
    /// Rows after this (branch, location, variant) key, for keyset paging.
    pub after: Option<(BranchId, Vec<u8>, VariantId)>,
    /// At most this many (default 50, at most 500).
    pub limit: u32,
}

/// One row of the on-hand list: the balance at a location, with the branch's figures of the
/// variant (cost, on order, in transit, reorder rule) and its catalog names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnHandRow {
    /// The branch.
    pub branch: BranchId,
    /// The location (`None` = no location).
    pub location: Option<LocationId>,
    /// The variant.
    pub variant: VariantId,
    /// Its product (`None` for a variant the catalog does not hold, e.g. not synced yet).
    pub product: Option<ProductId>,
    /// The product code.
    pub product_code: String,
    /// The product name.
    pub product_name: String,
    /// The variant's SKU.
    pub sku: String,
    /// On hand at the location.
    pub qty: Qty,
    /// On order at the branch (open POs, §4).
    pub on_order: Qty,
    /// In transit to or from the branch (§4).
    pub in_transit: Qty,
    /// The branch's reorder rule (min, max).
    pub rule: Option<(Qty, Qty)>,
    /// The branch's unit cost of the variant; `None` = no cost yet. The server leaves it out
    /// without `report.profit` (§12).
    pub unit_cost: Option<Money>,
}

/// The SQL of the on-hand rows before the filters on derived columns (`low`).
const ON_HAND_SELECT: &str = "SELECT b.branch_id, b.location_id, b.variant_id, b.qty, \
     c.ref_value, c.ref_qty, p.id AS product_id, COALESCE(p.code, '') AS product_code, \
     COALESCE(p.name, '') AS product_name, COALESCE(v.sku, '') AS sku, p.category_id, p.brand_id, p.search_key, \
     p.expiry_warn_days, r.min_qty, r.max_qty, \
     (SELECT CAST(COALESCE(SUM(x.qty), 0) AS SIGNED) FROM stock_balance x \
      WHERE x.branch_id = b.branch_id AND x.variant_id = b.variant_id) AS branch_qty, \
     (SELECT CAST(COALESCE(SUM(CASE WHEN l.base_qty > l.received_qty \
      THEN l.base_qty - l.received_qty ELSE 0 END), 0) AS SIGNED) \
      FROM purchase_order_line l JOIN purchase_order o ON o.id = l.order_id \
      WHERE o.branch_id = b.branch_id AND o.status = 'ordered' \
      AND l.variant_id = b.variant_id) AS on_order, \
     (SELECT CAST(COALESCE(SUM(tl.shipped_qty), 0) AS SIGNED) \
      FROM stock_transfer_line tl JOIN stock_transfer t ON t.id = tl.transfer_id \
      WHERE t.status = 'shipped' AND tl.variant_id = b.variant_id \
      AND (t.from_branch_id = b.branch_id OR t.to_branch_id = b.branch_id)) AS in_transit \
     FROM stock_balance b \
     LEFT JOIN variant v ON v.id = b.variant_id \
     LEFT JOIN product p ON p.id = v.product_id \
     LEFT JOIN stock_cost c ON c.branch_id = b.branch_id AND c.variant_id = b.variant_id \
     LEFT JOIN reorder_rule r ON r.branch_id = b.branch_id AND r.variant_id = b.variant_id";

/// The expiry horizon of a product as SQL over `p.expiry_warn_days`: `today` + its warn days,
/// else + the company's (§9). Days cannot be added to a `YYYYMMDD` key in portable SQL, so each
/// warn-days value in use gets its own branch of a `CASE`. Without any override it is just the
/// company's horizon (`CASE` needs at least one `WHEN`).
async fn horizon_sql(conn: &mut Conn<'_>, today: LocalDay) -> Result<(String, Vec<Arg>)> {
    let default = super::get_inventory_settings(conn.reborrow())
        .await?
        .expiry_warn_days;
    let overrides = sql::fetch_all(
        conn.reborrow(),
        "SELECT DISTINCT expiry_warn_days FROM product WHERE expiry_warn_days IS NOT NULL",
        args![],
        |row| row.i64("expiry_warn_days"),
    )
    .await?;
    let by = |days: i64| today.add_days(days).map_or(99_991_231, LocalDay::key);
    if overrides.is_empty() {
        return Ok(("?".into(), vec![by(i64::from(default)).into()]));
    }
    let mut text = String::from("CASE");
    let mut a: Vec<Arg> = Vec::new();
    for days in overrides {
        text.push_str(" WHEN p.expiry_warn_days = ? THEN ?");
        a.extend([Arg::from(days), Arg::from(by(days))]);
    }
    text.push_str(" ELSE ? END");
    a.push(by(i64::from(default)).into());
    Ok((text, a))
}

/// The on-hand list (spec 2.8 §4, §14): balances by branch, location and variant with the
/// branch's unit cost, on order, in transit and reorder rule, filtered by branch, location,
/// variant, category, brand, text, and "negative", "low" or "expiring". Variants that never
/// moved have no balance row and are not listed (the reorder screen's [`super::low_stock`]
/// covers rules without stock).
///
/// # Errors
/// The database error.
#[allow(clippy::too_many_lines)] // one query built from the filters, then its rows
pub async fn list_on_hand<'c>(
    conn: impl Into<Conn<'c>>,
    f: &OnHandFilter,
) -> Result<Vec<OnHandRow>> {
    let mut conn = conn.into();
    let mut conds: Vec<String> = Vec::new();
    let mut a: Vec<Arg> = Vec::new();
    if let Some(b) = f.branch {
        conds.push("branch_id = ?".into());
        a.push(b.into());
    }
    if let Some(l) = f.location {
        conds.push("location_id = ?".into());
        a.push(l.into());
    }
    if let Some(v) = f.variant {
        conds.push("variant_id = ?".into());
        a.push(v.into());
    }
    if let Some(c) = f.category {
        conds.push("category_id = ?".into());
        a.push(c.into());
    }
    if let Some(b) = f.brand {
        conds.push("brand_id = ?".into());
        a.push(b.into());
    }
    if let Some(q) = f.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        let key = sultan_core::catalog::search_key(q);
        let upper = q.to_uppercase();
        conds.push(
            "(search_key LIKE ? ESCAPE '!' OR product_code = ? OR sku = ? OR EXISTS \
             (SELECT 1 FROM barcode bc WHERE bc.variant_id = x.variant_id AND bc.code = ? \
             AND bc.archived = 0))"
                .into(),
        );
        a.extend([
            Arg::from(format!("%{}%", crate::catalog::escape_like(&key))),
            Arg::from(upper.clone()),
            Arg::from(upper),
            Arg::from(q.to_owned()),
        ]);
    }
    if f.negative {
        conds.push("qty < 0".into());
    }
    if f.low {
        conds
            .push("(qty < 0 OR (min_qty IS NOT NULL AND branch_qty + on_order <= min_qty))".into());
    }
    if let Some(today) = f.expiring {
        let (horizon, values) = horizon_sql(&mut conn, today).await?;
        conds.push(format!(
            "EXISTS (SELECT 1 FROM stock_lot lt JOIN variant lv ON lv.id = lt.variant_id \
             JOIN product p ON p.id = lv.product_id WHERE lt.branch_id = x.branch_id \
             AND lt.location_id = x.location_id AND lt.variant_id = x.variant_id \
             AND lt.qty > 0 AND lt.expiry <= {horizon})"
        ));
        a.extend(values);
    }
    if let Some((b, l, v)) = &f.after {
        conds.push(
            "(branch_id > ? OR (branch_id = ? AND (location_id > ? \
             OR (location_id = ? AND variant_id > ?))))"
                .into(),
        );
        a.extend([
            Arg::from(*b),
            Arg::from(*b),
            Arg::from(l.clone()),
            Arg::from(l.clone()),
            Arg::from(*v),
        ]);
    }
    let filter = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };
    let sql = format!(
        "SELECT * FROM ({ON_HAND_SELECT}) x{filter} \
         ORDER BY branch_id, location_id, variant_id LIMIT ?"
    );
    a.push(clamp_limit(f.limit, 50, 500).into());
    sql::fetch_all(conn, &sql, a, |row| {
        let location = row.bytes("location_id")?;
        let cost = map_ref(row)?;
        let rule = match (sql::opt_qty(row, "min_qty")?, sql::opt_qty(row, "max_qty")?) {
            (Some(min), Some(max)) => Some((min, max)),
            _ => None,
        };
        Ok(OnHandRow {
            branch: sql::id(row, "branch_id")?,
            location: if location == NO_LOCATION {
                None
            } else {
                sql::opt_id(row, "location_id")?
            },
            variant: sql::id(row, "variant_id")?,
            product: sql::opt_id(row, "product_id")?,
            product_code: row.text("product_code")?,
            product_name: row.text("product_name")?,
            sku: row.text("sku")?,
            qty: sql::qty(row, "qty")?,
            on_order: sql::qty(row, "on_order")?,
            in_transit: sql::qty(row, "in_transit")?,
            rule,
            unit_cost: cost
                .map(RefCost::unit_cost)
                .transpose()
                .map_err(|e| DbError::Corrupt(e.to_string()))?,
        })
    })
    .await
}

fn map_ref(row: &dyn Row) -> Result<Option<RefCost>> {
    Ok(
        match (
            sql::opt_money(row, "ref_value")?,
            sql::opt_qty(row, "ref_qty")?,
        ) {
            (Some(value), Some(qty)) => Some(RefCost { value, qty }),
            _ => None,
        },
    )
}

/// One row of a stock card: the movement and the balance after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardRow {
    /// The movement.
    pub movement: MovementRecord,
    /// On hand after it (at the card's location, or the branch).
    pub balance: Qty,
}

/// The stock card of `variant` at `branch` (and `location`) from `from` until `until`
/// (spec 2.8 §14, §16): its movements oldest first with the running balance, starting from the
/// balance before `from`. At most `limit` rows after the movement `after` (keyset paging).
///
/// # Errors
/// The database error.
#[allow(clippy::too_many_arguments)] // the card's filters, as the route takes them
pub async fn stock_card(
    tx: &mut Tx,
    branch: BranchId,
    location: Option<LocationId>,
    variant: VariantId,
    from: Option<Timestamp>,
    until: Option<Timestamp>,
    after: Option<Uuid>,
    limit: u32,
) -> Result<Vec<CardRow>> {
    let mut place = String::from(" WHERE branch_id = ? AND variant_id = ?");
    let mut a: Vec<Arg> = args![branch, variant];
    if let Some(l) = location {
        place.push_str(" AND location_id = ?");
        a.push(l.into());
    }
    // The opening balance: everything before the first row shown.
    let mut before = format!(
        "SELECT CAST(COALESCE(SUM(base_qty), 0) AS SIGNED) AS qty FROM stock_movement{place}"
    );
    let mut before_args = a.clone();
    let mut conds = Vec::new();
    if let Some(f) = from {
        conds.push(("occurred_at < ?", Arg::from(f)));
    }
    if let Some(id) = after {
        conds.push(("id <= ?", Arg::from(id)));
    }
    if conds.is_empty() {
        before.push_str(" AND 1 = 0");
    } else {
        let text: Vec<&str> = conds.iter().map(|(t, _)| *t).collect();
        before.push_str(" AND (");
        before.push_str(&text.join(" OR "));
        before.push(')');
        before_args.extend(conds.into_iter().map(|(_, v)| v));
    }
    let opening = sql::fetch_optional(Conn::Tx(&mut *tx), &before, before_args, |row| {
        sql::qty(row, "qty")
    })
    .await?
    .unwrap_or_default();
    let mut rows_sql = format!("SELECT {MOVEMENT_COLUMNS} FROM stock_movement{place}");
    if let Some(f) = from {
        rows_sql.push_str(" AND occurred_at >= ?");
        a.push(f.into());
    }
    if let Some(u) = until {
        rows_sql.push_str(" AND occurred_at < ?");
        a.push(u.into());
    }
    if let Some(id) = after {
        rows_sql.push_str(" AND id > ?");
        a.push(id.into());
    }
    rows_sql.push_str(" ORDER BY id LIMIT ?");
    a.push(clamp_limit(limit, 100, 1_000).into());
    let rows = sql::fetch_all(Conn::Tx(&mut *tx), &rows_sql, a, map_movement).await?;
    let qtys: Vec<Qty> = rows.iter().map(|r| r.qty).collect();
    let balances = running_balance(opening, &qtys).map_err(|e| DbError::Corrupt(e.to_string()))?;
    Ok(rows
        .into_iter()
        .zip(balances)
        .map(|(movement, balance)| CardRow { movement, balance })
        .collect())
}

/// A stored lot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LotRecord {
    /// The lot.
    pub id: LotId,
    /// The branch.
    pub branch: BranchId,
    /// The location.
    pub location: LocationId,
    /// The variant.
    pub variant: VariantId,
    /// The batch number (empty when none was typed).
    pub batch: String,
    /// The expiry date.
    pub expiry: LocalDay,
    /// Base units.
    pub qty: Qty,
}

/// The lots with stock (spec 2.8 §9, §16), by expiry: of a branch, location or variant, and
/// with `expiring_by` only those expiring on or before that day (the Expiring list passes
/// today + the warn days; expired lots are included).
///
/// # Errors
/// The database error.
pub async fn list_lots<'c>(
    conn: impl Into<Conn<'c>>,
    branch: Option<BranchId>,
    location: Option<LocationId>,
    variant: Option<VariantId>,
    expiring_by: Option<LocalDay>,
) -> Result<Vec<LotRecord>> {
    let mut sql = String::from(
        "SELECT id, branch_id, location_id, variant_id, batch, expiry, qty FROM stock_lot \
         WHERE qty > 0",
    );
    let mut a: Vec<Arg> = Vec::new();
    if let Some(b) = branch {
        sql.push_str(" AND branch_id = ?");
        a.push(b.into());
    }
    if let Some(l) = location {
        sql.push_str(" AND location_id = ?");
        a.push(l.into());
    }
    if let Some(v) = variant {
        sql.push_str(" AND variant_id = ?");
        a.push(v.into());
    }
    if let Some(d) = expiring_by {
        sql.push_str(" AND expiry <= ?");
        a.push(d.key().into());
    }
    sql.push_str(" ORDER BY expiry, created_at, id");
    sql::fetch_all(conn.into(), &sql, a, |row| {
        Ok(LotRecord {
            id: sql::id(row, "id")?,
            branch: sql::id(row, "branch_id")?,
            location: sql::id(row, "location_id")?,
            variant: sql::id(row, "variant_id")?,
            batch: row.text("batch")?,
            expiry: day(row, "expiry")?,
            qty: sql::qty(row, "qty")?,
        })
    })
    .await
}

/// The Expiring list (spec 2.8 §9, I22): the lots with stock that expire within their
/// product's warn days (else the company's) of `today`, expired ones included, by expiry.
///
/// # Errors
/// The database error.
pub async fn list_expiring_lots<'c>(
    conn: impl Into<Conn<'c>>,
    branch: Option<BranchId>,
    location: Option<LocationId>,
    variant: Option<VariantId>,
    today: LocalDay,
) -> Result<Vec<LotRecord>> {
    let mut conn = conn.into();
    let (horizon, values) = horizon_sql(&mut conn, today).await?;
    let mut sql = format!(
        "SELECT l.id, l.branch_id, l.location_id, l.variant_id, l.batch, l.expiry, l.qty \
         FROM stock_lot l JOIN variant v ON v.id = l.variant_id \
         JOIN product p ON p.id = v.product_id WHERE l.qty > 0 AND l.expiry <= {horizon}"
    );
    let mut a: Vec<Arg> = values;
    if let Some(b) = branch {
        sql.push_str(" AND l.branch_id = ?");
        a.push(b.into());
    }
    if let Some(l) = location {
        sql.push_str(" AND l.location_id = ?");
        a.push(l.into());
    }
    if let Some(v) = variant {
        sql.push_str(" AND l.variant_id = ?");
        a.push(v.into());
    }
    sql.push_str(" ORDER BY l.expiry, l.created_at, l.id");
    sql::fetch_all(conn, &sql, a, |row| {
        Ok(LotRecord {
            id: sql::id(row, "id")?,
            branch: sql::id(row, "branch_id")?,
            location: sql::id(row, "location_id")?,
            variant: sql::id(row, "variant_id")?,
            batch: row.text("batch")?,
            expiry: day(row, "expiry")?,
            qty: sql::qty(row, "qty")?,
        })
    })
    .await
}

/// The net movements of `variant` at `location` after the ledger row ID `mark` (an opname's
/// snapshot), other than the document `except`.
pub(crate) async fn movements_since(
    tx: &mut Tx,
    branch: BranchId,
    location: LocationId,
    variant: VariantId,
    mark: Uuid,
    except: Uuid,
) -> Result<Qty> {
    sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT CAST(COALESCE(SUM(base_qty), 0) AS SIGNED) AS qty FROM stock_movement \
         WHERE branch_id = ? AND location_id = ? AND variant_id = ? AND id > ? AND doc_id <> ?",
        args![branch, location, variant, mark, except],
        |row| sql::qty(row, "qty"),
    )
    .await
    .map(Option::unwrap_or_default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_totals() {
        for kind in MovementKind::ALL {
            assert_eq!(parse_kind(kind.as_str()).unwrap(), kind);
        }
        assert!(parse_kind("transfer").is_err());
        let p = |v: Option<i64>| Posted {
            id: Uuid::nil(),
            lot: None,
            qty: Qty::ONE,
            value: v.map(Money::from_minor),
        };
        assert_eq!(
            total_value(&[p(Some(1)), p(Some(2))]),
            Some(Money::from_minor(3))
        );
        assert_eq!(total_value(&[p(Some(1)), p(None)]), None);
        assert_eq!(total_value(&[]), Some(Money::ZERO));
    }
}
