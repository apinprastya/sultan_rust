//! Cash management at the register (spec 2.7, phase K4) driven headlessly against
//! [`FakeService`]: spec 2.7 §12 register cases 35–38 (a cash out above the cashier's limit with
//! its slip, the X report with a manager PIN, the float on close and open, the Z from the close
//! report), plus cash in with a required note, a safe drop, reversals, a slip reprint, the float
//! validation, the Z refusals and the payout slip.

#![allow(clippy::too_many_lines)] // the flows read top to bottom

use std::sync::Arc;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_client::fake::FakeService;
use sultan_client::{ClientError, Problem};
use sultan_core::catalog::gtin_check_digit;
use sultan_ui::{App, CashView, Options, Runner, build_app};

const T0: i64 = 1_800_000_000_000;

fn teh() -> String {
    let body = "899100000010";
    format!("{body}{}", gtin_check_digit(body).unwrap())
}

struct Till {
    app: App,
    fake: Arc<FakeService>,
    _runtime: tokio::runtime::Runtime,
}

/// A register in English with nobody logged in.
fn register(language: &str) -> Till {
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
    let fake = FakeService::default();
    let _ = fake.seed_pricing_demo();
    let fake = Arc::new(fake);
    fake.set_now(T0);
    let options = Options {
        register_label: "R1 · Kasir 1".to_owned(),
        language: Some(language.to_owned()),
        clock: Arc::new(|| T0),
        start: false,
        cart_file: None,
    };
    let app = build_app(
        fake.clone(),
        Runner::Block(runtime.handle().clone()),
        options,
    )
    .unwrap();
    app.ui.window().set_size(LogicalSize::new(1366.0, 768.0));
    app.ui.show().unwrap();
    Till {
        app,
        fake,
        _runtime: runtime,
    }
}

/// Ani (cashier) at the sell screen with Rp 500.000 opening cash.
fn till() -> Till {
    let t = register("en");
    t.log_in(0, "1234", "500000");
    t
}

fn key_text(name: &str) -> SharedString {
    match name {
        "Enter" => Key::Return.into(),
        "Esc" => Key::Escape.into(),
        "Backspace" => Key::Backspace.into(),
        "Up" => Key::UpArrow.into(),
        "Down" => Key::DownArrow.into(),
        "Left" => Key::LeftArrow.into(),
        "Right" => Key::RightArrow.into(),
        "Tab" => Key::Tab.into(),
        "Shift" => Key::Shift.into(),
        "Control" => Key::Control.into(),
        "F1" => Key::F1.into(),
        "F3" => Key::F3.into(),
        "F5" => Key::F5.into(),
        "F7" => Key::F7.into(),
        "F10" => Key::F10.into(),
        "F12" => Key::F12.into(),
        other => other.into(),
    }
}

impl Till {
    fn press(&self, parts: &[&str]) {
        let window = self.app.ui.window();
        for p in parts {
            window.dispatch_event(WindowEvent::KeyPressed { text: key_text(p) });
        }
        for p in parts.iter().rev() {
            window.dispatch_event(WindowEvent::KeyReleased { text: key_text(p) });
        }
    }

    fn keys(&self, names: &[&str]) {
        for n in names {
            self.press(&[n]);
        }
    }

    fn typed(&self, text: &str) {
        for c in text.chars() {
            self.press(&[&c.to_string()]);
        }
    }

    /// Logs user `index` (0 Ani, 1 Budi, …) in and opens a session with `opening` typed (or
    /// goes straight home when one is open).
    fn log_in(&self, index: usize, pin: &str, opening: &str) {
        for _ in 0..index {
            self.press(&["Right"]);
        }
        self.typed(pin);
        self.press(&["Enter"]);
        if self.screen() == "open" {
            self.typed(opening);
            self.press(&["Enter"]);
        }
        assert_eq!(self.screen(), "home");
    }

    #[track_caller]
    fn click(&self, label: &str) {
        let found: Vec<_> = ElementHandle::find_by_accessible_label(&self.app.ui, label)
            .filter(|e| e.accessible_role() == Some(AccessibleRole::Button))
            .collect();
        assert_eq!(found.len(), 1, "exactly one button labelled {label:?}");
        found[0].mock_single_click(PointerEventButton::Left);
    }

    fn screen(&self) -> String {
        self.app.ui.get_screen().into()
    }

    fn cash(&self) -> CashView {
        self.app.ui.get_cash()
    }

    fn note(&self) -> String {
        self.app.ui.get_home().note.code.into()
    }

    fn message(&self) -> String {
        self.app
            .ui
            .invoke_message(self.app.ui.get_home().note)
            .into()
    }

    fn chips(&self) -> Vec<String> {
        self.app
            .ui
            .get_chips()
            .iter()
            .map(|c| format!("{} {}", c.keys, c.label))
            .collect()
    }

    fn buttons(&self) -> Vec<String> {
        let ui = &self.app.ui;
        ui.get_actions()
            .iter()
            .chain(ui.get_actions_b().iter())
            .map(|c| format!("{} {}", c.keys, c.label))
            .collect()
    }

    fn text(&self) -> String {
        self.app
            .ui
            .get_text_lines()
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn reasons(&self) -> Vec<String> {
        self.app
            .ui
            .get_cash_reasons()
            .iter()
            .map(|r| r.label.to_string())
            .collect()
    }

    fn movements(&self) -> Vec<(String, String, String)> {
        self.app
            .ui
            .get_cash_rows()
            .iter()
            .map(|r| (r.number.into(), r.kind.into(), r.detail.into()))
            .collect()
    }

    /// Budi (manager, PIN 9999, first of the approvers) approves the open override.
    fn budi_approves(&self) {
        assert!(
            self.app.ui.get_approval().open,
            "the override dialog is open"
        );
        self.typed("9999");
        self.press(&["Enter"]);
    }

    /// Blind close with `counted` typed and the float as proposed: the close report.
    fn close(&self, counted: &str) {
        self.press(&["Control", "F10"]);
        assert_eq!(self.screen(), "close");
        self.typed(counted);
        self.keys(&["Enter", "Enter"]);
        assert_eq!(
            self.screen(),
            "report",
            "{:?}",
            self.app.ui.get_closing().note
        );
    }
}

/// Spec 2.7 §12 case 35, cash in with a required note, and a safe drop.
#[test]
fn cash_out_above_the_limit_needs_a_manager_and_shows_the_slip() {
    let t = till();
    t.typed(&teh());
    t.press(&["Enter"]);
    assert_eq!(t.app.ui.get_sell().items, "1");
    assert!(t.buttons().contains(&"Shift+F5 cash".to_owned()));
    assert!(t.buttons().contains(&"Shift+F12 x_report".to_owned()));
    // Shift+F5 with a cart: the dialog opens on cash in, with this session's (empty) list.
    t.press(&["Shift", "F5"]);
    let c = t.cash();
    assert!(c.open);
    assert_eq!(
        (c.mode.as_str(), c.kind.as_str(), c.step.as_str()),
        ("cash", "in", "form")
    );
    assert_eq!(t.reasons(), ["Tambahan modal / uang kecil", "Lainnya"]);
    assert!(t.movements().is_empty());
    assert_eq!(
        t.chips(),
        [
            "Enter ok",
            "I cash_in",
            "O cash_out",
            "D cash_drop",
            "Tab to_note",
            "Esc close"
        ]
    );
    // O (either case), ↓ to the parking reason, 75 000.
    t.typed("O");
    assert_eq!(t.cash().kind, "out");
    assert_eq!(
        t.reasons(),
        [
            "Belanja keperluan toko",
            "Ongkos kirim / parkir",
            "Kebersihan / keamanan",
            "Lainnya"
        ]
    );
    t.press(&["Down"]);
    t.typed("75000");
    assert_eq!(t.cash().amount, "Rp 75.000");
    t.press(&["Enter"]);
    let c = t.cash();
    assert_eq!(
        (c.step.as_str(), c.subject.as_str()),
        ("confirm", "Ongkos kirim / parkir")
    );
    assert_eq!(t.chips(), ["Enter post_cash", "Esc edit_amount"]);
    // Above Ani's Rp 50.000: Budi approves the movement for its amount.
    t.press(&["Enter"]);
    let a = t.app.ui.get_approval();
    assert_eq!(
        (a.action.as_str(), a.value.as_str(), a.limit.as_str()),
        ("cash.out", "Rp 75.000", "Rp 50.000")
    );
    t.budi_approves();
    assert!(!t.app.ui.get_approval().open);
    let c = t.cash();
    assert_eq!(c.step, "slip");
    assert!(c.subject.starts_with("JKT01-R1-K-"), "{}", c.subject);
    assert_eq!(t.note(), "cash_posted");
    for part in ["CASH OUT", "Ongkos kirim / parkir", "75.000", "Budi"] {
        assert!(t.text().contains(part), "{part}: {}", t.text());
    }
    let posted = t.fake.cash_movements_posted();
    assert_eq!(posted.len(), 1);
    assert_eq!(
        (
            posted[0].kind.as_str(),
            posted[0].amount,
            posted[0].approved_by.as_deref()
        ),
        ("out", 75_000, Some("u-budi"))
    );
    assert_eq!(t.movements().len(), 1);
    assert_eq!(t.movements()[0].1, "out");
    assert_eq!(t.movements()[0].2, "Ongkos kirim / parkir · Rp 75.000");
    // Enter: a fresh form, the kind kept.
    t.press(&["Enter"]);
    let c = t.cash();
    assert_eq!(
        (c.step.as_str(), c.kind.as_str(), c.amount.as_str()),
        ("form", "out", "")
    );

    // Cash in "Lainnya" needs a note (clicked: the kind tab and the reason).
    t.app
        .dispatch(sultan_ui::controller::Msg::PickSell("kind", 0));
    assert_eq!(t.cash().kind, "in");
    t.app
        .dispatch(sultan_ui::controller::Msg::PickSell("reason", 1));
    t.typed("20000");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "note_required");
    assert_eq!(t.message(), "Type a note: this needs one.");
    t.press(&["Tab"]);
    assert_eq!(t.cash().focus, "note");
    assert_eq!(t.chips(), ["Enter ok", "Tab to_list", "Esc close"]);
    t.typed("Modal dari bos");
    t.keys(&["Enter", "Enter"]);
    assert_eq!(t.cash().step, "slip", "{}", t.note());
    assert!(t.text().contains("CASH IN"), "{}", t.text());
    assert_eq!(
        t.fake.cash_movements_posted()[1].note.as_deref(),
        Some("Modal dari bos")
    );
    t.press(&["Esc"]);

    // A safe drop by a cashier needs `cash.drop`; no reason; a refused approval keeps the form.
    t.press(&["Right", "Right"]);
    assert_eq!(t.cash().kind, "drop");
    assert!(t.reasons().is_empty());
    t.typed("300000");
    t.keys(&["Enter", "Enter"]);
    assert_eq!(t.app.ui.get_approval().action, "cash.drop");
    t.press(&["Esc"]);
    assert_eq!(t.note(), "override_cancelled");
    assert_eq!(t.cash().step, "confirm");
    t.press(&["Enter"]);
    t.budi_approves();
    assert!(t.text().contains("SAFE DROP"), "{}", t.text());
    assert_eq!(t.fake.cash_movements_posted().len(), 3);
    // Esc twice: back to the sell screen, the cart kept.
    t.keys(&["Esc", "Esc"]);
    assert!(!t.cash().open);
    assert_eq!(t.app.ui.get_sell().items, "1");
    // An unexpected answer is shown and the form stays.
    t.press(&["Shift", "F5"]);
    t.typed("1000");
    t.fake.fail_on(
        "post_cash_movement",
        Problem::new(409, "no_open_session", "x").into(),
    );
    t.keys(&["Enter", "Enter"]);
    assert_eq!(
        (t.cash().step.as_str(), t.note().as_str()),
        ("form", "no_open_session")
    );
}

/// Reversal (R) with its note, the refusals, and a slip reprint (P).
#[test]
fn reversal_and_slip_reprint() {
    let t = till();
    t.press(&["Shift", "F5"]);
    t.typed("o");
    t.typed("25000");
    t.keys(&["Enter", "Enter"]);
    assert_eq!(t.cash().step, "slip", "within Ani's limit: {}", t.note());
    let original = t.cash().subject.to_string();
    assert_eq!(t.chips(), ["Enter / Esc done", "P reprint"]);
    t.press(&["Esc"]);
    // Tab, Tab: the list; R asks for the note.
    t.keys(&["Tab", "Tab"]);
    assert_eq!(t.cash().focus, "list");
    assert_eq!(
        t.chips(),
        ["R reverse", "P reprint", "Tab to_form", "Esc close"]
    );
    t.typed("r");
    assert_eq!(t.cash().step, "reverse");
    assert_eq!(t.chips(), ["Enter reverse", "Esc back"]);
    // Esc goes back to the list; R again.
    t.press(&["Esc"]);
    assert_eq!(
        (t.cash().step.as_str(), t.cash().focus.as_str()),
        ("form", "list")
    );
    t.typed("r");
    assert!(t.cash().subject.starts_with(&original));
    t.press(&["Enter"]);
    assert_eq!(t.note(), "note_required");
    t.typed("salah ketik");
    t.press(&["Backspace"]);
    t.typed("k");
    assert_eq!(t.cash().reverse_note, "salah ketik");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "cash_reversed");
    assert!(t.text().contains("REVERSAL"), "{}", t.text());
    let posted = t.fake.cash_movements_posted();
    assert_eq!(posted[1].reverses.as_deref(), Some(posted[0].id.as_str()));
    assert_eq!(posted[1].note.as_deref(), Some("salah ketik"));
    let rows = t.movements();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].2.contains(&format!("↩ {original}")), "{rows:?}");
    // The reversal cannot be reversed, nor the original again.
    t.press(&["Esc"]);
    t.keys(&["Tab", "Tab"]);
    t.typed("R");
    assert_eq!(t.note(), "cannot_reverse");
    t.press(&["Down"]);
    t.typed("r");
    assert_eq!(t.note(), "already_reversed");
    assert_eq!(t.cash().step, "form");
    // P reprints the highlighted slip (COPY); a click picks a row.
    t.app
        .dispatch(sultan_ui::controller::Msg::PickSell("movement", 1));
    t.typed("p");
    assert_eq!(t.cash().step, "slip");
    assert_eq!(t.cash().subject, original);
    assert_eq!(t.note(), "reprinted");
    assert!(t.text().contains("COPY"), "{}", t.text());
    // P again from the slip.
    t.typed("p");
    assert_eq!(t.fake.calls_of("cash_slip").len(), 4);
    // Tab cycles amount → note → list → amount; Esc closes the dialog.
    t.press(&["Esc"]);
    t.keys(&["Tab", "Tab", "Tab"]);
    assert_eq!(
        (t.cash().step.as_str(), t.cash().focus.as_str()),
        ("form", "amount")
    );
    t.app
        .dispatch(sultan_ui::controller::Msg::PickSell("field", 1));
    assert_eq!(t.cash().focus, "note");
    t.press(&["Esc"]);
    assert!(!t.cash().open);
}

/// Spec 2.7 §12 case 36.
#[test]
fn x_report_needs_a_manager_and_keeps_the_cart() {
    let t = till();
    t.typed(&teh());
    t.press(&["Enter"]);
    t.press(&["Shift", "F12"]);
    let a = t.app.ui.get_approval();
    assert_eq!(a.action, "session.view_expected");
    // Esc cancels: nothing shown.
    t.press(&["Esc"]);
    assert!(!t.cash().open);
    assert_eq!(t.note(), "override_cancelled");
    t.press(&["Shift", "F12"]);
    t.budi_approves();
    let c = t.cash();
    assert_eq!((c.open, c.mode.as_str()), (true, "x"));
    assert!(t.text().contains("X REPORT"), "{}", t.text());
    assert!(t.text().contains("EXPECTED CASH"), "{}", t.text());
    assert_eq!(t.chips(), ["Esc close"]);
    t.press(&["Esc"]);
    assert!(!t.cash().open);
    assert_eq!(t.app.ui.get_sell().items, "1", "the cart is kept");
    // A closed session meanwhile: said.
    t.fake.fail_on(
        "x_report_text",
        Problem::new(409, "session_closed", "x").into(),
    );
    t.press(&["Shift", "F12"]);
    assert_eq!(t.note(), "session_closed");
}

/// Spec 2.7 §12 case 37 and the float validation.
#[test]
fn close_keeps_a_float_and_the_next_open_proposes_it() {
    let t = till();
    let ui = &t.app.ui;
    t.press(&["Control", "F10"]);
    t.typed("700000");
    t.press(&["Enter"]);
    let c = ui.get_closing();
    assert!(c.confirm);
    assert_eq!(
        (c.float.as_str(), c.to_safe.as_str()),
        ("Rp 500.000", "Rp 200.000"),
        "default: the opening cash"
    );
    // More than counted: refused.
    t.typed("800000");
    assert_eq!(ui.get_closing().float, "Rp 800.000");
    t.press(&["Enter"]);
    assert_eq!(t.screen(), "close");
    assert_eq!(ui.get_closing().note.code, "float_above_count");
    assert_eq!(ui.get_closing().note.arg, "Rp 700.000");
    for _ in 0..6 {
        t.press(&["Backspace"]);
    }
    t.typed("200000");
    assert_eq!(ui.get_closing().to_safe, "Rp 500.000");
    t.press(&["Enter"]);
    assert_eq!(t.screen(), "report");
    let lines: Vec<(String, String)> = ui
        .get_report_lines()
        .iter()
        .map(|l| (l.label.into(), l.value.into()))
        .collect();
    assert!(
        lines.contains(&("float_kept".into(), "Rp 200.000".into())),
        "{lines:?}"
    );
    assert!(
        lines.contains(&("to_safe".into(), "Rp 500.000".into())),
        "{lines:?}"
    );
    assert!(t.text().contains("CLOSE REPORT"), "{}", t.text());
    assert!(t.text().contains("Kept as float"), "{}", t.text());
    t.press(&["Enter"]);
    assert_eq!(t.screen(), "lock");
    // The next open proposes the float, which can be edited.
    t.typed("1234");
    t.press(&["Enter"]);
    assert_eq!(t.screen(), "open");
    let o = ui.get_open();
    assert_eq!(
        (o.amount.as_str(), o.proposed.as_str()),
        ("Rp 200.000", "Rp 200.000")
    );
    t.press(&["Backspace"]);
    assert_eq!(ui.get_open().amount, "Rp 20.000");
    t.typed("0");
    t.press(&["Enter"]);
    assert_eq!(t.screen(), "home");
    assert_eq!(ui.get_home().opening, "Rp 200.000");
}

/// Spec 2.7 §12 case 38: a cashier runs the Z with a manager's approval.
#[test]
fn close_report_runs_the_z() {
    let t = till();
    t.close("500000");
    let r = t.app.ui.get_report();
    assert_eq!(r.z, "offered", "a cashier may not list the days: offered");
    assert_eq!(r.day, "2027-01-15");
    assert_eq!(t.chips(), ["Enter done", "Z end_of_day"]);
    t.typed("Z");
    assert_eq!(t.app.ui.get_report().z, "confirm");
    assert_eq!(t.chips(), ["Enter run_z", "Esc back"]);
    t.press(&["Esc"]);
    assert_eq!(t.app.ui.get_report().z, "offered");
    t.click("End of day (Z)");
    t.press(&["Enter"]);
    let a = t.app.ui.get_approval();
    assert_eq!(a.action, "day.close");
    t.budi_approves();
    let r = t.app.ui.get_report();
    assert_eq!(
        (r.z.as_str(), r.z_number.as_str()),
        ("done", "JKT01-Z-270115-01")
    );
    assert_eq!(r.note.code, "z_done");
    assert!(t.text().contains("Z REPORT"), "{}", t.text());
    assert!(t.text().contains("JKT01-Z-270115-01"), "{}", t.text());
    assert_eq!(t.chips(), ["Enter done"]);
    t.press(&["Enter"]);
    assert_eq!(t.screen(), "lock");
}

/// Case 38's other half: not offered to a manager while another session of the day is open;
/// a cashier's run says which; `nothing_to_close` is said too.
#[test]
fn z_is_refused_while_the_day_has_open_sessions() {
    let t = register("en");
    let other = t.fake.open_elsewhere("u-dedi", 0);
    // Budi may list the days: Dedi's open session hides the Z.
    t.log_in(1, "9999", "100000");
    t.close("100000");
    assert_eq!(t.app.ui.get_report().z, "hidden");
    assert_eq!(t.chips(), ["Enter done"]);
    t.typed("z");
    assert_eq!(t.app.ui.get_report().z, "hidden");
    t.press(&["Enter"]);
    // Ani cannot list them: the run itself refuses, naming the session.
    t.log_in(0, "1234", "0");
    t.close("0");
    assert_eq!(t.app.ui.get_report().z, "offered");
    t.typed("z");
    t.press(&["Enter"]);
    t.budi_approves();
    let r = t.app.ui.get_report();
    assert_eq!(r.z, "hidden");
    assert_eq!(r.note.code, "sessions_open");
    assert_eq!(r.note.arg, other.number);
    let text: String = t.app.ui.invoke_message(r.note).into();
    assert!(text.contains(&other.number), "{text}");
    t.press(&["Enter"]);
    // Nothing left to close (another register ran it): said, the key offered again.
    t.log_in(0, "1234", "0");
    t.close("0");
    t.fake
        .fail_on("run_z", Problem::new(409, "nothing_to_close", "x").into());
    t.keys(&["z", "Enter"]);
    let r = t.app.ui.get_report();
    assert_eq!(
        (r.z.as_str(), r.note.code.as_str()),
        ("offered", "nothing_to_close")
    );
    // A lost answer keeps the key for the retry.
    t.fake.fail_on("run_z", ClientError::Network("down".into()));
    t.keys(&["z", "Enter"]);
    assert_eq!(t.app.ui.get_report().note.code, "network");
}

/// The payout slip (spec 2.7 K7) after a customer balance payout (F3 → F7).
#[test]
fn payout_shows_its_slip() {
    let t = register("en");
    t.log_in(1, "9999", "500000");
    t.press(&["F3"]);
    t.typed("K7QH-29XM");
    t.press(&["Enter"]);
    t.press(&["F7"]);
    t.typed("2200");
    t.keys(&["Enter", "Enter"]);
    let d = t.app.ui.get_customer();
    assert_eq!(d.page, "slip");
    assert_eq!(t.chips(), ["Enter done", "Esc back"]);
    for part in ["BALANCE PAYOUT", "2.200", "Budi"] {
        assert!(t.text().contains(part), "{part}: {}", t.text());
    }
    t.press(&["Enter"]);
    assert_eq!(t.app.ui.get_customer().page, "shown");
    assert_eq!(t.fake.calls_of("payout_slip").len(), 1);
}

/// The Indonesian texts of the dialog.
#[test]
fn indonesian_cash_texts() {
    let t = register("id");
    t.log_in(0, "1234", "500000");
    t.press(&["Shift", "F5"]);
    assert_eq!(
        t.app.ui.invoke_chip_label("cash_drop".into()),
        "Setor ke brankas"
    );
    t.press(&["Enter"]);
    assert_eq!(t.message(), "Jumlah tersebut tidak valid.");
}
