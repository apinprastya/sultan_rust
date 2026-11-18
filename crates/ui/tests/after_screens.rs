//! The register after the sale (spec 2.6, phase S5) driven headlessly against [`FakeService`]'s
//! pricing demo (Teh Botol pcs 5 000, Aqua pcs 3 500 and box 84 000): hold and resume (with
//! `cart_taken` and a dropped cart), the last receipts with reprints and post-voids (cash and
//! balance), returns by receipt (partial, write-off, the return window) and without a receipt
//! (above the limit), exchanges both ways, the purchase history with VOID marks, and the keys.

#![allow(clippy::too_many_lines)] // the flows read top to bottom

use std::sync::Arc;

use serde_json::json;
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_client::fake::FakeService;
use sultan_client::{ClientError, Problem, ServerEvent};
use sultan_core::catalog::gtin_check_digit;
use sultan_ui::{App, Options, Runner, SellRow, SellView, build_app};

const T0: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;

fn aqua() -> String {
    let body = "899100000020";
    format!("{body}{}", gtin_check_digit(body).unwrap())
}

fn teh() -> String {
    let body = "899100000010";
    format!("{body}{}", gtin_check_digit(body).unwrap())
}

struct Till {
    app: App,
    fake: Arc<FakeService>,
    _runtime: tokio::runtime::Runtime,
}

fn till_with(language: &str) -> Till {
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
    fake.seed_price_customer(
        sultan_client::fake::SEED_BUDI,
        sultan_client::fake::PriceCustomer {
            tier: Some(sultan_client::fake::SEED_GOLD.to_owned()),
            ..sultan_client::fake::PriceCustomer::default()
        },
    );
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
    till_with("en")
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
        "Del" => Key::Delete.into(),
        "Tab" => Key::Tab.into(),
        "Shift" => Key::Shift.into(),
        "F2" => Key::F2.into(),
        "F3" => Key::F3.into(),
        "F4" => Key::F4.into(),
        "F5" => Key::F5.into(),
        "F6" => Key::F6.into(),
        "F8" => Key::F8.into(),
        "F9" => Key::F9.into(),
        "F10" => Key::F10.into(),
        "F11" => Key::F11.into(),
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

    fn list(&self) -> Vec<SellRow> {
        self.app.ui.get_sell_rows().iter().collect()
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

    fn receipt(&self) -> String {
        self.app
            .ui
            .get_receipt_lines()
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
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

    /// Sells `codes` for exactly the cash due; returns the sale number.
    fn sell_cash(&self, codes: &[&str]) -> String {
        for c in codes {
            self.scan(c);
        }
        self.press(&["F4"]);
        self.press(&["Enter"]);
        self.press(&["Enter"]);
        let s = self.sell();
        assert!(s.done, "posted: {}", self.note());
        let number = s.done_number.to_string();
        self.press(&["Enter"]);
        number
    }

    fn attach_budi(&self) {
        self.press(&["F3"]);
        self.typed("08123456789");
        self.press(&["Enter"]);
        self.press(&["Enter"]);
        assert_ne!(self.app.ui.get_home().customer, "");
    }
}

#[test]
fn hold_resume_cart_taken_and_delete() {
    let t = till();
    t.scan(&teh());
    t.scan(&teh());
    assert!(t.buttons().contains(&"F11 hold".to_owned()));
    // F11: a note, then the cart is held and a new one starts.
    t.press(&["F11"]);
    assert_eq!(
        (t.sell().modal.as_str(), t.sell().modal_title.as_str()),
        ("note", "hold_note")
    );
    assert_eq!(t.chips(), ["Enter hold", "Esc cancel"]);
    t.typed("ambil nanti");
    t.press(&["Backspace"]);
    t.typed("i");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "cart_held");
    assert!(t.sell().empty);
    assert_eq!(t.fake.calls_of("hold_cart").len(), 1);
    // Shift+F11: the list (register, cashier, time; items, total, note).
    t.press(&["Shift", "F11"]);
    let s = t.sell();
    assert_eq!((s.modal.as_str(), s.modal_title.as_str()), ("list", "held"));
    let rows = t.list();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].label.starts_with("R1 · Kasir 1 · Ani · "),
        "{}",
        rows[0].label
    );
    assert_eq!(rows[0].detail, "2 item · Rp 10.000 · ambil nanti");
    assert_eq!(t.chips(), ["Enter resume", "Del delete_held", "Esc close"]);
    // `cart.held` from another register reloads the list.
    let lists = t.fake.calls_of("held_carts").len();
    t.fake.emit(ServerEvent {
        kind: "cart.held".into(),
        data: json!({}),
    });
    t.app.pump();
    assert_eq!(t.fake.calls_of("held_carts").len(), lists + 1);
    // Another register took it first: said, and the list loads again.
    t.fake.fail_on(
        "take_held_cart",
        Problem::new(409, "cart_taken", "taken").into(),
    );
    t.press(&["Enter"]);
    assert_eq!(t.note(), "cart_taken");
    assert_eq!(t.fake.calls_of("held_carts").len(), lists + 2);
    // Enter resumes it: the lines come back and are quoted again under a new cart ID.
    let quotes = t.fake.calls_of("quote").len();
    t.app
        .dispatch(sultan_ui::controller::Msg::PickSell("item", 0));
    t.press(&["Enter"]);
    assert_eq!(t.note(), "cart_resumed");
    assert_eq!(t.sell().modal, "");
    assert_eq!(t.sell().total, "Rp 10.000");
    assert_eq!(t.fake.calls_of("quote").len(), quotes + 1);
    // Shift+F11 with lines: hold them first, then the list opens.
    t.press(&["Shift", "F11"]);
    assert_eq!(t.note(), "hold_first");
    assert_eq!(t.sell().modal, "note");
    t.press(&["Enter"]);
    assert_eq!(t.sell().modal_title, "held");
    assert_eq!(
        t.list().len(),
        1,
        "the taken cart is gone, the new one is listed"
    );
    assert_eq!(t.list()[0].detail, "2 item · Rp 10.000");
    // Del drops it: a cashier needs a manager (`sale.void`).
    t.press(&["Del"]);
    assert_eq!(t.app.ui.get_approval().action, "sale.void");
    t.budi_approves();
    assert_eq!(t.note(), "held_deleted");
    assert!(t.list().is_empty());
    assert_eq!(t.chips(), ["Esc close"]);
    t.press(&["Esc"]);
    assert_eq!(t.sell().modal, "");
    // Esc in the note keeps the cart.
    t.scan(&teh());
    t.press(&["F11"]);
    t.press(&["Esc"]);
    assert_eq!(t.sell().items, "1");
}

#[test]
fn last_receipts_reprint_and_post_void() {
    let t = till();
    let first = t.sell_cash(&[&teh()]);
    let second = t.sell_cash(&[&aqua()]);
    // F6: this session's sales, newest first.
    t.press(&["F6"]);
    let s = t.sell();
    assert_eq!(
        (s.modal.as_str(), s.modal_title.as_str()),
        ("list", "receipts")
    );
    let rows = t.list();
    assert_eq!(
        rows.iter().map(|r| r.label.to_string()).collect::<Vec<_>>(),
        [second.clone(), first.clone()]
    );
    assert!(rows[0].detail.ends_with("Rp 3.500"), "{}", rows[0].detail);
    // Enter shows the newest receipt; R reprints it without a manager (the last sale).
    t.press(&["Enter"]);
    let s = t.sell();
    assert_eq!((s.modal.as_str(), s.modal_receipt), ("slip", true));
    assert!(t.receipt().contains(&second), "{}", t.receipt());
    assert_eq!(
        t.chips(),
        ["R reprint", "V post_void", "T start_return", "Esc back"]
    );
    t.press(&["R"]);
    assert_eq!(t.note(), "reprinted");
    assert!(t.receipt().contains("COPY"));
    // Esc goes back to the list; the older one needs `sale.reprint`.
    t.press(&["Esc"]);
    assert_eq!(t.sell().modal_title, "receipts");
    t.press(&["Down"]);
    t.press(&["Enter"]);
    assert!(t.receipt().contains(&first));
    t.press(&["r"]);
    assert_eq!(t.app.ui.get_approval().action, "sale.reprint");
    t.budi_approves();
    assert_eq!(t.note(), "reprinted");
    assert!(t.receipt().contains("COPY"));
    // V: the post-void asks a reason; `other` needs text. Cash only without a customer.
    t.press(&["v"]);
    let s = t.sell();
    assert_eq!(
        (
            s.modal.as_str(),
            s.modal_title.as_str(),
            s.modal_refund.as_str()
        ),
        ("reason", "post_void", "cash")
    );
    assert_eq!(t.chips(), ["Enter post_void", "Esc cancel"]);
    for _ in 0..3 {
        t.press(&["Down"]);
    }
    t.press(&["Enter"]);
    assert_eq!(t.note(), "reason_text_required");
    t.typed("salah input");
    t.press(&["Enter"]);
    assert_eq!(t.app.ui.get_approval().action, "sale.post_void");
    t.budi_approves();
    let s = t.sell();
    assert_eq!(t.note(), "sale_voided");
    assert_eq!(
        (s.modal.as_str(), s.modal_big.as_str()),
        ("slip", "Rp 5.000")
    );
    assert!(t.receipt().contains("VOID"));
    assert_eq!(
        t.chips(),
        ["R reprint", "Esc back"],
        "a voided sale is only reprinted"
    );
    t.press(&["Esc"]);
    assert!(t.list()[1].detail.contains("VOID"));
    // A typed number opens that receipt; an unknown one is said.
    t.typed("NOPE");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "receipt_not_found");
    for _ in 0..4 {
        t.press(&["Backspace"]);
    }
    t.typed(&second);
    t.press(&["Enter"]);
    assert_eq!(t.sell().modal, "slip");
    // A sale of another branch cannot be voided here.
    let id = t.fake.posted_sales()[1].1.summary.id.clone();
    t.fake.move_sale_to_branch(&id, "branch-2");
    t.press(&["v"]);
    t.press(&["Enter"]);
    assert_eq!(t.note(), "other_branch_sale");
    assert_eq!(
        t.message(),
        "This sale is not from this branch (or cannot be returned here)."
    );
    // A cancelled approval leaves the dialog.
    t.fake.move_sale_to_branch(&id, "branch-1");
    t.press(&["Enter"]);
    assert!(t.app.ui.get_approval().open);
    t.press(&["Esc"]);
    assert!(!t.app.ui.get_approval().open);
    assert_eq!(t.note(), "override_cancelled");
    t.press(&["Esc"]);
    t.press(&["Esc"]);
    t.press(&["Esc"]);
    assert_eq!(t.sell().modal, "");
}

#[test]
fn post_void_to_the_balance_and_purchase_history() {
    let t = till();
    t.attach_budi();
    let before = t.app.ui.get_home().customer_balance.to_string();
    let first = t.sell_cash(&[&teh()]);
    // A second sale paged history: eleven sales of Budi.
    for _ in 0..10 {
        t.attach_budi();
        t.sell_cash(&[&aqua()]);
    }
    t.press(&["F6"]);
    t.press(&["Up"]);
    t.press(&["Enter"]);
    assert!(t.receipt().contains(&first));
    t.press(&["v"]);
    assert_eq!(
        t.chips(),
        ["Enter post_void", "← / → refund_method", "Esc cancel"]
    );
    t.press(&["Right"]);
    assert_eq!(t.sell().modal_refund, "balance");
    t.app
        .dispatch(sultan_ui::controller::Msg::PickSell("item", 1));
    t.press(&["Enter"]);
    t.budi_approves();
    let s = t.sell();
    assert_eq!(
        (s.modal_big.as_str(), s.modal_hint.as_str()),
        ("Rp 0", "Rp 4.750")
    );
    assert_eq!(s.modal_refund, "balance");
    t.press(&["Esc"]);
    t.press(&["Esc"]);
    // F3 → F9: the purchase history, newest first, ten at a time, VOID marked.
    t.attach_budi();
    assert_ne!(t.app.ui.get_home().customer_balance.to_string(), before);
    t.press(&["F3"]);
    t.press(&["F9"]);
    let s = t.sell();
    assert_eq!(
        (s.modal_title.as_str(), s.modal_name.as_str()),
        ("history", "Budi Santoso")
    );
    assert_eq!(t.list().len(), 10);
    assert_eq!(
        t.chips(),
        ["Enter show_receipt", "F8 more_page", "Esc close"]
    );
    t.press(&["F8"]);
    let rows = t.list();
    assert_eq!(rows.len(), 11);
    assert_eq!(rows[10].label, first);
    assert!(rows[10].detail.contains("VOID"), "{}", rows[10].detail);
    assert_eq!(t.chips(), ["Enter show_receipt", "Esc close"]);
    // Enter shows a sale's receipt; T starts its return (the attached customer's cart is empty).
    t.press(&["Enter"]);
    assert_eq!(t.sell().modal, "slip");
    t.press(&["t"]);
    let s = t.sell();
    assert_eq!(s.modal_title, "return_lines");
    assert_eq!(
        s.modal_refund, "cash",
        "Tab offers the sale's customer's balance"
    );
    t.press(&["Tab"]);
    assert_eq!(t.sell().modal_refund, "balance");
    t.press(&["+"]);
    t.press(&["Enter"]);
    let s = t.sell();
    assert_eq!(
        (
            s.modal_title.as_str(),
            s.modal_big.as_str(),
            s.modal_hint.as_str()
        ),
        ("return_done", "Rp 0", "Rp 3.325")
    );
    t.press(&["Enter"]);
    // A sale with a return cannot be voided.
    t.press(&["F6"]);
    t.press(&["Enter"]);
    t.press(&["v"]);
    t.press(&["Enter"]);
    t.budi_approves();
    assert_eq!(t.note(), "has_returns");
}

#[test]
fn returns_by_receipt_window_and_without_receipt() {
    let t = till();
    let number = t.sell_cash(&[&format!("3*{}", teh())]);
    // F10: which sale? (no customer: no recent receipts)
    t.press(&["F10"]);
    let s = t.sell();
    assert_eq!(
        (
            s.modal_title.as_str(),
            s.modal_field,
            s.modal_placeholder.as_str()
        ),
        ("return_find", true, "receipt_number")
    );
    assert_eq!(t.chips(), ["Enter find", "F2 no_receipt", "Esc close"]);
    t.typed(&number);
    t.press(&["Enter"]);
    let s = t.sell();
    assert_eq!(s.modal_title, "return_lines");
    assert_eq!(t.list()[0].detail, "0 / 3 · Rp 0");
    assert_eq!(
        t.chips(),
        [
            "Q edit_qty",
            "+ more",
            "- less",
            "* return_all",
            "Del write_off",
            "Esc back"
        ]
    );
    // One of three, written off.
    t.press(&["+"]);
    assert_eq!(t.list()[0].detail, "1 / 3 · Rp 5.000");
    assert_eq!(t.sell().modal_big, "Rp 5.000");
    t.press(&["Del"]);
    assert_eq!(t.sell().modal_title, "write_off");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "reason_text_required");
    t.typed("rusak");
    t.press(&["Enter"]);
    assert_eq!(t.list()[0].detail, "1 / 3 · Rp 5.000 · ✎ rusak");
    t.press(&["Enter"]);
    let s = t.sell();
    assert_eq!(
        (s.modal_title.as_str(), s.modal_big.as_str()),
        ("return_done", "Rp 5.000")
    );
    assert_eq!(t.list()[0].detail, "1 · Rp 5.000 · ✎ rusak");
    t.press(&["Enter"]);
    assert_eq!(t.sell().modal, "");
    // 40 days later: the window needs a manager (`refund.with_receipt`).
    t.fake.set_now(T0 + 40 * DAY);
    t.press(&["F10"]);
    t.typed(&number);
    t.press(&["Enter"]);
    assert_eq!(t.list()[0].detail, "0 / 2 · Rp 0");
    t.press(&["*"]);
    assert_eq!(t.list()[0].detail, "2 / 2 · Rp 10.000");
    t.press(&["Enter"]);
    assert_eq!(t.app.ui.get_approval().action, "refund.with_receipt");
    t.budi_approves();
    assert_eq!(t.sell().modal_title, "return_done");
    t.press(&["Enter"]);
    // Nothing is left; the server's rule is said.
    t.press(&["F10"]);
    t.typed(&number);
    t.press(&["Enter"]);
    assert_eq!(t.list()[0].detail, "0 / 0 · Rp 0");
    t.press(&["Esc"]);
    assert_eq!(t.sell().modal_title, "return_find");
    t.fake
        .fail_on("sale", Problem::new(422, "return_qty_exceeded", "x").into());
    t.typed(&number);
    t.press(&["Enter"]);
    assert_eq!(t.note(), "return_qty_exceeded");
    // F2: without a receipt the items are scanned and quoted; above the limit a manager approves.
    t.press(&["F2"]);
    let s = t.sell();
    assert_eq!(
        (s.modal_title.as_str(), s.modal_placeholder.as_str()),
        ("return_no_receipt", "item_code")
    );
    t.typed("NOPE");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "item_not_found");
    t.typed(&teh());
    assert_eq!(t.chips()[0], "Enter add_item");
    t.press(&["Esc"]);
    assert_eq!(t.sell().modal_value, "", "Esc clears the typed code");
    t.typed(&teh());
    t.press(&["Enter"]);
    t.typed(&teh());
    t.press(&["Enter"]);
    assert_eq!(t.list()[0].detail, "2 pcs · Rp 10.000");
    t.press(&["-"]);
    assert_eq!(t.list()[0].detail, "1 pcs · Rp 5.000");
    t.press(&["Enter"]);
    let a = t.app.ui.get_approval();
    assert_eq!(
        (a.action.as_str(), a.value.as_str()),
        ("refund.no_receipt", "Rp 5.000")
    );
    t.budi_approves();
    assert_eq!(t.sell().modal_big, "Rp 5.000");
    t.press(&["Enter"]);
    assert_eq!(
        t.fake.calls_of("post_return").len(),
        5,
        "3 returns, 2 asked for approval"
    );
    // With lines in the cart, F10 is not offered.
    t.scan(&teh());
    assert!(!t.buttons().iter().any(|b| b.starts_with("F10")));
}

#[test]
fn exchanges_both_ways() {
    let t = till();
    let number = t.sell_cash(&[&teh()]);
    // The customer takes something dearer: the credit pays first, they pay the rest.
    t.press(&["F10"]);
    t.typed(&number);
    t.press(&["Enter"]);
    t.press(&["+"]);
    assert!(t.chips().contains(&"F9 exchange".to_owned()));
    t.press(&["F9"]);
    assert_eq!(t.note(), "exchange_started");
    assert_eq!(
        t.message(),
        "Exchange: credit Rp 5.000. Scan the new items, then pay."
    );
    assert_eq!(t.sell().exchange, "Rp 5.000");
    assert!(t.buttons().contains(&"F10 cancel_exchange".to_owned()));
    t.scan(&aqua());
    t.scan(&aqua());
    let s = t.sell();
    assert_eq!(
        (s.exchange_rest.as_str(), s.exchange_back.as_str()),
        ("Rp 2.000", ""),
        "the totals panel: the customer pays the rest"
    );
    t.press(&["F4"]);
    let s = t.sell();
    assert_eq!((s.pay_rest.as_str(), s.pay_back.as_str()), ("Rp 2.000", ""));
    let paid: Vec<SellRow> = t.app.ui.get_pay_rows().iter().collect();
    assert_eq!(
        (paid[0].label.as_str(), paid[0].detail.as_str()),
        ("exchange", "Rp 5.000")
    );
    // Esc does not take the credit back; the panel closes.
    t.press(&["Esc"]);
    assert!(!t.sell().pay_open);
    t.press(&["F4"]);
    t.press(&["Enter"]);
    t.press(&["Enter"]);
    let s = t.sell();
    assert!(s.done, "{}", t.note());
    assert_eq!(
        (s.done_total.as_str(), s.done_refund.as_str()),
        ("Rp 7.000", "")
    );
    t.press(&["Enter"]);
    assert_eq!(t.sell().exchange, "");
    let returns = t.fake.calls_of("post_return");
    assert_eq!(returns.len(), 1);

    // Something cheaper: the rest of the credit is paid back in cash.
    let number = t.sell_cash(&["AQUA-BOX"]);
    t.press(&["F10"]);
    t.typed(&number);
    t.press(&["Enter"]);
    t.press(&["*"]);
    t.press(&["F9"]);
    t.scan(&teh());
    let s = t.sell();
    assert_eq!(
        (
            s.exchange.as_str(),
            s.exchange_rest.as_str(),
            s.exchange_back.as_str(),
            s.exchange_balance
        ),
        ("Rp 84.000", "", "Rp 79.000", false),
        "the totals panel: what is paid back in cash"
    );
    t.press(&["F4"]);
    let s = t.sell();
    assert_eq!(s.pay_step, "ready", "the credit covers it");
    assert_eq!(
        (s.pay_back.as_str(), s.exchange.as_str()),
        ("Rp 79.000", "Rp 84.000"),
        "the pay-back shows before Enter"
    );
    let paid: Vec<SellRow> = t.app.ui.get_pay_rows().iter().collect();
    assert_eq!(paid[0].detail, "Rp 5.000", "what the credit covers");
    t.press(&["Enter"]);
    let s = t.sell();
    assert!(s.done);
    assert_eq!(s.done_refund, "Rp 79.000");
    assert!(s.done_drawer);
    t.press(&["Enter"]);

    // F10 during an exchange cancels it.
    t.press(&["F10"]);
    t.typed(&number);
    t.press(&["Enter"]);
    t.press(&["Esc"]);
    t.press(&["Esc"]);
    assert_eq!(t.sell().modal, "");
    let number = t.sell_cash(&[&teh()]);
    t.press(&["F10"]);
    t.typed(&number);
    t.press(&["Enter"]);
    t.press(&["+"]);
    t.press(&["F9"]);
    t.press(&["F10"]);
    assert_eq!(t.note(), "exchange_cancelled");
    assert_eq!(t.sell().exchange, "");
}

/// §19 item 68: a weighed line is returned by a typed quantity (3 decimals), capped at what is
/// left; the refunds of its returns sum to the line amount (`core::sale::line_refund`).
#[test]
fn weighed_lines_return_by_typed_quantity() {
    let t = till();
    let product: sultan_client::Product = serde_json::from_value(json!({
        "code": "KEJU", "name": "Keju", "type": "weighed",
        "tax_class_id": "00000000-0000-7000-8000-000000000101",
        "units": [{ "id": "01900000-0000-7000-8000-000000000030", "name": "kg",
                    "kind": "decimal", "factor_thousandths": 1000 }],
        "base_unit_id": "01900000-0000-7000-8000-000000000030",
        "variants": [{ "id": "01900000-0000-7000-8000-000000000031", "plu": 400 }],
    }))
    .unwrap();
    t.fake
        .seed_product("01900000-0000-7000-8000-000000000032", &product);
    t.fake.seed_price_entry(
        sultan_client::pricing::DEFAULT_PRICE_LIST,
        "01900000-0000-7000-8000-000000000033",
        &sultan_client::PriceEntryBody {
            variant_id: "01900000-0000-7000-8000-000000000031".to_owned(),
            unit_id: "01900000-0000-7000-8000-000000000030".to_owned(),
            valid_from: None,
            valid_to: None,
            tiers: vec![sultan_client::PriceTier::fixed(1000, 10_001)],
        },
    );
    // 1.250 kg at 10 001 = 12 501.
    let number = t.sell_cash(&["1.25*400"]);
    t.press(&["F10"]);
    t.typed(&number);
    t.press(&["Enter"]);
    assert_eq!(t.list()[0].detail, "0 / 1,250 · Rp 0");
    assert!(t.chips().contains(&"Q edit_qty".to_owned()));
    // A digit opens the quantity: 0,5 kg.
    t.typed("0,5");
    let s = t.sell();
    assert_eq!(
        (
            s.modal_title.as_str(),
            s.modal_value.as_str(),
            s.modal_name.as_str()
        ),
        ("return_qty", "0,5", "Keju · ≤ 1,250")
    );
    assert_eq!(t.chips(), ["Enter ok", "Esc cancel"]);
    t.press(&["Enter"]);
    assert_eq!(t.list()[0].detail, "0,500 / 1,250 · Rp 5.000");
    t.press(&["Enter"]);
    let s = t.sell();
    assert_eq!(s.modal_title, "return_done");
    assert!(
        s.modal_name.ends_with("· Rp 5.000"),
        "the fake's refund: {}",
        s.modal_name
    );
    t.press(&["Enter"]);
    // Then the rest: Q, more than is left is capped at 0,750 kg.
    t.press(&["F10"]);
    t.typed(&number);
    t.press(&["Enter"]);
    assert_eq!(t.list()[0].detail, "0 / 0,750 · Rp 0");
    t.press(&["Q"]);
    assert_eq!(t.sell().modal_value, "");
    t.typed("2");
    t.press(&["Esc"]);
    assert_eq!(
        t.sell().modal_title,
        "return_lines",
        "Esc drops the typed quantity"
    );
    t.typed("1,,9");
    t.press(&["Backspace"]);
    t.typed("0001");
    t.press(&["Enter"]);
    assert_eq!(t.note(), "invalid_qty", "at most 3 decimals");
    t.press(&["Esc"]);
    t.typed("2");
    t.press(&["Enter"]);
    assert_eq!(t.list()[0].detail, "0,750 / 0,750 · Rp 7.501");
    t.press(&["Enter"]);
    let s = t.sell();
    assert_eq!(s.modal_title, "return_done");
    assert!(
        s.modal_name.ends_with("· Rp 7.501"),
        "5 000 + 7 501 = 12 501: {}",
        s.modal_name
    );
    t.press(&["Enter"]);

    // A whole unit takes whole numbers only: no decimal comma, `0` takes the line out.
    let number = t.sell_cash(&[&format!("3*{}", teh())]);
    t.press(&["F10"]);
    t.typed(&number);
    t.press(&["Enter"]);
    t.typed("2,5");
    assert_eq!(t.sell().modal_value, "25");
    t.press(&["Enter"]);
    assert_eq!(
        t.list()[0].detail,
        "3 / 3 · Rp 15.000",
        "capped at what is left"
    );
    t.typed("0");
    t.press(&["Enter"]);
    assert_eq!(t.list()[0].detail, "0 / 3 · Rp 0");
}

#[test]
fn recent_receipts_start_a_return_and_errors_are_said() {
    let t = till();
    t.attach_budi();
    let number = t.sell_cash(&[&teh()]);
    t.attach_budi();
    t.press(&["F10"]);
    let rows = t.list();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, number);
    t.app
        .dispatch(sultan_ui::controller::Msg::PickSell("item", 0));
    t.press(&["Down"]);
    t.press(&["Enter"]);
    assert_eq!(t.sell().modal_title, "return_lines");
    t.app
        .dispatch(sultan_ui::controller::Msg::PickSell("item", 0));
    t.press(&["+"]);
    t.fake
        .fail_on("post_return", ClientError::Network("down".into()));
    t.press(&["Enter"]);
    assert_eq!(t.note(), "network");
    assert_eq!(t.sell().modal_title, "return_lines");
    // F5 fails without the network too.
    t.press(&["Esc"]);
    t.press(&["Esc"]);
    t.fake
        .fail_on("open_drawer", ClientError::Network("down".into()));
    t.press(&["F5"]);
    assert_eq!(t.note(), "network");
    // A failed history page is said.
    t.press(&["F3"]);
    t.fake.fail_on(
        "customer_history",
        Problem::new(403, "permission_denied", "x").into(),
    );
    t.press(&["F9"]);
    assert_eq!(t.note(), "permission_denied");
}

#[test]
fn indonesian_labels() {
    let t = till_with("id");
    t.scan(&teh());
    t.press(&["F11"]);
    assert_eq!(t.app.ui.invoke_chip_label("hold".into()), "Tahan");
    assert_eq!(
        t.app.ui.invoke_chip_label("write_off".into()),
        "Barang rusak"
    );
    t.press(&["Enter"]);
    assert_eq!(
        t.message(),
        "Keranjang ditahan. Shift+F11 untuk melanjutkan."
    );
}
