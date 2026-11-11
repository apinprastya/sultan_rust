//! Drives the back office's customer screens (spec 2.5, phase M4) headlessly against
//! [`FakeService`], like `backoffice_screens.rs`: keys are window events, buttons are found by
//! their accessible label, text fields are set through the window's `edit` callback, and the
//! checks read the window's properties and the fake's customer book.
//!
//! The fake's seed: Budi Santoso (`K7QH-29XM`, Gold until 2027-12-31, balance Rp 2 200) and Siti
//! Rahma (`M4RT-2PQA`, group Grosir, Silver). `owner` is an Owner (every customer permission),
//! `budi` a Manager (all but `customer.erase`), `kasir` a Cashier (`customer.create` only).

use std::sync::Arc;
use std::time::Duration;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_client::fake::{FakeService, SEED_BUDI, SEED_GOLD, SEED_SITI};
use sultan_client::{BackOffice, Customer, ServerEvent};
use sultan_ui::{BackOfficeApp, BackOfficeOptions, Note, Runner, build_backoffice};

struct Office {
    app: BackOfficeApp,
    fake: Arc<FakeService>,
    runtime: tokio::runtime::Runtime,
}

fn office_with(fake: FakeService, language: &str) -> Office {
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
    let mut options = BackOfficeOptions::new("http://127.0.0.1:8080");
    options.language = Some(language.to_owned());
    options.debounce = Duration::ZERO;
    let app = build_backoffice(
        fake.clone(),
        Runner::Block(runtime.handle().clone()),
        options,
    )
    .unwrap();
    app.ui.window().set_size(LogicalSize::new(1366.0, 768.0));
    app.ui.show().unwrap();
    Office { app, fake, runtime }
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
        "F6" => Key::F6.into(),
        other => other.into(),
    }
}

/// A duplicate of Budi: signed up offline at a branch with Budi's phone (moved to
/// `phone_conflict` at HQ), with an email Budi lacks and a balance of Rp 500.
fn duplicate() -> Customer {
    Customer {
        id: "customer-dup".into(),
        code: "QW3R7TY9".into(),
        display_code: "QW3R-7TY9".into(),
        name: "Budi S.".into(),
        email: Some("budi@example.com".into()),
        phone_conflict: Some("+628123456789".into()),
        signed_up_at: 1_790_000_000_000,
        ..Customer::default()
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

    fn ctrl_shift(&self, c: &str) {
        self.press(&["Control", "Shift", c]);
    }

    fn buttons(&self, label: &str) -> Vec<ElementHandle> {
        ElementHandle::find_by_accessible_label(&self.app.ui, label)
            .filter(|e| {
                matches!(
                    e.accessible_role(),
                    Some(AccessibleRole::Button | AccessibleRole::Tab)
                )
            })
            .collect()
    }

    #[track_caller]
    fn click(&self, label: &str) {
        let found = self.buttons(label);
        assert_eq!(found.len(), 1, "exactly one button labelled {label:?}");
        found[0].mock_single_click(PointerEventButton::Left);
    }

    fn edit(&self, kind: &str, field: &str, value: &str) {
        self.app
            .ui
            .invoke_edit(kind.into(), 0, 0, field.into(), value.into());
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

    fn names(&self) -> Vec<String> {
        self.app
            .ui
            .get_customer_rows()
            .iter()
            .map(|r| r.name.to_string())
            .collect()
    }

    /// Opens the customer list and the customer named `name`.
    fn open(&self, name: &str) {
        self.press(&["F6"]);
        let row = self.names().iter().position(|n| n == name).unwrap();
        self.app
            .ui
            .invoke_pick("customer-open".into(), i32::try_from(row).unwrap(), 0);
        assert_eq!(self.screen(), "customer-form");
    }

    fn stored(&self, id: &str) -> Customer {
        let token = self
            .runtime
            .block_on(self.fake.password_login("owner", "owner-pass-1"));
        let token = token.unwrap().token;
        self.runtime
            .block_on(sultan_client::BackOffice::customer(
                self.fake.as_ref(),
                &token,
                id,
            ))
            .unwrap()
    }
}

#[test]
fn list_create_and_edit_with_phone_taken() {
    let o = office_with(FakeService::default(), "en");
    o.login("owner");
    o.click("Customers");
    assert_eq!(o.screen(), "customers");
    assert_eq!(o.names(), ["Budi Santoso", "Siti Rahma"]);
    let budi = o.app.ui.get_customer_rows().row_data(0).unwrap();
    assert_eq!(
        (budi.code.as_str(), budi.phone.as_str(), budi.tier.as_str()),
        ("K7QH-29XM", "0812****789", "Gold")
    );
    assert_eq!(budi.balance, "Rp 2.200");
    assert!(!budi.expired);
    let siti = o.app.ui.get_customer_rows().row_data(1).unwrap();
    assert_eq!(siti.group, "Grosir");

    // Search by company.
    o.edit("customer-list", "search", "toko");
    assert_eq!(o.names(), ["Siti Rahma"]);
    o.edit("customer-list", "search", "");
    assert_eq!(o.names().len(), 2);

    // A new customer: a bad phone is caught before the call.
    o.ctrl("n");
    assert_eq!(o.screen(), "customer-form");
    assert!(o.app.ui.get_customer_form().is_new);
    o.edit("customer", "name", "Ani Wijaya");
    o.edit("customer", "phone", "12");
    o.edit("customer", "birth_day", "29");
    o.edit("customer", "birth_month", "2");
    o.edit("customer", "tax_id", "123");
    o.ctrl("s");
    let form = o.app.ui.get_customer_form();
    assert_eq!(
        o.text(form.phone_error),
        "Not a phone number: use 8 to 15 digits, e.g. 0812-3456-789."
    );
    assert_eq!(
        o.text(form.tax_id_error),
        "A tax ID (NPWP or NIK) has 15 or 16 digits."
    );
    assert!(o.fake.calls_of("put_customer").is_empty());
    o.edit("customer", "phone", "0813-1111-2222");
    o.edit("customer", "tax_id", "01.234.567.8-901.000");
    o.click("Save");
    let form = o.app.ui.get_customer_form();
    assert_eq!(o.text(form.notice), "Saved.");
    assert!(!form.is_new);
    assert_eq!(form.title, "Ani Wijaya");
    assert_eq!(form.code.len(), 9, "a drawn code such as K7QH-29XM");
    assert_eq!(form.phone, "081311112222");
    assert_eq!(
        (form.birth_day.as_str(), form.birth_month.as_str()),
        ("29", "2")
    );
    assert_eq!(form.balance, "Rp 0");

    // Back to the list: the new customer is there.
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "customers");
    assert_eq!(o.names(), ["Ani Wijaya", "Budi Santoso", "Siti Rahma"]);

    // Budi takes Siti's phone: the error names Siti's code and masked phone.
    o.open("Budi Santoso");
    let form = o.app.ui.get_customer_form();
    assert_eq!(form.phone, "08123456789");
    assert_eq!(o.app.ui.get_balance_rows().row_count(), 1);
    assert_eq!(
        o.app.ui.get_balance_rows().row_data(0).unwrap().amount,
        "+Rp 2.200"
    );
    o.edit("customer", "phone", "0812-9876-5432");
    o.ctrl("s");
    let form = o.app.ui.get_customer_form();
    assert_eq!(
        o.text(form.notice),
        "Not saved: please fix the marked fields."
    );
    assert_eq!(
        o.text(form.phone_error),
        "This phone is already used by the customer M4RT-2PQA · 0812*****432."
    );
    assert!(form.dirty);

    // Leaving with unsaved changes asks first.
    o.press(&["Esc"]);
    assert_eq!(o.app.ui.get_confirm(), "discard-customer");
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "customer-form");
    o.edit("customer", "phone", "0812-3456-789");
    o.edit("customer", "email", "Budi@Example.com");
    o.ctrl("s");
    assert_eq!(
        o.stored(SEED_BUDI).email.as_deref(),
        Some("budi@example.com")
    );
    // An event from elsewhere reloads the list.
    o.press(&["Esc"]);
    let before = o.fake.calls_of("search_customers").len();
    o.fake.emit(ServerEvent {
        kind: "customer.changed".into(),
        data: serde_json::json!({ "customer_id": SEED_SITI }),
    });
    o.app.pump();
    assert_eq!(o.fake.calls_of("search_customers").len(), before + 1);
}

#[test]
fn tier_change_needs_manage() {
    let o = office_with(FakeService::default(), "en");
    // A cashier may sign up customers but not list them or set a tier.
    o.login("kasir");
    o.press(&["F6"]);
    assert_eq!(o.screen(), "customers");
    let list = o.app.ui.get_customer_list();
    assert!(!list.can_search && list.can_create && !list.show_archived);
    assert_eq!(
        o.text(list.notice),
        "Listing and searching customers needs the permission customer.search. You can still add a customer (Ctrl+N)."
    );
    assert!(o.names().is_empty());
    o.click("New customer");
    let form = o.app.ui.get_customer_form();
    assert!(form.can_edit && !form.can_manage && !form.can_archive && !form.can_erase);
    // Picking a tier without the permission changes nothing.
    o.app
        .ui
        .invoke_choose("customer".into(), 0, "tier".into(), 1);
    assert_eq!(o.app.ui.get_customer_form().tier_index, 0);
    o.edit("customer", "name", "Wati");
    o.ctrl("s");
    let form = o.app.ui.get_customer_form();
    assert_eq!(o.text(form.notice), "Saved.");
    // Saved, but the cashier may not edit it any more (customer.edit).
    assert!(!form.can_edit);
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_customer_form().notice),
        "Not allowed: this needs the permission customer.edit."
    );
    o.ctrl_shift("a");
    assert_eq!(o.app.ui.get_confirm(), "");
    // Review screens need customer.manage.
    o.press(&["Esc"]);
    o.ctrl("4");
    assert_eq!(o.screen(), "duplicates");
    assert_eq!(
        o.text(o.app.ui.get_duplicates().notice),
        "Not allowed: this needs the permission customer.manage."
    );
    o.ctrl("6");
    o.ctrl("s");
    assert!(!o.app.ui.get_balance_settings().can_edit);
}

#[test]
fn owner_sets_tier_and_archives() {
    let o = office_with(FakeService::default(), "en");
    o.login("owner");
    o.open("Siti Rahma");
    let form = o.app.ui.get_customer_form();
    assert!(form.can_manage && form.can_archive && form.can_erase);
    assert_eq!(form.group_index, 1, "Grosir");
    // Tiers best first: (No tier), Gold, Silver.
    assert_eq!(
        o.app.ui.get_tier_choices().iter().collect::<Vec<_>>(),
        ["(No tier)", "Gold", "Silver"]
    );
    o.app
        .ui
        .invoke_choose("customer".into(), 0, "tier".into(), 1);
    o.edit("customer", "valid_until", "2027-02-30");
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_customer_form().valid_until_error),
        "Type the date as YYYY-MM-DD, e.g. 2027-12-31."
    );
    o.edit("customer", "valid_until", "2027-06-30");
    o.app
        .ui
        .invoke_toggle("customer".into(), 0, "manual_tier".into(), true);
    o.ctrl("s");
    let siti = o.stored(SEED_SITI);
    assert_eq!(siti.tier_id.as_deref(), Some(SEED_GOLD));
    assert_eq!(siti.tier_valid_until.as_deref(), Some("2027-06-30"));
    assert!(siti.manual_tier);

    // Archive (asks first), then unarchive.
    o.ctrl_shift("a");
    assert_eq!(o.app.ui.get_confirm(), "archive-customer");
    assert_eq!(o.app.ui.get_confirm_name(), "Siti Rahma (M4RT-2PQA)");
    o.press(&["Enter"]);
    let form = o.app.ui.get_customer_form();
    assert!(form.archived && !form.can_edit);
    assert_eq!(
        o.text(form.notice),
        "The customer is archived. The till no longer finds the customer."
    );
    o.click("Unarchive");
    assert_eq!(o.app.ui.get_confirm(), "unarchive-customer");
    o.click("Yes");
    assert!(!o.app.ui.get_customer_form().archived);

    // An expired tier is marked in the list and on the form.
    o.fake.set_now(1_830_000_000_000); // 2027-12-27 → Siti's 2027-06-30 has passed
    o.press(&["Esc"]);
    o.press(&["F5"]);
    let siti = o.app.ui.get_customer_rows().row_data(1).unwrap();
    assert!(siti.expired);
    assert_eq!(siti.until, "2027-06-30");
    o.press(&["Down"]);
    o.press(&["Enter"]);
    assert!(o.app.ui.get_customer_form().tier_expired);
}

#[test]
fn merge_possible_duplicates() {
    let fake = FakeService::default();
    fake.seed_customer(duplicate());
    fake.add_customer_balance("customer-dup", "change_saved", 500);
    let o = office_with(fake, "en");
    o.login("owner");
    o.press(&["F6"]);
    o.ctrl("4");
    assert_eq!(o.screen(), "duplicates");
    let rows = o.app.ui.get_duplicate_rows();
    assert_eq!(rows.row_count(), 1);
    assert_eq!(rows.row_data(0).unwrap().phone, "08123456789");
    // The first duplicate is picked, and the holder of its phone is the one to keep.
    let merge = o.app.ui.get_merge();
    assert_eq!(
        (merge.from_name.as_str(), merge.into_name.as_str()),
        ("Budi S.", "Budi Santoso")
    );
    assert_eq!(merge.code, "K7QH-29XM");
    assert!(merge.ready);

    // Typing another code finds that customer instead; the same customer is refused.
    o.edit("merge", "code", "qw3r7ty9");
    assert_eq!(
        o.text(o.app.ui.get_merge().notice),
        "That is the same customer. Type the code of the customer to keep."
    );
    o.ctrl("m");
    assert_eq!(o.app.ui.get_confirm(), "", "nothing to merge into");
    o.edit("merge", "code", "ZZZZ-ZZZZ");
    assert_eq!(
        o.text(o.app.ui.get_merge().notice),
        "No active customer has this code or phone."
    );
    o.edit("merge", "code", "K7QH29XM");
    assert!(o.app.ui.get_merge().ready);

    o.ctrl("m");
    assert_eq!(o.app.ui.get_confirm(), "merge");
    assert_eq!(o.app.ui.get_confirm_name(), "Budi S. (QW3R-7TY9)");
    assert_eq!(o.app.ui.get_confirm_other(), "Budi Santoso (K7QH-29XM)");
    o.click("Merge");
    assert_eq!(o.app.ui.get_confirm(), "");
    assert_eq!(o.app.ui.get_duplicate_rows().row_count(), 0);
    assert_eq!(
        o.text(o.app.ui.get_duplicates().notice),
        "Merged. The customer K7QH-29XM was kept."
    );
    let budi = o.stored(SEED_BUDI);
    assert_eq!(budi.balance, 2_700);
    assert_eq!(budi.email.as_deref(), Some("budi@example.com"));
    let dup = o.stored("customer-dup");
    assert_eq!(dup.merged_into_id.as_deref(), Some(SEED_BUDI));
}

#[test]
fn erase_refused_with_a_balance() {
    let o = office_with(FakeService::default(), "en");
    o.login("owner");
    o.open("Budi Santoso");
    o.ctrl_shift("e");
    assert_eq!(o.app.ui.get_confirm(), "erase");
    assert_eq!(o.app.ui.get_confirm_other(), "K7QH-29XM");
    assert_eq!(o.app.ui.get_erase_balance(), "Rp 2.200");
    // Enter does nothing until the code is typed.
    assert!(!o.app.ui.get_erase_ready());
    o.press(&["Enter"]);
    assert_eq!(o.app.ui.get_confirm(), "erase");
    o.edit("erase", "code", "k7qh29xm");
    assert!(o.app.ui.get_erase_ready());
    o.press(&["Enter"]);
    assert_eq!(o.app.ui.get_confirm(), "");
    assert_eq!(
        o.text(o.app.ui.get_customer_form().notice),
        "Cannot erase: the balance is Rp 2.200. It must be spent or paid out at the till first."
    );
    assert_eq!(o.stored(SEED_BUDI).name, "Budi Santoso");

    // Siti's balance is 0: the erase goes through.
    o.press(&["Esc"]);
    o.open("Siti Rahma");
    o.click("Erase personal data");
    assert_eq!(o.app.ui.get_erase_balance(), "");
    o.edit("erase", "code", "M4RT-2PQA");
    o.click("Erase");
    let form = o.app.ui.get_customer_form();
    assert!(form.erased && !form.can_erase && !form.can_edit);
    assert_eq!(form.title, "Erased customer");
    assert_eq!(o.text(form.notice), "The personal data is erased.");
    assert_eq!(o.stored(SEED_SITI).phone, None);

    // A manager may not erase.
    let o = office_with(FakeService::default(), "en");
    o.login("budi");
    o.open("Siti Rahma");
    assert!(!o.app.ui.get_customer_form().can_erase);
    o.ctrl_shift("e");
    assert_eq!(o.app.ui.get_confirm(), "");
    assert_eq!(
        o.text(o.app.ui.get_customer_form().notice),
        "Not allowed: this needs the permission customer.erase."
    );
}

#[test]
fn balance_settings_groups_tiers_and_negative_balances() {
    let fake = FakeService::default();
    fake.add_customer_balance(SEED_SITI, "payment", -300);
    let o = office_with(fake, "id");
    o.login("owner");
    o.press(&["F6"]);

    // Balance settings (Ctrl+6).
    o.ctrl("6");
    assert_eq!(o.screen(), "balance-settings");
    let s = o.app.ui.get_balance_settings();
    assert_eq!(
        (s.per_sale.as_str(), s.max_balance.as_str()),
        ("5000", "100000")
    );
    o.edit("settings", "max_saved_per_sale", "-5");
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_balance_settings().per_sale_error),
        "Isi jumlah dalam Rupiah, 0 atau lebih."
    );
    o.edit("settings", "max_saved_per_sale", "3.000");
    o.ctrl("s");
    assert_eq!(o.text(o.app.ui.get_balance_settings().notice), "Tersimpan.");
    assert_eq!(o.fake.balance_settings().max_saved_per_sale, 3_000);

    // Groups (Ctrl+2): add one, a taken name is refused.
    o.ctrl("2");
    assert_eq!(o.screen(), "customer-groups");
    assert_eq!(o.app.ui.get_group_editor().name, "Grosir");
    o.ctrl("n");
    assert!(o.app.ui.get_group_editor().is_new);
    o.edit("group", "name", "Grosir");
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_group_editor().notice),
        o.text(Note {
            code: "duplicate_name".into(),
            ..Note::default()
        })
    );
    o.edit("group", "name", "Reseller");
    o.ctrl("s");
    assert_eq!(o.app.ui.get_group_rows().row_count(), 2);
    o.ctrl_shift("a");
    assert!(o.app.ui.get_group_editor().archived);

    // Tiers (Ctrl+3): a new tier suggests the next rank; a taken rank is refused.
    o.ctrl("3");
    assert_eq!(o.app.ui.get_tier_rows().row_count(), 2);
    o.ctrl("n");
    assert_eq!(o.app.ui.get_tier_editor().rank, "3");
    o.edit("tier", "name", "Platinum");
    o.edit("tier", "rank", "2");
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_tier_editor().notice),
        "Tingkat lain sudah memakai peringkat ini."
    );
    o.edit("tier", "rank", "x");
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_tier_editor().notice),
        "Peringkat harus bilangan bulat."
    );
    o.edit("tier", "rank", "3");
    o.ctrl("s");
    assert_eq!(o.app.ui.get_tier_rows().row_count(), 3);
    o.press(&["Down"]);
    assert!(!o.app.ui.get_tier_editor().is_new);

    // Negative balances (Ctrl+5): Siti, Enter opens her.
    o.ctrl("5");
    assert_eq!(o.screen(), "negative-balances");
    let rows = o.app.ui.get_negative_rows();
    assert_eq!(rows.row_count(), 1);
    assert_eq!(rows.row_data(0).unwrap().balance, "-Rp 300");
    assert!(rows.row_data(0).unwrap().negative);
    o.press(&["Enter"]);
    assert_eq!(o.screen(), "customer-form");
    assert_eq!(o.app.ui.get_customer_form().balance, "-Rp 300");
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "negative-balances", "back where it came from");
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "customers");
}

#[test]
fn archived_customers_and_paging() {
    let fake = FakeService::default();
    for i in 0..25 {
        fake.seed_customer(Customer {
            id: format!("c-{i:02}"),
            code: format!("A{:07}", 2_000_000 + i)
                .replace('0', "2")
                .replace('1', "3"),
            name: format!("Pelanggan {i:02}"),
            archived: i == 0,
            archived_at: (i == 0).then_some(1),
            ..Customer::default()
        });
    }
    let o = office_with(fake, "en");
    o.login("owner");
    o.press(&["F6"]);
    assert_eq!(o.names().len(), 20);
    assert!(o.app.ui.get_customer_list().has_more);
    // Moving past the last row loads the next page.
    for _ in 0..20 {
        o.press(&["Down"]);
    }
    assert_eq!(o.names().len(), 26, "24 active + Budi + Siti");
    o.app
        .ui
        .invoke_toggle("customer-list".into(), 0, "archived".into(), true);
    o.click("Load more");
    assert_eq!(o.names().len(), 27);
    assert!(o.app.ui.get_customer_rows().iter().any(|r| r.archived));
}
