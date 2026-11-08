//! Drives the real register window headlessly (`i-slint-backend-testing`, as in
//! `sultan-spike/tests/shortcuts.rs`) against [`FakeService`] (plan Phase 8): keys are dispatched
//! as window events, clicks go to elements found by their accessible label, and the checks read
//! the window's properties and the calls the fake recorded.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_client::fake::{FakeService, LOCKOUT_MS};
use sultan_client::{ClientError, Problem, ServerEvent};
use sultan_ui::controller::Msg;
use sultan_ui::{App, Note, Options, Runner, build_app};

const T0: i64 = 1_800_000_000_000;

struct Till {
    app: App,
    fake: Arc<FakeService>,
    now: Arc<AtomicI64>,
    _runtime: tokio::runtime::Runtime,
}

fn till_with(fake: FakeService, language: Option<&str>) -> Till {
    // One testing platform per test thread.
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
    let fake = Arc::new(fake);
    fake.set_now(T0);
    let now = Arc::new(AtomicI64::new(T0));
    let clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let options = Options {
        register_label: "R1 · Kasir 1".to_owned(),
        language: language.map(str::to_owned),
        clock,
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
        now,
        _runtime: runtime,
    }
}

fn till() -> Till {
    till_with(FakeService::default(), Some("en"))
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
        "Control" => Key::Control.into(),
        "F2" => Key::F2.into(),
        "F5" => Key::F5.into(),
        "F10" => Key::F10.into(),
        "F12" => Key::F12.into(),
        other => other.into(),
    }
}

impl Till {
    /// Presses a combination: all parts down in order, released in reverse.
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
        for name in names {
            self.press(&[name]);
        }
    }

    fn type_digits(&self, digits: &str) {
        for c in digits.chars() {
            self.press(&[&c.to_string()]);
        }
    }

    /// Clicks the (only visible) element with this accessible label.
    #[track_caller]
    fn click(&self, label: &str) {
        // Texts carry their text as accessible label too; only buttons are clicked.
        let found: Vec<_> = ElementHandle::find_by_accessible_label(&self.app.ui, label)
            .filter(|e| e.accessible_role() == Some(AccessibleRole::Button))
            .collect();
        assert_eq!(found.len(), 1, "exactly one button labelled {label:?}");
        found[0].mock_single_click(PointerEventButton::Left);
    }

    fn screen(&self) -> String {
        self.app.ui.get_screen().into()
    }

    fn text(&self, note: Note) -> String {
        self.app.ui.invoke_message(note).into()
    }

    fn chips(&self) -> Vec<(String, String)> {
        self.app
            .ui
            .get_chips()
            .iter()
            .map(|c| (c.keys.into(), c.label.into()))
            .collect()
    }

    fn advance(&self, ms: i64) {
        let now = self.now.fetch_add(ms, Ordering::SeqCst) + ms;
        self.fake.set_now(now);
        self.app.tick();
    }

    /// Picks `user` (by tile index) and types the PIN with the keyboard.
    fn login(&self, tile: usize, pin: &str) {
        for _ in 0..tile {
            self.press(&["Right"]);
        }
        self.type_digits(pin);
        self.press(&["Enter"]);
    }

    /// Ani logs in and opens a session with Rp 500.000.
    fn ani_at_home(&self) {
        self.login(0, "1234");
        self.type_digits("500000");
        self.press(&["Enter"]);
        assert_eq!(self.screen(), "home");
    }
}

#[test]
fn lock_screen_and_pin_pad_by_keyboard() {
    let t = till();
    let ui = &t.app.ui;
    assert!(ui.get_has_focus());
    assert_eq!(t.screen(), "lock");
    assert_eq!(ui.get_register_label(), "R1 · Kasir 1");
    assert_eq!(ui.get_branch_label(), "", "unknown until the first login");
    assert_eq!(ui.get_clock().len(), 16);
    let names: Vec<String> = ui.get_users().iter().map(|u| u.name.into()).collect();
    assert_eq!(names, ["Ani", "Budi", "Citra", "Dedi"]);
    assert_eq!(ui.get_users().row_data(1).unwrap().initials, "B");
    assert_eq!(
        t.chips(),
        [
            ("Enter".into(), "enter_pin".into()),
            ("F5".into(), "refresh".into())
        ]
    );

    // Arrows move between tiles; Enter opens the pad.
    t.keys(&["Right", "Right", "Left", "Down"]);
    assert_eq!(ui.get_lock().cursor, 1, "one row only: Down stays");
    t.press(&["Left"]);
    t.press(&["Enter"]);
    let lock = ui.get_lock();
    assert!(lock.pad_open);
    assert_eq!(
        (lock.pad_name.as_str(), lock.pad_initials.as_str()),
        ("Ani", "A")
    );
    assert_eq!(
        t.chips(),
        [
            ("Enter".into(), "log_in".into()),
            ("Esc".into(), "back".into())
        ]
    );

    // The PIN is only ever shown as a length.
    t.type_digits("125");
    t.press(&["Backspace"]);
    t.type_digits("34");
    assert_eq!(ui.get_lock().pin_length, 4);
    t.type_digits("1234567");
    assert_eq!(ui.get_lock().pin_length, 6, "at most 6 digits");
    t.keys(&["Backspace", "Backspace"]);
    // Ctrl+digit is not a digit.
    t.press(&["Control", "5"]);
    assert_eq!(ui.get_lock().pin_length, 4);
    // Esc closes the pad and forgets the PIN.
    t.press(&["Esc"]);
    assert!(!ui.get_lock().pad_open);

    // A digit on the lock screen opens the pad of the highlighted user directly.
    t.type_digits("1234");
    t.press(&["Enter"]);
    assert_eq!(t.screen(), "open");
    assert_eq!(ui.get_user_name(), "Ani");
    assert_eq!(ui.get_branch_label(), "JKT01 · Toko Jakarta");
    assert!(t.fake.watching(), "events are watched after login");
    assert_eq!(t.fake.calls_of("pin_login"), ["u-ani"]);
}

#[test]
fn pin_pad_by_click() {
    let t = till();
    t.click("Budi");
    let lock = t.app.ui.get_lock();
    assert!(lock.pad_open);
    assert_eq!(lock.pad_name, "Budi");
    // Too short.
    t.click("9");
    t.click("OK");
    assert_eq!(t.app.ui.get_lock().note.code, "pin_too_short");
    t.click("⌫");
    assert_eq!(t.app.ui.get_lock().pin_length, 0);
    for _ in 0..4 {
        t.click("9");
    }
    assert_eq!(t.app.ui.get_lock().pin_length, 4);
    assert_eq!(t.app.ui.get_lock().note.code, "", "typing hides the hint");
    t.click("OK");
    assert_eq!(t.screen(), "open");
    assert_eq!(t.app.ui.get_user_name(), "Budi");
}

#[test]
fn wrong_pin_then_lockout_countdown() {
    let t = till();
    let ui = &t.app.ui;
    t.login(0, "0000");
    let note = ui.get_lock().note;
    assert_eq!((note.code.as_str(), note.tone), ("wrong_pin", -1));
    assert_eq!(t.text(note), "Wrong PIN. Try again.");
    assert_eq!(ui.get_lock().pin_length, 0, "a wrong PIN is cleared");
    // Typing hides "wrong PIN".
    t.type_digits("0");
    assert_eq!(ui.get_lock().note.code, "");
    t.press(&["Backspace"]);

    for _ in 0..4 {
        t.type_digits("0000");
        t.press(&["Enter"]);
    }
    let note = ui.get_lock().note;
    assert_eq!(
        (note.code.as_str(), note.arg.as_str()),
        ("locked_out", "5:00")
    );
    assert_eq!(t.text(note), "Too many wrong PINs. Try again in 5:00.");
    // The countdown runs with the clock, and typing does not hide it.
    t.advance(61_000);
    t.type_digits("1");
    assert_eq!(ui.get_lock().note.arg, "3:59");
    // Even the right PIN is refused while locked.
    t.type_digits("234");
    t.press(&["Enter"]);
    assert_eq!(t.screen(), "lock");
    assert_eq!(ui.get_lock().note.code, "locked_out");
    // Over: the message disappears, and the right PIN works.
    t.advance(LOCKOUT_MS);
    assert_eq!(ui.get_lock().note.code, "");
    t.type_digits("1234");
    t.press(&["Enter"]);
    assert_eq!(t.screen(), "open");
}

#[test]
fn indonesian_texts() {
    let t = till_with(FakeService::default(), Some("id"));
    t.login(0, "0000");
    assert_eq!(t.text(t.app.ui.get_lock().note), "PIN salah. Coba lagi.");
    assert_eq!(t.app.ui.invoke_chip_label("lock".into()), "Kunci");
    let note = Note {
        code: "locked_out".into(),
        arg: "4:59".into(),
        tone: -1,
    };
    assert_eq!(
        t.text(note),
        "Terlalu banyak PIN salah. Coba lagi dalam 4:59."
    );
    // An unknown language falls back to English.
    let t = till_with(FakeService::default(), Some("xx"));
    assert_eq!(t.app.ui.invoke_chip_label("lock".into()), "Lock");
}

#[test]
fn every_string_is_translated() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/ui");
    let po = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/translations/id/LC_MESSAGES/sultan-ui.po"
    ))
    .unwrap();
    let mut missing = Vec::new();
    let mut count = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let source = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        for part in source.split("@tr(\"").skip(1) {
            let msgid = &part[..part.find('"').unwrap()];
            count += 1;
            if !po.contains(&format!("msgid \"{msgid}\"\nmsgstr \"")) {
                missing.push(msgid.to_owned());
            }
        }
    }
    assert!(count > 50, "{count}");
    assert!(missing.is_empty(), "not in the .po file: {missing:#?}");
}

#[test]
fn open_session_amount_entry() {
    let t = till();
    let ui = &t.app.ui;
    t.login(0, "1234");
    assert_eq!(t.screen(), "open");
    assert_eq!(ui.get_open().amount, "Rp 0");
    assert_eq!(
        t.chips(),
        [
            ("Enter".into(), "open_session".into()),
            ("Esc".into(), "lock".into())
        ]
    );
    t.type_digits("0500000");
    assert_eq!(ui.get_open().amount, "Rp 500.000", "no leading zero");
    t.press(&["Backspace"]);
    assert_eq!(ui.get_open().amount, "Rp 50.000");
    t.click("0");
    assert_eq!(ui.get_open().amount, "Rp 500.000");

    // A lost answer is retried with the same key; a refused one gets a new key.
    t.fake.fail_next(ClientError::Network("down".into()));
    t.press(&["Enter"]);
    assert_eq!(ui.get_open().note.code, "network");
    t.fake.fail_next(
        Problem::new(422, "invalid_field", "x")
            .with("field", "opening_cash")
            .into(),
    );
    t.press(&["Enter"]);
    assert_eq!(ui.get_open().note.code, "invalid_amount");
    t.type_digits("0");
    assert_eq!(ui.get_open().note.code, "", "typing hides the message");
    t.press(&["Backspace"]);
    t.press(&["Enter"]);
    let keys: Vec<String> = t
        .fake
        .calls_of("open_session")
        .iter()
        .map(|c| c.split("key=").nth(1).unwrap().to_owned())
        .collect();
    assert_eq!(keys.len(), 3);
    assert_eq!(
        keys[0], keys[1],
        "retry after a network error keeps the key"
    );
    assert_ne!(keys[1], keys[2], "a refused request gets a new key");

    assert_eq!(t.screen(), "home");
    let home = ui.get_home();
    assert_eq!(
        home.number, "JKT01-R1-S-270115-01",
        "Phase 9: the session number"
    );
    assert_eq!(home.opening, "Rp 500.000");
    assert_eq!(home.since.len(), 5);
    assert_eq!(home.note.code, "session_opened");
    assert_eq!(t.fake.session().unwrap().opening_cash, 500_000);
}

#[test]
fn open_conflicts_are_handled() {
    let t = till();
    let ui = &t.app.ui;
    t.login(0, "1234");
    // Ani has a session on another register.
    t.fake.fail_next(
        Problem::new(409, "session_already_open", "x")
            .with("conflict", "user")
            .into(),
    );
    t.press(&["Enter"]);
    assert_eq!(t.screen(), "open");
    assert_eq!(ui.get_open().note.code, "already_open_user");
    // Budi opened here meanwhile: 409, then the screen shows whose session it is.
    t.fake.open_for("u-budi", 100_000);
    t.press(&["Enter"]);
    let open = ui.get_open();
    assert!(open.blocked);
    assert_eq!(open.blocked_name, "Budi");
    assert_eq!(open.note.code, "already_open_register");
}

#[test]
fn another_users_session_blocks_this_register() {
    let fake = FakeService::default();
    fake.open_for("u-budi", 100_000);
    let t = till_with(fake, None);
    let ui = &t.app.ui;
    t.login(0, "1234");
    assert_eq!(t.screen(), "open");
    let open = ui.get_open();
    assert!(open.blocked);
    assert_eq!(open.blocked_name, "Budi");
    assert_eq!(open.blocked_since.len(), 5);
    assert_eq!(open.blocked_number, "JKT01-R1-S-270115-01");
    assert_eq!(t.chips(), [("Esc".into(), "back_to_lock".into())]);
    // Nothing to type, nothing to open.
    t.type_digits("5");
    t.press(&["Enter"]);
    assert!(t.fake.calls_of("open_session").is_empty());
    t.click("Back to lock");
    assert_eq!(t.screen(), "lock");
    assert_eq!(ui.get_user_name(), "");
    assert_eq!(t.fake.calls_of("logout").len(), 1);

    // Budi himself goes straight to his session.
    t.login(1, "9999");
    assert_eq!(t.screen(), "home");
    assert_eq!(ui.get_home().opening, "Rp 100.000");
}

#[test]
fn blind_close_shows_expected_cash_only_in_the_report() {
    let t = till();
    let ui = &t.app.ui;
    t.fake.set_cash_sales(250_000);
    t.ani_at_home();
    t.press(&["F10"]);
    assert_eq!(t.screen(), "close");
    let rows: Vec<String> = ui.get_denoms().iter().map(|r| r.label.into()).collect();
    assert_eq!(rows.first().unwrap(), "Rp 100.000");
    assert_eq!(rows.last().unwrap(), "Rp 100");
    assert!(
        ui.get_denoms().row_data(6).unwrap().coin,
        "Rp 1.000 is a coin too"
    );

    // Type a total, then count notes instead: 7 × 100.000 + 2 × 20.000 = 740.000.
    t.type_digits("9");
    assert_eq!(ui.get_closing().total, "Rp 9");
    t.press(&["Down"]);
    t.type_digits("7");
    t.click("Rp 20.000");
    t.type_digits("2");
    let closing = ui.get_closing();
    assert_eq!(
        (closing.total.as_str(), closing.total_locked),
        ("Rp 740.000", true)
    );
    assert_eq!(ui.get_denoms().row_data(2).unwrap().subtotal, "Rp 40.000");
    t.press(&["Up", "Up", "Up"]);
    assert_eq!(ui.get_closing().cursor, 0);
    t.type_digits("5");
    assert_eq!(
        ui.get_closing().total,
        "Rp 740.000",
        "the total follows the notes"
    );

    // Blind: expected cash (500.000 + 250.000) appears nowhere before the close.
    let expected = "750.000";
    let visible = [
        format!("{:?}", ui.get_closing()),
        format!("{:?}", ui.get_home()),
        format!("{:?}", ui.get_denoms().iter().collect::<Vec<_>>()),
    ];
    assert!(visible.iter().all(|v| !v.contains(expected)), "{visible:?}");
    assert_eq!(ui.get_report_lines().row_count(), 0);

    // Enter asks first; Esc goes back to the count.
    t.press(&["Enter"]);
    assert!(ui.get_closing().confirm);
    assert_eq!(t.chips()[0], ("Enter".into(), "confirm_close".into()));
    t.type_digits("1");
    assert_eq!(
        ui.get_closing().total,
        "Rp 740.000",
        "no typing while confirming"
    );
    t.press(&["Esc"]);
    assert!(!ui.get_closing().confirm);
    // A lost answer keeps the screen and the key.
    t.fake.fail_next(ClientError::Network("down".into()));
    t.keys(&["Enter", "Enter"]);
    assert_eq!(t.screen(), "close");
    assert_eq!(ui.get_closing().note.code, "network");
    t.keys(&["Enter", "Enter"]);
    let calls = t.fake.calls_of("close_session");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1], "same counted cash and key");
    assert!(calls[1].contains("counted=740000"), "{calls:?}");

    assert_eq!(t.screen(), "report");
    let report = ui.get_report();
    assert_eq!(report.number, "JKT01-R1-S-270115-01");
    assert_eq!(
        (report.over_short.as_str(), report.tone),
        ("-Rp 10.000", -1)
    );
    assert!(report.has_tenders && !report.forced);
    let lines: Vec<(String, String)> = ui
        .get_report_lines()
        .iter()
        .map(|l| (l.label.into(), l.value.into()))
        .collect();
    assert!(
        lines.contains(&("expected_cash".into(), "Rp 750.000".into())),
        "{lines:?}"
    );
    assert!(
        lines.contains(&("counted_cash".into(), "Rp 740.000".into())),
        "{lines:?}"
    );
    let tender = ui.get_report_tenders().row_data(0).unwrap();
    assert_eq!(
        (tender.label.as_str(), tender.value.as_str()),
        ("cash", "Rp 250.000 · 3×")
    );

    // Done → logged out to the lock screen.
    t.press(&["Enter"]);
    assert_eq!(t.screen(), "lock");
    assert_eq!(t.fake.calls_of("logout").len(), 1);
    assert_eq!(ui.get_report_lines().row_count(), 0);
}

#[test]
fn close_from_an_empty_count_and_back() {
    let t = till();
    t.ani_at_home();
    t.click("Close session");
    assert_eq!(t.screen(), "close");
    t.click("Counted cash");
    assert_eq!(t.app.ui.get_closing().cursor, 0);
    t.press(&["Esc"]);
    assert_eq!(t.screen(), "home");
    // A manager force-closed it meanwhile.
    t.press(&["F10"]);
    t.fake
        .fail_next(Problem::new(409, "session_closed", "x").into());
    t.keys(&["Enter", "Enter"]);
    assert_eq!(t.screen(), "open");
    assert_eq!(t.app.ui.get_open().note.code, "session_closed");
}

#[test]
fn override_dialog_approves_and_retries_with_the_token() {
    let t = till();
    let ui = &t.app.ui;
    t.ani_at_home();
    assert_eq!(
        t.chips(),
        [
            ("F2".into(), "test_override".into()),
            ("F10".into(), "close_session".into()),
            ("F12".into(), "lock".into())
        ]
    );
    t.press(&["F2"]);
    let dialog = ui.get_approval();
    assert!(dialog.open);
    assert_eq!(
        (
            dialog.action.as_str(),
            dialog.value.as_str(),
            dialog.limit.as_str()
        ),
        ("sale.discount", "15%", "10%")
    );
    let managers: Vec<String> = ui.get_managers().iter().map(|m| m.name.into()).collect();
    assert_eq!(managers, ["Budi", "Citra", "Dedi"], "never the requester");

    // Page shortcuts do not leak into the dialog.
    t.press(&["F10"]);
    t.press(&["F12"]);
    assert_eq!(t.screen(), "home");
    assert!(ui.get_approval().open);

    t.press(&["Enter"]);
    assert_eq!(ui.get_approval().note.code, "pin_too_short");
    // Citra's limit (12%) is too low.
    t.press(&["Down"]);
    t.type_digits("5555");
    t.press(&["Enter"]);
    let note = ui.get_approval().note;
    assert_eq!(
        (note.code.as_str(), note.arg.as_str()),
        ("override_limit_exceeded", "12%")
    );
    assert_eq!(t.text(note), "This manager's limit (12%) is too low.");
    assert_eq!(ui.get_approval().pin_length, 0);
    // Self-approval (the server's answer, shown inline).
    t.fake
        .fail_next(Problem::new(403, "self_approval", "x").into());
    t.type_digits("5555");
    t.press(&["Enter"]);
    assert_eq!(ui.get_approval().note.code, "self_approval");
    // Budi, wrong PIN, then right.
    t.click("Budi");
    assert_eq!(ui.get_approval().cursor, 0);
    t.type_digits("0000");
    t.press(&["Enter"]);
    assert_eq!(ui.get_approval().note.code, "wrong_pin");
    t.type_digits("99999");
    t.press(&["Backspace"]);
    t.click("Approve");

    assert!(!ui.get_approval().open);
    let protected = t.fake.calls_of("demo_protected");
    assert_eq!(protected.len(), 2);
    assert_eq!(protected[0], "override=-");
    assert!(
        protected[1].starts_with("override=override-"),
        "retried with the token: {protected:?}"
    );
    let note = ui.get_home().note;
    assert_eq!(
        (note.code.as_str(), note.arg.as_str(), note.tone),
        ("override_ok", "Budi", 1)
    );
    assert_eq!(
        t.text(note),
        "Approved by Budi. The action was carried out."
    );
}

#[test]
fn override_dialog_lockout_and_cancel() {
    let t = till();
    let ui = &t.app.ui;
    t.ani_at_home();
    t.press(&["F2"]);
    // Dedi (last; Up wraps) types a wrong PIN five times.
    t.press(&["Up"]);
    assert_eq!(ui.get_approval().cursor, 2);
    for _ in 0..5 {
        t.type_digits("0000");
        t.press(&["Enter"]);
    }
    let note = ui.get_approval().note;
    assert_eq!(
        (note.code.as_str(), note.arg.as_str()),
        ("locked_out", "5:00")
    );
    t.advance(30_000);
    assert_eq!(ui.get_approval().note.arg, "4:30");
    // Esc cancels: back home, nothing approved.
    t.press(&["Esc"]);
    assert!(!ui.get_approval().open);
    assert_eq!(ui.get_home().note.code, "override_cancelled");
    assert_eq!(t.fake.calls_of("demo_protected").len(), 1);
    // The Cancel button does the same.
    t.press(&["F2"]);
    t.click("Cancel");
    assert!(!ui.get_approval().open);
}

#[test]
fn override_not_needed_within_own_limit() {
    let t = till_with(
        FakeService::new(vec![
            sultan_client::fake::FakeUser::new("u-owner", "Owner", "1234", 10_000),
            sultan_client::fake::FakeUser::new("u-budi", "Budi", "9999", 2_000),
        ]),
        Some("en"),
    );
    t.ani_at_home();
    t.press(&["F2"]);
    assert!(!t.app.ui.get_approval().open);
    assert_eq!(t.app.ui.get_home().note.code, "override_not_needed");
    // Any other error of the action is shown on the page.
    t.fake
        .fail_next(Problem::new(403, "permission_denied", "x").into());
    t.press(&["F2"]);
    assert_eq!(t.app.ui.get_home().note.code, "permission_denied");
}

#[test]
fn failed_retry_with_token_stays_in_the_dialog() {
    let t = till();
    t.ani_at_home();
    t.press(&["F2"]);
    t.type_digits("9999");
    // `POST /overrides` succeeds; the retry with the token is refused (e.g. it expired).
    t.fake.fail_on(
        "demo_protected",
        Problem::new(403, "invalid_override", "x").into(),
    );
    t.press(&["Enter"]);
    assert_eq!(t.fake.calls_of("request_override").len(), 1);
    assert!(t.app.ui.get_approval().open);
    assert_eq!(t.app.ui.get_approval().note.code, "invalid_override");
    // Approve again: works.
    t.type_digits("9999");
    t.press(&["Enter"]);
    assert!(!t.app.ui.get_approval().open);
    assert_eq!(t.app.ui.get_home().note.code, "override_ok");
}

#[test]
fn logout_returns_to_lock_and_keeps_the_session() {
    let t = till();
    let ui = &t.app.ui;
    t.ani_at_home();
    t.press(&["F12"]);
    assert_eq!(t.screen(), "lock");
    assert_eq!(ui.get_user_name(), "");
    assert_eq!(t.fake.calls_of("logout").len(), 1);
    assert!(
        t.fake.session().is_some(),
        "locking keeps the cashier session open"
    );
    assert_eq!(
        ui.get_branch_label(),
        "JKT01 · Toko Jakarta",
        "kept for the next login"
    );
    // The logout's own `session.revoked` (sent by the fake, like the server) changes nothing.
    t.app.pump();
    assert_eq!(ui.get_lock().note.code, "");
    // Back in: straight to the open session.
    t.login(0, "1234");
    assert_eq!(t.screen(), "home");
    assert_eq!(
        t.fake.calls_of("branch").len(),
        1,
        "the branch is loaded once"
    );
}

#[test]
fn revoked_session_event_returns_to_lock() {
    let t = till();
    t.ani_at_home();
    let revoked = ServerEvent {
        kind: "session.revoked".into(),
        data: serde_json::json!({"user_id": "u-ani", "reason": "user_deactivated", "this_session": true}),
    };
    // An event of an earlier login is ignored.
    t.app.dispatch(Msg::Event(0, revoked.clone()));
    assert_eq!(t.screen(), "home");
    // Other events change nothing.
    t.fake.emit(ServerEvent {
        kind: "cashier_session.opened".into(),
        data: serde_json::Value::Null,
    });
    t.app.pump();
    assert_eq!(t.screen(), "home");
    t.fake.emit(revoked);
    t.app.pump();
    assert_eq!(t.screen(), "lock");
    assert_eq!(t.app.ui.get_lock().note.code, "session_ended");
    assert!(
        t.fake.calls_of("logout").is_empty(),
        "already revoked by the server"
    );
}

#[test]
fn errors_on_the_way() {
    let t = till();
    let ui = &t.app.ui;
    // Loading the users fails; F5 retries.
    t.fake.fail_next(ClientError::Network("down".into()));
    t.press(&["F5"]);
    assert_eq!(ui.get_lock().note.code, "network");
    t.click("Refresh");
    assert_eq!(ui.get_lock().note.code, "", "loaded now");
    assert_eq!(ui.get_users().row_count(), 4);

    // The session ends while working: back to lock with a message.
    t.ani_at_home();
    t.fake
        .fail_next(Problem::new(401, "invalid_session", "x").into());
    t.press(&["F2"]);
    assert_eq!(t.screen(), "lock");
    assert_eq!(ui.get_lock().note.code, "session_ended");
    assert!(t.fake.calls_of("logout").is_empty());

    // Loading the current session fails right after login: logged out again, with the reason.
    t.fake
        .fail_on("current_session", ClientError::Network("x".into()));
    t.login(0, "1234");
    assert_eq!(t.screen(), "lock");
    assert!(!ui.get_lock().pad_open);
    assert_eq!(ui.get_lock().note.code, "network");
    assert_eq!(t.fake.calls_of("logout").len(), 1);
    // A device that was unpaired.
    t.type_digits("1234");
    t.press(&["Enter"]);
    assert_eq!(t.screen(), "home");
    t.fake
        .fail_next(Problem::new(401, "invalid_device_token", "x").into());
    t.press(&["F10"]);
    t.keys(&["Enter", "Enter"]);
    assert_eq!(t.screen(), "lock");
    let note = ui.get_lock().note;
    assert_eq!(note.code, "device_unpaired");
    assert!(t.text(note).starts_with("This register is not paired"));
}

#[test]
fn unknown_clicks_are_ignored() {
    let t = till();
    t.app.ui.invoke_action("nope".into());
    t.app.ui.invoke_pick("nothing".into(), 0);
    t.app.ui.invoke_pick("user".into(), -1);
    t.app.ui.invoke_pump();
    assert_eq!(t.screen(), "lock");
    assert!(!t.app.ui.get_lock().pad_open);
    assert!(
        !t.app.ui.invoke_key("a".into(), false, false),
        "unused keys are passed on"
    );
}
