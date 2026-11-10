//! Drives the real setup window headlessly (`i-slint-backend-testing`, as `screens.rs` does for
//! the register window) against a fake [`SetupService`]: keys are window events, buttons are
//! found by their accessible label, and the checks read the window's properties and the calls
//! the fake recorded.

use std::sync::{Arc, Mutex};

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_ui::setup::{
    AppType, BranchChoice, DbCheck, ManagerSession, Paired, SetupError, SetupFuture, SetupService,
    ShopReport, ShopRequest, TerminalRequest,
};
use sultan_ui::{Note, Runner, Setup, build_setup};

/// Answers from a script and records every call.
#[derive(Default)]
struct FakeSetup {
    calls: Mutex<Vec<String>>,
    db: Mutex<Option<Result<DbCheck, SetupError>>>,
    shop: Mutex<Option<ShopRequest>>,
    fail_login: Mutex<bool>,
    register: Mutex<Option<TerminalRequest>>,
}

fn ready<T: Send + 'static>(value: Result<T, SetupError>) -> SetupFuture<T> {
    Box::pin(async move { value })
}

impl FakeSetup {
    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl SetupService for FakeSetup {
    fn check_db(&self, url: &str) -> SetupFuture<DbCheck> {
        self.record(format!("check_db {url}"));
        ready(
            self.db
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Ok(DbCheck::Empty)),
        )
    }

    fn init_shop(&self, request: &ShopRequest) -> SetupFuture<ShopReport> {
        self.record(format!("init_shop {}", request.company));
        *self.shop.lock().unwrap() = Some(request.clone());
        ready(Ok(ShopReport {
            company: request.company.clone(),
            branch_code: request.branch_code.clone(),
            db_url: request
                .db_url
                .clone()
                .unwrap_or_else(|| "sqlite://x".into()),
        }))
    }

    fn check_server(&self, server: &str) -> SetupFuture<()> {
        self.record(format!("check_server {server}"));
        ready(Ok(()))
    }

    fn manager_login(
        &self,
        server: &str,
        username: &str,
        _password: &str,
    ) -> SetupFuture<ManagerSession> {
        self.record(format!("login {server} {username}"));
        if *self.fail_login.lock().unwrap() {
            return ready(Err(SetupError::new("invalid_credentials", "")));
        }
        ready(Ok(ManagerSession {
            token: "tok".into(),
            branch_id: "b1".into(),
            branches: vec![
                BranchChoice {
                    id: "b1".into(),
                    code: "JKT01".into(),
                    name: "Jakarta".into(),
                },
                BranchChoice {
                    id: "b2".into(),
                    code: "BDG01".into(),
                    name: "Bandung".into(),
                },
            ],
        }))
    }

    fn register_terminal(
        &self,
        server: &str,
        token: &str,
        request: &TerminalRequest,
    ) -> SetupFuture<Paired> {
        self.record(format!("register {server} {token}"));
        *self.register.lock().unwrap() = Some(request.clone());
        ready(Ok(Paired {
            register_code: request.code.clone(),
            register_name: request.name.clone(),
            server: server.to_owned(),
        }))
    }

    fn pair_with_code(&self, server: &str, code: &str) -> SetupFuture<Paired> {
        self.record(format!("pair {server} {code}"));
        ready(Err(SetupError::new("invalid_pairing_code", "")))
    }
}

struct Wizard {
    setup: Setup,
    fake: Arc<FakeSetup>,
    _runtime: tokio::runtime::Runtime,
}

fn wizard_with(fake: FakeSetup, language: &str) -> Wizard {
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
    let setup = build_setup(
        fake.clone(),
        Runner::Block(runtime.handle().clone()),
        "/shop/sultan.db",
        Some(language),
    )
    .unwrap();
    setup.ui.window().set_size(LogicalSize::new(1024.0, 760.0));
    setup.ui.show().unwrap();
    Wizard {
        setup,
        fake,
        _runtime: runtime,
    }
}

fn wizard() -> Wizard {
    wizard_with(FakeSetup::default(), "en")
}

fn key_text(name: &str) -> SharedString {
    match name {
        "Enter" => Key::Return.into(),
        "Esc" => Key::Escape.into(),
        "Down" => Key::DownArrow.into(),
        "Right" => Key::RightArrow.into(),
        "Up" => Key::UpArrow.into(),
        other => other.into(),
    }
}

impl Wizard {
    fn press(&self, name: &str) {
        let window = self.setup.ui.window();
        window.dispatch_event(WindowEvent::KeyPressed {
            text: key_text(name),
        });
        window.dispatch_event(WindowEvent::KeyReleased {
            text: key_text(name),
        });
    }

    #[track_caller]
    fn click(&self, label: &str) {
        let found: Vec<_> = ElementHandle::find_by_accessible_label(&self.setup.ui, label)
            .filter(|e| e.accessible_role() == Some(AccessibleRole::Button))
            .collect();
        assert_eq!(found.len(), 1, "exactly one button labelled {label:?}");
        found[0].mock_single_click(PointerEventButton::Left);
    }

    fn field(&self, id: &str, value: &str) {
        self.setup.ui.invoke_field(id.into(), value.into());
    }

    fn page(&self) -> String {
        self.setup.ui.get_page().into()
    }

    fn note(&self) -> String {
        let note: Note = self.setup.ui.get_note();
        self.setup.ui.invoke_message(note).into()
    }
}

#[test]
fn branch_server_is_shown_but_cannot_be_picked() {
    let w = wizard();
    assert_eq!(w.page(), "welcome");
    assert!(w.setup.ui.get_has_focus());
    w.click("Branch server");
    assert_eq!(w.page(), "welcome");
    assert!(
        w.note().starts_with("A branch server needs HQ sync"),
        "{}",
        w.note()
    );
    assert_eq!(w.setup.ui.get_cursor(), 2);
    // Keyboard: Down twice from the branch card stays on the last card; Enter picks it.
    w.press("Down");
    w.press("Down");
    assert_eq!(w.setup.ui.get_cursor(), 3);
    w.press("Enter");
    assert_eq!(w.page(), "server");
    assert_eq!(w.setup.ui.get_app_type(), "terminal");
    w.press("Esc");
    assert_eq!(w.page(), "welcome");
    // Unknown callbacks are ignored.
    w.setup.ui.invoke_action("nope".into());
    w.setup.ui.invoke_pick("nothing".into(), 0);
    w.setup.ui.invoke_pick("type".into(), -1);
    w.setup.ui.invoke_field("nothing".into(), "x".into());
    w.setup.ui.invoke_pump();
    assert_eq!(w.page(), "welcome");
    assert_eq!(w.setup.ui.invoke_chip_label("next".into()), "Next");
}

#[test]
fn single_app_by_buttons() {
    let w = wizard();
    w.click("Single app");
    assert_eq!(w.page(), "shop");
    assert_eq!(w.setup.ui.get_form().location, "Store floor");
    w.click("Next");
    assert_eq!(w.note(), "Please fill in: Company name.");
    w.field("company", "PT A");
    w.field("branch_code", "JKT01");
    w.click("WITA (Makassar)");
    assert_eq!(w.setup.ui.get_timezone(), 1);
    w.click("Next");
    assert_eq!(w.page(), "owner");
    assert_eq!(w.setup.ui.get_form().owner_username, "owner");
    for (id, value) in [
        ("password", "owner-pass-1"),
        ("password2", "owner-pass-1"),
        ("pin", "1234"),
        ("pin2", "4321"),
    ] {
        w.field(id, value);
    }
    w.click("Set up");
    assert_eq!(w.note(), "The two PINs are not the same.");
    w.field("pin2", "1234");
    w.click("Set up");
    assert_eq!(w.page(), "done");
    let request = w.fake.shop.lock().unwrap().clone().unwrap();
    assert_eq!(request.app_type, AppType::Single);
    assert_eq!(request.timezone, "Asia/Makassar");
    assert_eq!(w.setup.ui.get_summary_a(), "PT A");
    assert_eq!(w.setup.ui.get_summary_b(), "JKT01");
    assert_eq!(w.setup.outcome(), None);
    w.click("Open Sultan");
    assert_eq!(w.setup.outcome(), Some(AppType::Single));
    assert!(!w.setup.ui.window().is_visible(), "the window closes");
}

#[test]
fn hq_server_tests_the_database() {
    let fake = FakeSetup::default();
    *fake.db.lock().unwrap() = Some(Ok(DbCheck::Initialised("PT Old".into())));
    let w = wizard_with(fake, "en");
    w.press("2");
    assert_eq!(w.page(), "database");
    assert_eq!(w.setup.ui.get_form().sqlite_path, "/shop/sultan.db");
    w.click("Test connection");
    assert_eq!(
        w.note(),
        "This database is already set up (company PT Old). Pick another one."
    );
    *w.fake.db.lock().unwrap() = Some(Ok(DbCheck::Empty));
    w.click("MySQL server");
    assert_eq!(w.setup.ui.get_db_kind(), 1);
    w.field("mysql_url", "mysql://u:p@h/sultan");
    w.click("Next");
    assert_eq!(w.page(), "network");
    assert_eq!(w.setup.ui.get_form().listen, "0.0.0.0:8080");
    w.click("Next");
    w.field("company", "PT HQ");
    w.field("branch_code", "HQ");
    w.click("Next");
    for (id, value) in [
        ("password", "owner-pass-1"),
        ("password2", "owner-pass-1"),
        ("pin", "1234"),
        ("pin2", "1234"),
    ] {
        w.field(id, value);
    }
    w.press("Enter");
    assert_eq!(w.page(), "done");
    let request = w.fake.shop.lock().unwrap().clone().unwrap();
    assert_eq!(request.db_url.as_deref(), Some("mysql://u:p@h/sultan"));
    assert_eq!(request.listen.as_deref(), Some("0.0.0.0:8080"));
    assert_eq!(
        w.fake.calls(),
        [
            "check_db sqlite:///shop/sultan.db",
            "check_db mysql://u:p@h/sultan",
            "init_shop PT HQ"
        ]
    );
    w.click("Finish");
    assert_eq!(w.setup.outcome(), Some(AppType::Hq));
}

#[test]
fn terminal_registers_with_a_manager_login() {
    let fake = FakeSetup::default();
    *fake.fail_login.lock().unwrap() = true;
    let w = wizard_with(fake, "en");
    w.click("Terminal");
    w.field("server", "192.168.1.10");
    w.click("Connect");
    assert_eq!(w.page(), "manager");
    w.field("manager", "owner");
    w.field("manager_password", "pw");
    w.click("Log in");
    assert_eq!(w.note(), "Wrong username or password.");
    *w.fake.fail_login.lock().unwrap() = false;
    w.click("Log in");
    assert_eq!(w.page(), "terminal");
    let branches: Vec<String> = w.setup.ui.get_branches().iter().map(Into::into).collect();
    assert_eq!(branches, ["JKT01 · Jakarta", "BDG01 · Bandung"]);
    w.click("BDG01 · Bandung");
    assert_eq!(w.setup.ui.get_branch(), 1);
    w.field("register_code", "R2");
    w.field("register_name", "Kasir 2");
    w.click("Register");
    assert_eq!(w.page(), "done");
    assert_eq!(
        w.fake.register.lock().unwrap().clone().unwrap().branch_id,
        "b2"
    );
    assert_eq!(w.setup.ui.get_summary_c(), "http://192.168.1.10:8080");
    w.press("Enter");
    assert_eq!(w.setup.outcome(), Some(AppType::Terminal));
}

#[test]
fn terminal_pairing_code_in_indonesian() {
    let w = wizard_with(FakeSetup::default(), "id");
    w.click("Terminal");
    w.field("server", "h:9000");
    w.press("Enter");
    w.click("Saya punya kode pairing");
    assert_eq!(w.page(), "code");
    w.field("pairing_code", "abcd-1234");
    w.click("Pasangkan");
    assert_eq!(
        w.note(),
        "Kode pairing tidak dikenal, sudah dipakai, atau kedaluwarsa."
    );
    assert_eq!(
        w.fake.calls(),
        ["check_server http://h:9000", "pair http://h:9000 ABCD-1234"]
    );
    w.click("Pakai login manajer");
    assert_eq!(w.page(), "manager");
}
