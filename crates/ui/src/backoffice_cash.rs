//! The glue of the back office's cash screens (spec 2.7 phases K5 and K7): starting the calls of
//! [`crate::backoffice::cash`] and showing its state in the window. Kept apart from
//! [`crate::backoffice_app`] only for size.
//!
//! Nota photos: Ctrl+U asks the window's [`PhotoSource`] for a file (a file over 10 MB is
//! `413 payload_too_large`, a picker failure [`ClientError::Unexpected`], as at the register);
//! the stored JPEGs are decoded into `slint::Image`s once per photo ID (a photo never changes).

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use slint::{Model, ModelRc, SharedString, VecModel};
use sultan_client::{
    BackOffice, CashMovement, ClientError, CloseReport, CountAmount, Problem, SessionDetail,
};

use crate::amount::{rupiah, signed_rupiah};
use crate::backoffice::cash::{
    Call, DIRECTIONS, KINDS, Pane, PhotoImage, Reply, STATUSES, TextPane,
};
use crate::backoffice::sales::place_of;
use crate::backoffice::{Controller, Msg};
use crate::controller::Notice;
use crate::input::date_time;
use crate::photo::{PhotoError, PhotoSource};
use crate::{
    BackOfficeWindow, BoCashFilters, BoDayClose, BoDocList, BoFact, BoMovementRow, BoPendingRow,
    BoPhoto, BoPhotoPane, BoReasonEditor, BoReasonRow, BoSessionRow, BoSessionView, BoTextPane,
    BoZRow, Note,
};

type MsgFuture = Pin<Box<dyn Future<Output = Msg> + Send>>;

/// The models of the cash screens.
#[derive(Default)]
pub struct CashModels {
    session_rows: Rc<VecModel<BoSessionRow>>,
    facts: Rc<VecModel<BoFact>>,
    session_movements: Rc<VecModel<BoMovementRow>>,
    session_text: Rc<VecModel<SharedString>>,
    movement_rows: Rc<VecModel<BoMovementRow>>,
    slip: Rc<VecModel<SharedString>>,
    pending_rows: Rc<VecModel<BoPendingRow>>,
    z_rows: Rc<VecModel<BoZRow>>,
    z_text: Rc<VecModel<SharedString>>,
    reason_rows: Rc<VecModel<BoReasonRow>>,
    branch_choices: Rc<VecModel<SharedString>>,
    register_choices: Rc<VecModel<SharedString>>,
    cashier_choices: Rc<VecModel<SharedString>>,
    session_choices: Rc<VecModel<SharedString>>,
    reason_choices: Rc<VecModel<SharedString>>,
    photos: Rc<VecModel<BoPhoto>>,
    /// Decoded nota photos by photo ID.
    decoded: RefCell<HashMap<String, slint::Image>>,
}

impl CashModels {
    /// Hands the models to the window.
    pub fn attach(&self, ui: &BackOfficeWindow) {
        ui.set_cash_session_rows(ModelRc::from(self.session_rows.clone()));
        ui.set_cash_session_facts(ModelRc::from(self.facts.clone()));
        ui.set_cash_session_movements(ModelRc::from(self.session_movements.clone()));
        ui.set_cash_session_text(ModelRc::from(self.session_text.clone()));
        ui.set_cash_movement_rows(ModelRc::from(self.movement_rows.clone()));
        ui.set_cash_slip(ModelRc::from(self.slip.clone()));
        ui.set_pending_rows(ModelRc::from(self.pending_rows.clone()));
        ui.set_z_rows(ModelRc::from(self.z_rows.clone()));
        ui.set_z_text(ModelRc::from(self.z_text.clone()));
        ui.set_cash_reason_rows(ModelRc::from(self.reason_rows.clone()));
        ui.set_cash_branch_choices(ModelRc::from(self.branch_choices.clone()));
        ui.set_cash_register_choices(ModelRc::from(self.register_choices.clone()));
        ui.set_cash_cashier_choices(ModelRc::from(self.cashier_choices.clone()));
        ui.set_cash_session_choices(ModelRc::from(self.session_choices.clone()));
        ui.set_cash_reason_choices(ModelRc::from(self.reason_choices.clone()));
        ui.set_cash_photos(ModelRc::from(self.photos.clone()));
    }

    /// The image of a stored photo, decoded once.
    fn image(&self, id: &str, bytes: &[u8]) -> slint::Image {
        self.decoded
            .borrow_mut()
            .entry(id.to_owned())
            .or_insert_with(|| crate::backoffice_app::decode(bytes))
            .clone()
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

/// Starts a cash call; the answer comes back as [`Msg::Cash`]. `photos` answers
/// [`Call::PickPhoto`].
#[allow(clippy::too_many_lines)] // one arm per call
pub fn start_cash(
    service: &dyn BackOffice,
    photos: &dyn PhotoSource,
    token: &str,
    call: &Call,
    lang: &str,
) -> MsgFuture {
    macro_rules! call {
        ($future:expr, $reply:expr) => {{
            let future = $future;
            let wrap = $reply;
            Box::pin(async move { Msg::Cash(Box::new(wrap(future.await))) }) as MsgFuture
        }};
    }
    match call {
        Call::Sessions { query, append, seq } => {
            let (append, seq) = (*append, *seq);
            call!(service.sessions(token, query), move |r| Reply::Sessions(
                append, seq, r
            ))
        }
        Call::Session(id) => {
            let id = id.clone();
            call!(service.session(token, &id), move |r| Reply::Session(
                id,
                Box::new(r)
            ))
        }
        Call::SessionMovements(id) => {
            let query = sultan_client::CashQuery {
                cashier_session_id: Some(id.clone()),
                limit: Some(crate::backoffice::cash::SESSION_MOVEMENTS),
                ..sultan_client::CashQuery::default()
            };
            let id = id.clone();
            call!(service.cash_movements(token, &query), move |r| {
                Reply::SessionMovements(id, r)
            })
        }
        Call::XText(id) => {
            let id = id.clone();
            call!(service.x_report_text(token, &id, lang, None), move |r| {
                Reply::Text(Pane::Session, id, r)
            })
        }
        Call::CloseText { id, copy } => {
            let id = id.clone();
            call!(
                service.close_report_text(token, &id, *copy, lang),
                move |r| { Reply::Text(Pane::Session, id, r) }
            )
        }
        Call::Movements { query, append, seq } => {
            let (append, seq) = (*append, *seq);
            call!(service.cash_movements(token, query), move |r| {
                Reply::Movements(append, seq, r)
            })
        }
        Call::Slip { id, copy } => {
            let id = id.clone();
            call!(service.cash_slip(token, &id, *copy, lang, None), move |r| {
                Reply::Text(Pane::Slip, id, r)
            })
        }
        Call::Pending(branch) => {
            let branch = branch.clone();
            call!(service.pending_days(token, Some(&branch)), move |r| {
                Reply::Pending(branch, r)
            })
        }
        Call::ZList { query, append, seq } => {
            let (append, seq) = (*append, *seq);
            call!(service.z_reports(token, query), move |r| Reply::ZList(
                append, seq, r
            ))
        }
        Call::ZText { id, copy } => {
            let id = id.clone();
            call!(service.z_report_text(token, &id, *copy, lang), move |r| {
                Reply::Text(Pane::Z, id, r)
            })
        }
        Call::RunZ { body, key } => {
            let day = body.day.clone();
            call!(service.run_z(token, body, key, None), move |r| Reply::ZRun(
                day,
                Box::new(r)
            ))
        }
        Call::Reasons { all } => {
            if *all {
                call!(service.all_cash_reasons(token), Reply::Reasons)
            } else {
                call!(service.cash_reasons(token), Reply::Reasons)
            }
        }
        Call::PutReason { id, body } => call!(service.put_cash_reason(token, id, body), |r| {
            Reply::ReasonSaved(false, r)
        }),
        Call::ArchiveReason(id) => call!(service.delete_cash_reason(token, id), |r| {
            Reply::ReasonSaved(true, r)
        }),
        Call::Movement(id) => call!(service.cash_movement(token, id), Reply::Movement),
        Call::Photo {
            movement_id,
            photo_id,
        } => {
            let (m, p) = (movement_id.clone(), photo_id.clone());
            call!(service.cash_photo(token, movement_id, photo_id), move |r| {
                Reply::Photo(m, p, r)
            })
        }
        Call::PickPhoto(movement_id) => {
            let m = movement_id.clone();
            call!(photos.pick_file(), move |r: crate::photo::Picked| {
                Reply::Picked(
                    m,
                    r.map_err(|e| match e {
                        PhotoError::TooLarge => ClientError::Api(Problem::new(
                            413,
                            "payload_too_large",
                            "the file is larger than 10 MB",
                        )),
                        PhotoError::Failed(message) => ClientError::Unexpected(message),
                    }),
                )
            })
        }
        Call::Upload(upload) => {
            let sent = upload.clone();
            call!(
                service.add_cash_photo(
                    token,
                    &upload.movement_id,
                    &upload.photo_id,
                    upload.bytes.clone()
                ),
                move |r| Reply::Uploaded(sent, r)
            )
        }
    }
}

fn fact(label: &str, value: impl Into<SharedString>) -> BoFact {
    BoFact {
        label: label.into(),
        value: value.into(),
        raw: false,
        tone: 0,
    }
}

fn counted(c: CountAmount) -> String {
    format!("{} · {}", c.count, rupiah(c.amount))
}

fn lines(text: &[String]) -> Vec<SharedString> {
    text.iter()
        .map(|l| SharedString::from(l.as_str()))
        .collect()
}

fn pane(p: &TextPane) -> BoTextPane {
    BoTextPane {
        loading: p.loading,
        notice: note(p.notice.as_ref()),
        copy: p.copy,
        empty: p.lines.is_empty() && !p.loading && p.notice.is_none(),
    }
}

/// A cash movement as a table row: the reason (or the reversed number), the signed effect, and
/// a flag (`reversal`, `reversed`, `missing_nota`).
fn movement_row(m: &CashMovement) -> BoMovementRow {
    let reason = match (&m.reverses_number, &m.reason_name, &m.note) {
        (Some(n), _, _) => format!("↩ {n}"),
        (None, Some(r), Some(note)) => format!("{r} · {note}"),
        (None, Some(r), None) => r.clone(),
        (None, None, note) => note.clone().unwrap_or_default(),
    };
    let flag = if m.reverses.is_some() {
        "reversal"
    } else if m.reversed_by.is_some() {
        "reversed"
    } else if m.missing_nota {
        "missing_nota"
    } else {
        ""
    };
    BoMovementRow {
        number: m.number.as_str().into(),
        time: date_time(m.posted_at).into(),
        place: place_of(&m.number).into(),
        kind: m.kind.as_str().into(),
        reason: reason.into(),
        amount: signed_rupiah(m.effect).into(),
        flag: flag.into(),
        photos: index(m.photo_ids.len()),
    }
}

/// The facts of a closed session from its stored close report.
fn closed_facts(r: &CloseReport, out: &mut Vec<BoFact>) {
    if let Some(at) = r.session.closed_at {
        out.push(fact("closed_at", date_time(at)));
    }
    if let Some(by) = &r.forced_by_name {
        out.push(fact("forced_by", by.clone()));
    }
    out.push(fact("expected_cash", rupiah(r.expected_cash)));
    out.push(fact("counted_cash", rupiah(r.counted_cash)));
    let mut over = fact("over_short", signed_rupiah(r.over_short));
    over.tone = if r.over_short < 0 { -1 } else { 0 };
    out.push(over);
    if let Some(kept) = r.float.float_kept {
        out.push(fact("float_kept", rupiah(kept)));
    }
    if let Some(safe) = r.float.to_safe {
        out.push(fact("to_safe", rupiah(safe)));
    }
    let c = &r.cash;
    out.push(fact("cash_in", counted(c.cash_in)));
    out.push(fact("cash_out", counted(c.cash_out)));
    out.push(fact("drops", counted(c.drops)));
    if c.reversals.count > 0 {
        out.push(fact("reversals", counted(c.reversals)));
    }
    out.push(fact("drawer_opens", c.drawer_opens.to_string()));
    out.push(fact("x_reads", c.x_reports.to_string()));
    if c.missing_nota > 0 {
        let mut f = fact("missing_nota", c.missing_nota.to_string());
        f.tone = -1;
        out.push(f);
    }
}

/// The nota photos under the slip (Ctrl+2).
fn render_photos(ui: &BackOfficeWindow, c: &Controller, models: &CashModels) {
    let p = &c.cash.photos;
    let rows: Vec<BoPhoto> = p
        .photos
        .iter()
        .map(|(id, image)| match image {
            PhotoImage::Loading => BoPhoto {
                loading: true,
                ..BoPhoto::default()
            },
            PhotoImage::Loaded(bytes) => BoPhoto {
                image: models.image(id, bytes),
                ..BoPhoto::default()
            },
            PhotoImage::Failed(n) => BoPhoto {
                notice: note(Some(n)),
                ..BoPhoto::default()
            },
        })
        .collect();
    let selected = rows.get(p.selected).cloned().unwrap_or_default();
    sync(&models.photos, rows);
    let movement = c.cash.movements.rows.iter().find(|m| m.id == p.movement_id);
    ui.set_cash_photo_pane(BoPhotoPane {
        shown: !p.movement_id.is_empty(),
        selected: index(p.selected),
        large: selected,
        zoom: p.zoom,
        busy: p.busy,
        missing: movement.is_some_and(|m| m.missing_nota),
        notice: note(p.notice.as_ref()),
    });
}

fn session_facts(c: &Controller, d: &SessionDetail) -> Vec<BoFact> {
    let s = &d.session;
    let listed = c.cash.sessions.rows.iter().find(|r| r.id == s.id);
    let cashier = d
        .close_report
        .as_ref()
        .map(|r| r.cashier_name.clone())
        .or_else(|| listed.and_then(|r| r.cashier_name.clone()))
        .unwrap_or_default();
    let mut out = vec![
        fact("cashier", cashier),
        fact("place", place_of(&s.number)),
        fact("opened_at", date_time(s.opened_at)),
        fact("opening_cash", rupiah(s.opening_cash)),
    ];
    match &d.close_report {
        Some(r) => closed_facts(r, &mut out),
        None => out.push(fact(
            "movements",
            c.cash.session.movements.len().to_string(),
        )),
    }
    out
}

/// Shows the cash screens.
#[allow(clippy::too_many_lines)] // one block per screen
#[allow(clippy::many_single_char_names)] // short names per block, as in `backoffice_sales`
pub fn render_cash(ui: &BackOfficeWindow, c: &Controller, models: &CashModels) {
    let st = &c.cash;
    ui.set_cash_serial(i32::try_from(st.serial % 1_000_000).unwrap_or(0));
    let f = &st.filters;
    let with_all = |labels: Vec<String>| {
        let mut out = vec![SharedString::new()];
        out.extend(labels.into_iter().map(SharedString::from));
        out
    };
    sync(
        &models.branch_choices,
        with_all(
            c.reference
                .branches
                .iter()
                .map(|b| b.code.clone())
                .collect(),
        ),
    );
    sync(
        &models.register_choices,
        with_all(st.registers.iter().map(|r| r.1.clone()).collect()),
    );
    sync(
        &models.cashier_choices,
        with_all(st.cashiers.iter().map(|r| r.1.clone()).collect()),
    );
    sync(
        &models.session_choices,
        with_all(st.seen_sessions.iter().map(|r| r.1.clone()).collect()),
    );
    sync(
        &models.reason_choices,
        with_all(
            st.reasons
                .iter()
                .map(|r| {
                    let sign = if r.direction == "in" { "+" } else { "−" };
                    format!("{sign} {}", r.name)
                })
                .collect(),
        ),
    );
    ui.set_cash_filters(BoCashFilters {
        branch_index: index(f.branch),
        register_index: index(f.register),
        cashier_index: index(f.cashier),
        session_index: index(f.session),
        status_index: index(f.status.min(STATUSES.len() - 1)),
        kind_index: index(f.kind.min(KINDS.len() - 1)),
        reason_index: index(f.reason),
        missing_nota: f.missing_nota,
        from: f.from.as_str().into(),
        until: f.until.as_str().into(),
        date_error: f.bad_date.map_or_else(Note::default, |field| Note {
            code: "invalid_date".into(),
            arg: field.into(),
            tone: -1,
        }),
    });
    let can_read = c.can_read_sales();
    let list =
        |cursor: usize, loading: bool, more: bool, n: Option<&Notice>, len: usize| BoDocList {
            cursor: index(cursor),
            loading,
            more,
            notice: note(n),
            empty: len == 0 && !loading && can_read,
        };

    // Sessions.
    let l = &st.sessions;
    ui.set_cash_session_list(list(
        l.cursor,
        l.loading,
        l.next.is_some(),
        l.notice.as_ref(),
        l.rows.len(),
    ));
    sync(
        &models.session_rows,
        l.rows
            .iter()
            .map(|s| BoSessionRow {
                number: s.number.as_str().into(),
                opened: date_time(s.opened_at).into(),
                closed: s.closed_at.map(date_time).unwrap_or_default().into(),
                place: place_of(&s.number).into(),
                cashier: s.cashier_name.clone().unwrap_or_default().into(),
                opening: rupiah(s.opening_cash).into(),
                expected: s.expected_cash.map(rupiah).unwrap_or_default().into(),
                status: s.status.as_str().into(),
            })
            .collect(),
    );

    // The session detail.
    let v = &st.session;
    let facts = v
        .detail
        .as_ref()
        .map(|d| session_facts(c, d))
        .unwrap_or_default();
    ui.set_cash_session_view(BoSessionView {
        title: v
            .detail
            .as_ref()
            .map(|d| d.session.number.clone())
            .unwrap_or_default()
            .into(),
        open: v.is_open(),
        loading: v.detail.is_none() && v.notice.is_none(),
        notice: note(v.notice.as_ref()),
        cursor: index(v.cursor),
        can_reprint: v.detail.is_some() && !v.is_open(),
    });
    sync(&models.facts, facts);
    sync(
        &models.session_movements,
        v.movements.iter().map(movement_row).collect(),
    );
    ui.set_cash_session_pane(pane(&v.text));
    sync(&models.session_text, lines(&v.text.lines));

    // Movements.
    let l = &st.movements;
    ui.set_cash_movement_list(list(
        l.cursor,
        l.loading,
        l.next.is_some(),
        l.notice.as_ref(),
        l.rows.len(),
    ));
    sync(
        &models.movement_rows,
        l.rows.iter().map(movement_row).collect(),
    );
    ui.set_cash_slip_pane(pane(&st.slip));
    sync(&models.slip, lines(&st.slip.lines));
    render_photos(ui, c, models);

    // Day close.
    let d = &st.day;
    let branch = d
        .branch
        .as_ref()
        .and_then(|b| c.reference.branches.iter().find(|x| x.id == *b))
        .map(|b| format!("{} · {}", b.code, b.name))
        .unwrap_or_default();
    let branch_index = d
        .branch
        .as_ref()
        .and_then(|b| c.reference.branches.iter().position(|x| x.id == *b))
        .map_or(0, |i| index(i + 1));
    ui.set_day_close(BoDayClose {
        branch: branch.into(),
        branch_index,
        cursor: index(d.cursor),
        pending_count: index(d.pending.len()),
        loading: d.pending_loading || d.z.loading,
        more: d.z.next.is_some(),
        notice: note(d.notice.as_ref()),
        list_notice: note(d.z.notice.as_ref()),
        running: d.running,
        can_run: c.can_close_day(),
        empty: d.pending.is_empty() && d.z.rows.is_empty() && !d.pending_loading && can_read,
    });
    sync(
        &models.pending_rows,
        d.pending
            .iter()
            .map(|p| BoPendingRow {
                day: p.day.as_str().into(),
                open: i32::try_from(p.open).unwrap_or(i32::MAX),
                closed: i32::try_from(p.closed).unwrap_or(i32::MAX),
            })
            .collect(),
    );
    sync(
        &models.z_rows,
        d.z.rows
            .iter()
            .map(|z| BoZRow {
                number: z.number.as_str().into(),
                day: z.day.as_str().into(),
                sessions: z.session_count.to_string().into(),
                net: rupiah(z.net_sales).into(),
                over_short: signed_rupiah(z.over_short).into(),
                deposit: rupiah(z.cash_to_deposit).into(),
                supplementary: z.supplementary,
            })
            .collect(),
    );
    ui.set_z_pane(pane(&st.z_text));
    sync(&models.z_text, lines(&st.z_text.lines));

    // Cash reasons.
    sync(
        &models.reason_rows,
        st.reasons
            .iter()
            .map(|r| BoReasonRow {
                name: r.name.as_str().into(),
                direction: r.direction.as_str().into(),
                note_required: r.note_required,
                active: r.active,
                archived: r.archived,
            })
            .collect(),
    );
    let e = &st.editor;
    ui.set_cash_reason_editor(BoReasonEditor {
        cursor: index(e.cursor),
        is_new: e.id.is_none(),
        name: e.name.as_str().into(),
        direction_index: index(e.direction.min(DIRECTIONS.len() - 1)),
        note_required: e.note_required,
        active: e.active,
        sort: e.sort.as_str().into(),
        archived: e.archived,
        can_edit: c.reasons_editable() && !e.archived && !e.saving,
        notice: note(e.notice.as_ref()),
    });
    ui.set_cash_reasons_read_only(note(c.reasons_read_only().as_ref()));
}
