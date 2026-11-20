//! What the cash routes of spec 2.7 (phase K3) share: the session figures of the close report,
//! the X and the Z (`core::cash::session_figures`), their DTOs, the retry loop of cash writes and
//! the printable texts.
//!
//! **Session figures** ([`session_figures`]): the session's sales events (`sultan_db::sales`), its
//! till voids (audit), its balance payouts and its cash movements, photo counts, no-sale drawer
//! opens and X reads (`sultan_db::cash::session_cash`), with every cash reason (archived ones too,
//! for the names) and the company's `nota_flag_above`. They give both blocks of the close report:
//! the `sales` block (spec 2.6 §10, with the netted cash in / out / drops) and the `cash` block
//! (§4: per kind and reason, reversals, drawer opens, X reads, missing notas). A closed session
//! reports the missing-nota count stored at its close; an X and a Z count them as of now.
//!
//! **Texts** ([`TextQuery`], [`TextDto`]): `?lang=id|en` (default `en`), `width=32|42|48`
//! (default: the branch's receipt width) and `copy=true` (a reprint, printed COPY), built by
//! `core::cash` like the receipts. `large` lists the lines printed double size (2.11).

use std::collections::{BTreeMap, HashMap};
use std::future::Future;

use serde::{Deserialize, Serialize};
use sultan_core::cash::{
    CashError, CashFigures, CashLabel, CashReason, Direction, ReasonLine, SessionFigures,
    SessionInput, TextOptions,
};
use sultan_core::doc_number::BranchTimeZone;
use sultan_core::ids::{BranchId, CashReasonId};
use sultan_core::money::Money;
use sultan_core::sale::{
    CountAmount, Label, ReceiptHeader, ReceiptWidth, SalesSessionReport, SessionEvent, TenderKey,
    TenderKind, TenderReport,
};
use sultan_db::cash::{self, CashDbError};
use sultan_db::cashier_sessions::SessionRecord;
use sultan_db::org::Branch;
use sultan_db::{DbError, Tx, customers, org, sales};
use utoipa::{IntoParams, ToSchema};

use super::cashier_sessions::{CountAmountDto, SalesReportDto, SalesTenderDto};
use crate::error::ApiError;
use crate::retry::{self, Failure};
use crate::state::AppState;

/// A failure inside a cash write: an API answer, or a repository error that may be retried.
#[derive(Debug)]
pub enum Fail {
    /// Answer this.
    Api(ApiError),
    /// A cash repository error; retried when it is a lost race.
    Db(CashDbError),
}

impl From<ApiError> for Fail {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<CashDbError> for Fail {
    fn from(e: CashDbError) -> Self {
        Self::Db(e)
    }
}

impl From<DbError> for Fail {
    fn from(e: DbError) -> Self {
        Self::Db(CashDbError::Db(e))
    }
}

impl From<CashError> for Fail {
    fn from(e: CashError) -> Self {
        Self::Api(e.into())
    }
}

/// Runs a cash write with the 2.1 §5 retry loop (a lost race on a number, a reversal or a Z
/// link reruns the whole transaction); every attempt lost → `409 conflict`.
///
/// # Errors
/// The final error.
pub async fn run<T, F, Fut>(what: &'static str, op: F) -> Result<T, ApiError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Fail>>,
{
    retry::run(what, op, |f| matches!(f, Fail::Db(e) if e.is_retryable()))
        .await
        .map_err(|failure| match failure {
            Failure::Final(Fail::Api(e)) | Failure::Exhausted(Fail::Api(e)) => e,
            Failure::Final(Fail::Db(e)) => e.into(),
            Failure::Exhausted(Fail::Db(_)) => retry::conflict(),
        })
}

/// The session's figures (see the module docs). `stored_missing` replaces the live missing-nota
/// count (a closed session's count at its close).
///
/// # Errors
/// The database error, or `422 amount_overflow`.
pub async fn session_figures(
    tx: &mut Tx,
    record: &SessionRecord,
    stored_missing: Option<u32>,
) -> Result<SessionFigures, ApiError> {
    let id = record.session.id();
    let mut events = sales::session_events(&mut *tx, id).await?;
    super::cashier_sessions::till_voids(tx, id, &mut events).await?;
    let payouts = customers::session_payouts(&mut *tx, id).await?;
    if payouts > Money::ZERO {
        events.push(SessionEvent::Payout(payouts));
    }
    let inputs = cash::session_cash(&mut *tx, id).await?;
    let reasons: Vec<CashReason> = cash::list_cash_reasons(&mut *tx, true)
        .await?
        .into_iter()
        .map(|r| r.reason)
        .collect();
    let flag = cash::nota_flag_above(&mut *tx).await?;
    let mut figures = sultan_core::cash::session_figures(&SessionInput {
        opening_cash: record.session.opening_cash(),
        events: &events,
        movements: &inputs.movements,
        reasons: &reasons,
        photos: &inputs.photos,
        nota_flag_above: flag,
        drawer_opens: inputs.drawer_opens,
        x_reports: inputs.x_reports,
    })?;
    if let Some(missing) = stored_missing {
        figures.missing_nota = missing;
    }
    Ok(figures)
}

/// One cash reason's line in a report (§4): its movements, its amount net of reversals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReasonLineDto {
    /// The reason.
    pub reason_id: String,
    /// Its name when the report was made.
    pub name: String,
    /// `in` or `out`.
    pub direction: String,
    /// Its list order.
    pub sort: i32,
    /// Movements with it, reversals not counted.
    pub count: u32,
    /// Σ amounts minus Σ reversed amounts.
    pub amount: i64,
    /// Reversals with it.
    pub reversals: u32,
}

/// The cash block of a close report, an X or a Z (spec 2.7 §4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CashReportDto {
    /// Cash in: movements that are not reversals, and the amount net of reversals.
    pub cash_in: CountAmountDto,
    /// Cash out, the same way.
    pub cash_out: CountAmountDto,
    /// Safe drops, the same way.
    pub drops: CountAmountDto,
    /// Reversals of any kind, with Σ of their amounts.
    pub reversals: CountAmountDto,
    /// Per reason, in before out, then by sort and name.
    pub reasons: Vec<ReasonLineDto>,
    /// No-sale drawer opens (`POST /drawer/open`, K22).
    pub drawer_opens: u32,
    /// X reads (K12).
    pub x_reports: u32,
    /// Cash outs missing a nota (§2a): at the close for a close report, now for an X, at the run
    /// for a Z.
    pub missing_nota: u32,
}

impl From<&SessionFigures> for CashReportDto {
    fn from(f: &SessionFigures) -> Self {
        let c = &f.cash;
        // The order of `CashFigures::reason_lines`: in before out, then sort and name.
        let mut lines: Vec<(&CashReasonId, &ReasonLine)> = c.reasons.iter().collect();
        lines.sort_by(|(_, a), (_, b)| {
            (a.direction, a.sort, &a.name).cmp(&(b.direction, b.sort, &b.name))
        });
        Self {
            cash_in: c.cash_in.into(),
            cash_out: c.cash_out.into(),
            drops: c.drops.into(),
            reversals: c.reversals.into(),
            reasons: lines
                .into_iter()
                .map(|(id, line)| ReasonLineDto {
                    reason_id: id.to_string(),
                    name: line.name.clone(),
                    direction: line.direction.as_str().to_owned(),
                    sort: line.sort,
                    count: line.count,
                    amount: line.amount.minor(),
                    reversals: line.reversals,
                })
                .collect(),
            drawer_opens: f.drawer_opens,
            x_reports: f.x_reports,
            missing_nota: f.missing_nota,
        }
    }
}

fn count_amount(c: CountAmountDto) -> CountAmount {
    CountAmount {
        count: c.count,
        amount: Money::from_minor(c.amount),
    }
}

impl From<&SalesSessionReport> for SalesReportDto {
    fn from(r: &SalesSessionReport) -> Self {
        Self {
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
                .map(|(k, t)| SalesTenderDto {
                    tender_type_id: k.tender_type.map(|t| t.to_string()),
                    kind: k.kind.as_str().to_owned(),
                    sales: t.sales.into(),
                    refunds: t.refunds.into(),
                })
                .collect(),
            line_voids: r.line_voids.into(),
            cart_voids: r.cart_voids.into(),
            post_voids: r.post_voids.into(),
            returns: r.returns.into(),
            line_discounts: r.line_discounts.into(),
            cart_discounts: r.cart_discounts.into(),
            price_overrides: r.price_overrides.into(),
            saved_change: r.saved_change.minor(),
            balance_paid: r.balance_paid.minor(),
        }
    }
}

/// A stored snapshot that does not read back: `500 internal` (it was written by this server).
fn corrupt(what: &str) -> ApiError {
    tracing::error!("stored cash snapshot: {what}");
    ApiError::internal()
}

/// The `core` figures of stored DTOs (a Z's snapshot, for its text).
///
/// # Errors
/// `500 internal` for an unknown tender kind, reason ID or direction.
pub fn core_figures(
    sales: &SalesReportDto,
    cash: &CashReportDto,
) -> Result<SessionFigures, ApiError> {
    let m = Money::from_minor;
    let mut tenders = BTreeMap::new();
    for t in &sales.tenders {
        let key = TenderKey {
            kind: t
                .kind
                .parse::<TenderKind>()
                .map_err(|_| corrupt("tender kind"))?,
            tender_type: t
                .tender_type_id
                .as_deref()
                .map(str::parse)
                .transpose()
                .map_err(|_| corrupt("tender type"))?,
        };
        tenders.insert(
            key,
            TenderReport {
                sales: count_amount(t.sales),
                refunds: count_amount(t.refunds),
            },
        );
    }
    let mut reasons = BTreeMap::new();
    for r in &cash.reasons {
        let id: CashReasonId = r.reason_id.parse().map_err(|_| corrupt("reason id"))?;
        let direction = Direction::from_key(&r.direction).ok_or_else(|| corrupt("direction"))?;
        reasons.insert(
            id,
            ReasonLine {
                name: r.name.clone(),
                direction,
                sort: r.sort,
                count: r.count,
                amount: m(r.amount),
                reversals: r.reversals,
            },
        );
    }
    Ok(SessionFigures {
        sales: SalesSessionReport {
            opening_cash: m(sales.opening_cash),
            cash_received: m(sales.cash_received),
            change_given: m(sales.change_given),
            cash_refunds: m(sales.cash_refunds),
            cash_rounding: m(sales.cash_rounding),
            cash_in: m(sales.cash_in),
            cash_out: m(sales.cash_out),
            drops: m(sales.drops),
            payouts: m(sales.payouts),
            expected_cash: m(sales.expected_cash),
            tenders,
            line_voids: count_amount(sales.line_voids),
            cart_voids: count_amount(sales.cart_voids),
            post_voids: count_amount(sales.post_voids),
            returns: count_amount(sales.returns),
            line_discounts: count_amount(sales.line_discounts),
            cart_discounts: count_amount(sales.cart_discounts),
            price_overrides: count_amount(sales.price_overrides),
            saved_change: m(sales.saved_change),
            balance_paid: m(sales.balance_paid),
        },
        cash: CashFigures {
            cash_in: count_amount(cash.cash_in),
            cash_out: count_amount(cash.cash_out),
            drops: count_amount(cash.drops),
            reversals: count_amount(cash.reversals),
            reasons,
        },
        drawer_opens: cash.drawer_opens,
        x_reports: cash.x_reports,
        missing_nota: cash.missing_nota,
    })
}

/// Query of the text routes (slips, X, close report, Z).
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct TextQuery {
    /// The label language: `id` (Indonesian) or `en` (default).
    pub lang: Option<String>,
    /// 32, 42 or 48 columns (default: the branch's receipt width, `GET /settings/sales`).
    pub width: Option<usize>,
    /// A reprint: printed COPY (and audited where the route says so).
    #[serde(default)]
    pub copy: bool,
}

/// A printable text (spec 2.7: slips, X, close report, Z), shown on screen until 2.11 prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TextDto {
    /// A reprint (printed COPY).
    pub copy: bool,
    /// The text, one entry per line.
    pub lines: Vec<String>,
    /// The text with a newline after each line.
    pub text: String,
    /// Indexes into `lines` printed double size (a slip's amount, a Z's cash to deposit).
    pub large: Vec<usize>,
}

impl TextDto {
    /// The DTO of a built text.
    #[must_use]
    pub fn new(printout: &sultan_core::cash::Printout, copy: bool) -> Self {
        Self {
            copy,
            text: printout.text(),
            lines: printout.lines.clone(),
            large: printout.large.clone(),
        }
    }
}

/// What a text of a branch needs: the branch (header, time zone), the width, the language and the
/// tender type names.
pub struct TextContext {
    /// The branch.
    pub branch: Branch,
    /// Its receipt header lines.
    pub header_lines: Vec<String>,
    /// Its time zone.
    pub tz: BranchTimeZone,
    /// The width.
    pub width: ReceiptWidth,
    /// `id` or `en`.
    pub lang: String,
    tender_names: HashMap<sultan_core::ids::TenderTypeId, String>,
}

impl TextContext {
    /// Loads the context of `branch` for `query`.
    ///
    /// # Errors
    /// `404 not_found` (branch), `422 invalid_field` (`width`), or the database error.
    pub async fn load(
        state: &AppState,
        branch: BranchId,
        query: &TextQuery,
    ) -> Result<Self, ApiError> {
        let branch = org::get_branch(&state.db, branch)
            .await?
            .ok_or_else(|| ApiError::not_found("branch"))?;
        let width = super::sales::receipt_width(state, query.width, branch.id).await?;
        let tender_names = sales::list_tender_types(&state.db, true)
            .await?
            .into_iter()
            .map(|t| (t.tender.id, t.tender.name))
            .collect();
        Ok(Self {
            header_lines: branch.receipt_header.lines().map(str::to_owned).collect(),
            tz: branch.timezone.parse().unwrap_or_default(),
            width,
            lang: query.lang.clone().unwrap_or_else(|| "en".to_owned()),
            tender_names,
            branch,
        })
    }

    /// The branch block.
    #[must_use]
    pub fn header(&self) -> ReceiptHeader<'_> {
        ReceiptHeader {
            lines: &self.header_lines,
            branch_name: &self.branch.name,
            address: &self.branch.address,
            tax_id: self.branch.npwp.as_deref(),
        }
    }

    /// Runs `build` with the text options of this context.
    pub fn build<T>(&self, build: impl FnOnce(&TextOptions<'_>) -> T) -> T {
        let lang = self.lang.as_str();
        let label = |l: CashLabel| l.text(lang).to_owned();
        let tender_name = |key: TenderKey| {
            key.tender_type
                .and_then(|id| self.tender_names.get(&id).cloned())
                .unwrap_or_else(|| {
                    if key.kind == TenderKind::Exchange {
                        Label::Exchange.text(lang).to_owned()
                    } else {
                        key.kind.as_str().to_owned()
                    }
                })
        };
        build(&TextOptions {
            width: self.width,
            tz: self.tz,
            label: &label,
            tender_name: &tender_name,
        })
    }
}

/// The display name of a user, empty when unknown.
///
/// # Errors
/// The database error.
pub async fn user_name(
    state: &AppState,
    user: Option<sultan_core::ids::UserId>,
) -> Result<Option<String>, ApiError> {
    Ok(match user {
        Some(u) => sultan_db::users::get_user(&state.db, u)
            .await?
            .map(|u| u.display_name),
        None => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figures_round_trip_through_the_dtos() {
        use sultan_core::cash::{CashKind, CashMovement};
        use sultan_core::ids::{CashMovementId, CashierSessionId, TenderTypeId};
        let m = Money::from_minor;
        let session = CashierSessionId::generate();
        let reason = CashReason {
            id: CashReasonId::generate(),
            name: "Parkir".into(),
            direction: Direction::Out,
            active: true,
            sort: 20,
            note_required: false,
        };
        let out = CashMovement::new(
            CashMovementId::generate(),
            session,
            CashKind::Out,
            m(5_000),
            Some(&reason),
            None,
        )
        .unwrap();
        let cash_tender = TenderKey {
            kind: TenderKind::Cash,
            tender_type: Some(TenderTypeId::generate()),
        };
        let events = [
            SessionEvent::Tender {
                key: cash_tender,
                amount: m(10_000),
                cash_received: Some(m(20_000)),
                change_given: m(10_000),
                change_saved: m(0),
            },
            SessionEvent::Tender {
                key: TenderKey {
                    kind: TenderKind::Exchange,
                    tender_type: None,
                },
                amount: m(1_000),
                cash_received: None,
                change_given: m(0),
                change_saved: m(0),
            },
        ];
        let figures = sultan_core::cash::session_figures(&SessionInput {
            opening_cash: m(100_000),
            events: &events,
            movements: &[out],
            reasons: &[reason],
            photos: &BTreeMap::new(),
            nota_flag_above: m(1_000),
            drawer_opens: 2,
            x_reports: 1,
        })
        .unwrap();
        let sales = SalesReportDto::from(&figures.sales);
        let cash = CashReportDto::from(&figures);
        assert_eq!(cash.reasons.len(), 1);
        assert_eq!(cash.missing_nota, 1);
        assert_eq!(core_figures(&sales, &cash).unwrap(), figures);
        // A snapshot that does not parse is an internal error.
        let mut bad = cash.clone();
        bad.reasons[0].direction = "sideways".into();
        assert_eq!(core_figures(&sales, &bad).unwrap_err().code(), "internal");
        let mut bad = sales;
        bad.tenders[0].kind = "barter".into();
        assert_eq!(core_figures(&bad, &cash).unwrap_err().code(), "internal");
    }
}
