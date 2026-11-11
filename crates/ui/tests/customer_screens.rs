//! The register's customer step (spec 2.5, phase M5) driven headlessly against
//! [`FakeService`]'s seeded customers (Budi `K7QH-29XM` / `0812-3456-789`, Gold until 2027-12-31,
//! balance Rp 2 200; Siti of Toko Siti, Grosir, Silver): lookup by phone and by code (typed in
//! lower case), name search for a manager only, sign-up with `phone_taken`, attach and detach,
//! and the balance payout.

use std::sync::Arc;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_client::fake::FakeService;
use sultan_client::{ClientError, Problem};
use sultan_ui::{App, CustomerView, HomeView, Options, Runner, build_app};

const T0: i64 = 1_800_000_000_000;

struct Till {
    app: App,
    fake: Arc<FakeService>,
    _runtime: tokio::runtime::Runtime,
}

fn key_text(name: &str) -> SharedString {
    match name {
        "Enter" => Key::Return.into(),
        "Esc" => Key::Escape.into(),
        "Backspace" => Key::Backspace.into(),
        "Tab" => Key::Tab.into(),
        "Down" => Key::DownArrow.into(),
        "Right" => Key::RightArrow.into(),
        "F3" => Key::F3.into(),
        "F4" => Key::F4.into(),
        "F6" => Key::F6.into(),
        "F7" => Key::F7.into(),
        "F8" => Key::F8.into(),
        "F12" => Key::F12.into(),
        other => other.into(),
    }
}

/// A register in `language`; `user` 0 is Ani (cashier, PIN 1234), 1 is Budi (manager, 9999).
fn till(language: &str, user: usize) -> Till {
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
    fake.set_now(T0);
    let options = Options {
        register_label: "R1 · Kasir 1".to_owned(),
        language: Some(language.to_owned()),
        clock: Arc::new(|| T0),
        start: false,
    };
    let app = build_app(
        fake.clone(),
        Runner::Block(runtime.handle().clone()),
        options,
    )
    .unwrap();
    app.ui.window().set_size(LogicalSize::new(1366.0, 768.0));
    app.ui.show().unwrap();
    let t = Till {
        app,
        fake,
        _runtime: runtime,
    };
    t.log_in(user);
    t
}

impl Till {
    fn press(&self, name: &str) {
        let window = self.app.ui.window();
        window.dispatch_event(WindowEvent::KeyPressed {
            text: key_text(name),
        });
        window.dispatch_event(WindowEvent::KeyReleased {
            text: key_text(name),
        });
    }

    fn keys(&self, names: &[&str]) {
        for name in names {
            self.press(name);
        }
    }

    /// Types every character, as a scanner or the keyboard does.
    fn typed(&self, text: &str) {
        for c in text.chars() {
            self.press(&c.to_string());
        }
    }

    /// Logs `user` in and opens a session if needed.
    fn log_in(&self, user: usize) {
        for _ in 0..user {
            self.press("Right");
        }
        self.typed(["1234", "9999"][user]);
        self.press("Enter");
        if self.app.ui.get_screen() == "open" {
            self.typed("500000");
            self.press("Enter");
        }
        assert_eq!(self.app.ui.get_screen(), "home");
    }

    fn dialog(&self) -> CustomerView {
        self.app.ui.get_customer()
    }

    fn home(&self) -> HomeView {
        self.app.ui.get_home()
    }

    fn message(&self) -> String {
        self.app.ui.invoke_message(self.dialog().note).into()
    }

    fn chips(&self) -> Vec<String> {
        self.app
            .ui
            .get_chips()
            .iter()
            .map(|c| format!("{} {}", c.keys, c.label))
            .collect()
    }

    /// Looks `text` up in the open dialog (phone or code mode).
    fn find(&self, text: &str) -> CustomerView {
        self.typed(text);
        self.press("Enter");
        self.dialog()
    }

    #[track_caller]
    fn click(&self, label: &str) {
        let found: Vec<_> = ElementHandle::find_by_accessible_label(&self.app.ui, label)
            .filter(|e| e.accessible_role() == Some(AccessibleRole::Button))
            .collect();
        assert_eq!(found.len(), 1, "exactly one button labelled {label:?}");
        found[0].mock_single_click(PointerEventButton::Left);
    }

    #[track_caller]
    fn text_input(&self, label: &str) -> ElementHandle {
        let mut found: Vec<_> = ElementHandle::find_by_accessible_label(&self.app.ui, label)
            .filter(|e| e.accessible_role() == Some(AccessibleRole::TextInput))
            .collect();
        assert_eq!(found.len(), 1, "exactly one field labelled {label:?}");
        found.remove(0)
    }

    /// What the field labelled `label` shows.
    #[track_caller]
    fn field(&self, label: &str) -> String {
        self.text_input(label)
            .accessible_value()
            .unwrap_or_default()
            .into()
    }
}

#[test]
fn lookup_by_phone_and_by_code() {
    let t = till("en", 0);
    assert!(!t.dialog().open);
    assert_eq!(t.home().customer, "");
    t.click("Customer");
    let d = t.dialog();
    assert!(d.open);
    assert_eq!(d.page, "find");
    // Ani is a cashier: no name search (Tab does nothing), but she may sign up.
    assert_eq!(t.chips(), ["Enter find", "F6 new_customer", "Esc close"]);
    assert!(!d.can_search);
    t.press("Tab");
    assert!(!t.dialog().by_name);

    // By phone, in any accepted form.
    t.typed("0812-3456-789");
    assert_eq!(t.field("Phone or member code"), "0812-3456-789");
    t.press("Enter");
    let d = t.dialog();
    assert_eq!(d.page, "shown");
    assert_eq!(
        (d.name.as_str(), d.code.as_str(), d.phone.as_str()),
        ("Budi Santoso", "K7QH-29XM", "0812****789")
    );
    assert_eq!(
        (d.tier.as_str(), d.tier_until.as_str(), d.tier_expired),
        ("Gold", "2027-12-31", false)
    );
    assert_eq!(d.balance, "Rp 2.200");
    assert_eq!(d.line, "Budi S. · 0812****789 · Gold");
    assert_eq!((d.group.as_str(), d.attached), ("", false));
    assert_eq!(
        t.chips(),
        ["Enter attach", "Esc back"],
        "no payout for a cashier"
    );
    assert_eq!(t.fake.calls_of("customer_by_phone"), ["0812-3456-789"]);

    // By member code, typed in lower case (or scanned into this field).
    t.press("Esc");
    assert_eq!(t.dialog().query, "", "the field is empty for the next code");
    let d = t.find("k7qh-29xm");
    assert_eq!(
        (d.page.as_str(), d.name.as_str()),
        ("shown", "Budi Santoso")
    );
    assert_eq!(t.fake.calls_of("customer_by_code"), ["k7qh-29xm"]);
    assert!(
        t.fake.calls_of("lookup").is_empty(),
        "never a product lookup"
    );

    // Errors, translated.
    t.press("Esc");
    t.find("0812-0000-111");
    assert_eq!(
        t.message(),
        "No customer has the phone or member code 0812-0000-111."
    );
    t.find("K7QH-29X0");
    assert_eq!(t.message(), "K7QH-29X0 is not a valid member code.");
    t.find("12");
    assert_eq!(t.message(), "That is not a valid phone number.");
    t.press("Enter");
    assert_eq!(t.message(), "Type a phone number or a member code first.");
    t.fake
        .fail_on("customer_by_phone", ClientError::Network("down".into()));
    t.find("0812-3456-789");
    assert_eq!(
        t.message(),
        "Cannot reach the server. Check the network and try again."
    );
    // Esc closes the dialog from the Find page.
    t.click("Close");
    assert!(!t.dialog().open);
    assert_eq!(t.app.ui.get_screen(), "home");
}

#[test]
fn an_expired_tier_is_flagged_and_left_off_the_receipt() {
    let t = till("en", 0);
    // 2028-02-23: Budi's Gold (until 2027-12-31) no longer applies.
    t.fake.set_now(1_835_000_000_000);
    t.press("F3");
    let d = t.find("08123456789");
    assert!(d.tier_expired);
    assert_eq!(d.line, "Budi S. · 0812****789");
}

#[test]
fn name_search_is_for_managers_only() {
    let t = till("en", 1);
    t.press("F3");
    assert!(t.dialog().can_search);
    assert_eq!(
        t.chips(),
        ["Enter find", "Tab by_name", "F6 new_customer", "Esc close"]
    );
    t.press("Tab");
    assert!(t.dialog().by_name);
    t.press("Enter");
    assert_eq!(t.message(), "Type part of a name first.");
    t.typed("Zz");
    t.press("Enter");
    assert_eq!(t.message(), "No customer is named “Zz”.");
    t.keys(&["Backspace", "Backspace"]);
    t.typed("Siti");
    assert_eq!(t.field("Name"), "Siti");
    t.press("Enter");
    let rows: Vec<_> = t.app.ui.get_customers().iter().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Siti Rahma");
    assert_eq!(
        rows[0].detail,
        "M4RT-2PQA · 0812*****432 · Toko Siti · Silver"
    );
    assert_eq!(t.chips()[0], "Enter open_customer");
    t.click("Siti Rahma");
    let d = t.dialog();
    assert_eq!(
        (d.page.as_str(), d.company.as_str(), d.group.as_str()),
        ("shown", "Toko Siti", "Grosir")
    );
    assert_eq!(d.line, "Toko Siti · 0812*****432 · Silver");
    assert_eq!(
        t.chips(),
        ["Enter attach", "Esc back"],
        "a balance of 0: nothing to pay out"
    );
    // Back to phone or code.
    t.keys(&["Esc", "Tab"]);
    assert!(!t.dialog().by_name);
}

#[test]
fn attach_and_detach() {
    let t = till("en", 0);
    t.press("F3");
    t.find("0812 3456 789");
    t.press("Enter");
    assert!(!t.dialog().open);
    let home = t.home();
    assert_eq!(
        (home.customer.as_str(), home.customer_code.as_str()),
        ("Budi S. · 0812****789 · Gold", "K7QH-29XM")
    );
    assert_eq!(home.customer_balance, "Rp 2.200");
    assert_eq!(
        t.app.ui.invoke_message(home.note).as_str(),
        "Customer attached: Budi S. · 0812****789 · Gold"
    );
    assert!(t.chips().contains(&"F4 detach_customer".to_owned()));
    // F3 opens the attached customer; the strip opens it too.
    t.press("F3");
    let d = t.dialog();
    assert_eq!((d.page.as_str(), d.attached), ("shown", true));
    t.keys(&["Esc", "Esc"]);
    t.click("Attached customer");
    assert_eq!(t.dialog().page, "shown");
    t.keys(&["Esc", "Esc"]);
    t.click("Detach customer");
    assert_eq!(t.home().customer, "");
    assert_eq!(
        t.app.ui.invoke_message(t.home().note).as_str(),
        "Customer detached."
    );
    assert!(!t.chips().contains(&"F4 detach_customer".to_owned()));
    // Locking forgets the customer.
    t.press("F3");
    t.find("K7QH29XM");
    t.press("Enter");
    assert_ne!(t.home().customer, "");
    t.press("F12");
    t.log_in(0);
    assert_eq!(t.home().customer, "");
}

#[test]
fn sign_up_offers_the_customer_who_has_the_phone() {
    let t = till("en", 0);
    t.press("F3");
    t.click("New customer");
    assert_eq!(t.dialog().page, "sign-up");
    t.press("Enter");
    assert_eq!(t.message(), "The name is required.");
    t.typed("Rudi Hartono");
    t.press("Tab");
    t.typed("0812-3456-789");
    assert_eq!(
        (t.field("Name"), t.field("Phone (optional)")),
        ("Rudi Hartono".to_owned(), "0812-3456-789".to_owned())
    );
    t.press("Enter");
    assert_eq!(
        t.message(),
        "This phone already belongs to customer K7QH-29XM · 0812****789. F8 shows that customer."
    );
    assert!(t.chips().contains(&"F8 show_that_customer".to_owned()));
    t.press("F8");
    let d = t.dialog();
    assert_eq!(
        (d.page.as_str(), d.name.as_str()),
        ("shown", "Budi Santoso")
    );
    // Back to the form: another phone; the first try is lost on the network.
    t.press("Esc");
    t.press("F6");
    let d = t.dialog();
    assert_eq!(
        (d.new_name.as_str(), d.new_phone.as_str()),
        ("Rudi Hartono", "0812-3456-789")
    );
    t.text_input("Phone (optional)")
        .mock_single_click(PointerEventButton::Left);
    for _ in 0.."0812-3456-789".len() {
        t.press("Backspace");
    }
    t.typed("0813 1111 2222");
    t.fake
        .fail_on("sign_up_customer", ClientError::Network("down".into()));
    t.press("Enter");
    assert_eq!(t.dialog().page, "sign-up");
    t.click("Sign up");
    let d = t.dialog();
    assert_eq!(
        (d.page.as_str(), d.name.as_str()),
        ("shown", "Rudi Hartono")
    );
    assert_eq!(d.phone, "0813*****222");
    assert!(t.message().starts_with("Signed up. Member code "));
    let ids = t.fake.calls_of("sign_up_customer");
    assert_eq!(ids.len(), 3);
    assert_eq!(ids[1], ids[2], "the retry sends the same id");
    assert!(uuid::Uuid::parse_str(&ids[2]).is_ok_and(|u| u.get_version_num() == 7));
    t.press("Enter");
    assert!(t.home().customer.starts_with("Rudi H. · 0813*****222"));
}

#[test]
fn payout_checks_the_balance_and_pays_out() {
    let t = till("en", 1);
    t.press("F3");
    t.find("K7QH-29XM");
    assert_eq!(t.chips(), ["Enter attach", "F7 pay_out", "Esc back"]);
    t.press("Enter");
    t.press("F3");
    t.click("Pay out");
    assert_eq!(t.dialog().page, "payout");
    t.press("Enter");
    assert_eq!(t.message(), "That amount is not valid.");
    t.typed("5000");
    assert_eq!(t.field("Amount to pay out"), "Rp 5.000");
    t.press("Enter");
    assert!(t.dialog().confirm);
    assert_eq!(t.chips(), ["Enter confirm_payout", "Esc edit_amount"]);
    t.press("Enter");
    assert_eq!(t.message(), "The balance is only Rp 2.200.");
    assert!(!t.dialog().confirm);
    // No open session (closed by a manager meanwhile).
    t.fake.fail_on(
        "balance_payout",
        Problem::new(409, "no_open_session", "x").into(),
    );
    t.keys(&["Enter", "Enter"]);
    assert_eq!(t.message(), "There is no open session on this register.");
    for _ in 0..4 {
        t.press("Backspace");
    }
    t.typed("2200");
    t.keys(&["Enter", "Esc"]);
    assert!(!t.dialog().confirm, "Esc changes the amount");
    t.press("Enter");
    t.click("Yes, pay out");
    let d = t.dialog();
    assert_eq!((d.page.as_str(), d.balance.as_str()), ("shown", "Rp 0"));
    assert_eq!(t.message(), "Paid out Rp 2.200 in cash.");
    assert_eq!(t.chips(), ["Enter attach", "Esc back"], "nothing left");
    // The attached customer follows.
    assert_eq!(t.home().customer_balance, "Rp 0");
    let calls = t.fake.calls_of("balance_payout");
    assert_eq!(calls.len(), 3);
    let key = |c: &str| c.rsplit("key=").next().unwrap().to_owned();
    assert_ne!(
        key(&calls[0]),
        key(&calls[1]),
        "a refused payout gets a new key"
    );
    assert!(calls[2].contains("amount=2200"));
}

#[test]
fn indonesian_customer_texts() {
    let t = till("id", 0);
    t.press("F3");
    t.find("K7QH-29X0");
    assert_eq!(t.message(), "K7QH-29X0 bukan kode member yang valid.");
    assert_eq!(
        t.app.ui.invoke_chip_label("new_customer".into()),
        "Pelanggan baru"
    );
}
