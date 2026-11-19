//! Cash movements (spec 2.7 §1, §2, K2, K9, K10): cash in, cash out and safe drops of a cashier
//! session, created posted ([`post_cash_movement`]), read one by one or in pages
//! ([`get_cash_movement`], [`list_cash_movements`]).

use serde_json::{Value, json};
use sultan_core::cash::{CashKind, CashMovement, is_missing_nota};
use sultan_core::doc_number::{BranchTimeZone, LocalDay, cash_movement_number};
use sultan_core::ids::{
    BranchId, CashMovementId, CashReasonId, CashierSessionId, RegisterId, UserId,
};
use sultan_core::money::Money;
use sultan_core::time::Timestamp;

use super::{Actor, CashDbError, CashResult, Change, Written, clamp_limit, count, record};
use crate::TokenHash;
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// A cash movement to post: the movement as `core::cash` built it ([`CashMovement::new`] or
/// [`CashMovement::reversal`]) and where and by whom it was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCashMovement {
    /// The movement (its `session` is the caller's open session).
    pub movement: CashMovement,
    /// The register's branch.
    pub branch: BranchId,
    /// The register.
    pub register: RegisterId,
    /// The cashier (the session's user).
    pub user: UserId,
    /// The manager who approved it at the till (`X-Override`), if any.
    pub approved_by: Option<UserId>,
    /// When it was posted.
    pub posted_at: Timestamp,
    /// The hash of the posted body (computed by the server), for idempotent replays.
    pub request_hash: TokenHash,
}

/// A stored cash movement with what lists and slips show next to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CashMovementRecord {
    /// The movement.
    pub movement: CashMovement,
    /// `JKT01-R2-K-261119-01`.
    pub number: String,
    /// The branch.
    pub branch: BranchId,
    /// The register.
    pub register: RegisterId,
    /// The cashier.
    pub user: UserId,
    /// The approving manager, if any.
    pub approved_by: Option<UserId>,
    /// When it was posted.
    pub posted_at: Timestamp,
    /// The reason's current name (archived reasons too); `None` for a drop.
    pub reason_name: Option<String>,
    /// The number of the movement this one reverses ("reversal of …").
    pub reverses_number: Option<String>,
    /// The movement that reverses this one, if any.
    pub reversed_by: Option<CashMovementId>,
    /// The number of nota photos.
    pub photos: u32,
}

impl CashMovementRecord {
    /// Whether it is "missing nota" at the company's `nota_flag_above` (§2a,
    /// [`is_missing_nota`]).
    #[must_use]
    pub fn missing_nota(&self, flag_above: Money) -> bool {
        is_missing_nota(
            &self.movement,
            usize::try_from(self.photos).unwrap_or(usize::MAX),
            self.reversed_by.is_some(),
            flag_above,
        )
    }
}

/// Filters for [`list_cash_movements`] (spec 2.7 §10, §11). All set fields must match; newest
/// first by ID (keyset paging: `before` = the last ID of the previous page).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CashFilter {
    /// Only this branch.
    pub branch: Option<BranchId>,
    /// Only this register.
    pub register: Option<RegisterId>,
    /// Only this cashier session.
    pub cashier_session: Option<CashierSessionId>,
    /// Only this kind.
    pub kind: Option<CashKind>,
    /// Only this reason.
    pub reason: Option<CashReasonId>,
    /// Posted at or after.
    pub from: Option<Timestamp>,
    /// Posted before.
    pub until: Option<Timestamp>,
    /// Only the cash outs missing a nota at this `nota_flag_above` (`missing_nota=true`, §2a); a
    /// threshold of 0 (off) matches nothing.
    pub missing_nota_above: Option<Money>,
    /// Page cursor.
    pub before: Option<CashMovementId>,
    /// Page size; `0` = 50, at most [`MAX_CASH_PAGE`].
    pub limit: u32,
}

/// The largest page of [`list_cash_movements`].
pub const MAX_CASH_PAGE: u32 = 200;

const SELECT: &str = "SELECT m.id, m.number, m.kind, m.amount, m.reason_id, m.note, m.branch_id, \
     m.register_id, m.cashier_session_id, m.user_id, m.approved_by, m.reverses, m.posted_at, \
     (SELECT c.name FROM cash_reason c WHERE c.id = m.reason_id) AS reason_name, \
     (SELECT o.number FROM cash_movement o WHERE o.id = m.reverses) AS reverses_number, \
     (SELECT r.id FROM cash_movement r WHERE r.reverses = m.id) AS reversed_by, \
     (SELECT COUNT(*) FROM cash_movement_photo p WHERE p.movement_id = m.id) AS photos \
     FROM cash_movement m";

/// The `WHERE` condition of a cash out missing a nota; binds the threshold twice.
const MISSING_NOTA: &str = "m.kind = 'out' AND m.reverses IS NULL AND ? > 0 AND m.amount >= ? \
     AND NOT EXISTS (SELECT 1 FROM cash_movement_photo p WHERE p.movement_id = m.id) \
     AND NOT EXISTS (SELECT 1 FROM cash_movement r WHERE r.reverses = m.id)";

pub(super) fn kind(row: &dyn Row, col: &str) -> Result<CashKind> {
    let key = row.text(col)?;
    CashKind::from_key(&key).ok_or_else(|| DbError::Corrupt(format!("unknown cash kind `{key}`")))
}

/// The movement part of a row (the `core` type).
pub(super) fn map_movement(row: &dyn Row) -> Result<CashMovement> {
    Ok(CashMovement {
        id: sql::id(row, "id")?,
        session: sql::id(row, "cashier_session_id")?,
        kind: kind(row, "kind")?,
        amount: sql::money(row, "amount")?,
        reason: sql::opt_id(row, "reason_id")?,
        note: row.opt_text("note")?,
        reverses: sql::opt_id(row, "reverses")?,
    })
}

fn map(row: &dyn Row) -> Result<CashMovementRecord> {
    Ok(CashMovementRecord {
        movement: map_movement(row)?,
        number: row.text("number")?,
        branch: sql::id(row, "branch_id")?,
        register: sql::id(row, "register_id")?,
        user: sql::id(row, "user_id")?,
        approved_by: sql::opt_id(row, "approved_by")?,
        posted_at: sql::ts(row, "posted_at")?,
        reason_name: row.opt_text("reason_name")?,
        reverses_number: row.opt_text("reverses_number")?,
        reversed_by: sql::opt_id(row, "reversed_by")?,
        photos: count(row, "photos")?,
    })
}

/// One cash movement.
///
/// # Errors
/// The database error.
pub async fn get_cash_movement<'c>(
    conn: impl Into<Conn<'c>>,
    id: CashMovementId,
) -> Result<Option<CashMovementRecord>> {
    let sql = format!("{SELECT} WHERE m.id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map).await
}

/// One cash movement by its number.
///
/// # Errors
/// The database error.
pub async fn get_cash_movement_by_number<'c>(
    conn: impl Into<Conn<'c>>,
    number: &str,
) -> Result<Option<CashMovementRecord>> {
    let sql = format!("{SELECT} WHERE m.number = ?");
    sql::fetch_optional(conn.into(), &sql, args![number], map).await
}

/// Cash movements matching `filter`, newest first (`GET /cash-movements?…`).
///
/// # Errors
/// The database error.
pub async fn list_cash_movements<'c>(
    conn: impl Into<Conn<'c>>,
    filter: &CashFilter,
) -> Result<Vec<CashMovementRecord>> {
    let mut conditions: Vec<&str> = Vec::new();
    let mut values: Vec<Arg> = Vec::new();
    let mut add = |condition: &'static str, value: Arg| {
        conditions.push(condition);
        values.push(value);
    };
    if let Some(b) = filter.branch {
        add("m.branch_id = ?", b.into());
    }
    if let Some(r) = filter.register {
        add("m.register_id = ?", r.into());
    }
    if let Some(s) = filter.cashier_session {
        add("m.cashier_session_id = ?", s.into());
    }
    if let Some(k) = filter.kind {
        add("m.kind = ?", k.as_str().into());
    }
    if let Some(r) = filter.reason {
        add("m.reason_id = ?", r.into());
    }
    if let Some(from) = filter.from {
        add("m.posted_at >= ?", from.into());
    }
    if let Some(until) = filter.until {
        add("m.posted_at < ?", until.into());
    }
    if let Some(before) = filter.before {
        add("m.id < ?", before.into());
    }
    if let Some(flag) = filter.missing_nota_above {
        conditions.push(MISSING_NOTA);
        values.push(flag.into());
        values.push(flag.into());
    }
    let clause = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    values.push(clamp_limit(filter.limit, 50, MAX_CASH_PAGE).into());
    let sql = format!("{SELECT}{clause} ORDER BY m.id DESC LIMIT ?");
    sql::fetch_all(conn.into(), &sql, values, map).await
}

/// Whether movement `id` was posted already: `Some(stored)` when with the same body hash,
/// [`CashDbError::Conflict`] with another, `None` when the ID is new. The server asks first, so
/// a replay answers the stored movement even when the session closed since.
///
/// # Errors
/// [`CashDbError::Conflict`], or the database error.
pub async fn replayed_cash_movement(
    tx: &mut Tx,
    id: CashMovementId,
    hash: &TokenHash,
) -> CashResult<Option<CashMovementRecord>> {
    let stored = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT request_hash FROM cash_movement WHERE id = ?",
        args![id],
        |row| sql::hash(row, "request_hash"),
    )
    .await?;
    match stored {
        None => Ok(None),
        Some(h) if h == *hash => Ok(get_cash_movement(Conn::Tx(&mut *tx), id).await?),
        Some(_) => Err(CashDbError::Conflict("cash movement", *id.as_uuid())),
    }
}

/// Posts a cash movement in `tx` (`PUT /cash-movements/{id}`, spec 2.7 §2, §11): checks the
/// session and, for a reversal, its target; allocates the number; stores the movement; appends
/// the audit row `cash_movement.post` and the outbox event `cash_movement.posted`. Replaying the
/// same id with the same `request_hash` writes nothing and returns the stored movement
/// ([`Written::Unchanged`]).
///
/// Validation of the amount, reason direction, note and permissions is the caller's job
/// (`core::cash::CashMovement::new` / `reversal`, `check`). The database checks again what it
/// relies on:
/// - the session is open, on `register`, and `user`'s (else [`CashDbError::NoOpenSession`]);
/// - a new in / out names an existing, active, non-archived reason of its direction
///   (`invalid_field` `reason`; §2: an archived reason is refused for new movements);
/// - a reversal names a movement of the same session (`invalid_field` `reverses`) that is not a
///   reversal itself (same), is not reversed yet (`already_reversed`), and copies its kind,
///   amount and reason (`invalid_field` `reverses`).
///
/// # Errors
/// As above; [`CashDbError::Conflict`] when the id exists with another body;
/// [`CashDbError::Invalid`] (`register`) for a register of another branch;
/// [`CashDbError::NotFound`] (`register`); a retryable [`DbError::Conflict`] when another
/// transaction took the number or reversed the same movement.
pub async fn post_cash_movement(
    tx: &mut Tx,
    new: &NewCashMovement,
    now: Timestamp,
) -> CashResult<(Written, CashMovementRecord)> {
    let m = &new.movement;
    if let Some(stored) = replayed_cash_movement(tx, m.id, &new.request_hash).await? {
        return Ok((Written::Unchanged, stored));
    }
    check_session(tx, new).await?;
    match m.reverses {
        Some(target) => check_reversal(tx, m, target).await?,
        None => check_reason(tx, m).await?,
    }
    let (number, day, seq) = allocate(tx, new.branch, new.register, new.posted_at).await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO cash_movement (id, number, kind, amount, reason_id, note, branch_id, \
         register_id, cashier_session_id, user_id, approved_by, reverses, local_day, seq, \
         posted_at, request_hash, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            m.id,
            number.as_str(),
            m.kind.as_str(),
            m.amount,
            m.reason,
            m.note.as_deref(),
            new.branch,
            new.register,
            m.session,
            new.user,
            new.approved_by,
            m.reverses,
            day.key(),
            i64::from(seq),
            new.posted_at,
            &new.request_hash,
            now,
        ],
    )
    .await?;
    let payload = movement_json(new, &number);
    let actor = Actor {
        user: Some(new.user),
        branch: Some(new.branch),
        register: Some(new.register),
    };
    record(
        tx,
        &actor,
        now,
        Change {
            action: "cash_movement.post",
            target: format!("cash_movement:{}", m.id),
            before: None,
            after: json!({
                "number": number,
                "kind": m.kind.as_str(),
                "amount": m.amount.minor(),
                "reverses": m.reverses.map(|r| r.to_string()),
                "approved_by": new.approved_by.map(|u| u.to_string()),
            }),
            outbox: ("cash_movement.posted", &payload),
        },
    )
    .await?;
    let stored = get_cash_movement(Conn::Tx(&mut *tx), m.id)
        .await?
        .ok_or(CashDbError::NotFound("cash movement"))?;
    Ok((Written::Created, stored))
}

fn movement_json(new: &NewCashMovement, number: &str) -> Value {
    let m = &new.movement;
    json!({
        "id": m.id.to_string(),
        "number": number,
        "kind": m.kind.as_str(),
        "amount": m.amount.minor(),
        "reason_id": m.reason.map(|r| r.to_string()),
        "note": m.note,
        "branch_id": new.branch.to_string(),
        "register_id": new.register.to_string(),
        "cashier_session_id": m.session.to_string(),
        "user_id": new.user.to_string(),
        "approved_by": new.approved_by.map(|u| u.to_string()),
        "reverses": m.reverses.map(|r| r.to_string()),
        "posted_at": new.posted_at.unix_millis(),
    })
}

/// The movement's session must be the user's open session on the register.
async fn check_session(tx: &mut Tx, new: &NewCashMovement) -> CashResult<()> {
    let open = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT 1 FROM cashier_session WHERE id = ? AND register_id = ? AND user_id = ? \
         AND closed_at IS NULL",
        args![new.movement.session, new.register, new.user],
        |_| Ok(()),
    )
    .await?;
    open.ok_or(CashDbError::NoOpenSession)
}

/// A new in / out needs an existing, usable reason of its direction; a drop none.
async fn check_reason(tx: &mut Tx, m: &CashMovement) -> CashResult<()> {
    let Some(id) = m.reason else {
        return if m.kind == CashKind::Drop {
            Ok(())
        } else {
            Err(CashDbError::invalid("reason", "required"))
        };
    };
    let reason = super::get_cash_reason(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or_else(|| CashDbError::invalid("reason", "no such cash reason"))?;
    if m.kind.direction() != Some(reason.reason.direction) {
        return Err(CashDbError::invalid(
            "reason",
            "the reason is for the other direction",
        ));
    }
    if !reason.for_rules().active {
        return Err(CashDbError::invalid("reason", "the reason is archived"));
    }
    Ok(())
}

/// A reversal's target: same session, not a reversal, not reversed yet, and copied.
async fn check_reversal(tx: &mut Tx, m: &CashMovement, target: CashMovementId) -> CashResult<()> {
    let original = get_cash_movement(Conn::Tx(&mut *tx), target)
        .await?
        .ok_or_else(|| CashDbError::invalid("reverses", "no such cash movement"))?;
    let o = &original.movement;
    if o.session != m.session {
        return Err(CashDbError::invalid(
            "reverses",
            "not a movement of this open session",
        ));
    }
    if o.is_reversal() {
        return Err(CashDbError::invalid(
            "reverses",
            "a reversal cannot be reversed",
        ));
    }
    if original.reversed_by.is_some() {
        return Err(sultan_core::cash::CashError::AlreadyReversed.into());
    }
    if (o.kind, o.amount, o.reason) != (m.kind, m.amount, m.reason) {
        return Err(CashDbError::invalid(
            "reverses",
            "a reversal has the original's kind, amount and reason",
        ));
    }
    Ok(())
}

/// The next number of `register` on the branch-local day of `at` (§1, K9): number, day, seq.
async fn allocate(
    tx: &mut Tx,
    branch: BranchId,
    register: RegisterId,
    at: Timestamp,
) -> CashResult<(String, LocalDay, u32)> {
    let (register_branch, branch_code, register_code, timezone) = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT r.branch_id, b.code AS branch_code, r.code AS register_code, b.timezone \
         FROM register r JOIN branch b ON b.id = r.branch_id WHERE r.id = ?",
        args![register],
        |row| {
            Ok((
                sql::id::<BranchId>(row, "branch_id")?,
                row.text("branch_code")?,
                row.text("register_code")?,
                row.text("timezone")?,
            ))
        },
    )
    .await?
    .ok_or(CashDbError::NotFound("register"))?;
    if register_branch != branch {
        return Err(CashDbError::invalid(
            "register",
            "the register belongs to another branch",
        ));
    }
    let tz: BranchTimeZone = timezone
        .parse()
        .map_err(|e| DbError::Corrupt(format!("branch time zone: {e}")))?;
    let day = LocalDay::of(at, tz);
    let last = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT MAX(seq) AS seq FROM cash_movement WHERE register_id = ? AND local_day = ?",
        args![register, day.key()],
        |row| row.opt_i64("seq"),
    )
    .await?
    .flatten()
    .unwrap_or(0);
    let seq = u32::try_from(last + 1)
        .map_err(|_| DbError::Corrupt(format!("cash movement sequence {last} out of range")))?;
    Ok((
        cash_movement_number(&branch_code, &register_code, day, seq),
        day,
        seq,
    ))
}
