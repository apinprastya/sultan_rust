//! The session home's barcode field (phase C5, spec 2.2 §6) driven headlessly against
//! [`FakeService`]'s catalog: keys typed as a scanner sends them, the item panel's view, the
//! error messages in both languages, Esc, the buttons, and a `catalog.changed` event.

use std::sync::Arc;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use serde_json::json;
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, SharedString};
use sultan_client::fake::{BRANCH_ID, FakeService};
use sultan_client::{BackOffice, ClientError, Problem, Product};
use sultan_core::catalog::gtin_check_digit;
use sultan_ui::{App, Options, Runner, ScanView, build_app};

const T0: i64 = 1_800_000_000_000;
const STANDARD: &str = "00000000-0000-7000-8000-000000000101";
const SHIRT: &str = "01900000-0000-7000-8000-000000000001";
const CHEESE: &str = "01900000-0000-7000-8000-000000000002";

fn id(n: u32) -> String {
    format!("01900000-0000-7000-8000-{n:012}")
}

fn ean13(body: &str) -> String {
    format!("{body}{}", gtin_check_digit(body).unwrap())
}

/// Kaos Polos (Size Medium, Color Red; pcs) with barcode `089912345678C` (an EAN-13 with a leading 0), and Keju Edam (weighed,
/// PLU 123, kg, open price, needs a note).
fn catalog() -> FakeService {
    let fake = FakeService::default();
    let shirt: Product = serde_json::from_value(json!({
        "code": "KAOS", "name": "Kaos Polos", "type": "stock", "tax_class_id": STANDARD,
        "options": [
            { "id": id(10), "name": "Size", "values": [{ "id": id(11), "name": "Medium", "code": "M" }] },
            { "id": id(12), "name": "Color", "values": [{ "id": id(13), "name": "Red", "code": "RED" }] },
        ],
        "units": [{ "id": id(14), "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
        "base_unit_id": id(14),
        "variants": [{ "id": id(15), "option_value_ids": [id(11), id(13)] }],
        "barcodes": [{ "id": id(16), "variant_id": id(15), "unit_id": id(14),
                      "code": ean13("089912345678") }],
    }))
    .unwrap();
    fake.seed_product(SHIRT, &shirt);
    let cheese: Product = serde_json::from_value(json!({
        "code": "KEJU", "name": "Keju Edam", "type": "weighed", "tax_class_id": STANDARD,
        "open_price": true, "requires_note": true,
        "units": [{ "id": id(20), "name": "kg", "kind": "decimal", "factor_thousandths": 1000 }],
        "base_unit_id": id(20),
        "variants": [{ "id": id(21), "plu": 123 }],
    }))
    .unwrap();
    fake.seed_product(CHEESE, &cheese);
    fake
}

struct Till {
    app: App,
    fake: Arc<FakeService>,
    _runtime: tokio::runtime::Runtime,
}

fn till(language: &str) -> Till {
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
    let fake = Arc::new(catalog());
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
    // Ani logs in and opens a session.
    t.typed("1234");
    t.press("Enter");
    t.typed("500000");
    t.press("Enter");
    assert_eq!(t.app.ui.get_screen(), "home");
    t
}

fn key_text(name: &str) -> SharedString {
    match name {
        "Enter" => Key::Return.into(),
        "Esc" => Key::Escape.into(),
        "Backspace" => Key::Backspace.into(),
        "F12" => Key::F12.into(),
        other => other.into(),
    }
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

    /// Types every character, as a scanner does.
    fn typed(&self, text: &str) {
        for c in text.chars() {
            self.press(&c.to_string());
        }
    }

    /// Scans `code`: the characters, then Enter.
    fn scan(&self, code: &str) -> ScanView {
        self.typed(code);
        self.press("Enter");
        self.view()
    }

    fn view(&self) -> ScanView {
        self.app.ui.get_scan()
    }

    fn message(&self, view: &ScanView) -> String {
        self.app.ui.invoke_message(view.note.clone()).into()
    }

    #[track_caller]
    fn click(&self, label: &str) {
        let found: Vec<_> = ElementHandle::find_by_accessible_label(&self.app.ui, label)
            .filter(|e| e.accessible_role() == Some(AccessibleRole::Button))
            .collect();
        assert_eq!(found.len(), 1, "exactly one button labelled {label:?}");
        found[0].mock_single_click(PointerEventButton::Left);
    }

    fn owner(&self) -> String {
        futures_now(self.fake.password_login("owner", "owner-pass-1")).token
    }
}

fn futures_now<T>(future: sultan_client::BoxFuture<T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
        .unwrap()
}

#[test]
fn a_barcode_shows_the_item() {
    let t = till("en");
    let ui = &t.app.ui;
    assert_eq!(t.view().state, 0, "nothing scanned yet");
    let code = ean13("089912345678");
    t.typed(&code);
    assert_eq!(t.view().code, code.as_str(), "typed into the field");
    t.press("Enter");
    let v = t.view();
    assert_eq!(v.code, "", "the field is empty for the next scan");
    assert_eq!(v.state, 1);
    assert_eq!(
        (v.name.as_str(), v.variant.as_str(), v.unit.as_str()),
        ("Kaos Polos", "Medium / Red", "pcs")
    );
    assert_eq!(
        (v.qty.as_str(), v.sku.as_str(), v.plu.as_str()),
        ("1 pcs", "KAOS-M-RED", "")
    );
    assert_eq!(
        (v.r#match.as_str(), v.matched.as_str(), v.scanned.as_str()),
        ("barcode", code.as_str(), code.as_str())
    );
    assert_eq!((v.kind.as_str(), v.amount.as_str()), ("stock", ""));
    assert!(!v.open_price && !v.requires_note);
    assert_eq!(t.fake.calls_of("lookup"), std::slice::from_ref(&code));
    assert_eq!(
        ui.invoke_message(sultan_ui::Note::default()),
        "",
        "no message"
    );

    // The UPC-A form (12 digits) and the SKU typed in lower case, with a typo fixed.
    let v = t.scan(&code[1..]);
    assert_eq!(v.r#match, "normalized_barcode");
    assert_eq!(v.matched, code.as_str(), "the stored EAN-13");
    t.typed("kaos-m-redd");
    t.press("Backspace");
    t.press("Enter");
    let v = t.view();
    assert_eq!(
        (v.r#match.as_str(), v.matched.as_str()),
        ("sku", "KAOS-M-RED")
    );
    assert_eq!(v.scanned, "kaos-m-red");
}

#[test]
fn scale_labels_carry_weight_or_price() {
    let t = till("en");
    let v = t.scan(&ean13("210012301250"));
    assert_eq!(v.state, 1);
    assert_eq!(
        (v.name.as_str(), v.variant.as_str(), v.kind.as_str()),
        ("Keju Edam", "", "weighed")
    );
    assert_eq!(
        (v.qty.as_str(), v.plu.as_str(), v.r#match.as_str()),
        ("1,250 kg", "123", "scale_weight")
    );
    assert_eq!(v.amount, "");
    assert!(v.open_price && v.requires_note);

    let v = t.scan(&ean13("220012315000"));
    assert_eq!(
        (v.qty.as_str(), v.amount.as_str(), v.r#match.as_str()),
        ("", "Rp 15.000", "scale_price")
    );
    // The PLU typed alone.
    let v = t.scan("123");
    assert_eq!((v.r#match.as_str(), v.qty.as_str()), ("plu", "1 kg"));
    // A label of 0.
    let v = t.scan(&ean13("210012300000"));
    assert_eq!((v.state, v.note.code.as_str()), (2, "scale_value_zero"));
}

#[test]
fn not_found_and_not_sold_here_are_explained() {
    let t = till("en");
    let v = t.scan("NOPE");
    assert_eq!(v.state, 2);
    assert_eq!(
        (v.note.code.as_str(), v.note.arg.as_str(), v.note.tone),
        ("item_not_found", "NOPE", -1)
    );
    assert_eq!(t.message(&v), "No item has the code NOPE.");

    let owner = t.owner();
    futures_now(
        t.fake
            .put_branch_blocks(&owner, SHIRT, &[BRANCH_ID.to_owned()]),
    );
    let v = t.scan("KAOS-M-RED");
    assert_eq!((v.state, v.note.code.as_str()), (2, "item_not_sold_here"));
    assert_eq!(t.message(&v), "KAOS-M-RED is not sold at this branch.");

    futures_now(t.fake.set_archived(&owner, CHEESE, true, "k"));
    let v = t.scan("123");
    assert_eq!(t.message(&v), "123 is archived and can no longer be sold.");

    // A network failure is shown in the panel too.
    t.fake
        .fail_on("lookup", ClientError::Network("down".into()));
    let v = t.scan("123");
    assert_eq!(v.note.code, "network");
}

#[test]
fn indonesian_messages() {
    let t = till("id");
    let v = t.scan("NOPE");
    assert_eq!(t.message(&v), "Tidak ada barang dengan kode NOPE.");
    let owner = t.owner();
    futures_now(
        t.fake
            .put_branch_blocks(&owner, SHIRT, &[BRANCH_ID.to_owned()]),
    );
    let v = t.scan("KAOS-M-RED");
    assert_eq!(t.message(&v), "KAOS-M-RED tidak dijual di cabang ini.");
    assert_eq!(t.app.ui.invoke_chip_label("look_up".into()), "Cari");
    assert_eq!(t.app.ui.invoke_chip_label("clear".into()), "Hapus");
}

#[test]
fn esc_and_buttons_clear_and_look_up() {
    let t = till("en");
    t.typed("KAOS");
    t.press("Esc");
    assert_eq!(t.view().code, "", "Esc clears the field");
    let v = t.scan("KAOS-M-RED");
    assert_eq!(v.state, 1);
    t.press("Esc");
    let v = t.view();
    assert_eq!((v.state, v.code.as_str()), (0, ""), "and the item");
    // Enter on an empty field does nothing.
    t.press("Enter");
    assert!(t.fake.calls_of("lookup").len() == 1);

    // The on-screen buttons do the same.
    t.typed("123");
    t.click("Look up");
    assert_eq!(t.view().r#match, "plu");
    t.click("Clear");
    assert_eq!(t.view().state, 0);

    // Function keys still work with a half-typed code: F12 locks.
    t.typed("89912");
    t.press("F12");
    assert_eq!(t.app.ui.get_screen(), "lock");
}

#[test]
fn catalog_changes_refresh_the_shown_item() {
    let t = till("en");
    let v = t.scan("KAOS-M-RED");
    assert_eq!(v.state, 1);
    let owner = t.owner();
    // Another product changes: no new lookup.
    futures_now(
        t.fake
            .put_branch_blocks(&owner, CHEESE, &[BRANCH_ID.to_owned()]),
    );
    t.app.pump();
    assert_eq!(t.fake.calls_of("lookup").len(), 1);
    // The shown product is blocked here: the panel says so.
    futures_now(
        t.fake
            .put_branch_blocks(&owner, SHIRT, &[BRANCH_ID.to_owned()]),
    );
    t.app.pump();
    let v = t.view();
    assert_eq!((v.state, v.note.code.as_str()), (2, "item_not_sold_here"));
    assert_eq!(t.fake.calls_of("lookup").len(), 2);
    // Unblocked again: any catalog change looks a failed code up again.
    futures_now(t.fake.put_branch_blocks(&owner, SHIRT, &[]));
    t.app.pump();
    assert_eq!(t.view().state, 1);
}

#[test]
fn an_ended_login_during_a_lookup_locks() {
    let t = till("en");
    t.fake.fail_on(
        "lookup",
        ClientError::Api(Problem::new(401, "invalid_session", "x")),
    );
    t.scan("123");
    assert_eq!(t.app.ui.get_screen(), "lock");
    assert_eq!(t.app.ui.get_lock().note.code, "session_ended");
}
