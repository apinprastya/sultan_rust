//! The glue of the back office's sales screens (spec 2.6 phase S6): starting the calls of
//! [`crate::backoffice::sales`] and showing its state in the window. Kept apart from
//! [`crate::backoffice_app`] only for size.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use slint::{Model, ModelRc, SharedString, VecModel};
use sultan_client::{BackOffice, Sale, SaleReturn};
use sultan_core::sale::mask_reference;

use crate::amount::rupiah;
use crate::backoffice::sales::{self, Call, Reply, STATUSES, WIDTHS, place_of};
use crate::backoffice::{Controller, Msg, Screen};
use crate::controller::Notice;
use crate::input::date_time;
use crate::{
    BackOfficeWindow, BoDocList, BoDocRow, BoFact, BoSalesDetail, BoSalesFilters, BoSalesSettings,
    BoTenderEditor, BoTenderRow, BoWidthRow, Note,
};

type MsgFuture = Pin<Box<dyn Future<Output = Msg> + Send>>;

/// The models of the sales screens.
#[derive(Default)]
pub struct SalesModels {
    sale_rows: Rc<VecModel<BoDocRow>>,
    return_rows: Rc<VecModel<BoDocRow>>,
    history_rows: Rc<VecModel<BoDocRow>>,
    facts: Rc<VecModel<BoFact>>,
    links: Rc<VecModel<BoDocRow>>,
    receipt: Rc<VecModel<SharedString>>,
    tender_rows: Rc<VecModel<BoTenderRow>>,
    width_rows: Rc<VecModel<BoWidthRow>>,
    branch_choices: Rc<VecModel<SharedString>>,
    register_choices: Rc<VecModel<SharedString>>,
}

impl SalesModels {
    /// Hands the models to the window.
    pub fn attach(&self, ui: &BackOfficeWindow) {
        ui.set_sale_rows(ModelRc::from(self.sale_rows.clone()));
        ui.set_return_rows(ModelRc::from(self.return_rows.clone()));
        ui.set_history_rows(ModelRc::from(self.history_rows.clone()));
        ui.set_detail_facts(ModelRc::from(self.facts.clone()));
        ui.set_detail_links(ModelRc::from(self.links.clone()));
        ui.set_detail_receipt(ModelRc::from(self.receipt.clone()));
        ui.set_tender_rows(ModelRc::from(self.tender_rows.clone()));
        ui.set_width_rows(ModelRc::from(self.width_rows.clone()));
        ui.set_sales_branch_choices(ModelRc::from(self.branch_choices.clone()));
        ui.set_sales_register_choices(ModelRc::from(self.register_choices.clone()));
    }
}

fn sync<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.iter().ne(rows.iter().cloned()) {
        model.set_vec(rows);
    }
}

fn note(notice: Option<&Notice>) -> Note {
    notice.map_or_else(Note::default, |n| Note {
        code: n.code.into(),
        arg: n.arg.as_str().into(),
        tone: n.tone.value(),
    })
}

fn index(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(i32::MAX)
}

/// The window language for receipts: `id` or `en`.
#[must_use]
pub fn receipt_language(language: Option<&str>) -> String {
    let language = language.map_or_else(
        || {
            std::env::var("LANG")
                .unwrap_or_default()
                .split(['_', '.'])
                .next()
                .unwrap_or_default()
                .to_owned()
        },
        str::to_owned,
    );
    if language.starts_with("id") {
        "id".to_owned()
    } else {
        "en".to_owned()
    }
}

/// Starts a sales call; the answer comes back as [`Msg::Sales`].
pub fn start_sales(service: &dyn BackOffice, token: &str, call: &Call, lang: &str) -> MsgFuture {
    macro_rules! call {
        ($future:expr, $reply:expr) => {{
            let future = $future;
            let wrap = $reply;
            Box::pin(async move { Msg::Sales(Box::new(wrap(future.await))) }) as MsgFuture
        }};
    }
    match call {
        Call::Sales { query, append, seq } => {
            let (append, seq) = (*append, *seq);
            call!(service.sales(token, query), move |r| Reply::Sales(
                append, seq, r
            ))
        }
        Call::Returns { query, append, seq } => {
            let (append, seq) = (*append, *seq);
            call!(service.returns(token, query), move |r| Reply::Returns(
                append, seq, r
            ))
        }
        Call::SaleReturns(id, query) => {
            let id = id.clone();
            call!(service.returns(token, query), move |r| Reply::SaleReturns(
                id, r
            ))
        }
        Call::History(id, before, seq) => {
            let (append, seq) = (before.is_some(), *seq);
            call!(
                service.customer_history(token, id, before.as_deref()),
                move |r| Reply::History(append, seq, r)
            )
        }
        Call::Sale(id) => call!(service.sale(token, id), Reply::Sale),
        Call::Return(id) => call!(service.sale_return(token, id), |r| Reply::Return(Box::new(
            r
        ))),
        Call::SaleReceipt(id) => call!(
            service.receipt(token, id, false, lang, None),
            Reply::Receipt
        ),
        Call::ReturnReceipt(id) => call!(
            service.return_receipt(token, id, false, lang, None),
            Reply::Receipt
        ),
        Call::TenderTypes => call!(service.all_tender_types(token), Reply::TenderTypes),
        Call::PutTender { id, body } => call!(service.put_tender_type(token, id, body), |r| {
            Reply::TenderSaved(false, r)
        }),
        Call::ArchiveTender(id) => call!(service.delete_tender_type(token, id), |r| {
            Reply::TenderSaved(true, r)
        }),
        Call::Settings => call!(service.sales_settings(token), |r| Reply::Settings(false, r)),
        Call::PutSettings(body) => call!(service.put_sales_settings(token, body), |r| {
            Reply::Settings(true, r)
        }),
        Call::FindCustomer(text) => {
            let typed = text.clone();
            if let Ok(code) = sultan_core::customer::CustomerCode::parse(text) {
                call!(service.customer_by_code(token, code.as_str()), move |r| {
                    Reply::Found(typed, r)
                })
            } else {
                call!(service.customer_by_phone(token, text), move |r| {
                    Reply::Found(typed, r)
                })
            }
        }
    }
}

fn status_of(voided: bool) -> SharedString {
    if voided { "voided" } else { "posted" }.into()
}

fn fact(label: &str, value: impl Into<SharedString>) -> BoFact {
    BoFact {
        label: label.into(),
        value: value.into(),
        raw: false,
        tone: 0,
    }
}

fn raw(label: &str, value: impl Into<SharedString>) -> BoFact {
    BoFact {
        label: label.into(),
        value: value.into(),
        raw: true,
        tone: 0,
    }
}

fn qty(thousandths: i64) -> String {
    crate::backoffice::form::qty_text(thousandths)
}

fn who(name: Option<&String>, code: Option<&String>) -> String {
    match (name, code) {
        (Some(n), Some(c)) => format!("{n} · {c}"),
        (Some(n), None) => n.clone(),
        _ => String::new(),
    }
}

fn tender_name(c: &Controller, id: Option<&String>, kind: &str) -> String {
    id.and_then(|id| c.sales.tenders.iter().find(|t| t.id == *id))
        .map_or_else(|| kind.to_owned(), |t| t.name.clone())
}

#[allow(clippy::too_many_lines)] // every block of the sale, in receipt order
fn sale_facts(c: &Controller, sale: &Sale) -> Vec<BoFact> {
    let s = &sale.summary;
    let mut out = vec![
        fact("posted_at", date_time(s.posted_at)),
        fact("place", place_of(&s.number)),
        fact("cashier", s.cashier_name.clone().unwrap_or_default()),
    ];
    if s.customer_id.is_some() {
        out.push(fact(
            "customer",
            who(s.customer_name.as_ref(), s.customer_code.as_ref()),
        ));
    }
    if s.tax_invoice {
        out.push(fact("tax_invoice", "✓"));
    }
    for l in &sale.lines {
        let mut value = format!(
            "{} × {} = {}",
            qty(l.qty_thousandths),
            rupiah(l.unit_price),
            rupiah(l.gross)
        );
        if l.line_discount > 0 {
            let _ = write!(value, " − {}", rupiah(l.line_discount));
        }
        if l.returned_qty_thousandths > 0 {
            let _ = write!(value, " · ↩ {}", qty(l.returned_qty_thousandths));
        }
        out.push(raw(&l.name, value));
    }
    out.push(fact("subtotal", rupiah(s.subtotal)));
    if s.line_discount > 0 {
        out.push(fact("line_discount", rupiah(-s.line_discount)));
    }
    if s.cart_discount > 0 {
        out.push(fact("cart_discount", rupiah(-s.cart_discount)));
    }
    if s.tax > 0 {
        out.push(fact(
            if s.tax_inclusive {
                "tax_included"
            } else {
                "tax"
            },
            rupiah(s.tax),
        ));
    }
    if s.cash_rounding != 0 {
        out.push(fact("cash_rounding", rupiah(s.cash_rounding)));
    }
    out.push(fact("paid", rupiah(s.paid)));
    for t in &sale.tenders {
        let name = tender_name(c, t.tender_type_id.as_ref(), &t.kind);
        let mut value = rupiah(t.cash_received.unwrap_or(t.amount));
        if let Some(r) = &t.reference {
            let _ = write!(value, " · Ref {}", mask_reference(r));
        }
        out.push(raw(&name, value));
    }
    if s.change_given > 0 {
        out.push(fact("change", rupiah(s.change_given)));
    }
    if s.change_saved > 0 {
        out.push(fact("saved_change", rupiah(s.change_saved)));
    }
    if let Some(v) = &sale.void {
        let mut f = fact("voided_at", date_time(v.voided_at));
        f.tone = -1;
        out.push(f);
        let reason = v.reason_text.clone().unwrap_or_else(|| v.reason.clone());
        out.push(fact("void_reason", reason));
        if v.cash > 0 {
            out.push(fact("refund_cash", rupiah(v.cash)));
        }
        if v.to_balance + v.payment_reversed > 0 {
            out.push(fact(
                "refund_balance",
                rupiah(v.to_balance + v.payment_reversed),
            ));
        }
    }
    out
}

fn return_facts(c: &Controller, ret: &SaleReturn) -> Vec<BoFact> {
    let s = &ret.summary;
    let mut out = vec![
        fact("posted_at", date_time(s.posted_at)),
        fact("place", place_of(&s.number)),
        fact("cashier", s.cashier_name.clone().unwrap_or_default()),
    ];
    if s.customer_id.is_some() {
        out.push(fact(
            "customer",
            who(s.customer_name.as_ref(), s.customer_code.as_ref()),
        ));
    }
    if s.sale_id.is_none() {
        out.push(fact("no_receipt", "✓"));
    }
    if let Some(r) = &s.reason {
        out.push(fact("return_reason", r.clone()));
    }
    for l in &ret.lines {
        let mut value = format!("{} = {}", qty(l.qty_thousandths), rupiah(l.refund));
        if let Some(w) = &l.write_off_reason {
            let _ = write!(value, " · ✎ {w}");
        }
        out.push(raw(&l.name, value));
    }
    out.push(fact("refund", rupiah(s.refund)));
    if s.cash_rounding != 0 {
        out.push(fact("cash_rounding", rupiah(s.cash_rounding)));
    }
    for t in &ret.tenders {
        let name = if t.kind == "exchange" {
            String::new()
        } else {
            tender_name(c, t.tender_type_id.as_ref(), &t.kind)
        };
        if name.is_empty() {
            out.push(fact("exchange", rupiah(t.amount)));
        } else {
            out.push(raw(&name, rupiah(t.amount)));
        }
    }
    out
}

/// Shows the sales screens and the customer history.
#[allow(clippy::too_many_lines)] // one block per screen
#[allow(clippy::many_single_char_names)] // short names per block, as in `backoffice_app`
pub fn render_sales(ui: &BackOfficeWindow, c: &Controller, models: &SalesModels) {
    let st = &c.sales;
    ui.set_sales_serial(i32::try_from(st.serial % 1_000_000).unwrap_or(0));
    let f = &st.filters;
    let mut branches = vec![SharedString::from("")];
    branches.extend(
        c.reference
            .branches
            .iter()
            .map(|b| SharedString::from(&b.code)),
    );
    sync(&models.branch_choices, branches);
    let mut registers = vec![SharedString::from("")];
    registers.extend(st.registers.iter().map(|r| SharedString::from(&r.1)));
    sync(&models.register_choices, registers);
    ui.set_sales_filters(BoSalesFilters {
        branch_index: index(f.branch),
        register_index: index(f.register),
        from: f.from.as_str().into(),
        until: f.until.as_str().into(),
        customer: f.customer.as_str().into(),
        number: f.number.as_str().into(),
        status_index: index(f.status.min(STATUSES.len() - 1)),
        date_error: f.bad_date.map_or_else(Note::default, |field| Note {
            code: "invalid_date".into(),
            arg: field.into(),
            tone: -1,
        }),
    });
    let can_read = c.can_read_sales();
    let list =
        |l_cursor: usize, loading: bool, more: bool, n: Option<&Notice>, len: usize| BoDocList {
            cursor: index(l_cursor),
            loading,
            more,
            notice: note(n),
            empty: len == 0 && !loading && can_read,
        };
    let l = &st.sales;
    ui.set_sale_list(list(
        l.cursor,
        l.loading,
        l.next.is_some(),
        l.notice.as_ref(),
        l.rows.len(),
    ));
    sync(
        &models.sale_rows,
        l.rows
            .iter()
            .map(|s| BoDocRow {
                number: s.number.as_str().into(),
                time: date_time(s.posted_at).into(),
                place: place_of(&s.number).into(),
                cashier: s.cashier_name.clone().unwrap_or_default().into(),
                customer: s.customer_name.clone().unwrap_or_default().into(),
                total: rupiah(s.paid).into(),
                status: status_of(s.status == "voided"),
                kind: "sale".into(),
            })
            .collect(),
    );
    let l = &st.returns;
    ui.set_return_list(list(
        l.cursor,
        l.loading,
        l.next.is_some(),
        l.notice.as_ref(),
        l.rows.len(),
    ));
    sync(
        &models.return_rows,
        l.rows
            .iter()
            .map(|r| BoDocRow {
                number: r.number.as_str().into(),
                time: date_time(r.posted_at).into(),
                place: place_of(&r.number).into(),
                cashier: r.cashier_name.clone().unwrap_or_default().into(),
                customer: r.customer_name.clone().unwrap_or_default().into(),
                total: rupiah(r.refund).into(),
                status: if r.exchange_sale_id.is_some() {
                    "exchange"
                } else if r.sale_id.is_none() {
                    "no_receipt"
                } else {
                    "posted"
                }
                .into(),
                kind: "return".into(),
            })
            .collect(),
    );
    let l = &st.history;
    let title = st
        .history_of
        .as_ref()
        .map(|h| h.1.clone())
        .unwrap_or_default();
    ui.set_history_list(BoDocList {
        cursor: index(l.cursor),
        loading: l.loading,
        more: l.next.is_some(),
        notice: note(l.notice.as_ref()),
        empty: l.rows.is_empty() && !l.loading,
    });
    ui.set_history_name(title.as_str().into());
    sync(
        &models.history_rows,
        l.rows
            .iter()
            .map(|h| BoDocRow {
                number: h.number.as_str().into(),
                time: date_time(h.posted_at).into(),
                place: place_of(&h.number).into(),
                cashier: SharedString::new(),
                customer: if h.customer_name == title {
                    SharedString::new()
                } else {
                    h.customer_name.as_str().into()
                },
                total: rupiah(h.total).into(),
                status: status_of(h.voided),
                kind: h.kind.as_str().into(),
            })
            .collect(),
    );

    // The detail.
    let d = &st.detail;
    let (title, voided, facts) = match (c.screen(), &d.sale, &d.ret) {
        (Screen::SaleDetail, Some(s), _) => (
            s.summary.number.clone(),
            s.summary.status == "voided",
            sale_facts(c, s),
        ),
        (Screen::ReturnDetail, _, Some(r)) => (r.summary.number.clone(), false, return_facts(c, r)),
        _ => (String::new(), false, Vec::new()),
    };
    ui.set_sales_detail(BoSalesDetail {
        title: title.into(),
        voided,
        loading: facts.is_empty() && d.notice.is_none(),
        notice: note(d.notice.as_ref()),
        cursor: index(d.cursor),
    });
    sync(&models.facts, facts);
    sync(
        &models.links,
        d.links
            .iter()
            .map(|l| BoDocRow {
                number: l.number.as_str().into(),
                kind: l.kind.into(),
                status: l.role.into(),
                ..BoDocRow::default()
            })
            .collect(),
    );
    sync(
        &models.receipt,
        d.receipt
            .iter()
            .map(|l| SharedString::from(l.as_str()))
            .collect(),
    );

    // Tender types.
    sync(
        &models.tender_rows,
        st.tenders
            .iter()
            .map(|t| BoTenderRow {
                name: t.name.as_str().into(),
                kind: t.kind.as_str().into(),
                needs_reference: t.needs_reference,
                opens_drawer: t.opens_drawer,
                active: t.active,
                archived: t.archived,
            })
            .collect(),
    );
    let e = &st.editor;
    let editable = c.tenders_editable() && !e.archived && !e.saving;
    ui.set_tender_editor(BoTenderEditor {
        cursor: index(e.cursor),
        is_new: e.id.is_none(),
        name: e.name.as_str().into(),
        kind_index: index(e.kind),
        needs_reference: e.needs_reference,
        opens_drawer: e.opens_drawer,
        active: e.active,
        sort: e.sort.as_str().into(),
        archived: e.archived,
        can_edit: editable,
        notice: note(e.notice.as_ref()),
    });
    ui.set_tender_read_only(note(c.tenders_read_only().as_ref()));

    // Sales settings.
    let s = &st.settings;
    ui.set_sales_settings(BoSalesSettings {
        return_days: s.return_days.as_str().into(),
        can_edit: c.can_manage_tenders() && !s.replica,
        loaded: s.stored.is_some(),
        notice: note(s.notice.as_ref()),
    });
    sync(
        &models.width_rows,
        s.widths
            .iter()
            .map(|(b, i)| BoWidthRow {
                branch: c
                    .reference
                    .branches
                    .iter()
                    .find(|x| x.id == *b)
                    .map_or_else(|| b.clone(), |x| format!("{} · {}", x.code, x.name))
                    .into(),
                index: index((*i).min(WIDTHS.len() - 1)),
            })
            .collect(),
    );
    let _ = sales::PAGE;
}
