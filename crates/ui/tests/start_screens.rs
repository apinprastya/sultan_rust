//! Drives the real Start window headlessly (`i-slint-backend-testing`, as `setup_screens.rs` does
//! for the wizard) against a fake [`StartService`]: keys are window events, tiles and buttons
//! are found by their accessible label, and the checks read the window's properties, the
//! outcome and the calls the fake recorded.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_ui::start::{AppKind, Choice, StartError, StartFuture, StartInfo, StartService};
use sultan_ui::{Runner, Start, build_start};

/// Answers checks from a script (then "ok") and records every call.
#[derive(Default)]
struct FakeStart {
    checks: Mutex<VecDeque<Result<(), StartError>>>,
    calls: Mutex<Vec<String>>,
    read_only: bool,
}

impl FakeStart {
    fn answering(checks: Vec<Result<(), StartError>>) -> Self {
        Self {
            checks: Mutex::new(checks.into()),
            ..Self::default()
        }
    }

    fn then(&self, answer: Result<(), StartError>) {
        self.checks.lock().unwrap().push_back(answer);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl StartService for FakeStart {
    fn check(&self) -> StartFuture {
        self.calls.lock().unwrap().push("check".into());
        let answer = self.checks.lock().unwrap().pop_front().unwrap_or(Ok(()));
        Box::pin(async move { answer })
    }

    fn set_language(&self, language: &str) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("language {language}"));
        if self.read_only {
            Err("read-only file".into())
        } else {
            Ok(())
        }
    }
}

fn network() -> Result<(), StartError> {
    Err(StartError::new("network", "192.168.1.10:8080"))
}

fn terminal(language: &str, fixed: bool) -> StartInfo {
    StartInfo {
        kind: AppKind::Terminal,
        server: "http://192.168.1.10:8080".into(),
        register_code: "R2".into(),
        register_name: "Kasir 2".into(),
        branch: String::new(),
        version: "0.1.0".into(),
        language: language.into(),
        language_fixed: fixed,
    }
}

struct Screen {
    start: Start,
    fake: Arc<FakeStart>,
    _runtime: tokio::runtime::Runtime,
}

fn open(fake: FakeStart, info: StartInfo) -> Screen {
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
    let start = build_start(fake.clone(), Runner::Block(runtime.handle().clone()), info).unwrap();
    start.ui.window().set_size(LogicalSize::new(1366.0, 768.0));
    start.ui.show().unwrap();
    Screen {
        start,
        fake,
        _runtime: runtime,
    }
}

fn key_text(name: &str) -> SharedString {
    match name {
        "Enter" => Key::Return.into(),
        "Esc" => Key::Escape.into(),
        "F1" => Key::F1.into(),
        "F2" => Key::F2.into(),
        "F3" => Key::F3.into(),
        "F5" => Key::F5.into(),
        "F6" => Key::F6.into(),
        "F8" => Key::F8.into(),
        other => other.into(),
    }
}

impl Screen {
    fn press(&self, name: &str) {
        let window = self.start.ui.window();
        window.dispatch_event(WindowEvent::KeyPressed {
            text: key_text(name),
        });
        window.dispatch_event(WindowEvent::KeyReleased {
            text: key_text(name),
        });
    }

    #[track_caller]
    fn click(&self, label: &str) {
        let found: Vec<_> = ElementHandle::find_by_accessible_label(&self.start.ui, label)
            .filter(|e| e.accessible_role() == Some(AccessibleRole::Button))
            .collect();
        assert_eq!(found.len(), 1, "exactly one button labelled {label:?}");
        found[0].mock_single_click(PointerEventButton::Left);
    }

    fn page(&self) -> String {
        self.start.ui.get_page().into()
    }

    fn note(&self) -> String {
        let ui = &self.start.ui;
        ui.invoke_message(ui.get_note()).into()
    }

    fn connection(&self) -> String {
        self.start.ui.invoke_connection_text().into()
    }

    fn open(&self) -> bool {
        self.start.ui.window().is_visible()
    }

    fn chips(&self) -> Vec<String> {
        self.start
            .ui
            .get_chips()
            .iter()
            .map(|c| format!("{} {}", c.keys, c.id))
            .collect()
    }
}

#[test]
fn f1_opens_the_register_and_f2_the_back_office() {
    let s = open(FakeStart::default(), terminal("en", false));
    assert!(s.start.ui.get_has_focus());
    assert_eq!(s.page(), "home");
    assert_eq!(s.start.ui.get_conn(), "ok");
    assert_eq!(s.connection(), "Connected to 192.168.1.10:8080");
    assert_eq!(s.chips(), ["F5 refresh"], "the tiles carry F1–F3");
    s.press("F1");
    assert_eq!(s.start.outcome(), Some(Choice::Register));
    assert!(!s.open());
    assert_eq!(s.fake.calls(), ["check"]);

    let s = open(FakeStart::default(), terminal("en", false));
    s.press("F2");
    assert_eq!(s.start.outcome(), Some(Choice::BackOffice));
    assert!(!s.open());
}

#[test]
fn tiles_are_buttons_too() {
    let s = open(FakeStart::default(), terminal("en", false));
    s.click("Cash register");
    assert_eq!(s.start.outcome(), Some(Choice::Register));

    let s = open(FakeStart::default(), terminal("id", false));
    s.click("Back office");
    assert_eq!(s.start.outcome(), Some(Choice::BackOffice));

    let s = open(FakeStart::default(), terminal("id", false));
    s.click("Pengaturan");
    assert_eq!(s.page(), "settings");
    s.click("Kembali");
    assert_eq!(s.page(), "home");
    s.click("Periksa koneksi");
    assert_eq!(s.fake.calls(), ["check", "check"]);
    assert!(s.open());
}

#[test]
fn settings_change_the_language_and_save_it() {
    let s = open(FakeStart::default(), terminal("en", false));
    s.press("F3");
    assert_eq!(s.page(), "settings");
    assert_eq!(s.chips(), ["F6 language", "F8 setup", "Esc back"]);
    assert_eq!(s.start.ui.get_language(), "en");
    s.press("F6");
    assert_eq!(s.start.ui.get_language(), "id");
    // The window speaks Indonesian at once.
    assert_eq!(s.connection(), "Terhubung ke 192.168.1.10:8080");
    s.click("English");
    assert_eq!(s.start.ui.get_language(), "en");
    assert_eq!(s.connection(), "Connected to 192.168.1.10:8080");
    s.click("English");
    assert_eq!(
        s.fake.calls(),
        ["check", "language id", "language en"],
        "a click on the language in use saves nothing"
    );
    s.press("Esc");
    assert_eq!(s.page(), "home");
    assert!(s.start.outcome().is_none());

    // A file that cannot be written: the language changes for now, the window says why.
    let fake = FakeStart {
        read_only: true,
        ..FakeStart::default()
    };
    let s = open(fake, terminal("en", false));
    s.press("F3");
    s.click("Bahasa Indonesia");
    assert_eq!(
        s.note(),
        "Pengaturan tidak bisa disimpan: read-only file",
        "{}",
        s.note()
    );
}

#[test]
fn sultan_lang_fixes_the_language() {
    let s = open(FakeStart::default(), terminal("en", true));
    s.press("F3");
    assert_eq!(s.chips(), ["F8 setup", "Esc back"]);
    s.press("F6");
    s.click("Bahasa Indonesia");
    assert_eq!(s.start.ui.get_language(), "en");
    assert!(s.start.ui.get_language_fixed());
    assert_eq!(s.fake.calls(), ["check"]);
}

#[test]
fn set_up_again_asks_first() {
    let s = open(FakeStart::default(), terminal("en", false));
    s.press("F3");
    s.press("F8");
    assert!(s.start.ui.get_confirm());
    assert!(s.chips().is_empty(), "the dialog has its own buttons");
    s.press("Esc");
    assert!(!s.start.ui.get_confirm());
    assert_eq!(s.page(), "settings");
    s.press("F8");
    s.click("Cancel");
    assert!(!s.start.ui.get_confirm());
    s.press("F8");
    s.press("Enter");
    assert_eq!(s.start.outcome(), Some(Choice::SetupAgain));
    assert!(!s.open());

    let s = open(FakeStart::default(), terminal("en", false));
    s.click("Settings");
    s.press("F8");
    s.click("Continue");
    assert_eq!(s.start.outcome(), Some(Choice::SetupAgain));
}

#[test]
fn an_unreachable_server_is_shown_and_nothing_opens() {
    let s = open(FakeStart::answering(vec![network()]), terminal("en", false));
    assert_eq!(s.start.ui.get_conn(), "failed");
    assert_eq!(s.connection(), "Not connected to 192.168.1.10:8080");
    assert_eq!(s.note(), "", "no message until something is asked for");
    // F1 checks again first; still nothing: the reason, and the window stays.
    s.fake.then(network());
    s.press("F1");
    assert!(s.open());
    assert_eq!(s.start.outcome(), None);
    assert!(
        s.note()
            .starts_with("Cannot reach the server at 192.168.1.10:8080."),
        "{}",
        s.note()
    );
    s.fake.then(network());
    s.press("F2");
    assert!(s.open());
    assert_eq!(s.fake.calls(), ["check", "check", "check"]);
    // The server is back: F5 shows it, F1 opens.
    s.press("F5");
    assert_eq!(s.start.ui.get_conn(), "ok");
    assert_eq!(s.note(), "");
    s.press("F1");
    assert_eq!(s.start.outcome(), Some(Choice::Register));

    // A check that fails, then succeeds when the register is asked for.
    let s = open(FakeStart::answering(vec![network()]), terminal("en", false));
    s.press("F1");
    assert_eq!(s.start.outcome(), Some(Choice::Register));
}

#[test]
fn an_unpaired_register_can_still_open_the_back_office() {
    let unpaired = Err(StartError::new("unpaired", "invalid_device_token"));
    let s = open(FakeStart::answering(vec![unpaired]), terminal("en", false));
    assert_eq!(
        s.connection(),
        "Connected to 192.168.1.10:8080, but this register is not paired"
    );
    s.press("F1");
    assert!(
        s.note()
            .starts_with("The server does not know this register")
    );
    // The window shows it in the Start screen's words.
    let shown: Vec<_> = ElementHandle::find_by_accessible_label(&s.start.ui, &s.note()).collect();
    assert_eq!(shown.len(), 1, "the message is on screen");
    assert!(s.open());
    s.press("F2");
    assert_eq!(s.start.outcome(), Some(Choice::BackOffice));
}

#[test]
fn a_single_app_says_so_in_both_languages() {
    let info = StartInfo {
        kind: AppKind::Single,
        server: "http://127.0.0.1:41234".into(),
        register_code: "R1".into(),
        register_name: "Kasir 1".into(),
        branch: "JKT01 · Toko Jakarta".into(),
        ..terminal("id", false)
    };
    let s = open(FakeStart::default(), info);
    assert_eq!(s.start.ui.get_kind(), "single");
    assert_eq!(
        s.connection(),
        "Aplikasi tunggal: server berjalan di komputer ini"
    );
    assert_eq!(s.start.ui.get_server(), "127.0.0.1:41234");
    assert_eq!(s.start.ui.get_branch(), "JKT01 · Toko Jakarta");
    assert_eq!(s.start.ui.get_version(), "0.1.0");
    assert_eq!(s.start.controller().info.register_code, "R1");
    s.press("F3");
    s.press("F6");
    assert_eq!(
        s.connection(),
        "Single app: the server runs on this computer"
    );
    // Keys that mean nothing here are left alone.
    s.press("F1");
    s.press("x");
    assert!(s.open());
}
