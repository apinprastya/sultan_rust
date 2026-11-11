//! Drives the back office's pricing screens (spec 2.3, phase R4) headlessly against
//! [`FakeService`], like `backoffice_customers.rs`: keys are window events, buttons are found by
//! their accessible label, text fields are set through the window's `edit` callback, and the
//! checks read the window's properties and the fake's pricing state.
//!
//! In the fake, `price.manage` goes with `catalog.manage`: `owner` and `budi` may write prices,
//! `kasir` reads only. [`FakeService::seed_pricing_demo`] adds Teh Botol (pcs 5 000, from 12
//! pcs 4 500), Aqua 600ml (pcs 3 500, box(24) without a price) and the Gold tier list (−5%).

use std::sync::Arc;
use std::time::Duration;

use chrono::{Days, NaiveDate};
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use serde_json::json;
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_client::fake::{FakeService, SEED_GOLD};
use sultan_client::pricing::DEFAULT_PRICE_LIST;
use sultan_client::{
    BackOffice, PriceList, PriceRule, PriceTier, PricingSettings, ProductPrices, ServerEvent,
};
use sultan_core::doc_number::BranchTimeZone;
use sultan_core::pricing::sale_date;
use sultan_core::time::Timestamp;
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
        "F7" => Key::F7.into(),
        other => other.into(),
    }
}

/// Today as the back office sees it (the branch's local date).
fn today() -> NaiveDate {
    sale_date(
        Timestamp::from_unix_millis(chrono::Utc::now().timestamp_millis()),
        BranchTimeZone::default(),
    )
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

    fn edit(&self, kind: &str, row: i32, field: &str, value: &str) {
        self.app
            .ui
            .invoke_edit(kind.into(), row, 0, field.into(), value.into());
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

    fn login(&self, user: &str) {
        self.edit("login", 0, "username", user);
        self.edit("login", 0, "password", &format!("{user}-pass-1"));
        self.app.ui.invoke_action("login".into());
        assert_eq!(self.screen(), "products");
    }

    fn token(&self) -> String {
        self.runtime
            .block_on(self.fake.password_login("owner", "owner-pass-1"))
            .unwrap()
            .token
    }

    fn lists(&self) -> Vec<PriceList> {
        let token = self.token();
        self.runtime
            .block_on(self.fake.price_lists(&token, true))
            .unwrap()
    }

    fn product_prices(&self, id: &str) -> ProductPrices {
        let token = self.token();
        self.runtime
            .block_on(self.fake.product_prices(&token, id))
            .unwrap()
    }

    fn list_names(&self) -> Vec<String> {
        self.app
            .ui
            .get_price_list_rows()
            .iter()
            .map(|r| r.name.to_string())
            .collect()
    }

    /// Opens the product named `name` from the product list.
    fn open(&self, name: &str) {
        let row = self
            .app
            .ui
            .get_rows()
            .iter()
            .position(|r| r.name == name)
            .unwrap();
        self.app
            .ui
            .invoke_pick("open".into(), i32::try_from(row).unwrap(), 0);
        assert_eq!(self.screen(), "form");
    }

    fn entry_tiers(&self) -> Vec<String> {
        self.app
            .ui
            .get_price_entry_rows()
            .iter()
            .map(|r| r.tiers.to_string())
            .collect()
    }
}

#[test]
fn branch_list_and_conflict() {
    let o = office_with(FakeService::default(), "en");
    o.login("owner");
    o.press(&["F7"]);
    assert_eq!(o.screen(), "price-lists");
    assert_eq!(o.list_names(), ["Default"]);
    assert_eq!(
        o.app.ui.get_price_read_only().code,
        "",
        "the owner may edit"
    );
    let editor = o.app.ui.get_price_list_editor();
    assert!(
        editor.is_default && !editor.is_new,
        "the default list is picked"
    );

    // The default list cannot be archived or switched off.
    o.ctrl_shift("a");
    assert_eq!(
        o.text(o.app.ui.get_price_list_editor().notice),
        "The default list cannot be archived, switched off or given a rule."
    );
    o.app
        .ui
        .invoke_toggle("price-list".into(), 0, "active".into(), false);
    assert!(o.app.ui.get_price_list_editor().active);

    // A branch list: the name follows the picked branch.
    o.ctrl("n");
    let editor = o.app.ui.get_price_list_editor();
    assert!(editor.is_new && !editor.is_default);
    assert_eq!(editor.kind, "branch");
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_price_list_editor().target_error),
        "Pick what the list applies to."
    );
    let targets: Vec<String> = o
        .app
        .ui
        .get_target_choices()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert_eq!(targets.len(), 3, "(pick) and two branches: {targets:?}");
    assert_eq!(targets[0], "(Pick…)");
    o.choose("price-list", 0, "target", 1);
    let branch_name = targets[1].clone();
    assert_eq!(o.app.ui.get_price_list_editor().name, branch_name.as_str());
    o.edit("price-list", 0, "name", "Bali prices");
    o.choose("price-list", 0, "rule", 2);
    o.edit("price-list", 0, "rule", "abc");
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_price_list_editor().rule_error),
        "Type a number, e.g. -5 for 5% off or -500 for Rp 500 off."
    );
    o.edit("price-list", 0, "rule", "+1.000");
    o.ctrl("s");
    assert_eq!(o.text(o.app.ui.get_price_list_editor().notice), "Saved.");
    assert_eq!(o.list_names(), ["Bali prices", "Default"]);
    let bali = o.lists().into_iter().find(|l| l.kind == "branch").unwrap();
    assert_eq!(bali.rule, Some(PriceRule::amount(1000)));
    let row = o.app.ui.get_price_list_rows().row_data(0).unwrap();
    assert_eq!(
        (row.target.as_str(), row.rule.as_str()),
        (branch_name.as_str(), "+Rp 1.000")
    );

    // A second active list for the same branch: price_list_conflict names the first.
    o.ctrl("n");
    o.choose("price-list", 0, "target", 1);
    o.edit("price-list", 0, "name", "Bali 2");
    o.click("Save");
    assert_eq!(
        o.text(o.app.ui.get_price_list_editor().notice),
        "Another active price list already applies to this target (Bali prices). Switch it off or \
         archive it first."
    );
    // Switched off it may exist.
    o.app
        .ui
        .invoke_toggle("price-list".into(), 0, "active".into(), false);
    o.ctrl("s");
    assert_eq!(o.list_names(), ["Bali 2", "Bali prices", "Default"]);
    assert!(!o.app.ui.get_price_list_rows().row_data(0).unwrap().active);

    // Archive asks first.
    o.press(&["Down"]);
    assert_eq!(o.app.ui.get_price_list_editor().name, "Bali prices");
    o.ctrl_shift("a");
    assert_eq!(o.app.ui.get_confirm(), "archive-price-list");
    assert_eq!(o.app.ui.get_confirm_name(), "Bali prices");
    o.press(&["Enter"]);
    assert_eq!(o.list_names(), ["Bali 2", "Default"]);
    assert_eq!(
        o.text(o.app.ui.get_price_list_editor().notice),
        "The price list Bali prices is archived. It no longer applies."
    );
    // Esc goes back to the products.
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "products");
}

#[test]
fn gold_tier_and_customer_lists() {
    let o = office_with(FakeService::default(), "en");
    o.login("owner");
    o.click("Pricing");
    o.ctrl("n");
    // A membership tier: Gold, −5% on everything.
    o.choose("price-list", 0, "kind", 2);
    assert_eq!(o.app.ui.get_price_list_editor().kind, "tier");
    let targets: Vec<String> = o
        .app
        .ui
        .get_target_choices()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert_eq!(targets, ["(Pick…)", "Gold", "Silver"]);
    o.choose("price-list", 0, "target", 1);
    o.choose("price-list", 0, "rule", 1);
    o.edit("price-list", 0, "rule", "-5");
    o.ctrl("s");
    let gold = o.lists().into_iter().find(|l| l.kind == "tier").unwrap();
    assert_eq!(
        (gold.name.as_str(), gold.target_id.as_deref(), gold.rule),
        ("Gold", Some(SEED_GOLD), Some(PriceRule::pct(-500)))
    );
    let row = o.app.ui.get_price_list_rows().row_data(0).unwrap();
    assert_eq!(
        (row.kind.as_str(), row.target.as_str(), row.rule.as_str()),
        ("default", "", ""),
        "the default list sorts first"
    );
    let row = o.app.ui.get_price_list_rows().row_data(1).unwrap();
    assert_eq!((row.target.as_str(), row.rule.as_str()), ("Gold", "-5%"));
    // −100% is refused by the rules.
    o.edit("price-list", 0, "rule", "-100");
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_price_list_editor().rule_error),
        "This rule is not allowed: the default list takes fixed prices only, and a discount must \
         be less than 100%."
    );

    // One customer: found by member code; the name follows the customer.
    o.ctrl("n");
    o.choose("price-list", 0, "kind", 3);
    o.edit("price-list", 0, "customer", "zzzz-zzzz");
    o.click("Find customer");
    assert_eq!(
        o.text(o.app.ui.get_price_list_editor().notice),
        "No active customer has this code or phone."
    );
    o.edit("price-list", 0, "customer", "k7qh-29xm");
    o.click("Find customer");
    let editor = o.app.ui.get_price_list_editor();
    assert_eq!(editor.name, "Budi Santoso");
    assert_eq!(editor.target_name, "Budi Santoso (K7QH-29XM)");
    o.ctrl("s");
    assert_eq!(o.text(o.app.ui.get_price_list_editor().notice), "Saved.");
    let rows = o.app.ui.get_price_list_rows();
    let budi = rows.iter().find(|r| r.kind == "customer").unwrap();
    assert_eq!(budi.target, "Budi Santoso (K7QH-29XM)");
    // Kind and target are fixed once created.
    o.choose("price-list", 0, "kind", 0);
    assert_eq!(o.app.ui.get_price_list_editor().kind, "customer");
}

#[test]
#[allow(clippy::too_many_lines)] // one flow: overlap, schedule, delete
fn product_prices_tab() {
    let fake = FakeService::default();
    let demo = fake.seed_pricing_demo();
    let o = office_with(fake, "en");
    o.login("owner");
    o.open("Teh Botol");
    o.ctrl("4");
    assert_eq!(o.app.ui.get_form().tab, 3);
    assert_eq!(o.entry_tiers(), ["1 → Rp 5.000  ·  12 → Rp 4.500"]);
    let lists: Vec<String> = o
        .app
        .ui
        .get_price_list_choices()
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(lists, ["Default · 1", "Gold · 0"]);
    let prices = o.app.ui.get_prices();
    assert_eq!(prices.list_kind, "default");
    assert!(
        prices.has_editor && !prices.editor_new,
        "the first entry is open"
    );
    assert_eq!(prices.cursor, 0);
    let row = o.app.ui.get_price_entry_rows().row_data(0).unwrap();
    assert_eq!(
        (
            row.unit.as_str(),
            row.status.as_str(),
            row.valid_from.as_str()
        ),
        ("pcs", "current", "")
    );

    // A second open-ended price overlaps the first.
    o.ctrl("n");
    assert!(o.app.ui.get_prices().editor_new);
    o.edit("price-tier", 0, "value", "6.000");
    o.press(&["Control", "Enter"]);
    assert_eq!(
        o.text(o.app.ui.get_prices().notice),
        "The dates overlap another price of the same variant and unit (… – …). Give that one a \
         last day first, or use “Schedule new price”."
    );
    assert_eq!(o.app.ui.get_prices().from_error.code, "dates_overlap");
    // Typing errors are caught before the call.
    o.edit("price-tier", 0, "min_qty", "0");
    o.edit("price-entry", 0, "valid_from", "2027-02-30");
    o.click("Save price");
    let tiers = o.app.ui.get_price_tier_rows();
    assert_eq!(
        o.text(tiers.row_data(0).unwrap().qty_error),
        "Type a quantity above zero."
    );
    assert_eq!(o.app.ui.get_prices().from_error.code, "invalid_date");
    o.click("Cancel");
    assert!(!o.app.ui.get_prices().has_editor);

    // Schedule a new price: the current one ends today, the new one starts tomorrow.
    o.press(&["Down"]);
    assert!(o.app.ui.get_prices().has_editor);
    o.ctrl_shift("n");
    let tomorrow = today() + Days::new(1);
    let prices = o.app.ui.get_prices();
    assert!(prices.scheduling && prices.editor_new);
    assert_eq!(prices.valid_from, tomorrow.to_string().as_str());
    o.edit("price-tier", 0, "value", "5500");
    o.edit("price-tier", 1, "value", "5000");
    o.press(&["Control", "Enter"]);
    assert_eq!(o.text(o.app.ui.get_prices().notice), "Saved.");
    let stored = o.product_prices(&demo.teh.id);
    let mut dates: Vec<(Option<String>, Option<String>, i64)> = stored
        .entries
        .iter()
        .map(|e| {
            (
                e.valid_from.clone(),
                e.valid_to.clone(),
                e.tiers[0].price.unwrap(),
            )
        })
        .collect();
    dates.sort();
    assert_eq!(
        dates,
        [
            (None, Some(today().to_string()), 5_000),
            (Some(tomorrow.to_string()), None, 5_500),
        ]
    );
    let statuses: Vec<String> = o
        .app
        .ui
        .get_price_entry_rows()
        .iter()
        .map(|r| r.status.to_string())
        .collect();
    assert_eq!(statuses, ["current", "scheduled"]);
    // Scheduling again from the new price before it starts: refused (no day in between).
    o.edit("price-entry", 0, "valid_from", &today().to_string());
    o.ctrl_shift("n");
    o.edit("price-entry", 0, "valid_from", &tomorrow.to_string());
    o.press(&["Control", "Enter"]);
    assert_eq!(o.app.ui.get_prices().from_error.code, "schedule_after");
    o.click("Cancel");

    // Delete the scheduled price (asks first).
    o.press(&["Down"]);
    assert_eq!(o.app.ui.get_prices().cursor, 1);
    o.ctrl_shift("d");
    assert_eq!(o.app.ui.get_confirm(), "delete-price-entry");
    o.press(&["Enter"]);
    assert_eq!(o.text(o.app.ui.get_prices().notice), "Deleted.");
    assert_eq!(o.product_prices(&demo.teh.id).entries.len(), 1);
}

#[test]
fn a_gold_price_with_tiers_for_the_box() {
    let fake = FakeService::default();
    let demo = fake.seed_pricing_demo();
    let o = office_with(fake, "en");
    o.login("owner");
    o.open("Aqua 600ml");
    o.click("Prices");
    o.ctrl("l");
    let prices = o.app.ui.get_prices();
    assert_eq!(
        (prices.list_kind.as_str(), prices.list_rule.as_str()),
        ("tier", "-5%")
    );
    assert!(!prices.has_editor);
    o.click("New price");
    let units: Vec<String> = o
        .app
        .ui
        .get_price_unit_choices()
        .iter()
        .map(|u| u.to_string())
        .collect();
    assert_eq!(units, ["pcs", "box"]);
    o.choose("price-entry", 0, "unit", 1);
    assert_eq!(o.app.ui.get_prices().unit_name, "box");
    o.edit("price-tier", 0, "value", "80.000");
    o.ctrl("t");
    assert_eq!(o.app.ui.get_price_tier_rows().row_count(), 2);
    o.edit("price-tier", 1, "min_qty", "2");
    o.choose("price-tier", 1, "mode", 1);
    o.edit("price-tier", 1, "value", "-10");
    // A third tier, removed again.
    o.click("Add tier");
    o.app.ui.invoke_pick("remove-tier".into(), 2, 0);
    assert_eq!(o.app.ui.get_price_tier_rows().row_count(), 2);
    o.press(&["Control", "Enter"]);
    assert_eq!(o.text(o.app.ui.get_prices().notice), "Saved.");
    let stored = o.product_prices(&demo.aqua.id);
    let gold = stored
        .entries
        .iter()
        .find(|e| e.list_id == demo.gold_list)
        .unwrap();
    assert_eq!(
        gold.tiers,
        [
            PriceTier::fixed(1000, 80_000),
            PriceTier::rule(2000, PriceRule::pct(-1000))
        ]
    );
    assert_eq!(
        o.entry_tiers(),
        ["1 → Rp 80.000  ·  2 → -10%"],
        "the Gold list's entries"
    );
    // The default list picker shows the counts.
    o.choose("prices", 0, "list", 0);
    assert_eq!(o.app.ui.get_prices().list_kind, "default");
    assert_eq!(
        o.app.ui.get_price_list_choices().row_data(1).unwrap(),
        "Gold · 1"
    );
    assert!(o.lists().iter().any(|l| l.id == DEFAULT_PRICE_LIST));
}

#[test]
fn tax_rates_and_settings() {
    let o = office_with(FakeService::default(), "en");
    o.login("owner");
    o.press(&["F7"]);
    o.ctrl("2");
    assert_eq!(o.screen(), "tax-rates");
    let classes: Vec<String> = o
        .app
        .ui
        .get_tax_class_rows()
        .iter()
        .map(|c| c.to_string())
        .collect();
    let standard = classes.iter().position(|c| c == "Standard").unwrap();
    o.app
        .ui
        .invoke_pick("tax-class".into(), i32::try_from(standard).unwrap(), 0);
    assert_eq!(o.app.ui.get_tax_rates().class_name, "Standard");
    let row = o.app.ui.get_tax_rate_rows().row_data(0).unwrap();
    assert_eq!(
        (
            row.valid_from.as_str(),
            row.rate.as_str(),
            row.status.as_str()
        ),
        ("2022-04-01", "11%", "current")
    );
    assert_eq!(
        o.app.ui.get_tax_rates().valid_from,
        today().to_string().as_str(),
        "today by default"
    );
    o.edit("tax-rate", 0, "rate", "101");
    o.ctrl("s");
    assert_eq!(
        o.text(o.app.ui.get_tax_rates().rate_error),
        "Type a rate between 0 and 100, e.g. 11 or 11.5."
    );
    // A future rate.
    o.edit("tax-rate", 0, "rate", "12");
    o.edit("tax-rate", 0, "valid_from", "2099-01-01");
    o.click("Add rate");
    assert_eq!(o.text(o.app.ui.get_tax_rates().notice), "Saved.");
    let rows: Vec<(String, String, String)> = o
        .app
        .ui
        .get_tax_rate_rows()
        .iter()
        .map(|r| (r.valid_from.into(), r.rate.into(), r.status.into()))
        .collect();
    assert_eq!(
        rows,
        [
            ("2099-01-01".into(), "12%".into(), "scheduled".into()),
            ("2022-04-01".into(), "11%".into(), "current".into()),
        ]
    );
    assert_eq!(o.app.ui.get_tax_rates().rate, "", "the field is cleared");

    // Settings: switching tax-inclusive off warns before saving.
    o.ctrl("3");
    assert_eq!(o.screen(), "pricing-settings");
    let s = o.app.ui.get_pricing_settings();
    assert!(s.tax_inclusive && s.tax_registered && s.can_edit);
    assert_eq!(s.rounding_index, 0);
    o.app
        .ui
        .invoke_toggle("pricing-settings".into(), 0, "tax_inclusive".into(), false);
    assert!(o.app.ui.get_pricing_settings().mode_changed);
    o.choose("pricing-settings", 0, "cash_rounding", 3);
    o.ctrl("s");
    assert_eq!(o.app.ui.get_confirm(), "tax-mode");
    assert_eq!(o.app.ui.get_confirm_name(), "exclusive");
    o.press(&["Esc"]);
    assert_eq!(o.app.ui.get_confirm(), "");
    o.ctrl("s");
    o.press(&["Enter"]);
    assert_eq!(o.text(o.app.ui.get_pricing_settings().notice), "Saved.");
    let token = o.token();
    let stored = o.runtime.block_on(o.fake.pricing_settings(&token)).unwrap();
    assert_eq!(
        stored,
        PricingSettings {
            tax_inclusive: false,
            tax_registered: true,
            cash_rounding: "nearest_500".into(),
        }
    );
    // Without a mode change it saves at once.
    o.app
        .ui
        .invoke_toggle("pricing-settings".into(), 0, "tax_registered".into(), false);
    o.click("Save");
    assert_eq!(o.app.ui.get_confirm(), "");
    assert!(
        !o.runtime
            .block_on(o.fake.pricing_settings(&token))
            .unwrap()
            .tax_registered
    );
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "price-lists");
}

#[test]
fn without_price_manage_everything_is_read_only() {
    let fake = FakeService::default();
    let _ = fake.seed_pricing_demo();
    let o = office_with(fake, "en");
    o.login("kasir");
    o.press(&["F7"]);
    assert_eq!(
        o.text(o.app.ui.get_price_read_only()),
        "Read-only: changing prices needs the permission price.manage. Ask the owner."
    );
    assert!(!o.app.ui.get_price_list_editor().can_edit);
    o.ctrl("n");
    assert_eq!(
        o.text(o.app.ui.get_price_list_editor().notice),
        "Not allowed: this needs the permission price.manage."
    );
    o.ctrl("s");
    o.ctrl("2");
    assert!(!o.app.ui.get_tax_rates().can_edit);
    o.ctrl("s");
    assert_eq!(
        o.app.ui.get_tax_rates().notice.code,
        "permission_denied_catalog"
    );
    o.ctrl("3");
    assert!(!o.app.ui.get_pricing_settings().can_edit);
    o.app
        .ui
        .invoke_toggle("pricing-settings".into(), 0, "tax_inclusive".into(), false);
    assert!(o.app.ui.get_pricing_settings().tax_inclusive, "unchanged");

    // The Prices tab shows the prices, but writes are refused.
    o.press(&["F2"]);
    o.open("Teh Botol");
    o.ctrl("4");
    assert_eq!(o.entry_tiers().len(), 1);
    assert!(!o.app.ui.get_prices().can_edit);
    o.ctrl("n");
    assert_eq!(
        o.text(o.app.ui.get_prices().notice),
        "Not allowed: this needs the permission price.manage."
    );
    assert!(o.fake.calls_of("put_price_entry").is_empty());
    assert!(o.fake.calls_of("put_price_list").is_empty());
}

#[test]
fn indonesian_texts_and_live_refresh() {
    let fake = FakeService::default();
    let demo = fake.seed_pricing_demo();
    let o = office_with(fake, "id");
    o.login("owner");
    o.press(&["F7"]);
    assert_eq!(o.list_names(), ["Default", "Gold"]);
    assert_eq!(
        o.app.ui.invoke_label("kind_tier".into()),
        "Tingkat keanggotaan"
    );
    // A list written elsewhere shows up after its `price.changed`.
    let token = o.token();
    o.runtime
        .block_on(o.fake.put_price_list(
            &token,
            "0190a000-0000-7000-8000-000000000999",
            &sultan_client::PriceListBody {
                kind: "group".into(),
                target_id: Some("group-grosir".into()),
                name: "Grosir".into(),
                active: true,
                rule: Some(PriceRule::amount(-500)),
                archived: false,
            },
        ))
        .unwrap();
    o.fake.emit(ServerEvent {
        kind: "price.changed".into(),
        data: json!({ "scope": "price_list", "variant_ids": null }),
    });
    o.app.pump();
    assert_eq!(o.list_names(), ["Default", "Grosir", "Gold"]);
    let grosir = o.app.ui.get_price_list_rows().row_data(1).unwrap();
    assert_eq!(
        (grosir.target.as_str(), grosir.rule.as_str()),
        ("Grosir", "-Rp 500")
    );

    // The open Prices tab reloads too.
    o.press(&["F2"]);
    o.open("Teh Botol");
    o.ctrl("4");
    assert_eq!(o.entry_tiers().len(), 1);
    let calls = o.fake.calls_of("product_prices").len();
    o.fake.emit(ServerEvent {
        kind: "price.changed".into(),
        data: json!({ "scope": "price_entry", "variant_ids": [demo.teh.variants[0].id] }),
    });
    o.app.pump();
    assert_eq!(o.fake.calls_of("product_prices").len(), calls + 1);
    assert_eq!(
        o.app
            .ui
            .invoke_label(o.app.ui.get_price_entry_rows().row_data(0).unwrap().status),
        "Berlaku"
    );
}
