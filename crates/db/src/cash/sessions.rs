//! What cash management adds to a cashier session (spec 2.7 §3, §4, §5): its cash movements,
//! photo counts, no-sale drawer opens and X reads (the inputs of `core::cash::session_figures`),
//! the float kept at the close and proposed at the next open (K20), and the close-time
//! missing-nota count.

use std::collections::BTreeMap;

use sultan_core::cash::{CashFigures, CashMovement, proposed_float as propose};
use sultan_core::ids::{CashMovementId, CashierSessionId, RegisterId, ZReportId};
use sultan_core::money::Money;
use sultan_core::sale::SessionEvent;
use sultan_core::time::Timestamp;

use super::count;
use super::movements::map_movement;
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, args};

/// The audit action of a no-sale drawer open (`POST /drawer/open`, K22), target
/// `cashier_session:<id>`.
pub const DRAWER_OPEN_ACTION: &str = "drawer.open";

/// The audit action of an X read (K12), target `cashier_session:<id>`.
pub const X_REPORT_ACTION: &str = "session.x_report";

/// A session's cash inputs of `core::cash::SessionInput` (everything but the opening cash, the
/// sales events, the reasons and the company's `nota_flag_above`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionCash {
    /// The session's cash movements, reversals included, in posting order.
    pub movements: Vec<CashMovement>,
    /// The nota photos per movement (movements without one are left out).
    pub photos: BTreeMap<CashMovementId, usize>,
    /// No-sale drawer opens ([`DRAWER_OPEN_ACTION`] audit entries on the session).
    pub drawer_opens: u32,
    /// X reads ([`X_REPORT_ACTION`] audit entries on the session).
    pub x_reports: u32,
}

/// The cash columns of a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCashState {
    /// The float kept at the close (§3); `None` while open, or for a session closed before 2.7.
    pub float_kept: Option<Money>,
    /// The proposed opening cash, stored when the typed opening cash differed (§3).
    pub opening_proposed: Option<Money>,
    /// The cash outs missing a nota at the close (§4); `None` while open or before 2.7.
    pub missing_nota: Option<u32>,
    /// The Z that covers the session (§6).
    pub z_report: Option<ZReportId>,
}

/// The session's movements (in posting order), photo counts, drawer opens and X reads.
///
/// # Errors
/// The database error.
pub async fn session_cash<'c>(
    conn: impl Into<Conn<'c>>,
    session: CashierSessionId,
) -> Result<SessionCash> {
    let mut conn = conn.into();
    let movements = session_movements(conn.reborrow(), session).await?;
    let photos = sql::fetch_all(
        conn.reborrow(),
        "SELECT p.movement_id, COUNT(*) AS n FROM cash_movement_photo p \
         JOIN cash_movement m ON m.id = p.movement_id WHERE m.cashier_session_id = ? \
         GROUP BY p.movement_id",
        args![session],
        |row| {
            Ok((
                sql::id::<CashMovementId>(row, "movement_id")?,
                count(row, "n")? as usize,
            ))
        },
    )
    .await?
    .into_iter()
    .collect();
    let target = format!("cashier_session:{session}");
    let audits = sql::fetch_all(
        conn,
        "SELECT action, COUNT(*) AS n FROM audit_log WHERE target = ? AND action IN (?, ?) \
         GROUP BY action",
        args![target.as_str(), DRAWER_OPEN_ACTION, X_REPORT_ACTION],
        |row| Ok((row.text("action")?, count(row, "n")?)),
    )
    .await?;
    let of = |action: &str| {
        audits
            .iter()
            .find(|(a, _)| a == action)
            .map_or(0, |(_, n)| *n)
    };
    Ok(SessionCash {
        movements,
        photos,
        drawer_opens: of(DRAWER_OPEN_ACTION),
        x_reports: of(X_REPORT_ACTION),
    })
}

async fn session_movements(conn: Conn<'_>, session: CashierSessionId) -> Result<Vec<CashMovement>> {
    sql::fetch_all(
        conn,
        "SELECT id, cashier_session_id, kind, amount, reason_id, note, reverses \
         FROM cash_movement WHERE cashier_session_id = ? ORDER BY posted_at, id",
        args![session],
        map_movement,
    )
    .await
}

/// The session's cash movements as the netted session events of the close report (spec 2.7
/// §16 reading 4): `CashIn`, `CashOut` and `Drop`, each the kind's originals minus its reversals
/// and left out when 0. Add them to [`crate::sales::session_events`] for
/// `core::sale::session_report` and `core::cash::close_movements`; `core::cash::session_figures`
/// instead takes the movements themselves ([`session_cash`]).
///
/// # Errors
/// The database error; [`DbError::Corrupt`] when the stored amounts overflow.
pub async fn session_cash_events<'c>(
    conn: impl Into<Conn<'c>>,
    session: CashierSessionId,
) -> Result<Vec<SessionEvent>> {
    let movements = session_movements(conn.into(), session).await?;
    let figures = CashFigures::from_movements(&movements, &[])
        .map_err(|e| DbError::Corrupt(format!("cash movements of session {session}: {e}")))?;
    Ok(figures.events())
}

/// The company's `nota_flag_above` (§2a; `0` = off).
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] when the settings row is missing.
pub async fn nota_flag_above<'c>(conn: impl Into<Conn<'c>>) -> Result<Money> {
    sql::fetch_optional(
        conn.into(),
        "SELECT nota_flag_above FROM sales_settings WHERE id = 1",
        args![],
        |row| sql::money(row, "nota_flag_above"),
    )
    .await?
    .ok_or_else(|| DbError::Corrupt("sales_settings row is missing".into()))
}

/// When cash management was installed (`sales_settings.cash_since`, migration
/// `20261121000001_cash_since`): the upgrade of an existing install or the first migration run of
/// a fresh one. The days still to close start at its branch-local day (spec 2.7 §17 reading 10,
/// [`super::pending_days`]); `None` lists every day.
///
/// # Errors
/// The database error.
pub async fn cash_since<'c>(conn: impl Into<Conn<'c>>) -> Result<Option<Timestamp>> {
    Ok(sql::fetch_optional(
        conn.into(),
        "SELECT cash_since FROM sales_settings WHERE id = 1",
        args![],
        |row| sql::opt_ts(row, "cash_since"),
    )
    .await?
    .flatten())
}

/// The cash columns of a session; `None` for an unknown session.
///
/// # Errors
/// The database error.
pub async fn session_cash_state<'c>(
    conn: impl Into<Conn<'c>>,
    session: CashierSessionId,
) -> Result<Option<SessionCashState>> {
    sql::fetch_optional(
        conn.into(),
        "SELECT float_kept, opening_proposed, missing_nota, z_report_id FROM cashier_session \
         WHERE id = ?",
        args![session],
        |row| {
            Ok(SessionCashState {
                float_kept: row.opt_i64("float_kept")?.map(Money::from_minor),
                opening_proposed: row.opt_i64("opening_proposed")?.map(Money::from_minor),
                missing_nota: row
                    .opt_i64("missing_nota")?
                    .map(|n| {
                        u32::try_from(n).map_err(|_| DbError::Corrupt(format!("missing_nota {n}")))
                    })
                    .transpose()?,
                z_report: sql::opt_id(row, "z_report_id")?,
            })
        },
    )
    .await
}

/// The opening cash proposed on `register` (§3, `core::cash::proposed_float`): the float kept by
/// its last closed session (by close time), 0 when there is none or it kept none.
///
/// # Errors
/// The database error.
pub async fn proposed_float<'c>(conn: impl Into<Conn<'c>>, register: RegisterId) -> Result<Money> {
    Ok(propose(last_float_kept(conn, register).await?))
}

/// The float kept by the last closed session of `register` (by close time); `None` when the
/// register has no closed session or the last one kept none (closed before 2.7). The server stores
/// no `opening_proposed` then (spec 2.7 §18): there is nothing to compare the opening cash with.
///
/// # Errors
/// The database error.
pub async fn last_float_kept<'c>(
    conn: impl Into<Conn<'c>>,
    register: RegisterId,
) -> Result<Option<Money>> {
    Ok(sql::fetch_optional(
        conn.into(),
        "SELECT float_kept FROM cashier_session WHERE register_id = ? AND closed_at IS NOT NULL \
         ORDER BY closed_at DESC, id DESC LIMIT 1",
        args![register],
        |row| row.opt_i64("float_kept"),
    )
    .await?
    .flatten()
    .map(Money::from_minor))
}

/// Stores the proposal of an open session whose typed opening cash differed from it (§3,
/// `core::cash::opening_proposed`), in the open transaction after
/// [`crate::cashier_sessions::insert`].
///
/// # Errors
/// [`DbError::NotFound`] when the session is not open or has a proposal already.
pub async fn set_opening_proposed(
    tx: &mut Tx,
    session: CashierSessionId,
    proposed: Money,
) -> Result<()> {
    let changed = sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE cashier_session SET opening_proposed = ? WHERE id = ? AND closed_at IS NULL \
         AND opening_proposed IS NULL",
        args![proposed, session],
    )
    .await?;
    if changed == 0 {
        return Err(DbError::NotFound("open cashier session"));
    }
    Ok(())
}

/// Stores the float kept (§3) and the missing-nota count (§4) of a session, in the close
/// transaction after [`crate::cashier_sessions::close`]. Written once.
///
/// # Errors
/// [`DbError::NotFound`] when the session is not closed or has them already.
pub async fn record_close_cash(
    tx: &mut Tx,
    session: CashierSessionId,
    float_kept: Money,
    missing_nota: u32,
) -> Result<()> {
    let changed = sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE cashier_session SET float_kept = ?, missing_nota = ? WHERE id = ? \
         AND closed_at IS NOT NULL AND float_kept IS NULL",
        args![float_kept, i64::from(missing_nota), session],
    )
    .await?;
    if changed == 0 {
        return Err(DbError::NotFound("closed cashier session"));
    }
    Ok(())
}
