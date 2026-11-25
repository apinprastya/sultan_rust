//! Drives the back office's cash screens (spec 2.7, phase K5) headlessly against
//! [`FakeService`], like `backoffice_sales.rs`: keys are window events, buttons are found by
//! their accessible label, text fields are set through the window's `edit` callback.
//!
//! The history ([`seed`]): Ani sells three sales (one voided, one returned), takes Rp 100 000 in
//! (top-up) and pays Rp 25 000 out (shop supplies), then closes her session; the next day Budi
//! opens one with Rp 200 000 and drops Rp 150 000 to the safe. In the fake the owner holds
//! `report.sales`, `session.view_expected`, `day.close` and `tender.manage`; budi all but
//! `tender.manage`; kasir none.

use std::sync::Arc;
use std::time::Duration;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_client::fake::{FakeService, SEED_REASONS};
use sultan_client::{
    CashMovementRequest, ClientError, CloseRequest, Problem, ServerEvent, Service,
};
use sultan_core::doc_number::{BranchTimeZone, LocalDay};
use sultan_core::time::Timestamp;
use sultan_ui::{BackOfficeApp, BackOfficeOptions, Note, Runner, build_backoffice};

const DAY: i64 = 86_400_000;

struct Office {
    app: BackOfficeApp,
    fake: Arc<FakeService>,
    runtime: tokio::runtime::Runtime,
    /// Ani's closed session (number).
    closed: String,
    /// Budi's open session (ID, number).
    open: (String, String),
    /// The business day of the closed session, `YYYY-MM-DD`.
    day: String,
}

/// What [`seed`] posted.
struct Seeded {
    closed: String,
    open: (String, String),
    day: String,
}

fn movement(kind: &str, amount: i64, reason: Option<&str>) -> CashMovementRequest {
    CashMovementRequest {
        kind: Some(kind.to_owned()),
        amount: Some(amount),
        reason_id: reason.map(str::to_owned),
        ..CashMovementRequest::default()
    }
}

/// Posts the history of the module docs; with `blocked` a session of Ani's day stays open on
/// another register (it blocks that day's Z).
async fn seed(fake: &FakeService, blocked: bool) -> Seeded {
    fake.seed_sales_demo(3).await;
    let ani = fake.pin_login("u-ani", "1234").await.unwrap().token;
    let session = fake.current_session(&ani).await.unwrap().unwrap();
    for (id, body) in [
        (
            "0190a000-0000-7000-8000-00000000c001",
            movement("in", 100_000, Some(SEED_REASONS[0])),
        ),
        (
            "0190a000-0000-7000-8000-00000000c002",
            movement("out", 25_000, Some(SEED_REASONS[2])),
        ),
    ] {
        fake.post_cash_movement(&ani, id, &body, None)
            .await
            .unwrap();
    }
    if blocked {
        fake.open_elsewhere("u-citra", 0);
    }
    let close = CloseRequest {
        counted_cash: 120_000,
        counts: None,
        float_kept: None,
    };
    fake.close_session(&ani, &session.id, &close, "close")
        .await
        .unwrap();
    fake.set_now(1_800_000_000_000 + DAY);
    let budi = fake.pin_login("u-budi", "9999").await.unwrap().token;
    let open = fake
        .open_session(&budi, 200_000, "budi-open")
        .await
        .unwrap();
    fake.post_cash_movement(
        &budi,
        "0190a000-0000-7000-8000-00000000c003",
        &movement("drop", 150_000, None),
        None,
    )
    .await
    .unwrap();
    let (y, m, d) = LocalDay::of(
        Timestamp::from_unix_millis(session.opened_at),
        BranchTimeZone::default(),
    )
    .ymd();
    Seeded {
        closed: session.number,
        open: (open.id, open.number),
        day: format!("{y:04}-{m:02}-{d:02}"),
    }
}

fn office(blocked: bool) -> Office {
    thread_local!(static READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    if !READY.get() {
        i_slint_backend_testing::init_no_event_loop();
        READY.set(true);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let fake = Arc::new(FakeService::default());
    let seeded = runtime.block_on(seed(&fake, blocked));
    let mut options = BackOfficeOptions::new("http://127.0.0.1:8080");
    options.language = Some("en".to_owned());
    options.debounce = Duration::ZERO;
    let app = build_backoffice(
        fake.clone(),
        Runner::Block(runtime.handle().clone()),
        options,
    )
    .unwrap();
    app.ui.window().set_size(LogicalSize::new(1366.0, 768.0));
    app.ui.show().unwrap();
    Office {
        app,
        fake,
        runtime,
        closed: seeded.closed,
        open: seeded.open,
        day: seeded.day,
    }
}

fn key_text(name: &str) -> SharedString {
    match name {
        "Enter" => Key::Return.into(),
        "Esc" => Key::Escape.into(),
        "Up" => Key::UpArrow.into(),
        "Down" => Key::DownArrow.into(),
        "Control" => Key::Control.into(),
        "Shift" => Key::Shift.into(),
        "F5" => Key::F5.into(),
        "F9" => Key::F9.into(),
        other => other.into(),
    }
}

impl Office {
    fn press(&self, parts: &[&str]) {
        let window = self.app.ui.window();
        for p in parts {
            window.dispatch_event(WindowEvent::KeyPressed { text: key_text(p) });
        }
        for p in parts.iter().rev() {
            window.dispatch_event(WindowEvent::KeyReleased { text: key_text(p) });
        }
    }

    fn ctrl(&self, c: &str) {
        self.press(&["Control", c]);
    }

    #[track_caller]
    fn click(&self, label: &str) {
        let found: Vec<ElementHandle> =
            ElementHandle::find_by_accessible_label(&self.app.ui, label)
                .filter(|e| {
                    matches!(
                        e.accessible_role(),
                        Some(AccessibleRole::Button | AccessibleRole::Tab)
                    )
                })
                .collect();
        assert_eq!(found.len(), 1, "exactly one button labelled {label:?}");
        found[0].mock_single_click(PointerEventButton::Left);
    }

    fn edit(&self, kind: &str, field: &str, value: &str) {
        self.app
            .ui
            .invoke_edit(kind.into(), 0, 0, field.into(), value.into());
    }

    fn choose(&self, kind: &str, field: &str, index: i32) {
        self.app
            .ui
            .invoke_choose(kind.into(), 0, field.into(), index);
    }

    fn screen(&self) -> String {
        self.app.ui.get_screen().into()
    }

    fn text(&self, note: Note) -> String {
        self.app.ui.invoke_message(note).into()
    }

    fn login(&self, user: &str) {
        self.edit("login", "username", user);
        self.edit("login", "password", &format!("{user}-pass-1"));
        self.app.ui.invoke_action("login".into());
        assert_eq!(self.screen(), "products");
    }

    fn sessions(&self) -> Vec<String> {
        self.app
            .ui
            .get_cash_session_rows()
            .iter()
            .map(|r| r.number.to_string())
            .collect()
    }

    fn movements(&self) -> Vec<(String, String)> {
        self.app
            .ui
            .get_cash_movement_rows()
            .iter()
            .map(|r| (r.kind.to_string(), r.amount.to_string()))
            .collect()
    }

    fn joined(model: &slint::ModelRc<SharedString>) -> String {
        model
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn facts(&self) -> Vec<(String, String)> {
        self.app
            .ui
            .get_cash_session_facts()
            .iter()
            .map(|f| (f.label.to_string(), f.value.to_string()))
            .collect()
    }

    fn fact(&self, label: &str) -> Option<String> {
        self.facts()
            .into_iter()
            .find(|(l, _)| l == label)
            .map(|(_, v)| v)
    }
}

#[test]
#[allow(clippy::too_many_lines)] // one walk through the list, the filters and both details
fn sessions_list_filters_and_both_details() {
    let o = office(false);
    o.login("owner");
    o.press(&["F9"]);
    assert_eq!(o.screen(), "cash-sessions");
    assert_eq!(o.sessions(), [o.open.1.clone(), o.closed.clone()]);
    let rows: Vec<_> = o.app.ui.get_cash_session_rows().iter().collect();
    assert_eq!(
        (rows[0].cashier.as_str(), rows[0].status.as_str()),
        ("Budi", "open")
    );
    assert_eq!(
        (rows[1].cashier.as_str(), rows[1].place.as_str()),
        ("Ani", "JKT01 · R1")
    );
    assert!(!rows[1].closed.is_empty() && rows[0].closed.is_empty());
    // Status, cashier (learned from the rows, by name: Ani, Budi), dates.
    o.choose("cash-filter", "status", 2);
    assert_eq!(o.sessions(), [o.closed.as_str()]);
    o.choose("cash-filter", "status", 0);
    o.choose("cash-filter", "cashier", 2);
    assert_eq!(o.sessions(), [o.open.1.as_str()]);
    assert_eq!(
        o.app
            .ui
            .get_cash_cashier_choices()
            .iter()
            .collect::<Vec<_>>(),
        ["", "Ani", "Budi"]
    );
    o.click("Clear");
    o.edit("cash-filter", "from", "2027-02-30");
    o.click("Search");
    assert_eq!(o.app.ui.get_cash_filters().date_error.arg, "from");
    o.edit("cash-filter", "from", &o.day);
    o.edit("cash-filter", "until", &o.day);
    o.click("Search");
    assert_eq!(o.sessions(), [o.closed.as_str()]);
    o.click("Clear");
    assert_eq!(o.sessions().len(), 2);

    // The closed session: its stored close report, facts and movements; Ctrl+P a copy.
    o.press(&["Down"]);
    o.press(&["Enter"]);
    assert_eq!(o.screen(), "cash-session-detail");
    let view = o.app.ui.get_cash_session_view();
    assert_eq!((view.title.as_str(), view.open), (o.closed.as_str(), false));
    assert_eq!(o.fact("cashier").as_deref(), Some("Ani"));
    assert_eq!(o.fact("counted_cash").as_deref(), Some("Rp 120.000"));
    assert_eq!(o.fact("cash_in").as_deref(), Some("1 · Rp 100.000"));
    assert_eq!(o.fact("drawer_opens").as_deref(), Some("0"));
    let text = Office::joined(&o.app.ui.get_cash_session_text());
    assert!(text.contains("CLOSE REPORT"), "{text}");
    assert!(!o.app.ui.get_cash_session_pane().copy);
    assert_eq!(o.app.ui.get_cash_session_movements().row_count(), 2);
    o.press(&["Down"]);
    assert_eq!(o.app.ui.get_cash_session_view().cursor, 1);
    o.ctrl("p");
    assert!(o.app.ui.get_cash_session_pane().copy);
    assert!(Office::joined(&o.app.ui.get_cash_session_text()).contains("COPY"));
    assert_eq!(
        o.fake
            .calls_of("close_report_text")
            .last()
            .map(String::as_str),
        Some(format!("{} copy=true", o.fake.calls_of("session")[0]).as_str())
    );
    // Esc goes back to the list as it was.
    let listed = o.fake.calls_of("sessions").len();
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "cash-sessions");
    assert_eq!(o.fake.calls_of("sessions").len(), listed);

    // The open session: the live X (one audited read), no reprint.
    o.press(&["Up"]);
    o.click("Open");
    assert_eq!(o.screen(), "cash-session-detail");
    assert!(o.app.ui.get_cash_session_view().open);
    let text = Office::joined(&o.app.ui.get_cash_session_text());
    assert!(text.contains("X REPORT"), "{text}");
    assert_eq!(o.fake.calls_of("x_report_text").len(), 1);
    assert_eq!(o.fact("opening_cash").as_deref(), Some("Rp 200.000"));
    assert_eq!(o.fact("cashier").as_deref(), Some("Budi"));
    o.ctrl("p");
    assert_eq!(o.fake.calls_of("x_report_text").len(), 1, "no copy of an X");
    // A cash movement on it reloads its movements.
    let before = o.fake.calls_of("cash_movements").len();
    o.fake.emit(ServerEvent {
        kind: "cash.moved".into(),
        data: serde_json::json!({ "session_id": o.open.0 }),
    });
    o.app.pump();
    assert_eq!(o.fake.calls_of("cash_movements").len(), before + 1);
    // F5 reads the X again; a refused X says which permission is missing.
    o.fake.fail_on(
        "x_report_text",
        Problem::new(403, "permission_denied", "denied")
            .with("permission", "session.view_expected")
            .into(),
    );
    o.press(&["F5"]);
    let pane = o.app.ui.get_cash_session_pane();
    assert!(
        o.text(pane.notice).contains("session.view_expected"),
        "the pane says why"
    );
    assert_eq!(o.app.ui.get_cash_session_text().row_count(), 0);
    o.click("Back");
    assert_eq!(o.screen(), "cash-sessions");
}

#[test]
fn without_report_sales_the_lists_say_so() {
    let o = office(false);
    o.login("kasir");
    o.press(&["F9"]);
    assert!(o.sessions().is_empty());
    let n = o.app.ui.get_cash_session_list().notice;
    assert!(o.text(n).contains("report.sales"));
    o.ctrl("2");
    assert_eq!(o.screen(), "cash-movements");
    assert!(o.movements().is_empty());
    assert!(
        o.text(o.app.ui.get_cash_movement_list().notice)
            .contains("report.sales")
    );
    o.ctrl("3");
    assert!(
        o.text(o.app.ui.get_day_close().list_notice)
            .contains("report.sales")
    );
    assert_eq!(o.fake.calls_of("pending_days").len(), 0);
}

#[test]
fn movements_filters_and_the_slip() {
    let o = office(false);
    o.login("owner");
    o.press(&["F9"]);
    o.ctrl("2");
    assert_eq!(o.screen(), "cash-movements");
    assert_eq!(
        o.movements(),
        [
            ("drop".to_owned(), "-Rp 150.000".to_owned()),
            ("out".to_owned(), "-Rp 25.000".to_owned()),
            ("in".to_owned(), "+Rp 100.000".to_owned()),
        ]
    );
    let first = o.app.ui.get_cash_movement_rows().row_data(1).unwrap();
    assert!(first.reason.contains("Belanja"), "{}", first.reason);
    // Kind, reason (the seeded ones: in first, then out), session (seen in the sessions list).
    o.choose("cash-filter", "kind", 2);
    assert_eq!(o.movements().len(), 1);
    o.choose("cash-filter", "kind", 0);
    let reasons: Vec<SharedString> = o.app.ui.get_cash_reason_choices().iter().collect();
    let shop = reasons.iter().position(|r| r.contains("Belanja")).unwrap();
    o.choose("cash-filter", "reason", i32::try_from(shop).unwrap());
    assert_eq!(o.movements(), [("out".to_owned(), "-Rp 25.000".to_owned())]);
    o.choose("cash-filter", "reason", 0);
    let sessions: Vec<SharedString> = o.app.ui.get_cash_session_choices().iter().collect();
    let closed = sessions.iter().position(|s| *s == o.closed).unwrap();
    o.choose("cash-filter", "session", i32::try_from(closed).unwrap());
    assert_eq!(o.movements().len(), 2);
    o.click("Clear");
    assert_eq!(o.movements().len(), 3);
    // Enter shows the slip beside the list; Ctrl+P its copy.
    assert!(o.app.ui.get_cash_slip_pane().empty);
    o.press(&["Enter"]);
    let slip = Office::joined(&o.app.ui.get_cash_slip());
    assert!(slip.contains("SAFE DROP"), "{slip}");
    o.press(&["Down"]);
    o.ctrl("p");
    let slip = Office::joined(&o.app.ui.get_cash_slip());
    assert!(slip.contains("CASH OUT") && slip.contains("COPY"), "{slip}");
    assert!(o.app.ui.get_cash_slip_pane().copy);
    // A refused copy says which permission it needs.
    o.fake.fail_on(
        "cash_slip",
        Problem::new(403, "permission_denied", "denied")
            .with("permission", "sale.reprint")
            .into(),
    );
    o.ctrl("p");
    assert!(
        o.text(o.app.ui.get_cash_slip_pane().notice)
            .contains("sale.reprint")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // the run, its retry and each refusal
fn day_close_runs_the_z_after_asking() {
    let o = office(false);
    o.login("owner");
    o.press(&["F9"]);
    o.ctrl("3");
    assert_eq!(o.screen(), "day-close");
    let pending: Vec<_> = o.app.ui.get_pending_rows().iter().collect();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        (pending[0].day.as_str(), pending[0].closed, pending[0].open),
        (o.day.as_str(), 1, 0)
    );
    assert_eq!(o.app.ui.get_z_rows().row_count(), 0);
    assert_eq!(o.app.ui.get_day_close().branch, "JKT01 · Toko Jakarta");
    // Enter asks first; Esc keeps the day.
    o.press(&["Enter"]);
    assert_eq!(o.app.ui.get_confirm(), "run-z");
    assert_eq!(o.app.ui.get_confirm_name(), o.day.as_str());
    assert_eq!(o.app.ui.get_confirm_other(), "1");
    o.press(&["Esc"]);
    assert!(o.fake.calls_of("run_z").is_empty());
    // A network error keeps the idempotency key for the retry.
    o.fake.fail_on("run_z", ClientError::Network("down".into()));
    o.press(&["Enter"]);
    o.press(&["Enter"]);
    assert!(
        o.text(o.app.ui.get_day_close().notice)
            .contains("Cannot reach")
    );
    o.press(&["Enter"]);
    o.click("Run the Z");
    let runs = o.fake.calls_of("run_z");
    assert_eq!(runs.len(), 2);
    let key = |d: &str| {
        d.split(" key=")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_owned()
    };
    assert_eq!(key(&runs[0]), key(&runs[1]), "the retry replays the run");
    // The Z is posted, selected and shown; the day is closed.
    let day = o.app.ui.get_day_close();
    assert!(o.text(day.notice).contains("JKT01-Z-"));
    assert_eq!(o.app.ui.get_pending_rows().row_count(), 0);
    let z = o.app.ui.get_z_rows().row_data(0).unwrap();
    assert_eq!(z.sessions, "1");
    assert_eq!(day.cursor, 0);
    let text = Office::joined(&o.app.ui.get_z_text());
    assert!(
        text.contains("Z REPORT") && text.contains(z.number.as_str()),
        "{text}"
    );
    // The fake's Z counts the sales documents (phase K5 fake fix).
    assert!(text.contains("JKT01-R1-270115-0001"), "{text}");
    o.ctrl("p");
    assert!(o.app.ui.get_z_pane().copy);
    assert!(Office::joined(&o.app.ui.get_z_text()).contains("COPY"));
    o.press(&["Enter"]);
    assert!(!o.app.ui.get_z_pane().copy, "Enter shows it again");
    // `day.closed` elsewhere reloads the screen.
    let before = o.fake.calls_of("pending_days").len();
    o.fake.emit(ServerEvent {
        kind: "day.closed".into(),
        data: serde_json::json!({}),
    });
    o.app.pump();
    assert_eq!(o.fake.calls_of("pending_days").len(), before + 1);
}

#[test]
fn a_z_with_an_open_session_is_refused_with_its_number() {
    let o = office(true);
    o.login("owner");
    o.press(&["F9"]);
    o.ctrl("3");
    let pending = o.app.ui.get_pending_rows().row_data(0).unwrap();
    assert_eq!((pending.closed, pending.open), (1, 1));
    o.press(&["Enter"]);
    o.press(&["Enter"]);
    let notice = o.text(o.app.ui.get_day_close().notice);
    assert!(notice.contains("JKT01-R1-S-"), "{notice}");
    assert_eq!(o.app.ui.get_z_rows().row_count(), 0);
    // The other refusals are messages too.
    for (problem, words) in [
        (
            Problem::new(409, "nothing_to_close", "x"),
            "Nothing to close",
        ),
        (
            Problem::new(422, "invalid_field", "x").with("field", "day"),
            "has not started",
        ),
        (
            Problem::new(403, "permission_denied", "x").with("permission", "day.close"),
            "day.close",
        ),
        (Problem::new(403, "read_only_replica", "x"), "HQ server"),
    ] {
        o.fake.fail_on("run_z", problem.into());
        o.press(&["Enter"]);
        o.press(&["Enter"]);
        let notice = o.text(o.app.ui.get_day_close().notice);
        assert!(notice.contains(words), "{notice}");
    }
}

#[test]
fn a_manager_runs_the_z_of_the_own_branch() {
    // Budi (a manager at JKT01) holds `day.close`: the day close offers the run.
    let o = office(false);
    o.login("budi");
    o.press(&["F9"]);
    o.ctrl("3");
    assert!(o.app.ui.get_day_close().can_run);
    o.press(&["Enter"]);
    assert_eq!(o.app.ui.get_confirm(), "run-z");
    o.click("No");
    assert_eq!(o.app.ui.get_confirm(), "");
}

#[test]
#[allow(clippy::too_many_lines)] // the editor's whole cycle
fn cash_reasons_new_save_archive_and_read_only() {
    let o = office(false);
    o.login("owner");
    o.press(&["F9"]);
    o.ctrl("4");
    assert_eq!(o.screen(), "cash-reasons");
    let names = |o: &Office| -> Vec<String> {
        o.app
            .ui
            .get_cash_reason_rows()
            .iter()
            .map(|r| r.name.to_string())
            .collect()
    };
    assert_eq!(names(&o).len(), 6);
    assert_eq!(o.app.ui.get_cash_reasons_read_only().code, "");
    o.press(&["Down"]);
    assert_eq!(o.app.ui.get_cash_reason_editor().cursor, 1);
    // New: a cash out reason; bad input is refused on screen.
    o.ctrl("n");
    let e = o.app.ui.get_cash_reason_editor();
    assert!(e.is_new && e.active);
    assert_eq!(e.direction_index, 1);
    o.ctrl("s");
    assert!(
        o.text(o.app.ui.get_cash_reason_editor().notice)
            .contains("name")
    );
    o.edit("cash-reason", "name", "Parkir motor");
    o.edit("cash-reason", "sort", "x");
    o.ctrl("s");
    assert!(
        o.text(o.app.ui.get_cash_reason_editor().notice)
            .contains("whole number")
    );
    o.edit("cash-reason", "sort", "9");
    o.app
        .ui
        .invoke_toggle("cash-reason".into(), 0, "note_required".into(), true);
    o.ctrl("s");
    assert_eq!(names(&o).len(), 7);
    let e = o.app.ui.get_cash_reason_editor();
    assert!(!e.is_new && e.note_required);
    assert_eq!(o.text(e.notice), "Saved.");
    let stored = o
        .runtime
        .block_on(o.fake.cash_reasons(&o.fake_token()))
        .unwrap();
    let parkir = stored.iter().find(|r| r.name == "Parkir motor").unwrap();
    assert_eq!((parkir.direction.as_str(), parkir.sort), ("out", 9));
    // The direction is fixed once saved.
    o.choose("cash-reason", "direction", 0);
    assert_eq!(o.app.ui.get_cash_reason_editor().direction_index, 1);
    // Archive asks first.
    o.press(&["Control", "Shift", "a"]);
    assert_eq!(o.app.ui.get_confirm(), "archive-cash-reason");
    assert_eq!(o.app.ui.get_confirm_name(), "Parkir motor");
    o.press(&["Enter"]);
    let e = o.app.ui.get_cash_reason_editor();
    assert!(e.archived && !e.can_edit);
    assert!(o.text(e.notice).contains("archived"));
    // A branch node refuses writes: read-only from then on.
    o.press(&["Up"]);
    o.fake.fail_on(
        "put_cash_reason",
        Problem::new(403, "read_only_replica", "x").into(),
    );
    o.ctrl("s");
    assert!(
        o.text(o.app.ui.get_cash_reasons_read_only())
            .contains("branch server")
    );
    assert!(!o.app.ui.get_cash_reason_editor().can_edit);
    let puts = o.fake.calls_of("put_cash_reason").len();
    o.ctrl("s");
    o.ctrl("n");
    assert_eq!(o.fake.calls_of("put_cash_reason").len(), puts);
    assert!(!o.app.ui.get_cash_reason_editor().is_new);
}

#[test]
fn cash_reasons_are_read_only_without_tender_manage() {
    let o = office(false);
    o.login("budi");
    o.press(&["F9"]);
    o.ctrl("4");
    let ro = o.app.ui.get_cash_reasons_read_only();
    assert!(o.text(ro).contains("tender.manage"));
    assert_eq!(
        o.app.ui.get_cash_reason_rows().row_count(),
        6,
        "the active ones"
    );
    assert!(
        o.fake
            .calls_of("cash_reasons")
            .contains(&"all=false".to_owned())
    );
    assert!(!o.app.ui.get_cash_reason_editor().can_edit);
    o.ctrl("n");
    o.ctrl("s");
    o.press(&["Control", "Shift", "a"]);
    assert_eq!(o.app.ui.get_confirm(), "");
    assert!(o.fake.calls_of("put_cash_reason").is_empty());
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "cash-sessions");
}

impl Office {
    fn fake_token(&self) -> String {
        self.runtime
            .block_on(self.fake.pin_login("u-budi", "9999"))
            .unwrap()
            .token
    }
}
