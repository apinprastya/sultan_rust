//! Prices on the session home's barcode lookup (spec 2.3, phase R5), driven headlessly against
//! [`FakeService`]'s pricing demo (Teh Botol pcs 5 000, from 12 pcs 4 500; Aqua pcs 3 500 with
//! box(24) falling back to 84 000; a Gold tier list −5%) plus a weighed product at 12 990 per kg
//! and an open-price cheese: the store price, no price, scale labels, the attached customer's
//! price through `POST /pricing/quote`, its failures, and a refresh on `price.changed`.

use std::sync::Arc;

use serde_json::json;
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, LogicalSize, SharedString};
use sultan_client::fake::{
    FakeService, PriceCustomer, SEED_BUDI, SEED_GOLD, SEED_GROUP, SEED_SILVER, SEED_SITI,
};
use sultan_client::pricing::DEFAULT_PRICE_LIST;
use sultan_client::{
    BackOffice, ClientError, PriceEntryBody, PriceTier, Problem, Product, ServerEvent,
};
use sultan_core::catalog::gtin_check_digit;
use sultan_ui::{App, Options, Runner, ScanView, build_app};

const T0: i64 = 1_800_000_000_000;
const STANDARD: &str = "00000000-0000-7000-8000-000000000101";
const BEEF: &str = "01900000-0000-7000-8000-000000000003";
const CHEESE: &str = "01900000-0000-7000-8000-000000000004";
const SHIRT: &str = "01900000-0000-7000-8000-000000000005";

fn id(n: u32) -> String {
    format!("01900000-0000-7000-8000-{n:012}")
}

fn ean13(body: &str) -> String {
    format!("{body}{}", gtin_check_digit(body).unwrap())
}

/// Teh: `8991000000107`; Aqua: `8991000000206` (pcs) and `AQUA-BOX`.
fn teh() -> String {
    ean13("899100000010")
}

fn product(json: serde_json::Value) -> Product {
    serde_json::from_value(json).unwrap()
}

/// The pricing demo; Daging Sapi (weighed, PLU 200, 12 990 per kg); Keju Edam (weighed, PLU 300,
/// open price, 80 000 per kg as the suggestion); Kaos (pcs, no price).
fn priced() -> FakeService {
    let fake = FakeService::default();
    let _ = fake.seed_pricing_demo();
    fake.seed_product(
        BEEF,
        &product(json!({
            "code": "SAPI", "name": "Daging Sapi", "type": "weighed", "tax_class_id": STANDARD,
            "units": [{ "id": id(20), "name": "kg", "kind": "decimal", "factor_thousandths": 1000 }],
            "base_unit_id": id(20),
            "variants": [{ "id": id(21), "plu": 200 }],
        })),
    );
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
        })),
    );
    for (entry, variant, unit, price) in [(22, 21, 20, 12_990), (32, 31, 30, 80_000)] {
        fake.seed_price_entry(
            DEFAULT_PRICE_LIST,
            &id(entry),
            &PriceEntryBody {
                variant_id: id(variant),
                unit_id: id(unit),
                valid_from: None,
                valid_to: None,
                tiers: vec![PriceTier::fixed(1000, price)],
            },
        );
    }
    // Budi holds Gold (the demo's −5% list); Siti's group and tier have no lists.
    fake.seed_price_customer(
        SEED_BUDI,
        PriceCustomer {
            tier: Some(SEED_GOLD.to_owned()),
            ..PriceCustomer::default()
        },
    );
    fake.seed_price_customer(
        SEED_SITI,
        PriceCustomer {
            group: Some(SEED_GROUP.to_owned()),
            tier: Some(SEED_SILVER.to_owned()),
            ..PriceCustomer::default()
        },
    );
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
    let fake = Arc::new(priced());
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
        "F3" => Key::F3.into(),
        "F4" => Key::F4.into(),
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

    fn typed(&self, text: &str) {
        for c in text.chars() {
            self.press(&c.to_string());
        }
    }

    fn scan(&self, code: &str) -> ScanView {
        self.typed(code);
        self.press("Enter");
        self.view()
    }

    fn view(&self) -> ScanView {
        self.app.ui.get_scan()
    }

    /// The customer-price message, translated.
    fn cust_message(&self) -> String {
        self.app.ui.invoke_message(self.view().cust_note).into()
    }

    fn source(&self, view: &ScanView) -> String {
        self.app
            .ui
            .invoke_price_source(view.cust_source.clone(), view.cust_name.clone())
            .into()
    }

    /// Finds the customer with `phone` (F3) and attaches them (Enter).
    fn attach(&self, phone: &str) {
        self.press("F3");
        self.typed(phone);
        self.press("Enter");
        self.press("Enter");
        assert_ne!(self.app.ui.get_home().customer, "", "attached");
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
fn the_store_price_of_the_scanned_unit() {
    let t = till("en");
    let v = t.scan(&teh());
    assert_eq!(
        (v.price_kind.as_str(), v.price.as_str(), v.total.as_str()),
        ("unit", "Rp 5.000", "")
    );
    assert_eq!(v.cust_state, 0, "no customer: no quote");
    assert!(t.fake.calls_of("quote").is_empty());

    // Box(24) has no price of its own: 24 × 3 500.
    let v = t.scan("AQUA-BOX");
    assert_eq!(
        (v.unit.as_str(), v.price_kind.as_str(), v.price.as_str()),
        ("box", "unit", "Rp 84.000")
    );
    let v = t.scan(&ean13("899100000020"));
    assert_eq!((v.unit.as_str(), v.price.as_str()), ("pcs", "Rp 3.500"));

    // No list prices the shirt.
    let v = t.scan("KAOS");
    assert_eq!((v.price_kind.as_str(), v.price.as_str()), ("none", ""));

    // Open price: typed at sale, the list price is a suggestion.
    let v = t.scan("300");
    assert!(v.open_price);
    assert_eq!(
        (v.price_kind.as_str(), v.price.as_str()),
        ("open", "Rp 80.000")
    );
}

#[test]
fn scale_labels_price_by_weight_or_carry_a_final_amount() {
    let t = till("en");
    // 1.250 kg × 12 990 = 16 237.5 → 16 238.
    let v = t.scan(&ean13("210020001250"));
    assert_eq!(
        (v.price_kind.as_str(), v.qty.as_str(), v.price.as_str()),
        ("weight", "1,250 kg", "Rp 12.990")
    );
    assert_eq!(v.total, "Rp 16.238");
    // A price label: the printed 15 000 is final, its qty derived (15 000 / 12 990).
    let v = t.scan(&ean13("220020015000"));
    assert_eq!(
        (v.price_kind.as_str(), v.amount.as_str(), v.qty.as_str()),
        ("label", "Rp 15.000", "1,155 kg")
    );
    assert_eq!(v.price, "Rp 12.990");
    // Without a store price the qty stays empty and the amount still stands.
    let owner = t.owner();
    futures_now(
        t.fake
            .delete_price_entry(&owner, DEFAULT_PRICE_LIST, &id(22)),
    );
    let v = t.scan(&ean13("220020015000"));
    assert_eq!(
        (
            v.price_kind.as_str(),
            v.amount.as_str(),
            v.qty.as_str(),
            v.price.as_str()
        ),
        ("label", "Rp 15.000", "", "")
    );
    let v = t.scan(&ean13("210020001250"));
    assert_eq!((v.price_kind.as_str(), v.total.as_str()), ("none", ""));
}

#[test]
fn an_attached_customer_gets_their_price() {
    let t = till("en");
    // The item first, then the customer: the shown item is quoted on attach.
    let v = t.scan(&teh());
    assert_eq!(v.cust_state, 0);
    t.attach("0812-3456-789");
    let v = t.view();
    assert_eq!(
        (v.cust_state, v.cust_price.as_str(), v.cust_diff.as_str()),
        (2, "Rp 4.750", "\u{2212}5%")
    );
    assert_eq!(
        (v.cust_source.as_str(), v.cust_name.as_str()),
        ("tier", "Gold")
    );
    assert_eq!(t.source(&v), "Gold member price");
    assert_eq!(v.price, "Rp 5.000", "the store price stays");
    let quotes = t.fake.calls_of("quote");
    assert_eq!(quotes.len(), 1);
    assert!(
        quotes[0].contains("\"customer_id\":\"customer-budi\""),
        "{}",
        quotes[0]
    );
    assert!(
        quotes[0].contains("\"qty_thousandths\":1000"),
        "{}",
        quotes[0]
    );

    // Each scan is quoted: the box falls back to 24 × 3 325.
    let v = t.scan("AQUA-BOX");
    assert_eq!(
        (v.price.as_str(), v.cust_price.as_str()),
        ("Rp 84.000", "Rp 79.800")
    );
    // A weight label is quoted for its weight: 12 990 −5% = 12 340.5 → 12 341; × 1.250.
    let v = t.scan(&ean13("210020001250"));
    assert_eq!(
        (
            v.cust_price.as_str(),
            v.cust_total.as_str(),
            v.total.as_str()
        ),
        ("Rp 12.341", "Rp 15.426", "Rp 16.238")
    );
    assert!(t.fake.calls_of("quote")[2].contains("\"qty_thousandths\":1250"));
    // A price label is final and an open price is typed: never quoted.
    let before = t.fake.calls_of("quote").len();
    let v = t.scan(&ean13("220020015000"));
    assert_eq!((v.cust_state, v.amount.as_str()), (0, "Rp 15.000"));
    let v = t.scan("300");
    assert_eq!(v.cust_state, 0);
    assert_eq!(t.fake.calls_of("quote").len(), before);

    // F4 detaches: the customer price goes.
    t.scan(&teh());
    assert_eq!(t.view().cust_state, 2);
    t.press("F4");
    assert_eq!(t.view().cust_state, 0);
    assert_eq!(t.view().price, "Rp 5.000");
}

#[test]
fn a_customer_without_lists_pays_the_store_price() {
    let t = till("en");
    t.attach("0812-9876-5432");
    let v = t.scan(&teh());
    assert_eq!((v.cust_state, v.cust_diff.as_str()), (3, ""));
    assert_eq!(
        (v.cust_source.as_str(), t.source(&v).as_str()),
        ("default", "Default store price")
    );
}

#[test]
fn a_failed_quote_keeps_the_store_price() {
    let t = till("en");
    t.attach("0812-3456-789");
    // The whole quote is refused: the customer was merged away meanwhile.
    t.fake.fail_on(
        "quote",
        Problem::new(422, "invalid_reference", "x")
            .with("field", "customer_id")
            .into(),
    );
    let v = t.scan(&teh());
    assert_eq!((v.cust_state, v.price.as_str()), (4, "Rp 5.000"));
    assert_eq!(v.cust_note.code, "price_reference");
    assert_eq!(
        t.cust_message(),
        "This customer or item cannot be priced any more. Detach the customer and find them again."
    );
    // A line without a price: the quote's per-line `no_price`.
    let v = t.scan("KAOS");
    assert_eq!((v.cust_state, v.cust_note.code.as_str()), (4, "no_price"));
    assert_eq!(t.cust_message(), "No price is set for this item.");
    // The network is down.
    t.fake.fail_on("quote", ClientError::Network("down".into()));
    let v = t.scan(&teh());
    assert_eq!(
        (v.cust_note.code.as_str(), v.price.as_str()),
        ("network", "Rp 5.000")
    );
    // Any other refusal is shown with its code.
    t.fake
        .fail_on("quote", Problem::new(422, "amount_overflow", "x").into());
    assert_eq!(t.scan(&teh()).cust_note.code, "invalid_amount");
    t.fake
        .fail_on("quote", Problem::new(500, "internal", "x").into());
    let v = t.scan(&teh());
    assert_eq!(
        (v.cust_note.code.as_str(), v.cust_note.arg.as_str()),
        ("error", "internal")
    );
}

#[test]
fn indonesian_prices() {
    let t = till("id");
    t.attach("0812-3456-789");
    let v = t.scan(&teh());
    assert_eq!(t.source(&v), "Harga member Gold");
    let v = t.scan("KAOS");
    assert_eq!(t.cust_message(), "Barang ini belum punya harga.");
    assert_eq!(v.price_kind, "none");
}

#[test]
fn an_ended_login_during_a_quote_locks() {
    let t = till("en");
    t.attach("0812-3456-789");
    t.fake.fail_on(
        "quote",
        ClientError::Api(Problem::new(401, "invalid_session", "x")),
    );
    t.scan(&teh());
    assert_eq!(t.app.ui.get_screen(), "lock");
}

#[test]
fn price_changes_refresh_the_shown_item() {
    let t = till("en");
    t.attach("0812-3456-789");
    let v = t.scan(&teh());
    assert_eq!(
        (v.price.as_str(), v.cust_price.as_str()),
        ("Rp 5.000", "Rp 4.750")
    );
    let teh_variant = "0190a000-0000-7000-8000-000000000103";
    // Another variant's price changes: no new lookup.
    t.fake.emit(ServerEvent {
        kind: "price.changed".into(),
        data: json!({ "scope": "entry", "variant_ids": [id(21)] }),
    });
    t.app.pump();
    assert_eq!(t.fake.calls_of("lookup").len(), 1);
    // Teh goes up to 6 000 in the back office: looked up and quoted again.
    let owner = t.owner();
    futures_now(t.fake.put_price_entry(
        &owner,
        DEFAULT_PRICE_LIST,
        "0190a000-0000-7000-8000-000000000110",
        &PriceEntryBody {
            variant_id: teh_variant.to_owned(),
            unit_id: "0190a000-0000-7000-8000-000000000101".to_owned(),
            valid_from: None,
            valid_to: None,
            tiers: vec![PriceTier::fixed(1000, 6_000)],
        },
    ));
    t.app.pump();
    let v = t.view();
    assert_eq!(
        (v.price.as_str(), v.cust_price.as_str()),
        ("Rp 6.000", "Rp 5.700")
    );
    assert_eq!(t.fake.calls_of("lookup").len(), 2);
    assert_eq!(t.fake.calls_of("quote").len(), 2);
    // A list or settings change (no variant IDs) may change any price.
    t.fake.emit(ServerEvent {
        kind: "price.changed".into(),
        data: json!({ "scope": "settings", "variant_ids": null }),
    });
    t.app.pump();
    assert_eq!(t.fake.calls_of("lookup").len(), 3);
    // A failed code is not looked up again for a price change.
    t.scan("NOPE");
    t.fake.emit(ServerEvent {
        kind: "price.changed".into(),
        data: json!({ "scope": "settings", "variant_ids": null }),
    });
    t.app.pump();
    assert_eq!(t.fake.calls_of("lookup").len(), 4);
}
