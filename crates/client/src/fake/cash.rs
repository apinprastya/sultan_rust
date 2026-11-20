//! The fake's cash management (spec 2.7, phase K3), close enough to the server for the register
//! and back-office UI tests of K4 and K5.
//!
//! **Rules kept** (all from `core::cash`): the seeded cash reasons (the server's IDs) and HQ
//! edits with a fixed direction; a cash in / out / drop needs the caller's open session
//! (`409 no_open_session`), a reason of the kind's direction (none for a drop), an active reason
//! and the note rules (`422 invalid_field`); a reversal names a movement of the same session,
//! not reversed (`409 already_reversed`), not a reversal, and needs a note; numbers
//! `JKT01-R1-K-<YYMMDD>-<NN>`; a replay of the same body answers the stored movement, another
//! body `409 conflict`. Permissions: a register user marked
//! [`super::FakeUser::with_reports`] (Budi) is a Manager (`cash.out` up to Rp 2 000 000,
//! `cash.drop`, `session.view_expected`, `day.close`, `sale.reprint`); the others are Cashiers
//! (`cash.in`, `cash.out` up to Rp 50 000). Anything above needs an `X-Override` token for that
//! action, value and context (the movement ID, the session ID, `branch-1:<day>`), like the
//! server. The close report gets the cash block, the float kept (`422 invalid_field`
//! `float_kept` above the count) and the proposed opening of the next session; the X counts
//! itself; the Z (`core::cash::check_z_run`, `build_z`) covers the closed sessions of a business
//! day not in a Z yet (`409 sessions_open` / `nothing_to_close`, a future day
//! `422 invalid_field`), numbered `JKT01-Z-<YYMMDD>-<NN>`, with an idempotency key. Texts come
//! from `core::cash` (slip, X, close report, Z) with the fake's branch block.
//!
//! **Simplified:** one register, no photos (`missing_nota` at the default Rp 100 000), no
//! events, a session's sales are the fake's `cash_sales`, and a Z has no sales documents (no
//! sales count or tax lines).

use std::collections::HashMap;

use sultan_core::cash::{
    CashError, CashKind, CashMovement as Core, CashReason as CoreReason, CloseFigures,
    CloseReportInput, DEFAULT_NOTA_FLAG_ABOVE, Direction, Printout, SessionFigures, SessionHeader,
    SessionInput, SessionSales, SlipInput, SlipKind, TextOptions, XReportInput, ZCandidate,
    ZReport as CoreZ, ZReportInput, ZSession, ZTotals as CoreTotals, build_close_report,
    build_slip, build_x_report, build_z, build_z_report, check_z_run, close_figures,
    is_missing_nota, session_figures, x_report,
};
use sultan_core::doc_number::{BranchTimeZone, LocalDay, cash_movement_number, z_number};
use sultan_core::ids::{CashMovementId, CashReasonId, CashierSessionId};
use sultan_core::money::Money;
use sultan_core::sale::{
    CountAmount as CoreCount, ReceiptHeader, ReceiptWidth, SalesSessionReport, SessionEvent,
    TenderKey, TenderKind,
};
use sultan_core::time::Timestamp;

use super::{BRANCH_ID, FakeService, FakeUser, REGISTER_ID, State, api, ready};
use crate::cash::{
    CashMovement, CashMovementPage, CashMovementRequest, CashQuery, CashReason, CashReasonBody,
    CashReport, CashText, CountAmount, FloatInfo, PendingDay, ProposedFloat, ReasonLine, SaleRef,
    SalesReport, SalesTender, TaxLine, XReport, ZQuery, ZRegister, ZReport, ZReportPage,
    ZRunRequest, ZSessionLine, ZSnapshot, ZSummary, ZTotals,
};
use crate::{CashierSession, ClientError, CloseReport, CloseRequest, Problem, TenderTotal};

/// The seeded cash reasons' IDs (as on the server): top-up, other (in), shop supplies, delivery /
/// parking, cleaning / security, other (out).
pub const SEED_REASONS: [&str; 6] = [
    "00000000-0000-7000-8000-000000000401",
    "00000000-0000-7000-8000-000000000402",
    "00000000-0000-7000-8000-000000000403",
    "00000000-0000-7000-8000-000000000404",
    "00000000-0000-7000-8000-000000000405",
    "00000000-0000-7000-8000-000000000406",
];

/// A Manager's `cash.out` limit (the seed), and a Cashier's.
const MANAGER_CASH_OUT: i64 = 2_000_000;
const CASHIER_CASH_OUT: i64 = 50_000;

struct Stored {
    request: CashMovementRequest,
    movement: CashMovement,
    core: Core,
}

/// The fake's cash state.
pub(super) struct Cash {
    reasons: Vec<CashReason>,
    movements: Vec<Stored>,
    sessions: HashMap<String, CashierSessionId>,
    closes: HashMap<String, CloseFigures>,
    proposed: HashMap<String, i64>,
    x_reads: HashMap<String, u32>,
    z: Vec<(ZReport, CoreZ)>,
    in_z: HashMap<String, String>,
    keyed_z: HashMap<String, ZReport>,
}

impl Default for Cash {
    fn default() -> Self {
        let reasons = sultan_core::cash::SEED_REASONS
            .iter()
            .zip(SEED_REASONS)
            .map(|(r, id)| CashReason {
                id: id.to_owned(),
                name: r.name_id.to_owned(),
                direction: r.direction.as_str().to_owned(),
                note_required: r.note_required,
                active: true,
                sort: r.sort,
                archived: false,
            })
            .collect();
        Self {
            reasons,
            movements: Vec::new(),
            sessions: HashMap::new(),
            closes: HashMap::new(),
            proposed: HashMap::new(),
            x_reads: HashMap::new(),
            z: Vec::new(),
            in_z: HashMap::new(),
            keyed_z: HashMap::new(),
        }
    }
}

fn problem(status: u16, code: &str) -> Problem {
    Problem::new(status, code, code)
}

fn invalid(field: &str) -> ClientError {
    problem(422, "invalid_field").with("field", field).into()
}

fn denied(permission: &str) -> ClientError {
    problem(403, "permission_denied")
        .with("permission", permission)
        .into()
}

/// A `core::cash` rule error as the server answers it.
fn rule(error: &CashError) -> ClientError {
    let status = match error {
        CashError::InvalidField { .. } | CashError::AmountOverflow => 422,
        _ => 409,
    };
    let mut p = Problem::new(status, error.code(), &error.to_string());
    if let Some(field) = error.field() {
        p = p.with("field", field);
    }
    if let CashError::SessionsOpen { numbers } = error {
        p = p.with("sessions", numbers.clone());
    }
    p.into()
}

fn tz() -> BranchTimeZone {
    BranchTimeZone::default()
}

fn rp(minor: i64) -> Money {
    Money::from_minor(minor)
}

fn day_text(day: LocalDay) -> String {
    let (y, mo, d) = day.ymd();
    format!("{y:04}-{mo:02}-{d:02}")
}

fn parse_day(value: &str) -> Option<LocalDay> {
    let b = value.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    value
        .replace('-', "")
        .parse()
        .ok()
        .and_then(LocalDay::from_key)
}

/// The caller of a cash call: a register user, or a back-office user.
enum Who {
    Till(FakeUser),
    Office {
        id: String,
        name: String,
        reports: bool,
        owner: bool,
    },
}

impl Who {
    fn id(&self) -> &str {
        match self {
            Self::Till(u) => &u.id,
            Self::Office { id, .. } => id,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Till(u) => &u.name,
            Self::Office { name, .. } => name,
        }
    }

    /// `report.sales` (and the manager's other cash permissions).
    const fn reports(&self) -> bool {
        match self {
            Self::Till(u) => u.reports,
            Self::Office { reports, .. } => *reports,
        }
    }
}

/// Whether `user` holds `permission` for `value` without an override.
fn holds(user: &FakeUser, permission: &str, value: Option<i64>) -> bool {
    match permission {
        "cash.in" => true,
        "cash.out" => value.unwrap_or(0) <= cash_out_limit(user),
        _ => user.reports,
    }
}

const fn cash_out_limit(user: &FakeUser) -> i64 {
    if user.reports {
        MANAGER_CASH_OUT
    } else {
        CASHIER_CASH_OUT
    }
}

/// Whether `approver` may approve `action` for `value` (`POST /overrides` of a cash action).
pub(super) fn may_approve(approver: &FakeUser, action: &str, value: Option<i64>) -> bool {
    match action {
        "cash.out" => value.unwrap_or(0) <= cash_out_limit(approver),
        _ => approver.reports,
    }
}

/// Whether `action` is one of the fake's cash permissions (its override limits are Rupiah).
pub(super) fn is_cash_action(action: &str) -> bool {
    matches!(
        action,
        "cash.in" | "cash.out" | "cash.drop" | "day.close" | "session.view_expected"
    )
}

fn count(c: CoreCount) -> CountAmount {
    CountAmount {
        count: c.count,
        amount: c.amount.minor(),
    }
}

/// The client's sales block of `core`'s.
pub(super) fn sales_report(r: &SalesSessionReport) -> SalesReport {
    SalesReport {
        opening_cash: r.opening_cash.minor(),
        cash_received: r.cash_received.minor(),
        change_given: r.change_given.minor(),
        cash_refunds: r.cash_refunds.minor(),
        cash_rounding: r.cash_rounding.minor(),
        cash_in: r.cash_in.minor(),
        cash_out: r.cash_out.minor(),
        drops: r.drops.minor(),
        payouts: r.payouts.minor(),
        expected_cash: r.expected_cash.minor(),
        tenders: r
            .tenders
            .iter()
            .map(|(k, t)| SalesTender {
                tender_type_id: k.tender_type.map(|t| t.to_string()),
                kind: k.kind.as_str().to_owned(),
                sales: count(t.sales),
                refunds: count(t.refunds),
            })
            .collect(),
        line_voids: count(r.line_voids),
        cart_voids: count(r.cart_voids),
        post_voids: count(r.post_voids),
        returns: count(r.returns),
        line_discounts: count(r.line_discounts),
        cart_discounts: count(r.cart_discounts),
        price_overrides: count(r.price_overrides),
        saved_change: r.saved_change.minor(),
        balance_paid: r.balance_paid.minor(),
    }
}

/// The client's cash block of `core`'s figures.
pub(super) fn cash_report(f: &SessionFigures) -> CashReport {
    let c = &f.cash;
    let mut lines: Vec<_> = c.reasons.iter().collect();
    lines.sort_by(|(_, a), (_, b)| {
        (a.direction, a.sort, &a.name).cmp(&(b.direction, b.sort, &b.name))
    });
    CashReport {
        cash_in: count(c.cash_in),
        cash_out: count(c.cash_out),
        drops: count(c.drops),
        reversals: count(c.reversals),
        reasons: lines
            .into_iter()
            .map(|(id, l)| ReasonLine {
                reason_id: id.to_string(),
                name: l.name.clone(),
                direction: l.direction.as_str().to_owned(),
                sort: l.sort,
                count: l.count,
                amount: l.amount.minor(),
                reversals: l.reversals,
            })
            .collect(),
        drawer_opens: f.drawer_opens,
        x_reports: f.x_reports,
        missing_nota: f.missing_nota,
    }
}

fn totals(t: &CoreTotals) -> ZTotals {
    let sale = |s: &sultan_core::cash::SaleRef| SaleRef {
        number: s.number.clone(),
        posted_at: s.posted_at.unix_millis(),
    };
    ZTotals {
        sessions: t.sessions,
        sales_count: t.sales_count,
        gross_sales: t.gross_sales.minor(),
        net_sales: t.net_sales.minor(),
        first_sale: t.first_sale.as_ref().map(sale),
        last_sale: t.last_sale.as_ref().map(sale),
        tax: t
            .tax
            .iter()
            .map(|l| TaxLine {
                rate_bp: l.rate_bp,
                base: l.base.minor(),
                tax: l.tax.minor(),
            })
            .collect(),
        expected_cash: t.figures.expected_cash().minor(),
        counted_cash: t.counted_cash.minor(),
        over_short: t.over_short.minor(),
        float_kept: t.float_kept.minor(),
        to_safe: t.to_safe.minor(),
        float_difference: t.float_difference.minor(),
        cash_to_deposit: t.cash_to_deposit.minor(),
        sales: sales_report(&t.figures.sales),
        cash: cash_report(&t.figures),
    }
}

fn snapshot(z: &CoreZ) -> ZSnapshot {
    ZSnapshot {
        number: z.number.clone(),
        day: day_text(z.day),
        seq: z.seq,
        supplementary: z.supplementary,
        run_at: z.run_at.unix_millis(),
        sessions: z
            .sessions
            .iter()
            .map(|s| ZSessionLine {
                number: s.number.clone(),
                register: s.register.clone(),
                cashier: s.cashier.clone(),
                opened_at: s.opened_at.unix_millis(),
                closed_at: s.closed_at.unix_millis(),
                forced: s.forced,
                opening_cash: s.opening_cash.minor(),
                expected_cash: s.expected_cash.minor(),
                counted_cash: s.counted_cash.minor(),
                over_short: s.over_short.minor(),
                float_kept: s.float_kept.minor(),
                to_safe: s.to_safe.minor(),
                float_difference: s.float_difference.minor(),
            })
            .collect(),
        registers: z
            .registers
            .iter()
            .map(|(r, t)| ZRegister {
                register: r.clone(),
                totals: totals(t),
            })
            .collect(),
        branch: totals(&z.branch),
    }
}

/// Builds a text with the fake's branch block (`Toko Jakarta`, Asia/Jakarta, 32 columns).
fn text(
    lang: &str,
    copy: bool,
    build: impl FnOnce(ReceiptHeader<'_>, &TextOptions<'_>) -> Printout,
) -> CashText {
    let label = |l: sultan_core::cash::CashLabel| l.text(lang).to_owned();
    let tender = |k: TenderKey| k.kind.as_str().to_owned();
    let opts = TextOptions {
        width: ReceiptWidth::default(),
        tz: tz(),
        label: &label,
        tender_name: &tender,
    };
    let header = ReceiptHeader {
        lines: &[],
        branch_name: "Toko Jakarta",
        address: "",
        tax_id: None,
    };
    let p = build(header, &opts);
    CashText {
        copy,
        text: p.text(),
        lines: p.lines,
        large: p.large,
    }
}

impl State {
    fn who(&self, token: &str) -> Result<Who, ClientError> {
        if let Ok(user) = self.caller(token) {
            return Ok(Who::Till(user));
        }
        let user = self.bo_user(token)?;
        Ok(Who::Office {
            reports: user.manage || user.manage_here,
            owner: user.manage,
            id: user.id,
            name: user.name,
        })
    }

    /// `Ok(approver)` when `user` holds `permission` for `value`, or a token approves it on
    /// `context` (the server's `overrides::authorize`).
    fn cash_authorize(
        &mut self,
        user: &FakeUser,
        permission: &str,
        value: Option<i64>,
        context: &str,
        token: Option<&str>,
    ) -> Result<Option<String>, ClientError> {
        if holds(user, permission, value) {
            return Ok(None);
        }
        let Some(token) = token else {
            let mut p = problem(403, "needs_override").with("permission", permission);
            if permission == "cash.out" {
                p = p.with("limit", cash_out_limit(user));
            }
            return Err(p.into());
        };
        match self.overrides.get(token).cloned() {
            Some((a, v, c, approver)) if a == permission && v == value && c == context => {
                self.overrides.remove(token);
                Ok(Some(approver))
            }
            _ => Err(api(403, "invalid_override")),
        }
    }

    fn core_session(&mut self, session: &str) -> CashierSessionId {
        *self
            .cash
            .sessions
            .entry(session.to_owned())
            .or_insert_with(CashierSessionId::generate)
    }

    fn core_reasons(&self) -> Vec<CoreReason> {
        self.cash
            .reasons
            .iter()
            .filter_map(|r| {
                Some(CoreReason {
                    id: r.id.parse().ok()?,
                    name: r.name.clone(),
                    direction: Direction::from_key(&r.direction)?,
                    active: r.active && !r.archived,
                    sort: r.sort,
                    note_required: r.note_required,
                })
            })
            .collect()
    }

    /// The movement of a request (see the module docs).
    fn build_movement(
        &mut self,
        id: CashMovementId,
        session: &str,
        body: &CashMovementRequest,
    ) -> Result<Core, ClientError> {
        let core_session = self.core_session(session);
        if let Some(target) = &body.reverses {
            let Some(original) = self
                .cash
                .movements
                .iter()
                .find(|m| m.movement.id == *target)
            else {
                return Err(invalid("reverses"));
            };
            let o = &original.core;
            if body.kind.as_deref().is_some_and(|k| k != o.kind.as_str())
                || body.amount.is_some_and(|a| a != o.amount.minor())
            {
                return Err(invalid("reverses"));
            }
            let reversed = original.movement.reversed_by.is_some();
            return Core::reversal(id, core_session, o, reversed, body.note.as_deref())
                .map_err(|e| rule(&e));
        }
        let kind = body
            .kind
            .as_deref()
            .and_then(CashKind::from_key)
            .ok_or_else(|| invalid("kind"))?;
        let amount = body.amount.ok_or_else(|| invalid("amount"))?;
        let reason = match &body.reason_id {
            Some(r) => Some(
                self.core_reasons()
                    .into_iter()
                    .find(|c| c.id.to_string() == *r)
                    .ok_or_else(|| invalid("reason"))?,
            ),
            None => None,
        };
        Core::new(
            id,
            core_session,
            kind,
            rp(amount),
            reason.as_ref(),
            body.note.as_deref(),
        )
        .map_err(|e| rule(&e))
    }

    fn post_cash(
        &mut self,
        token: &str,
        id: &str,
        body: &CashMovementRequest,
        override_token: Option<&str>,
    ) -> Result<CashMovement, ClientError> {
        let user = self.caller(token)?;
        if let Some(stored) = self.cash.movements.iter().find(|m| m.movement.id == id) {
            return if stored.request == *body {
                Ok(stored.movement.clone())
            } else {
                Err(api(409, "conflict"))
            };
        }
        let session = self
            .session
            .clone()
            .filter(|s| s.user_id == user.id)
            .ok_or_else(|| api(409, "no_open_session"))?;
        let mid: CashMovementId = id
            .parse()
            .map_err(|_| ClientError::from(problem(400, "invalid_body")))?;
        let core = self.build_movement(mid, &session.id, body)?;
        let value = core
            .kind
            .requested(core.amount)
            .map(|_| core.amount.minor());
        let approved_by = self.cash_authorize(
            &user,
            core.kind.permission().as_str(),
            value,
            id,
            override_token,
        )?;
        let day = LocalDay::of(Timestamp::from_unix_millis(self.now), tz());
        let prefix = format!("JKT01-R1-K-{}-", day.yymmdd());
        let seq = self
            .cash
            .movements
            .iter()
            .filter(|m| m.movement.number.starts_with(&prefix))
            .count()
            + 1;
        let reason_name = core.reason.and_then(|r| {
            self.cash
                .reasons
                .iter()
                .find(|x| x.id == r.to_string())
                .map(|x| x.name.clone())
        });
        let reverses_number = core.reverses.and_then(|r| {
            self.cash
                .movements
                .iter()
                .find(|m| m.movement.id == r.to_string())
                .map(|m| m.movement.number.clone())
        });
        let movement = CashMovement {
            id: id.to_owned(),
            number: cash_movement_number(
                "JKT01",
                "R1",
                day,
                u32::try_from(seq).unwrap_or(u32::MAX),
            ),
            kind: core.kind.as_str().to_owned(),
            amount: core.amount.minor(),
            effect: core.effect().minor(),
            reason_id: core.reason.map(|r| r.to_string()),
            reason_name,
            note: core.note.clone(),
            branch_id: BRANCH_ID.to_owned(),
            register_id: REGISTER_ID.to_owned(),
            cashier_session_id: session.id.clone(),
            user_id: user.id.clone(),
            approved_by,
            reverses: core.reverses.map(|r| r.to_string()),
            reverses_number,
            reversed_by: None,
            posted_at: self.now,
            photo_ids: Vec::new(),
            missing_nota: is_missing_nota(&core, 0, false, DEFAULT_NOTA_FLAG_ABOVE),
        };
        if let Some(target) = &movement.reverses
            && let Some(original) = self
                .cash
                .movements
                .iter_mut()
                .find(|m| m.movement.id == *target)
        {
            original.movement.reversed_by = Some(id.to_owned());
            original.movement.missing_nota = false;
        }
        self.cash.movements.push(Stored {
            request: body.clone(),
            movement: movement.clone(),
            core,
        });
        Ok(movement)
    }

    fn movement_readable(who: &Who, m: &CashMovement) -> Result<(), ClientError> {
        if who.reports() || m.user_id == who.id() {
            Ok(())
        } else {
            Err(denied("report.sales"))
        }
    }

    fn list_cash(&self, token: &str, q: &CashQuery) -> Result<CashMovementPage, ClientError> {
        let who = self.who(token)?;
        let own = q.cashier_session_id.as_ref().is_some_and(|s| {
            self.history
                .iter()
                .any(|(h, _)| h.id == *s && h.user_id == who.id())
        });
        if !own && !who.reports() {
            return Err(denied("report.sales"));
        }
        let limit = q.limit.unwrap_or(50) as usize;
        let newest = self.cash.movements.iter().rev().map(|m| &m.movement);
        let after: Vec<&CashMovement> = match &q.before_id {
            None => newest.collect(),
            Some(c) => newest.skip_while(|m| m.id != *c).skip(1).collect(),
        };
        let items: Vec<CashMovement> = after
            .into_iter()
            .filter(|m| {
                q.cashier_session_id
                    .as_ref()
                    .is_none_or(|s| *s == m.cashier_session_id)
                    && q.kind.as_ref().is_none_or(|k| *k == m.kind)
                    && q.reason_id
                        .as_ref()
                        .is_none_or(|r| Some(r) == m.reason_id.as_ref())
                    && q.branch_id.as_ref().is_none_or(|b| *b == m.branch_id)
                    && q.register_id.as_ref().is_none_or(|r| *r == m.register_id)
                    && q.from.is_none_or(|f| m.posted_at >= f)
                    && q.until.is_none_or(|u| m.posted_at < u)
                    && (q.missing_nota != Some(true) || m.missing_nota)
            })
            .take(limit)
            .cloned()
            .collect();
        let next_before_id = items
            .last()
            .filter(|_| items.len() == limit)
            .map(|m| m.id.clone());
        Ok(CashMovementPage {
            items,
            next_before_id,
        })
    }

    /// Whether `session` is the caller's open session (a copy of its slips is free).
    fn own_open(&self, who: &Who, session: &str) -> bool {
        matches!(who, Who::Till(_))
            && self
                .session
                .as_ref()
                .is_some_and(|s| s.id == session && s.user_id == who.id())
    }

    /// A copy outside the own open session needs `sale.reprint`.
    fn reprint(
        &mut self,
        who: &Who,
        free: bool,
        context: &str,
        token: Option<&str>,
    ) -> Result<(), ClientError> {
        if free {
            return Ok(());
        }
        match who {
            Who::Till(user) => {
                self.cash_authorize(user, "sale.reprint", None, context, token)?;
                Ok(())
            }
            Who::Office { reports: true, .. } => Ok(()),
            Who::Office { .. } => Err(denied("sale.reprint")),
        }
    }

    fn slip(
        &mut self,
        token: &str,
        id: &str,
        copy: bool,
        lang: &str,
        override_token: Option<&str>,
    ) -> Result<CashText, ClientError> {
        let who = self.who(token)?;
        let m = self
            .cash
            .movements
            .iter()
            .find(|m| m.movement.id == id)
            .map(|m| m.movement.clone())
            .ok_or_else(|| api(404, "not_found"))?;
        Self::movement_readable(&who, &m)?;
        if copy {
            let free = self.own_open(&who, &m.cashier_session_id);
            self.reprint(&who, free, id, override_token)?;
        }
        let cashier = self
            .user(&m.user_id)
            .map(|u| u.name.clone())
            .unwrap_or_default();
        let approver = m
            .approved_by
            .as_ref()
            .and_then(|a| self.user(a))
            .map(|u| u.name.clone());
        let kind = CashKind::from_key(&m.kind).ok_or_else(|| api(500, "internal"))?;
        Ok(text(lang, copy, |header, opts| {
            build_slip(
                &SlipInput {
                    header,
                    kind: SlipKind::from(kind),
                    number: &m.number,
                    posted_at: Timestamp::from_unix_millis(m.posted_at),
                    register: "R1",
                    cashier: &cashier,
                    approved_by: approver.as_deref(),
                    reversal_of: m.reverses_number.as_deref(),
                    reason: m.reason_name.as_deref(),
                    note: m.note.as_deref(),
                    customer: None,
                    balance_after: None,
                    amount: rp(m.amount),
                    copy,
                },
                opts,
            )
        }))
    }

    /// The figures of `session` as of now (`missing` replaces the missing-nota count).
    fn figures(&self, session: &CashierSession) -> SessionFigures {
        let mut events = Vec::new();
        if self.cash_sales > 0 && session.status == "open" {
            events.push(SessionEvent::Tender {
                key: TenderKey {
                    kind: TenderKind::Cash,
                    tender_type: None,
                },
                amount: rp(self.cash_sales),
                cash_received: Some(rp(self.cash_sales)),
                change_given: Money::ZERO,
                change_saved: Money::ZERO,
            });
        }
        let paid = self.customers.paid_out(&session.id);
        if paid > 0 {
            events.push(SessionEvent::Payout(rp(paid)));
        }
        let movements: Vec<Core> = self
            .cash
            .movements
            .iter()
            .filter(|s| s.movement.cashier_session_id == session.id)
            .map(|s| s.core.clone())
            .collect();
        session_figures(&SessionInput {
            opening_cash: rp(session.opening_cash),
            events: &events,
            movements: &movements,
            reasons: &self.core_reasons(),
            photos: &std::collections::BTreeMap::new(),
            nota_flag_above: DEFAULT_NOTA_FLAG_ABOVE,
            drawer_opens: self.sales.drawer_opens(&session.id),
            x_reports: self.cash.x_reads.get(&session.id).copied().unwrap_or(0),
        })
        .unwrap_or_default()
    }

    fn x_read(
        &mut self,
        token: &str,
        session_id: &str,
        override_token: Option<&str>,
    ) -> Result<(CashierSession, sultan_core::cash::XReport, Option<String>), ClientError> {
        let who = self.who(token)?;
        let Some(session) = self
            .history
            .iter()
            .find(|(h, _)| h.id == session_id)
            .map(|(h, _)| h.clone())
        else {
            return Err(api(404, "not_found"));
        };
        if session.status != "open" {
            return Err(problem(409, "session_closed")
                .with("session_id", session_id)
                .into());
        }
        let approver = match &who {
            Who::Till(user) if user.id == session.user_id => self.cash_authorize(
                user,
                "session.view_expected",
                None,
                session_id,
                override_token,
            )?,
            other if other.reports() => None,
            _ => return Err(denied("report.sales")),
        };
        *self.cash.x_reads.entry(session_id.to_owned()).or_default() += 1;
        let figures = self.figures(&session);
        let proposed = self.cash.proposed.get(session_id).copied().map(rp);
        let report = x_report(figures, proposed, Timestamp::from_unix_millis(self.now))
            .map_err(|e| rule(&e))?;
        Ok((session, report, approver))
    }

    /// Closes the open session `session_id` (blind close or force close) with the cash block and
    /// the float (spec 2.7 §3, §4).
    pub(super) fn close_with_cash(
        &mut self,
        cashier: &str,
        session_id: &str,
        body: &CloseRequest,
        forced_by: Option<String>,
    ) -> Result<CloseReport, ClientError> {
        let Some(mut session) = self.session.clone().filter(|o| o.id == session_id) else {
            return Err(problem(409, "session_closed")
                .with("session_id", session_id)
                .into());
        };
        let figures = self.figures(&session);
        let proposed = self.cash.proposed.get(session_id).copied().map(rp);
        let close = close_figures(
            figures,
            rp(body.counted_cash),
            body.float_kept.map(rp),
            proposed,
        )
        .map_err(|e| rule(&e))?;
        let expected = close.session.expected_cash().minor();
        let payouts = self.customers.paid_out(&session.id);
        let sales = &close.session.sales;
        "closed".clone_into(&mut session.status);
        session.closed_at = Some(self.now);
        session.expected_cash = Some(expected);
        session.float_kept = Some(close.float.float_kept.minor());
        self.session = None;
        let tenders = if self.cash_sales > 0 {
            vec![TenderTotal {
                tender: "cash".to_owned(),
                sales: self.cash_sales,
                refunds: 0,
                net: self.cash_sales,
                count: 3,
            }]
        } else {
            Vec::new()
        };
        let report = CloseReport {
            forced: forced_by.is_some(),
            branch_code: "JKT01".to_owned(),
            branch_name: "Toko Jakarta".to_owned(),
            timezone: "Asia/Jakarta".to_owned(),
            register_code: "R1".to_owned(),
            register_name: "Register 1".to_owned(),
            cashier_name: cashier.to_owned(),
            forced_by_name: forced_by,
            opening_cash: session.opening_cash,
            cash_sales: self.cash_sales,
            cash_refunds: 0,
            cash_in: sales.cash_in.minor(),
            cash_out: sales.cash_out.minor() + sales.drops.minor() + payouts,
            expected_cash: expected,
            counted_cash: body.counted_cash,
            over_short: close.over_short.minor(),
            transactions: if self.cash_sales > 0 { 3 } else { 0 },
            tenders,
            counts: body.counts.clone().unwrap_or_default(),
            sales: sales_report(sales),
            cash: cash_report(&close.session),
            float: FloatInfo {
                float_kept: Some(close.float.float_kept.minor()),
                to_safe: Some(close.float.to_safe.minor()),
                opening_proposed: close.float.opening_proposed.map(Money::minor),
                float_difference: close.float.float_difference.minor(),
            },
            session,
        };
        self.cash.closes.insert(session_id.to_owned(), close);
        if let Some(entry) = self.history.iter_mut().find(|(h, _)| h.id == session_id) {
            *entry = (report.session.clone(), Some(report.clone()));
        }
        Ok(report)
    }

    /// The float kept by the register's last closed session, if any.
    fn last_float(&self) -> Option<i64> {
        self.history
            .iter()
            .filter(|(h, _)| h.closed_at.is_some())
            .max_by_key(|(h, _)| h.closed_at)
            .and_then(|(h, _)| h.float_kept)
    }

    /// Stores the proposal of a session just opened with `opening_cash` (spec 2.7 §3).
    pub(super) fn propose(&mut self, session: &mut CashierSession) {
        if let Some(kept) = self.last_float()
            && kept != session.opening_cash
        {
            self.cash.proposed.insert(session.id.clone(), kept);
            session.opening_proposed = Some(kept);
        }
    }

    fn close_text(
        &self,
        token: &str,
        session_id: &str,
        copy: bool,
        lang: &str,
    ) -> Result<CashText, ClientError> {
        let who = self.who(token)?;
        let (session, report) = self
            .history
            .iter()
            .find(|(h, _)| h.id == session_id)
            .cloned()
            .ok_or_else(|| api(404, "not_found"))?;
        if !who.reports() && session.user_id != who.id() {
            return Err(denied("report.sales"));
        }
        let (Some(report), Some(close)) = (report, self.cash.closes.get(session_id)) else {
            return Err(api(404, "not_found"));
        };
        Ok(text(lang, copy, |header, opts| {
            build_close_report(
                &CloseReportInput {
                    header,
                    session: SessionHeader {
                        number: &session.number,
                        register: "R1",
                        cashier: &report.cashier_name,
                        opened_at: Timestamp::from_unix_millis(session.opened_at),
                        closed_at: session.closed_at.map(Timestamp::from_unix_millis),
                        forced: report.forced,
                        approved_by: report.forced_by_name.as_deref(),
                    },
                    report: close,
                    copy,
                },
                opts,
            )
        }))
    }

    fn run_z(
        &mut self,
        token: &str,
        body: &ZRunRequest,
        key: &str,
        override_token: Option<&str>,
    ) -> Result<ZReport, ClientError> {
        let who = self.who(token)?;
        if let Some(z) = self.cash.keyed_z.get(key) {
            return Ok(z.clone());
        }
        let day = parse_day(&body.day).ok_or_else(|| invalid("day"))?;
        if body.branch_id.as_ref().is_some_and(|b| b != BRANCH_ID) {
            return Err(invalid("branch_id"));
        }
        let approved_by = match &who {
            Who::Till(user) => {
                let context = format!("{BRANCH_ID}:{}", body.day);
                self.cash_authorize(user, "day.close", None, &context, override_token)?
            }
            Who::Office { reports: true, .. } => None,
            Who::Office { .. } => return Err(denied("day.close")),
        };
        let now = Timestamp::from_unix_millis(self.now);
        let history: Vec<(CashierSession, Option<CloseReport>)> = self.history.clone();
        let mut rows = Vec::with_capacity(history.len());
        for (h, report) in &history {
            let id = self.core_session(&h.id);
            let business_day = LocalDay::of(Timestamp::from_unix_millis(h.opened_at), tz());
            rows.push((id, h, report, business_day));
        }
        let candidates: Vec<ZCandidate<'_>> = rows
            .iter()
            .map(|(id, h, _, d)| ZCandidate {
                id: *id,
                number: &h.number,
                business_day: *d,
                open: h.status == "open",
                in_z: self.cash.in_z.contains_key(&h.id),
            })
            .collect();
        let covered =
            check_z_run(day, LocalDay::of(now, tz()), &candidates).map_err(|e| rule(&e))?;
        let seq = self
            .cash
            .z
            .iter()
            .filter(|(z, _)| z.report.day == body.day)
            .count()
            + 1;
        let number = z_number("JKT01", day, u32::try_from(seq).unwrap_or(u32::MAX));
        let sales = SessionSales::default();
        let mut inputs = Vec::new();
        let mut ids = Vec::new();
        for (id, h, report, _) in &rows {
            if !covered.contains(id) {
                continue;
            }
            let (Some(report), Some(close)) = (report, self.cash.closes.get(&h.id)) else {
                continue;
            };
            ids.push(h.id.clone());
            inputs.push(ZSession {
                number: &h.number,
                register: "R1",
                cashier: &report.cashier_name,
                opened_at: Timestamp::from_unix_millis(h.opened_at),
                closed_at: Timestamp::from_unix_millis(h.closed_at.unwrap_or(self.now)),
                forced: report.forced,
                close,
                missing_nota: close.session.missing_nota,
                sales: &sales,
            });
        }
        let core = build_z(
            &number,
            day,
            u32::try_from(seq).unwrap_or(u32::MAX),
            now,
            &inputs,
        )
        .map_err(|e| rule(&e))?;
        let z = ZReport {
            id: format!("z-{}", self.cash.z.len() + 1),
            branch_id: BRANCH_ID.to_owned(),
            user_id: who.id().to_owned(),
            run_by_name: who.name().to_owned(),
            approved_by_name: approved_by
                .as_ref()
                .and_then(|a| self.user(a))
                .map(|u| u.name.clone()),
            approved_by,
            session_ids: ids.clone(),
            report: snapshot(&core),
        };
        for id in ids {
            self.cash.in_z.insert(id, z.id.clone());
        }
        self.cash.z.push((z.clone(), core));
        self.cash.keyed_z.insert(key.to_owned(), z.clone());
        Ok(z)
    }

    fn z_readable(&self, token: &str, id: &str) -> Result<(Who, ZReport, CoreZ), ClientError> {
        let who = self.who(token)?;
        let (z, core) = self
            .cash
            .z
            .iter()
            .find(|(z, _)| z.id == id)
            .cloned()
            .ok_or_else(|| api(404, "not_found"))?;
        if !who.reports() && z.user_id != who.id() {
            return Err(denied("report.sales"));
        }
        Ok((who, z, core))
    }

    fn z_text(
        &self,
        token: &str,
        id: &str,
        copy: bool,
        lang: &str,
    ) -> Result<CashText, ClientError> {
        let (who, z, core) = self.z_readable(token, id)?;
        if copy && !who.reports() {
            return Err(denied("report.sales"));
        }
        Ok(text(lang, copy, |header, opts| {
            build_z_report(
                &ZReportInput {
                    header,
                    report: &core,
                    run_by: &z.run_by_name,
                    approved_by: z.approved_by_name.as_deref(),
                    copy,
                },
                opts,
            )
        }))
    }

    fn pending(&self, token: &str, branch: Option<&str>) -> Result<Vec<PendingDay>, ClientError> {
        let who = self.who(token)?;
        if !who.reports() {
            return Err(denied("report.sales"));
        }
        if matches!(who, Who::Office { .. }) && branch.is_none() {
            return Err(invalid("branch_id"));
        }
        let today = LocalDay::of(Timestamp::from_unix_millis(self.now), tz());
        let mut days: std::collections::BTreeMap<LocalDay, (u32, u32)> =
            std::collections::BTreeMap::new();
        for (h, _) in &self.history {
            if branch.is_some_and(|b| b != BRANCH_ID) || self.cash.in_z.contains_key(&h.id) {
                continue;
            }
            let d = LocalDay::of(Timestamp::from_unix_millis(h.opened_at), tz());
            let e = days.entry(d).or_default();
            if h.status == "open" {
                e.0 += 1;
            } else {
                e.1 += 1;
            }
        }
        Ok(days
            .into_iter()
            .filter(|(d, (_, closed))| *closed > 0 || *d < today)
            .map(|(d, (open, closed))| PendingDay {
                day: day_text(d),
                open,
                closed,
            })
            .collect())
    }

    fn owner_of(&self, token: &str) -> Result<(), ClientError> {
        match self.who(token)? {
            Who::Office { owner: true, .. } => Ok(()),
            _ => Err(denied("tender.manage")),
        }
    }

    fn put_reason(
        &mut self,
        token: &str,
        id: &str,
        body: &CashReasonBody,
    ) -> Result<CashReason, ClientError> {
        self.owner_of(token)?;
        let name = body.name.trim();
        if name.is_empty() || name.chars().count() > 60 {
            return Err(invalid("name"));
        }
        if Direction::from_key(&body.direction).is_none() {
            return Err(invalid("direction"));
        }
        if id.parse::<CashReasonId>().is_err() {
            return Err(problem(400, "invalid_body").into());
        }
        let reason = CashReason {
            id: id.to_owned(),
            name: name.to_owned(),
            direction: body.direction.clone(),
            note_required: body.note_required,
            active: body.active,
            sort: body.sort,
            archived: false,
        };
        match self.cash.reasons.iter_mut().find(|r| r.id == id) {
            Some(r) if r.archived => Err(api(409, "archived")),
            Some(r) if r.direction != body.direction => Err(invalid("direction")),
            Some(r) => {
                *r = reason.clone();
                Ok(reason)
            }
            None => {
                self.cash.reasons.push(reason.clone());
                Ok(reason)
            }
        }
    }

    fn archive_reason(&mut self, token: &str, id: &str) -> Result<CashReason, ClientError> {
        self.owner_of(token)?;
        let r = self
            .cash
            .reasons
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| api(404, "not_found"))?;
        r.archived = true;
        Ok(r.clone())
    }

    fn sorted_reasons(&self, all: bool) -> Vec<CashReason> {
        let mut list: Vec<CashReason> = self
            .cash
            .reasons
            .iter()
            .filter(|r| all || (r.active && !r.archived))
            .cloned()
            .collect();
        list.sort_by(|a, b| (&a.direction, a.sort, &a.id).cmp(&(&b.direction, b.sort, &b.id)));
        list
    }

    fn payout_slip(
        &mut self,
        token: &str,
        customer: &str,
        entry: &str,
        copy: bool,
        lang: &str,
        override_token: Option<&str>,
    ) -> Result<CashText, ClientError> {
        let who = self.who(token)?;
        let (e, line, balance) = self
            .customers
            .payout_entry(customer, entry)
            .ok_or_else(|| api(404, "not_found"))?;
        if copy {
            let free = e
                .cashier_session_id
                .as_ref()
                .is_some_and(|s| self.own_open(&who, s));
            self.reprint(&who, free, entry, override_token)?;
        }
        let cashier = e
            .user_id
            .as_ref()
            .and_then(|u| self.user(u))
            .map(|u| u.name.clone())
            .unwrap_or_default();
        Ok(text(lang, copy, |header, opts| {
            build_slip(
                &SlipInput {
                    header,
                    kind: SlipKind::Payout,
                    number: &e.id,
                    posted_at: Timestamp::from_unix_millis(e.occurred_at),
                    register: "R1",
                    cashier: &cashier,
                    approved_by: None,
                    reversal_of: None,
                    reason: None,
                    note: None,
                    customer: Some(&line),
                    balance_after: Some(rp(balance)),
                    amount: rp(e.amount.saturating_abs()),
                    copy,
                },
                opts,
            )
        }))
    }
}

fn run<T: Send + 'static>(
    fake: &FakeService,
    method: &'static str,
    detail: String,
    body: impl FnOnce(&mut State) -> Result<T, ClientError>,
) -> crate::BoxFuture<T> {
    let (mut s, failure) = fake.begin(method, detail);
    ready(match failure {
        Some(e) => Err(e),
        None => body(&mut s),
    })
}

/// The calls of [`crate::Service`] and [`crate::BackOffice`] on cash (spec 2.7), called from the
/// trait impls.
impl FakeService {
    /// The cash movements posted so far, oldest first.
    #[must_use]
    pub fn cash_movements_posted(&self) -> Vec<CashMovement> {
        self.state()
            .cash
            .movements
            .iter()
            .map(|m| m.movement.clone())
            .collect()
    }

    pub(super) fn cash_reasons_call(
        &self,
        token: &str,
        all: bool,
    ) -> crate::BoxFuture<Vec<CashReason>> {
        run(self, "cash_reasons", format!("all={all}"), |s| {
            if all {
                s.owner_of(token)?;
            } else {
                s.who(token)?;
            }
            Ok(s.sorted_reasons(all))
        })
    }

    pub(super) fn put_cash_reason_call(
        &self,
        token: &str,
        id: &str,
        body: &CashReasonBody,
    ) -> crate::BoxFuture<CashReason> {
        run(self, "put_cash_reason", id.to_owned(), |s| {
            s.put_reason(token, id, body)
        })
    }

    pub(super) fn delete_cash_reason_call(
        &self,
        token: &str,
        id: &str,
    ) -> crate::BoxFuture<CashReason> {
        run(self, "delete_cash_reason", id.to_owned(), |s| {
            s.archive_reason(token, id)
        })
    }

    pub(super) fn post_cash_call(
        &self,
        token: &str,
        id: &str,
        body: &CashMovementRequest,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<CashMovement> {
        let detail = format!("{id} override={}", override_token.unwrap_or("-"));
        run(self, "post_cash_movement", detail, |s| {
            s.post_cash(token, id, body, override_token)
        })
    }

    pub(super) fn cash_movement_call(
        &self,
        token: &str,
        id: &str,
    ) -> crate::BoxFuture<CashMovement> {
        run(self, "cash_movement", id.to_owned(), |s| {
            let who = s.who(token)?;
            let m = s
                .cash
                .movements
                .iter()
                .find(|m| m.movement.id == id)
                .map(|m| m.movement.clone())
                .ok_or_else(|| api(404, "not_found"))?;
            State::movement_readable(&who, &m)?;
            Ok(m)
        })
    }

    pub(super) fn cash_movements_call(
        &self,
        token: &str,
        query: &CashQuery,
    ) -> crate::BoxFuture<CashMovementPage> {
        let detail = serde_json::to_string(query).unwrap_or_default();
        run(self, "cash_movements", detail, |s| {
            s.list_cash(token, query)
        })
    }

    pub(super) fn cash_slip_call(
        &self,
        token: &str,
        id: &str,
        copy: bool,
        lang: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<CashText> {
        let detail = format!("{id} copy={copy} lang={lang}");
        run(self, "cash_slip", detail, |s| {
            s.slip(token, id, copy, lang, override_token)
        })
    }

    pub(super) fn payout_slip_call(
        &self,
        token: &str,
        customer: &str,
        entry: &str,
        copy: bool,
        lang: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<CashText> {
        let detail = format!("{entry} copy={copy}");
        run(self, "payout_slip", detail, |s| {
            s.payout_slip(token, customer, entry, copy, lang, override_token)
        })
    }

    pub(super) fn x_report_call(
        &self,
        token: &str,
        session_id: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<XReport> {
        let detail = format!("{session_id} override={}", override_token.unwrap_or("-"));
        run(self, "x_report", detail, |s| {
            let (session, x, approver) = s.x_read(token, session_id, override_token)?;
            let cashier = s
                .user(&session.user_id)
                .map(|u| u.name.clone())
                .unwrap_or_default();
            Ok(XReport {
                session_id: session.id.clone(),
                number: session.number.clone(),
                branch_code: "JKT01".to_owned(),
                branch_name: "Toko Jakarta".to_owned(),
                timezone: "Asia/Jakarta".to_owned(),
                register_code: "R1".to_owned(),
                cashier_name: cashier,
                opened_at: session.opened_at,
                read_at: x.read_at.unix_millis(),
                approved_by: approver,
                opening_cash: session.opening_cash,
                expected_cash: x.session.expected_cash().minor(),
                sales: sales_report(&x.session.sales),
                cash: cash_report(&x.session),
                opening_proposed: x.opening_proposed.map(Money::minor),
                float_difference: x.float_difference.minor(),
            })
        })
    }

    pub(super) fn x_report_text_call(
        &self,
        token: &str,
        session_id: &str,
        lang: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<CashText> {
        let detail = format!("{session_id} override={}", override_token.unwrap_or("-"));
        run(self, "x_report_text", detail, |s| {
            let (session, x, approver) = s.x_read(token, session_id, override_token)?;
            let cashier = s
                .user(&session.user_id)
                .map(|u| u.name.clone())
                .unwrap_or_default();
            let approver = approver.and_then(|a| s.user(&a)).map(|u| u.name.clone());
            Ok(text(lang, false, |header, opts| {
                build_x_report(
                    &XReportInput {
                        header,
                        session: SessionHeader {
                            number: &session.number,
                            register: "R1",
                            cashier: &cashier,
                            opened_at: Timestamp::from_unix_millis(session.opened_at),
                            closed_at: None,
                            forced: false,
                            approved_by: approver.as_deref(),
                        },
                        report: &x,
                    },
                    opts,
                )
            }))
        })
    }

    pub(super) fn close_report_text_call(
        &self,
        token: &str,
        session_id: &str,
        copy: bool,
        lang: &str,
    ) -> crate::BoxFuture<CashText> {
        let detail = format!("{session_id} copy={copy}");
        run(self, "close_report_text", detail, |s| {
            s.close_text(token, session_id, copy, lang)
        })
    }

    pub(super) fn proposed_float_call(&self, token: &str) -> crate::BoxFuture<ProposedFloat> {
        run(self, "proposed_float", String::new(), |s| {
            s.caller(token)?;
            let last = s.last_float();
            Ok(ProposedFloat {
                register_id: REGISTER_ID.to_owned(),
                proposed_float: last.unwrap_or(0),
                has_proposal: last.is_some(),
            })
        })
    }

    pub(super) fn run_z_call(
        &self,
        token: &str,
        body: &ZRunRequest,
        key: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<ZReport> {
        let detail = format!(
            "{} key={key} override={}",
            body.day,
            override_token.unwrap_or("-")
        );
        run(self, "run_z", detail, |s| {
            s.run_z(token, body, key, override_token)
        })
    }

    pub(super) fn z_report_text_call(
        &self,
        token: &str,
        id: &str,
        copy: bool,
        lang: &str,
    ) -> crate::BoxFuture<CashText> {
        run(self, "z_report_text", format!("{id} copy={copy}"), |s| {
            s.z_text(token, id, copy, lang)
        })
    }

    pub(super) fn pending_days_call(
        &self,
        token: &str,
        branch: Option<&str>,
    ) -> crate::BoxFuture<Vec<PendingDay>> {
        run(
            self,
            "pending_days",
            branch.unwrap_or_default().to_owned(),
            |s| s.pending(token, branch),
        )
    }

    pub(super) fn z_reports_call(
        &self,
        token: &str,
        query: &ZQuery,
    ) -> crate::BoxFuture<ZReportPage> {
        let detail = serde_json::to_string(query).unwrap_or_default();
        run(self, "z_reports", detail, |s| {
            if !s.who(token)?.reports() {
                return Err(denied("report.sales"));
            }
            let limit = query.limit.unwrap_or(50) as usize;
            let newest = s.cash.z.iter().rev().map(|(z, _)| z);
            let after: Vec<&ZReport> = match &query.before_id {
                None => newest.collect(),
                Some(c) => newest.skip_while(|z| z.id != *c).skip(1).collect(),
            };
            let items: Vec<ZSummary> = after
                .into_iter()
                .filter(|z| {
                    query.branch_id.as_ref().is_none_or(|b| *b == z.branch_id)
                        && query.from.as_ref().is_none_or(|f| z.report.day >= *f)
                        && query.until.as_ref().is_none_or(|u| z.report.day <= *u)
                })
                .take(limit)
                .map(|z| ZSummary {
                    id: z.id.clone(),
                    number: z.report.number.clone(),
                    branch_id: z.branch_id.clone(),
                    day: z.report.day.clone(),
                    seq: z.report.seq,
                    supplementary: z.report.supplementary,
                    session_count: z.report.branch.sessions,
                    sales_count: z.report.branch.sales_count,
                    net_sales: z.report.branch.net_sales,
                    expected_cash: z.report.branch.expected_cash,
                    counted_cash: z.report.branch.counted_cash,
                    over_short: z.report.branch.over_short,
                    cash_to_deposit: z.report.branch.cash_to_deposit,
                    run_at: z.report.run_at,
                    user_id: z.user_id.clone(),
                    approved_by: z.approved_by.clone(),
                })
                .collect();
            let next_before_id = items
                .last()
                .filter(|_| items.len() == limit)
                .map(|z| z.id.clone());
            Ok(ZReportPage {
                items,
                next_before_id,
            })
        })
    }

    pub(super) fn z_report_call(&self, token: &str, id: &str) -> crate::BoxFuture<ZReport> {
        run(self, "z_report", id.to_owned(), |s| {
            s.z_readable(token, id).map(|(_, z, _)| z)
        })
    }

    pub(super) fn force_close_call(
        &self,
        token: &str,
        session_id: &str,
        body: &CloseRequest,
        key: &str,
    ) -> crate::BoxFuture<CloseReport> {
        let detail = format!("{session_id} counted={} key={key}", body.counted_cash);
        run(self, "force_close_session", detail, |s| {
            let who = s.who(token)?;
            if !who.reports() {
                return Err(denied("session.force_close"));
            }
            let cashier = s
                .session
                .as_ref()
                .filter(|o| o.id == session_id)
                .and_then(|o| s.user(&o.user_id))
                .map(|u| u.name.clone())
                .unwrap_or_default();
            s.close_with_cash(&cashier, session_id, body, Some(who.name().to_owned()))
        })
    }
}
