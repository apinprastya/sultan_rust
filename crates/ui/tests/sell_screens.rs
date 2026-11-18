//! The sell screen (spec 2.6, phase S4) driven headlessly against [`FakeService`]'s pricing demo
//! (Teh Botol pcs 5 000, Aqua pcs 3 500 and box 84 000, a Gold tier −5% for Budi) plus an
//! open-price cheese (PLU 300) and a shirt without a price: adding and merging, the line actions
//! with and without a manager, the cart discount, voids, split payment, cash rounding, saved
//! change, the balance tender, a zero total, `cart_stale`, approvals at posting, the receipt, the
//! crash copy and every key.

use std::sync::Arc;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use serde_json::json;
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_client::fake::{FakeService, TENDER_CASH, TENDER_QRIS};
use sultan_client::pricing::DEFAULT_PRICE_LIST;
use sultan_client::{
    ClientError, Customer, PriceEntryBody, PriceTier, PricingSettings, Problem, Product,
    ServerEvent,
};
use sultan_core::catalog::gtin_check_digit;
use sultan_ui::crash::CartFile;
use sultan_ui::{App, CartRow, Options, Runner, SellView, build_app};

const T0: i64 = 1_800_000_000_000;
const STANDARD: &str = "00000000-0000-7000-8000-000000000101";
const CHEESE: &str = "01900000-0000-7000-8000-000000000004";
const SHIRT: &str = "01900000-0000-7000-8000-000000000005";

fn id(n: u32) -> String {
    format!("01900000-0000-7000-8000-{n:012}")
}

fn ean13(body: &str) -> String {
    format!("{body}{}", gtin_check_digit(body).unwrap())
}

fn teh() -> String {
    ean13("899100000010")
}

fn shirt() -> String {
    ean13("899100000099")
}

fn catalog() -> FakeService {
    let fake = FakeService::default();
    let _ = fake.seed_pricing_demo();
    let product = |v: serde_json::Value| -> Product { serde_json::from_value(v).unwrap() };
    fake.seed_product(
        CHEESE,
        &product(json!({
            "code": "KEJU", "name": "Keju Edam", "type": "weighed", "tax_class_id": STANDARD,
            "open_price": true,
            "units": [{ "id": id(30), "name": "kg", "kind": "decimal", "factor_thousandths": 1000 }],
            "base_unit_id": id(30),
            "variants": [{ "id": id(31), "plu": 300 }],
        })),
    );
    fake.seed_product(
        SHIRT,
        &product(json!({
            "code": "KAOS", "name": "Kaos Polos", "type": "stock", "tax_class_id": STANDARD,
            "units": [{ "id": id(40), "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
            "base_unit_id": id(40),
            "variants": [{ "id": id(41) }],
            "barcodes": [{ "id": id(42), "variant_id": id(41), "unit_id": id(40), "code": shirt() }],
        })),
    );
    fake.seed_price_entry(
        DEFAULT_PRICE_LIST,
        &id(32),
        &PriceEntryBody {
            variant_id: id(31),
            unit_id: id(30),
            valid_from: None,
            valid_to: None,
            tiers: vec![PriceTier::fixed(1000, 80_000)],
        },
    );
    fake.seed_price_customer(
        sultan_client::fake::SEED_BUDI,
        sultan_client::fake::PriceCustomer {
            tier: Some(sultan_client::fake::SEED_GOLD.to_owned()),
            ..sultan_client::fake::PriceCustomer::default()
        },
    );
    fake
}

struct Till {
    app: App,
    fake: Arc<FakeService>,
    _runtime: tokio::runtime::Runtime,
}

fn till_with(fake: FakeService, language: &str, cart_file: Option<CartFile>) -> Till {
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
    let options = Options {
        register_label: "R1 · Kasir 1".to_owned(),
        language: Some(language.to_owned()),
        clock: Arc::new(|| T0),
        start: false,
        cart_file,
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
    t.typed("1234");
    t.press(&["Enter"]);
    t.typed("500000");
    t.press(&["Enter"]);
    assert_eq!(t.app.ui.get_screen(), "home");
    t
}

fn till() -> Till {
    till_with(catalog(), "en", None)
}

fn key_text(name: &str) -> SharedString {
    match name {
        "Enter" => Key::Return.into(),
        "Esc" => Key::Escape.into(),
        "Backspace" => Key::Backspace.into(),
        "Up" => Key::UpArrow.into(),
        "Down" => Key::DownArrow.into(),
        "Del" => Key::Delete.into(),
        "Control" => Key::Control.into(),
        "Shift" => Key::Shift.into(),
        "F1" => Key::F1.into(),
        "F2" => Key::F2.into(),
        "F3" => Key::F3.into(),
        "F4" => Key::F4.into(),
        "F5" => Key::F5.into(),
        "F6" => Key::F6.into(),
        "F7" => Key::F7.into(),
        "F8" => Key::F8.into(),
        "F9" => Key::F9.into(),
        "F10" => Key::F10.into(),
        "F11" => Key::F11.into(),
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

    fn typed(&self, text: &str) {
        for c in text.chars() {
            self.press(&[&c.to_string()]);
        }
    }

    fn scan(&self, code: &str) {
        self.typed(code);
        self.press(&["Enter"]);
    }

    fn sell(&self) -> SellView {
        self.app.ui.get_sell()
    }

    fn rows(&self) -> Vec<CartRow> {
        self.app.ui.get_cart_rows().iter().collect()
    }

    fn row(&self, i: usize) -> (String, String, String) {
        let r = &self.rows()[i];
        (
            format!("{} {}", r.qty, r.unit),
            r.price.to_string(),
            r.amount.to_string(),
        )
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

    fn click(&self, label: &str) {
        let found: Vec<_> = ElementHandle::find_by_accessible_label(&self.app.ui, label)
            .filter(|e| e.accessible_role() == Some(AccessibleRole::Button))
            .collect();
        assert_eq!(found.len(), 1, "exactly one button labelled {label:?}");
        found[0].mock_single_click(PointerEventButton::Left);
    }

    fn buttons(&self) -> Vec<String> {
        let ui = &self.app.ui;
        ui.get_actions()
            .iter()
            .chain(ui.get_actions_b().iter())
            .map(|c| format!("{} {}", c.keys, c.label))
            .collect()
    }

    fn chips(&self) -> Vec<String> {
        self.app
            .ui
            .get_chips()
            .iter()
            .map(|c| format!("{} {}", c.keys, c.label))
            .collect()
    }

    /// Budi (manager, PIN 9999, first in the list) approves the open override.
    fn budi_approves(&self) {
        assert!(
            self.app.ui.get_approval().open,
            "the override dialog is open"
        );
        self.typed("9999");
        self.press(&["Enter"]);
    }
}

#[test]
fn adding_merging_and_open_prices() {
    let t = till();
    assert!(t.sell().empty);
    t.scan(&teh());
    assert_eq!(
        t.row(0),
        ("1 pcs".into(), "Rp 5.000".into(), "Rp 5.000".into())
    );
    // The same item again adds to the line (D8); `3*` sets the quantity.
    t.scan(&teh());
    t.scan(&format!("3*{}", teh()));
    assert_eq!(t.rows().len(), 1);
    assert_eq!(t.row(0).0, "5 pcs");
    let s = t.sell();
    assert_eq!(
        (s.total.as_str(), s.subtotal.as_str(), s.items.as_str()),
        ("Rp 25.000", "Rp 25.000", "5")
    );
    assert!(!s.pricing, "the quote answered");
    assert!(t.rows()[0].last, "the changed line is highlighted");
    // Another unit is another line.
    t.scan("AQUA-BOX");
    assert_eq!(
        t.row(1),
        ("1 box".into(), "Rp 84.000".into(), "Rp 84.000".into())
    );

    // An open-price item asks for its price; open-price lines never merge.
    t.scan("300");
    let s = t.sell();
    assert_eq!(
        (s.modal.as_str(), s.modal_title.as_str()),
        ("entry", "open_price")
    );
    assert_eq!(s.modal_hint, "Rp 80.000", "the suggestion");
    t.typed("x12a345");
    t.press(&["Backspace"]);
    t.typed("5");
    assert_eq!(t.sell().modal_value, "12345");
    t.press(&["Enter"]);
    assert_eq!(t.row(2).1, "Rp 12.345");
    assert_eq!(t.rows()[2].flag, "open");
    t.scan("300");
    t.press(&["Esc"]);
    assert_eq!(t.rows().len(), 3, "Esc adds nothing");
    t.scan("300");
    t.typed("1000");
    t.press(&["Enter"]);
    assert_eq!(t.rows().len(), 4);

    // An item without a price is not added; a bad quantity or code is explained.
    t.scan(&shirt());
    assert_eq!(t.rows().len(), 4);
    assert_eq!(t.note(), "no_price_item");
    assert_eq!(t.message(), "Kaos Polos has no price and was not added.");
    t.scan(&format!("0*{}", teh()));
    assert_eq!(t.note(), "invalid_qty");
    t.scan("*");
    assert_eq!(t.note(), "invalid_qty");
    t.scan("NOPE");
    assert_eq!(t.note(), "item_not_found");
    // A failed quote is shown; the next change quotes again.
    t.fake
        .fail_on("quote", Problem::new(422, "invalid_reference", "x").into());
    t.scan(&teh());
    assert_eq!(t.note(), "price_reference");
    t.scan(&teh());
    assert_eq!(t.row(0).0, "7 pcs");
}

#[test]
#[allow(clippy::too_many_lines)] // every line action in one cart
fn line_actions() {
    let t = till();
    t.scan(&teh());
    t.scan(&teh());
    t.scan(&ean13("899100000020"));
    // Ctrl+↑ gives the lines the keyboard: the last changed line.
    t.press(&["Control", "Up"]);
    assert!(t.sell().lines_focus);
    assert!(t.rows()[1].selected);
    assert!(
        t.chips().contains(&"Del void_line".to_owned()),
        "{:?}",
        t.chips()
    );
    t.press(&["Up"]);
    t.press(&["Up"]);
    assert!(t.rows()[0].selected);
    t.press(&["+"]);
    assert_eq!(t.row(0).0, "3 pcs");
    t.press(&["-"]);
    t.press(&["-"]);
    assert_eq!(t.row(0).0, "1 pcs");
    t.press(&["-"]);
    assert_eq!(t.note(), "invalid_qty", "below one is a void, not a qty");
    // Enter edits the qty.
    t.press(&["Enter"]);
    assert_eq!(t.sell().modal_title, "qty");
    t.typed("4");
    t.press(&["Enter"]);
    assert_eq!(t.row(0).0, "4 pcs");
    t.press(&["Enter"]);
    t.typed("1,5");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "invalid_qty", "pcs are whole");
    t.press(&["Esc"]);

    // U changes the unit of Aqua (pcs → box).
    t.press(&["Down"]);
    t.press(&["U"]);
    let s = t.sell();
    assert_eq!((s.modal.as_str(), s.modal_cursor), ("units", 0));
    let units: Vec<String> = t
        .app
        .ui
        .get_sell_rows()
        .iter()
        .map(|r| r.label.into())
        .collect();
    assert_eq!(units, ["pcs", "box"]);
    t.press(&["Down"]);
    t.press(&["Enter"]);
    assert_eq!(
        t.row(1),
        ("1 box".into(), "Rp 84.000".into(), "Rp 84.000".into())
    );
    // Teh has one unit only.
    t.press(&["Up"]);
    t.press(&["u"]);
    assert_eq!(t.note(), "no_other_unit");

    // P: a manual price, struck through with the listed one.
    t.press(&["p"]);
    t.typed("4500");
    t.press(&["Enter"]);
    let r = &t.rows()[0];
    assert_eq!(
        (r.price.as_str(), r.was.as_str(), r.struck, r.flag.as_str()),
        ("Rp 4.500", "Rp 5.000", true, "override")
    );
    // D: a line discount (percent first; % switches to Rupiah).
    t.press(&["d"]);
    assert!(t.sell().modal_pct);
    t.press(&["%"]);
    assert!(!t.sell().modal_pct);
    t.typed("1000");
    t.press(&["Enter"]);
    assert_eq!(t.rows()[0].discount, "-Rp 1.000");
    assert_eq!(t.rows()[0].amount, "Rp 17.000");
    t.press(&["d"]);
    t.press(&["Enter"]);
    assert_eq!(t.rows()[0].discount, "", "empty clears it");
    t.press(&["d"]);
    t.typed("101");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "invalid_discount");
    t.press(&["Esc"]);

    // Ctrl+D: the cart discount, split over the lines.
    t.press(&["Control", "d"]);
    t.typed("10");
    t.press(&["Enter"]);
    let s = t.sell();
    assert_eq!(
        (s.discount.as_str(), s.total.as_str()),
        ("Rp 10.200", "Rp 91.800")
    );
    // Esc and Ctrl+↓ go back to the barcode field.
    t.press(&["Esc"]);
    assert!(!t.sell().lines_focus);
    t.press(&["Control", "Up"]);
    t.press(&["Control", "Down"]);
    assert!(!t.sell().lines_focus);

    // Del voids a line, with a reason (§3), through `POST /cart-voids`.
    t.press(&["Control", "Up"]);
    t.press(&["Del"]);
    let s = t.sell();
    assert_eq!(
        (s.modal.as_str(), s.modal_title.as_str()),
        ("reason", "reason_line")
    );
    t.press(&["Down"]);
    t.press(&["Enter"]);
    assert_eq!(t.rows().len(), 1);
    assert_eq!(t.note(), "line_voided");
    let voids = t.fake.cart_voids();
    assert_eq!((voids[0].kind.as_str(), voids[0].amount), ("line", 18_000));
    // `other` needs a text.
    t.press(&["Del"]);
    t.press(&["Up"]);
    t.press(&["Enter"]);
    assert_eq!(t.note(), "reason_text_required");
    t.typed("rusak 2");
    t.press(&["Backspace"]);
    assert_eq!(t.sell().modal_text, "rusak ");
    // A cashier without `sale.void_line` needs a manager.
    t.fake.require_line_void_override(true);
    t.press(&["Enter"]);
    let a = t.app.ui.get_approval();
    assert_eq!(
        (a.action.as_str(), a.value.as_str()),
        ("sale.void_line", "")
    );
    t.budi_approves();
    assert!(!t.app.ui.get_approval().open);
    assert!(t.sell().empty);
    assert_eq!(
        t.fake.cart_voids()[1].approved_by.as_deref(),
        Some("u-budi")
    );
    assert!(
        t.fake.calls_of("cart_void").len() >= 3,
        "{:?}",
        t.fake.calls_of("cart_void")
    );
}

#[test]
fn void_cart_and_close_session() {
    let t = till();
    t.scan(&teh());
    // Closing needs an empty cart.
    assert!(!t.buttons().contains(&"Ctrl+F10 close_session".to_owned()));
    t.press(&["Control", "F10"]);
    assert_eq!(t.app.ui.get_screen(), "home");
    // One button for both keys, in the bar; the action grid does not repeat it.
    assert_eq!(t.chips(), ["Ctrl+N / Ctrl+Del void_cart"]);
    assert!(!t.buttons().iter().any(|b| b.ends_with("void_cart")));
    t.click("Void cart");
    assert_eq!(t.sell().modal_title, "reason_cart");
    t.press(&["Esc"]);
    // Ctrl+N asks for a reason, then a manager (`sale.void`); Esc cancels.
    t.press(&["Control", "n"]);
    assert_eq!(t.sell().modal_title, "reason_cart");
    t.press(&["Enter"]);
    assert_eq!(t.app.ui.get_approval().action, "sale.void");
    t.press(&["Esc"]);
    assert_eq!(t.note(), "override_cancelled");
    assert_eq!(t.rows().len(), 1);
    // Ctrl+Del is the same.
    t.press(&["Control", "Del"]);
    t.press(&["Enter"]);
    t.budi_approves();
    assert!(t.sell().empty);
    assert_eq!(t.note(), "cart_voided");
    assert_eq!(t.fake.cart_voids()[0].kind, "cart");
    // Now the session can close.
    assert!(t.buttons().contains(&"Ctrl+F10 close_session".to_owned()));
    t.press(&["Control", "F10"]);
    assert_eq!(t.app.ui.get_screen(), "close");
}

#[test]
fn split_payment_and_receipt() {
    let t = till();
    for _ in 0..5 {
        t.scan(&teh());
    }
    // F9: the non-cash list (no balance without a customer).
    t.press(&["F9"]);
    let s = t.sell();
    assert!(s.pay_open);
    assert_eq!(
        (s.pay_step.as_str(), s.pay_total.as_str()),
        ("pick", "Rp 25.000")
    );
    let names: Vec<String> = t
        .app
        .ui
        .get_tender_rows()
        .iter()
        .map(|r| r.label.into())
        .collect();
    assert_eq!(names, ["Kartu debit", "QRIS"]);
    t.press(&["Down"]);
    assert_eq!(t.sell().pay_cursor, 1);
    t.press(&["Enter"]);
    assert_eq!(t.sell().pay_step, "amount");
    // More than the rest is refused for non-cash.
    t.typed("30000");
    t.press(&["Enter"]);
    t.press(&["Enter"]);
    assert_eq!(t.note(), "tender_over_rest");
    t.press(&["Esc"]);
    assert_eq!(t.sell().pay_step, "pick");
    t.press(&["Enter"]);
    t.typed("20000");
    t.press(&["Enter"]);
    assert_eq!(t.sell().pay_step, "reference");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "tender_reference_required");
    t.typed("AB12345");
    t.press(&["Enter"]);
    let s = t.sell();
    assert_eq!(
        (s.pay_step.as_str(), s.pay_rest.as_str()),
        ("cash", "Rp 5.000")
    );
    let paid: Vec<(String, String)> = t
        .app
        .ui
        .get_pay_rows()
        .iter()
        .map(|r| (r.label.into(), r.detail.into()))
        .collect();
    assert_eq!(paid, [("QRIS · ***2345".into(), "Rp 20.000".into())]);
    // Cash: 10 000 for the rest of 5 000.
    assert_eq!(t.sell().pay_due, "Rp 5.000");
    t.typed("10000");
    t.press(&["Enter"]);
    let s = t.sell();
    assert_eq!(
        (s.pay_step.as_str(), s.pay_change.as_str()),
        ("ready", "Rp 5.000")
    );
    assert!(
        !t.chips().iter().any(|c| c.starts_with("F8")),
        "no customer"
    );
    // Esc takes the cash back; F4 pays it again.
    t.press(&["Esc"]);
    assert_eq!(t.sell().pay_step, "cash");
    t.typed("10000");
    t.press(&["Enter"]);
    t.press(&["Enter"]);
    let s = t.sell();
    assert!(s.done && !s.pay_open);
    assert_eq!(
        (s.done_change.as_str(), s.done_total.as_str()),
        ("Rp 5.000", "Rp 25.000")
    );
    assert!(s.done_drawer, "cash opens the drawer");
    let receipt: Vec<String> = t
        .app
        .ui
        .get_receipt_lines()
        .iter()
        .map(Into::into)
        .collect();
    let total = "TOTAL                     25.000".to_owned(); // `core::sale::build_receipt`
    assert!(receipt.contains(&total), "{receipt:?}");
    assert!(t.fake.calls_of("receipt")[0].ends_with("lang=en"));
    let (body, sale) = &t.fake.posted_sales()[0];
    let tenders: Vec<(&str, i64, Option<&str>)> = body
        .tenders
        .iter()
        .map(|x| (x.tender_type_id.as_str(), x.amount, x.reference.as_deref()))
        .collect();
    assert_eq!(
        tenders,
        [
            (TENDER_QRIS, 20_000, Some("AB12345")),
            (TENDER_CASH, 10_000, None)
        ]
    );
    assert_eq!(sale.summary.id, body_id(&t));
    // Enter: a new empty cart.
    t.press(&["Enter"]);
    assert!(!t.sell().done && t.sell().empty);
}

fn body_id(t: &Till) -> String {
    t.fake.calls_of("post_sale")[0]
        .split(' ')
        .next()
        .unwrap()
        .to_owned()
}

fn attach_budi(t: &Till) {
    t.press(&["F3"]);
    t.typed("081234567");
    t.typed("89");
    t.press(&["Enter"]);
    t.press(&["Enter"]);
    assert_ne!(t.app.ui.get_home().customer, "");
}

#[test]
fn cash_rounding_saved_change_and_the_balance() {
    let fake = catalog();
    fake.set_pricing_settings(PricingSettings {
        cash_rounding: "nearest_100".to_owned(),
        ..PricingSettings::default()
    });
    let t = till_with(fake, "en", None);
    // Cash rounding: 12 345 in cash → 12 300 (−45).
    t.scan("300");
    t.typed("12345");
    t.press(&["Enter"]);
    t.press(&["F4"]);
    assert_eq!(t.sell().pay_due, "Rp 12.300");
    t.press(&["Enter"]);
    let s = t.sell();
    assert_eq!(
        (
            s.pay_step.as_str(),
            s.pay_rounding.as_str(),
            s.pay_change.as_str()
        ),
        ("ready", "-Rp 45", "Rp 0")
    );
    t.press(&["Enter"]);
    assert_eq!(t.fake.posted_sales()[0].1.summary.cash_rounding, -45);
    t.press(&["Enter"]);

    // Budi (Gold −5%) pays with his balance (2 200), then cash, and saves change.
    attach_budi(&t);
    t.scan(&teh());
    assert_eq!(t.sell().total, "Rp 4.750");
    t.press(&["F9"]);
    let names: Vec<String> = t
        .app
        .ui
        .get_tender_rows()
        .iter()
        .map(|r| r.label.into())
        .collect();
    assert_eq!(names, ["Kartu debit", "QRIS", "Saldo pelanggan"]);
    assert_eq!(t.sell().pay_balance, "Rp 2.200");
    t.app
        .dispatch(sultan_ui::controller::Msg::PickSell("tender", 99));
    t.press(&["Up"]);
    t.press(&["Enter"]);
    t.typed("3000");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "insufficient_balance");
    for _ in 0..4 {
        t.press(&["Backspace"]);
    }
    assert_eq!(t.sell().pay_due, "Rp 2.200");
    t.press(&["Enter"]);
    let s = t.sell();
    assert_eq!(
        (s.pay_rest.as_str(), s.pay_due.as_str()),
        ("Rp 2.550", "Rp 2.600")
    );
    t.typed("5000");
    t.press(&["Enter"]);
    assert_eq!(t.sell().pay_change, "Rp 2.400");
    assert!(t.chips().contains(&"F8 save_change".to_owned()));
    t.press(&["F8"]);
    t.typed("9999");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "invalid_amount", "more than the change");
    t.press(&["Esc"]);
    t.press(&["F8"]);
    t.typed("400");
    t.press(&["Enter"]);
    let s = t.sell();
    assert_eq!(
        (s.pay_saved.as_str(), s.pay_change.as_str()),
        ("Rp 400", "Rp 2.000")
    );
    t.press(&["Enter"]);
    let (body, sale) = &t.fake.posted_sales()[1];
    assert_eq!(body.save_change, Some(400));
    assert_eq!(
        body.customer_id.as_deref(),
        Some(sultan_client::fake::SEED_BUDI)
    );
    assert_eq!(sale.summary.change_given, 2_000);
    assert_eq!(t.sell().done_change, "Rp 2.000");
    t.press(&["Enter"]);
    assert_eq!(
        t.app.ui.get_home().customer,
        "",
        "a new sale starts without a customer"
    );
}

#[test]
fn zero_total_approvals_and_a_stale_cart() {
    let t = till();
    // 100% off needs Budi; a zero total posts without a tender.
    t.scan(&teh());
    t.press(&["Control", "Up"]);
    t.press(&["d"]);
    t.typed("100");
    t.press(&["Enter"]);
    t.press(&["Esc"]);
    assert_eq!(t.sell().total, "Rp 0");
    t.press(&["F4"]);
    assert_eq!(t.sell().pay_step, "ready");
    t.press(&["Enter"]);
    let a = t.app.ui.get_approval();
    assert_eq!(
        (a.action.as_str(), a.value.as_str(), a.limit.as_str()),
        ("sale.discount", "100%", "10%")
    );
    // Budi's own limit (20%) is too low for 100%.
    t.budi_approves();
    assert_eq!(t.app.ui.get_approval().note.code, "override_limit_exceeded");
    t.press(&["Esc"]);
    assert!(t.sell().pay_open, "still paying");
    t.press(&["Esc"]);
    t.press(&["Control", "Up"]);
    t.press(&["d"]);
    t.typed("15");
    t.press(&["Enter"]);
    // A manual price 15% below too: two approvals, sent together.
    t.press(&["p"]);
    t.typed("4250");
    t.press(&["Enter"]);
    t.press(&["Esc"]);
    t.press(&["F4"]);
    t.press(&["Enter"]);
    t.press(&["Enter"]);
    assert_eq!(t.app.ui.get_approval().action, "price.override");
    t.budi_approves();
    let a = t.app.ui.get_approval();
    // 638 of 4 250, rounded up to a basis point (§19 item 5).
    assert_eq!(
        (a.action.as_str(), a.value.as_str()),
        ("sale.discount", "15.02%")
    );
    t.budi_approves();
    assert!(t.sell().done, "{:?}", t.app.ui.get_approval().note);
    let posts = t.fake.calls_of("post_sale");
    assert_eq!(posts.len(), 4);
    assert_eq!(posts[3].matches("override-").count(), 2, "{posts:?}");
    t.press(&["Enter"]);

    // A price changed in between: the new total, pay again.
    t.scan(&teh());
    t.press(&["F4"]);
    t.press(&["Enter"]);
    let quotes = t.fake.calls_of("quote").len();
    t.fake.fail_next(
        Problem::new(409, "cart_stale", "x")
            .with("quote", json!({ "totals": { "total": 4_000 } }))
            .into(),
    );
    t.press(&["Enter"]);
    assert!(!t.sell().pay_open);
    assert_eq!(
        t.message(),
        "Prices changed. New total Rp 4.000: pay again."
    );
    assert_eq!(t.fake.calls_of("quote").len(), quotes + 1);
    // Another error stays in the panel.
    t.press(&["F4"]);
    t.press(&["Enter"]);
    t.fake.fail_next(ClientError::Network("down".into()));
    t.press(&["Enter"]);
    assert!(t.sell().pay_open);
    assert_eq!(t.note(), "network");
    t.press(&["Enter"]);
    assert!(t.sell().done);
}

#[test]
fn crash_copy_is_restored_for_the_same_session_only() {
    let dir = std::env::temp_dir().join(format!("sultan-sell-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let file = CartFile::new(dir.join("register.cart"));
    let t = till_with(catalog(), "en", Some(file.clone()));
    t.scan(&teh());
    t.scan(&teh());
    let saved = file.load().expect("written on every change");
    assert!(saved.contains("\"qty_thousandths\":2000"), "{saved}");
    // Lock and log in again: the same session gets its cart back.
    t.press(&["F12"]);
    t.typed("1234");
    t.press(&["Enter"]);
    assert_eq!(t.app.ui.get_screen(), "home");
    assert_eq!(t.row(0).0, "2 pcs");
    assert_eq!(t.message(), "The cart was restored (1 lines).");
    // Posting deletes it.
    t.press(&["F4"]);
    t.press(&["Enter"]);
    t.press(&["Enter"]);
    assert!(t.sell().done);
    assert_eq!(file.load(), None);
    t.press(&["Enter"]);
    // Another session's copy is ignored.
    t.scan(&teh());
    let other = file
        .load()
        .unwrap()
        .replace(&t.fake.session().unwrap().id, "another-session");
    file.save(&other).unwrap();
    t.press(&["F12"]);
    t.typed("1234");
    t.press(&["Enter"]);
    assert!(t.sell().empty);
    // A cart void deletes the file too.
    file.save("not json").unwrap();
    t.press(&["F12"]);
    t.typed("1234");
    t.press(&["Enter"]);
    assert!(t.sell().empty);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_key_and_button() {
    let t = till();
    assert_eq!(
        t.buttons(),
        [
            "F1 help",
            "F2 search",
            "F3 customer",
            "F5 drawer",
            "F6 receipts",
            "F7 price_check",
            "F10 returns",
            "Shift+F11 resume",
            "F12 lock",
            "Ctrl+F10 close_session"
        ]
    );
    // F1: the key list.
    t.press(&["F1"]);
    assert_eq!(t.sell().modal, "help");
    assert!(t.app.ui.get_sell_rows().row_count() > 10);
    t.press(&["F1"]);
    assert_eq!(t.sell().modal, "");
    t.click("Help");
    t.press(&["Esc"]);
    assert_eq!(t.sell().modal, "");
    // F2: search, then add the picked hit by its SKU.
    t.press(&["F2"]);
    t.press(&["Enter"]);
    assert_eq!(t.sell().modal, "search", "nothing typed, nothing searched");
    t.typed("zzz");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "nothing_found");
    for _ in 0..3 {
        t.press(&["Backspace"]);
    }
    t.typed("aqua");
    t.press(&["Enter"]);
    let hits = t.app.ui.get_sell_rows().row_count();
    assert!(hits >= 1);
    t.press(&["Down"]);
    t.press(&["Up"]);
    t.app
        .dispatch(sultan_ui::controller::Msg::PickSell("item", 0));
    t.press(&["Enter"]);
    assert_eq!(t.sell().modal, "");
    assert_eq!(t.rows().len(), 1);
    assert_eq!(t.fake.calls_of("product_search").len(), 2);
    // F5 asks the server (a cashier needs a manager: `drawer.open`); F6, F10, F11 and
    // Shift+F11 are tested in `after_screens.rs`.
    t.press(&["F5"]);
    t.budi_approves();
    assert_eq!(t.note(), "drawer_opened");
    assert!(t.buttons().contains(&"F11 hold".to_owned()));
    assert!(
        !t.buttons().contains(&"F10 returns".to_owned()),
        "a return needs an empty cart"
    );
    // F7: price check shows the item without adding it.
    t.press(&["F7"]);
    assert!(t.sell().check);
    t.scan(&teh());
    assert_eq!(t.rows().len(), 1);
    assert_eq!(t.app.ui.get_scan().name, "Teh Botol");
    t.press(&["F7"]);
    assert!(!t.sell().check);
    // A click selects a line.
    t.app
        .dispatch(sultan_ui::controller::Msg::PickSell("line", 0));
    assert!(t.sell().lines_focus);
    t.press(&["Esc"]);
    // price.changed quotes the cart again.
    let quotes = t.fake.calls_of("quote").len();
    t.fake.emit(ServerEvent {
        kind: "price.changed".into(),
        data: json!({ "variant_ids": null }),
    });
    t.app.pump();
    assert_eq!(t.fake.calls_of("quote").len(), quotes + 1);
    // Buttons: pay cash, then back.
    t.click("Pay cash");
    assert!(t.sell().pay_open);
    // The panel's own keys now (the buttons behind it are covered).
    t.press(&["F9"]);
    assert_eq!(t.sell().pay_step, "pick");
    t.app
        .dispatch(sultan_ui::controller::Msg::PickSell("tender", 1));
    t.press(&["F4"]);
    assert_eq!(t.sell().pay_step, "cash");
    t.press(&["Esc"]);
    assert!(!t.sell().pay_open);
    // F12 locks (the cart waits in memory only with a crash copy).
    t.press(&["F12"]);
    assert_eq!(t.app.ui.get_screen(), "lock");
}

#[test]
fn tax_invoice_and_name_search() {
    let fake = catalog();
    let phone = sultan_core::customer::Phone::parse("0812 1111 2222").unwrap();
    fake.seed_customer(Customer {
        id: "customer-pt".to_owned(),
        code: "PTMJ2345".to_owned(),
        display_code: "PTMJ-2345".to_owned(),
        name: "Andi".to_owned(),
        company_name: Some("PT Maju".to_owned()),
        phone: Some(phone.as_str().to_owned()),
        tax_id: Some("01.234.567.8-901.000".to_owned()),
        ..Customer::default()
    });
    let t = till_with(fake, "id", None);
    t.scan(&teh());
    t.press(&["F3"]);
    t.typed("081211112222");
    t.press(&["Enter"]);
    t.press(&["Enter"]);
    assert!(
        t.buttons().contains(&"Ctrl+T tax_invoice".to_owned()),
        "{:?}",
        t.buttons()
    );
    t.press(&["Control", "t"]);
    assert!(t.sell().tax_invoice);
    assert_eq!(t.message(), "Penjualan ini faktur pajak.");
    t.press(&["Control", "F3"]);
    assert!(!t.sell().tax_invoice, "detaching turns it off");
    // Indonesian labels.
    assert_eq!(t.app.ui.invoke_chip_label("pay_cash".into()), "Bayar tunai");
    t.press(&["F4"]);
    t.press(&["Enter"]);
    t.press(&["Enter"]);
    assert!(t.fake.calls_of("receipt")[0].ends_with("lang=id"));
    let receipt: Vec<String> = t
        .app
        .ui
        .get_receipt_lines()
        .iter()
        .map(Into::into)
        .collect();
    for line in [
        "TOTAL                      5.000",
        "Kembali                        0",
    ] {
        assert!(receipt.contains(&line.to_owned()), "{receipt:?}");
    }
    t.press(&["Enter"]);
    // Shift+F3: name search, for those who may search (Ani may not: the plain dialog).
    t.press(&["Shift", "F3"]);
    assert!(!t.app.ui.get_customer().open || !t.app.ui.get_customer().by_name);
}
