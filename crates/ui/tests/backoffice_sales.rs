//! Drives the back office's sales screens (spec 2.6, phase S6) headlessly against
//! [`FakeService`], like `backoffice_prices.rs`: keys are window events, buttons are found by
//! their accessible label, text fields are set through the window's `edit` callback.
//!
//! [`FakeService::seed_sales_demo`] posts the history: sales of Teh Botol by ani, the second
//! voided, the third for "Ibu Emas" with one item returned. In the fake the owner holds
//! `report.sales` and `tender.manage`, budi `report.sales`, kasir neither.

use std::sync::Arc;
use std::time::Duration;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_client::fake::{FakeService, SalesDemo};
use sultan_client::{BackOffice, Problem, Service};
use sultan_ui::{BackOfficeApp, BackOfficeOptions, Note, Runner, build_backoffice};

struct Office {
    app: BackOfficeApp,
    fake: Arc<FakeService>,
    runtime: tokio::runtime::Runtime,
    demo: SalesDemo,
}

fn office(count: usize, language: &str) -> Office {
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
    let demo = runtime.block_on(fake.seed_sales_demo(count));
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
    Office {
        app,
        fake,
        runtime,
        demo,
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
        "F6" => Key::F6.into(),
        "F8" => Key::F8.into(),
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

    fn choose(&self, kind: &str, row: i32, field: &str, index: i32) {
        self.app
            .ui
            .invoke_choose(kind.into(), row, field.into(), index);
    }

    fn screen(&self) -> String {
        self.app.ui.get_screen().into()
    }

    fn text(&self, note: Note) -> String {
        self.app.ui.invoke_message(note).into()
    }

    fn label(&self, code: &str) -> String {
        self.app.ui.invoke_label(code.into()).into()
    }

    fn login(&self, user: &str) {
        self.edit("login", "username", user);
        self.edit("login", "password", &format!("{user}-pass-1"));
        self.app.ui.invoke_action("login".into());
        assert_eq!(self.screen(), "products");
    }

    fn sale_numbers(&self) -> Vec<String> {
        self.app
            .ui
            .get_sale_rows()
            .iter()
            .map(|r| r.number.to_string())
            .collect()
    }

    fn facts(&self) -> Vec<(String, String)> {
        self.app
            .ui
            .get_detail_facts()
            .iter()
            .map(|f| (f.label.to_string(), f.value.to_string()))
            .collect()
    }

    fn receipt(&self) -> String {
        self.app
            .ui
            .get_detail_receipt()
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn owner(&self) -> String {
        self.runtime
            .block_on(self.fake.password_login("owner", "owner-pass-1"))
            .unwrap()
            .token
    }
}

#[test]
#[allow(clippy::too_many_lines)] // one walk through the lists and the details
fn sales_list_filters_detail_and_returns() {
    let o = office(3, "en");
    o.login("owner");
    o.press(&["F8"]);
    assert_eq!(o.screen(), "sales");
    let numbers: Vec<String> = o
        .demo
        .sales
        .iter()
        .rev()
        .map(|s| s.summary.number.clone())
        .collect();
    assert_eq!(o.sale_numbers(), numbers, "newest first");
    let rows: Vec<_> = o.app.ui.get_sale_rows().iter().collect();
    assert_eq!(rows[0].customer, "Ibu Emas");
    assert_eq!(rows[1].status, "voided");
    assert_eq!(rows[2].cashier, "Ani");
    assert_eq!(rows[2].place, "JKT01 · R1");
    // Status: VOID only.
    o.choose("sales-filter", 0, "status", 2);
    assert_eq!(o.sale_numbers(), [numbers[1].clone()]);
    o.choose("sales-filter", 0, "status", 0);
    // A number, then the customer by member code.
    o.edit("sales-filter", "number", &numbers[2].to_lowercase());
    o.click("Search");
    assert_eq!(o.sale_numbers(), [numbers[2].clone()]);
    o.click("Clear");
    assert_eq!(o.sale_numbers().len(), 3);
    o.edit("sales-filter", "customer", "081234500001");
    o.click("Search");
    assert_eq!(o.sale_numbers(), [numbers[0].clone()]);
    o.edit("sales-filter", "customer", "0899999999");
    o.press(&["F5"]);
    o.click("Search");
    assert_eq!(
        o.text(o.app.ui.get_sale_list().notice),
        "No customer has this member code or phone."
    );
    o.click("Clear");
    o.edit("sales-filter", "from", "2026-13-01");
    o.click("Search");
    assert_eq!(o.app.ui.get_sales_filters().date_error.arg, "from");
    o.edit("sales-filter", "from", "2020-01-01");
    o.edit("sales-filter", "until", "2099-12-31");
    o.click("Search");
    assert_eq!(o.sale_numbers().len(), 3);
    // A register picked from those seen.
    o.choose("sales-filter", 0, "register", 1);
    assert_eq!(o.sale_numbers().len(), 3);
    o.choose("sales-filter", 0, "branch", 2);
    assert!(o.sale_numbers().is_empty(), "nothing at the other branch");
    assert!(o.app.ui.get_sale_list().empty);
    o.click("Clear");

    // The voided sale: VOID, tenders, receipt.
    o.press(&["Down"]);
    o.press(&["Enter"]);
    assert_eq!(o.screen(), "sale-detail");
    let d = o.app.ui.get_sales_detail();
    assert_eq!(d.title, numbers[1].as_str());
    assert!(d.voided);
    let facts = o.facts();
    assert!(facts.iter().any(|(l, _)| l == "voided_at"), "{facts:?}");
    assert!(facts.iter().any(|(l, v)| l == "Tunai" && v == "Rp 20.000"));
    assert!(o.receipt().contains("VOID"), "{}", o.receipt());
    assert_eq!(o.label("voided_at"), "Voided");
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "sales");
    assert_eq!(
        o.app.ui.get_sale_list().cursor,
        1,
        "the list kept its place"
    );

    // The customer's sale links to its return, which links back.
    o.press(&["Up"]);
    o.press(&["Enter"]);
    assert_eq!(o.screen(), "sale-detail");
    let facts = o.facts();
    assert!(
        facts
            .iter()
            .any(|(l, v)| l == "customer" && v.starts_with("Ibu Emas"))
    );
    let links: Vec<_> = o.app.ui.get_detail_links().iter().collect();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].number, o.demo.sale_return.summary.number.as_str());
    o.press(&["Enter"]);
    assert_eq!(o.screen(), "return-detail");
    assert!(o.receipt().contains("RETURN"), "{}", o.receipt());
    let links: Vec<_> = o.app.ui.get_detail_links().iter().collect();
    assert_eq!(links[0].status, "original");
    assert_eq!(links[0].number, numbers[0].as_str());
    o.app.ui.invoke_pick("doc-link".into(), 0, 0);
    assert_eq!(o.screen(), "sale-detail");
    o.press(&["Esc"]);
    assert_eq!(
        o.screen(),
        "return-detail",
        "Esc goes back through the details"
    );
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "sale-detail");
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "sales");

    // Returns (Ctrl+2).
    o.ctrl("2");
    assert_eq!(o.screen(), "returns");
    let rows: Vec<_> = o.app.ui.get_return_rows().iter().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].customer, "Ibu Emas");
    assert_eq!(rows[0].total, "Rp 4.750");
    o.click("Open");
    assert_eq!(o.screen(), "return-detail");
    assert!(o.facts().iter().any(|(l, _)| l == "refund"));
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "returns");
    o.app.ui.invoke_action("tab-sales".into());
    assert_eq!(o.screen(), "sales");
}

#[test]
fn load_more_past_the_last_row() {
    let o = office(53, "en");
    o.login("owner");
    o.press(&["F8"]);
    assert_eq!(o.sale_numbers().len(), 50);
    assert!(o.app.ui.get_sale_list().more);
    o.ctrl("l");
    assert_eq!(o.sale_numbers().len(), 53);
    assert!(!o.app.ui.get_sale_list().more);
    // Again from the top: moving past the last row loads the rest.
    o.press(&["F5"]);
    assert_eq!(o.sale_numbers().len(), 50);
    for _ in 0..50 {
        o.press(&["Down"]);
    }
    assert_eq!(o.sale_numbers().len(), 53);
    assert_eq!(o.app.ui.get_sale_list().cursor, 49);
}

#[test]
fn reading_needs_report_sales() {
    let o = office(3, "en");
    o.login("kasir");
    o.press(&["F8"]);
    assert!(o.sale_numbers().is_empty());
    assert_eq!(
        o.text(o.app.ui.get_sale_list().notice),
        "Reading sales needs the permission report.sales. Ask the owner."
    );
    // Tender types are read-only for kasir.
    o.ctrl("3");
    assert_eq!(o.screen(), "tender-types");
    assert!(!o.app.ui.get_tender_editor().can_edit);
    assert!(
        o.text(o.app.ui.get_tender_read_only())
            .contains("tender.manage")
    );
    o.ctrl("s");
    assert_eq!(o.fake.calls_of("put_tender_type").len(), 0);
    o.ctrl("4");
    assert!(!o.app.ui.get_sales_settings().can_edit);
}

#[test]
#[allow(clippy::too_many_lines)] // CRUD with the locked kind, archive, then the branch node
fn tender_types_crud_and_settings() {
    let o = office(3, "en");
    o.login("owner");
    o.press(&["F8"]);
    o.click("Tender types");
    assert_eq!(o.screen(), "tender-types");
    let names: Vec<String> = o
        .app
        .ui
        .get_tender_rows()
        .iter()
        .map(|r| r.name.to_string())
        .collect();
    assert_eq!(names, ["Tunai", "Kartu debit", "QRIS", "Saldo pelanggan"]);
    assert_eq!(o.app.ui.get_tender_editor().name, "Tunai");
    o.press(&["Down"]);
    assert_eq!(o.app.ui.get_tender_editor().name, "Kartu debit");
    // New: Debit BCA, a card.
    o.ctrl("n");
    assert!(o.app.ui.get_tender_editor().is_new);
    o.edit("tender", "name", " ");
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_tender_editor().notice),
        "Type a name (1 to 40 characters)."
    );
    o.edit("tender", "name", "Debit BCA");
    o.edit("tender", "sort", "x");
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_tender_editor().notice),
        "Type the position as a whole number, 0 or more."
    );
    o.edit("tender", "sort", "5");
    o.choose("tender", 0, "kind", 1);
    o.app
        .ui
        .invoke_toggle("tender".into(), 0, "needs_reference".into(), true);
    o.app
        .ui
        .invoke_toggle("tender".into(), 0, "opens_drawer".into(), false);
    o.click("Save");
    assert_eq!(o.text(o.app.ui.get_tender_editor().notice), "Saved.");
    let e = o.app.ui.get_tender_editor();
    assert!(!e.is_new);
    assert_eq!((e.name.as_str(), e.kind_index), ("Debit BCA", 1));
    let stored = o
        .runtime
        .block_on(o.fake.all_tender_types(&o.owner()))
        .unwrap();
    let bca = stored.iter().find(|t| t.name == "Debit BCA").unwrap();
    assert_eq!(bca.kind, "card");
    assert!(bca.needs_reference);
    // The kind is fixed once saved.
    o.choose("tender", 0, "kind", 2);
    assert_eq!(o.app.ui.get_tender_editor().kind_index, 1);
    o.app
        .ui
        .invoke_toggle("tender".into(), 0, "active".into(), false);
    o.ctrl("s");
    let stored = o
        .runtime
        .block_on(o.fake.all_tender_types(&o.owner()))
        .unwrap();
    assert!(
        !stored
            .iter()
            .find(|t| t.name == "Debit BCA")
            .unwrap()
            .active
    );
    // Archive asks first.
    o.press(&["Control", "Shift", "a"]);
    assert_eq!(o.app.ui.get_confirm(), "archive-tender");
    assert_eq!(o.app.ui.get_confirm_name(), "Debit BCA");
    o.press(&["Enter"]);
    assert_eq!(
        o.text(o.app.ui.get_tender_editor().notice),
        "Tender type archived."
    );
    assert!(o.app.ui.get_tender_editor().archived);
    assert!(!o.app.ui.get_tender_editor().can_edit);

    // Sales settings (Ctrl+4).
    o.ctrl("4");
    assert_eq!(o.screen(), "sales-settings");
    let s = o.app.ui.get_sales_settings();
    assert_eq!(s.return_days, "30");
    assert!(s.can_edit && s.loaded);
    let widths: Vec<_> = o.app.ui.get_width_rows().iter().collect();
    assert_eq!(widths.len(), 2);
    assert_eq!(widths[0].branch, "JKT01 · Toko Jakarta");
    o.edit("sales-settings", "return_days", "abc");
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_sales_settings().notice),
        "Type the days as a whole number from 0 to 3650."
    );
    o.edit("sales-settings", "return_days", "0");
    o.choose("sales-settings", 0, "width", 2);
    o.click("Save");
    assert_eq!(o.text(o.app.ui.get_sales_settings().notice), "Saved.");
    let stored = o
        .runtime
        .block_on(o.fake.sales_settings(&o.owner()))
        .unwrap();
    assert_eq!(stored.return_days, 0);
    assert_eq!(stored.receipt_widths[0].receipt_width, 48);
    // The receipts follow the saved width.
    let sale = o.demo.sales[0].summary.id.clone();
    let receipt = o
        .runtime
        .block_on(o.fake.receipt(&o.owner(), &sale, false, "en", None))
        .unwrap();
    assert!(receipt.lines.iter().any(|l| l.chars().count() == 48));
    assert!(receipt.lines.iter().all(|l| l.chars().count() <= 48));
    let ret = o.demo.sale_return.summary.id.clone();
    let receipt = o
        .runtime
        .block_on(o.fake.return_receipt(&o.owner(), &ret, false, "en", None))
        .unwrap();
    assert!(receipt.lines.iter().any(|l| l.chars().count() == 48));

    // A branch node refuses writes: the screens turn read-only and say why.
    o.ctrl("3");
    o.ctrl("n");
    o.edit("tender", "name", "Transfer BRI");
    o.fake
        .fail_next(Problem::new(403, "read_only_replica", "branch node").into());
    o.ctrl("s");
    assert!(!o.app.ui.get_tender_editor().can_edit);
    assert!(!o.text(o.app.ui.get_tender_read_only()).is_empty());
    let writes = o.fake.calls_of("put_tender_type").len();
    o.ctrl("s");
    assert_eq!(
        o.fake.calls_of("put_tender_type").len(),
        writes,
        "no more writes"
    );
}

#[test]
fn customer_purchase_history_tab() {
    let o = office(3, "en");
    o.login("owner");
    o.press(&["F6"]);
    let row = o
        .app
        .ui
        .get_customer_rows()
        .iter()
        .position(|r| r.name == "Ibu Emas")
        .unwrap();
    o.app
        .ui
        .invoke_pick("customer-open".into(), i32::try_from(row).unwrap(), 0);
    assert_eq!(o.screen(), "customer-form");
    assert!(o.app.ui.get_customer_form().can_history);
    o.ctrl("h");
    assert_eq!(o.screen(), "customer-history");
    assert_eq!(o.app.ui.get_history_name(), "Ibu Emas");
    let rows: Vec<_> = o.app.ui.get_history_rows().iter().collect();
    assert_eq!(rows.len(), 2, "the sale and its return");
    assert_eq!(rows[0].kind, "return");
    assert_eq!(
        rows[0].customer, "",
        "the customer's own name is not repeated"
    );
    assert_eq!(rows[1].place, "JKT01 · R1");
    // Enter opens the return; Esc comes back to the history, then to the form.
    o.press(&["Enter"]);
    assert_eq!(o.screen(), "return-detail");
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "customer-history");
    o.press(&["Down"]);
    o.press(&["Enter"]);
    assert_eq!(o.screen(), "sale-detail");
    o.press(&["Esc"]);
    o.click("Back to the customer");
    assert_eq!(o.screen(), "customer-form");
    assert_eq!(o.app.ui.get_customer_form().title, "Ibu Emas");
    o.click("Purchase history");
    assert_eq!(o.screen(), "customer-history");
    assert_eq!(
        o.fake.calls_of("customer_history").len(),
        1,
        "kept, not loaded again"
    );
}

#[test]
fn leaving_the_history_asks_before_dropping_customer_edits() {
    let o = office(3, "en");
    o.login("owner");
    o.press(&["F6"]);
    let row = o
        .app
        .ui
        .get_customer_rows()
        .iter()
        .position(|r| r.name == "Ibu Emas")
        .unwrap();
    o.app
        .ui
        .invoke_pick("customer-open".into(), i32::try_from(row).unwrap(), 0);
    o.edit("customer", "name", "Ibu Emas Baru");
    o.ctrl("h");
    assert_eq!(o.screen(), "customer-history");
    o.press(&["F8"]);
    assert_eq!(o.app.ui.get_confirm(), "discard");
    assert_eq!(o.screen(), "customer-history");
}

#[test]
fn indonesian_texts() {
    let o = office(3, "id");
    o.login("owner");
    o.press(&["F8"]);
    o.ctrl("1");
    assert_eq!(o.screen(), "sales");
    o.press(&["Enter"]);
    assert!(o.receipt().contains("Petugas"), "{}", o.receipt());
    assert_eq!(o.label("paid"), "Dibayar");
}
