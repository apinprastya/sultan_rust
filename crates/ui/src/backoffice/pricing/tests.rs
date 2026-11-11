use serde_json::json;
use sultan_client::{Me, Product, TaxClass};

use super::*;
use crate::backoffice::form::ProductForm;
use crate::backoffice::{FormState, Msg, Session};

#[test]
fn numbers_parse_like_a_cashier_types_them() {
    let pct = [
        ("-5", Some(-500)),
        ("−5", Some(-500)),
        ("2,5", Some(250)),
        ("+1.25", Some(125)),
        ("5 %", Some(500)),
        (".5", Some(50)),
        ("1.234", None),
        ("", None),
        ("x", None),
        ("1234567", None),
    ];
    for (text, want) in pct {
        assert_eq!(parse_pct(text), want, "{text:?}");
    }
    let amounts = [
        ("Rp 12.500", Some(12_500)),
        ("-500", Some(-500)),
        ("1,000", Some(1_000)),
        ("", None),
        ("1a", None),
        ("12345678901234", None),
    ];
    for (text, want) in amounts {
        assert_eq!(parse_amount(text), want, "{text:?}");
    }
    let qty = [
        ("12", Some(12_000)),
        ("0,5", Some(500)),
        (".25", Some(250)),
        ("1.2345", None),
        ("", None),
        ("-1", None),
    ];
    for (text, want) in qty {
        assert_eq!(parse_qty(text), want, "{text:?}");
    }
}

#[test]
fn texts_of_rules_tiers_and_dates() {
    assert_eq!(pct_text(-500), "-5%");
    assert_eq!(pct_text(250), "+2.5%");
    assert_eq!(rule_text(&PriceRule::amount(500)), "+Rp 500");
    assert_eq!(rule_text(&PriceRule::default()), "");
    assert_eq!(tier_text(&PriceTier::fixed(12_000, 4_500)), "12 → Rp 4.500");
    assert_eq!(
        tier_text(&PriceTier::rule(500, PriceRule::pct(-1000))),
        "0.5 → -10%"
    );
    let day = |d: &str| NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap();
    let today = day("2026-11-11");
    assert_eq!(status(None, None, today), "current");
    assert_eq!(status(Some("2026-11-12"), None, today), "scheduled");
    assert_eq!(status(None, Some("2026-11-10"), today), "ended");
    assert_eq!(
        status(Some("2026-11-11"), Some("2026-11-11"), today),
        "current"
    );
    assert_eq!(dates_text(Some("2026-12-01"), None), "2026-12-01 – …");
    // A tier's text fields come back as typed.
    let row = TierRow::of(&PriceTier::rule(1000, PriceRule::pct(-250)));
    assert_eq!((row.mode, row.value.as_str()), (RULE_PCT, "-2.5"));
    let row = TierRow::of(&PriceTier::rule(1000, PriceRule::amount(-500)));
    assert_eq!((row.mode, row.value.as_str()), (RULE_AMOUNT, "-500"));
    assert_eq!(rule_fields(None), (RULE_NONE, String::new()));
}

fn editor(tiers: &[(&str, usize, &str)]) -> EntryEditor {
    EntryEditor {
        id: "e".into(),
        list_id: "l".into(),
        is_new: true,
        variant: Some("v".into()),
        unit: Some("u".into()),
        valid_from: String::new(),
        valid_to: String::new(),
        tiers: tiers
            .iter()
            .map(|(q, m, v)| TierRow {
                min_qty: (*q).into(),
                mode: *m,
                value: (*v).into(),
            })
            .collect(),
        ends: None,
        issues: Vec::new(),
    }
}

#[test]
fn entry_body_checks_every_field() {
    let mut ok = editor(&[("1", 0, "5.000"), ("12", 1, "-5"), ("24", 2, "-500")]);
    ok.valid_from = "2026-12-1".into();
    let body = ok.body().unwrap();
    assert_eq!(body.valid_from.as_deref(), Some("2026-12-01"));
    assert_eq!(
        body.tiers,
        [
            PriceTier::fixed(1000, 5000),
            PriceTier::rule(12_000, PriceRule::pct(-500)),
            PriceTier::rule(24_000, PriceRule::amount(-500)),
        ]
    );
    let fields = |e: EntryEditor| -> Vec<(&'static str, Option<usize>, &'static str)> {
        e.body()
            .unwrap_err()
            .into_iter()
            .map(|i| (i.field, i.row, i.code))
            .collect()
    };
    let mut e = editor(&[("1", 0, "1")]);
    e.variant = None;
    e.unit = None;
    e.valid_from = "x".into();
    e.valid_to = "y".into();
    assert_eq!(
        fields(e),
        [
            ("variant", None, "pick_variant"),
            ("unit", None, "pick_unit"),
            ("valid_from", None, "invalid_date"),
            ("valid_to", None, "invalid_date"),
        ]
    );
    let mut e = editor(&[("1", 0, "1")]);
    e.valid_from = "2026-12-02".into();
    e.valid_to = "2026-12-01".into();
    assert_eq!(fields(e), [("valid_to", None, "dates_reversed")]);
    assert_eq!(fields(editor(&[])), [("tiers", None, "invalid_tiers")]);
    assert_eq!(
        fields(editor(&[("1", 0, "1"); 11]))[0],
        ("tiers", None, "invalid_tiers")
    );
    assert_eq!(
        fields(editor(&[
            ("1", 0, "1"),
            ("1.000", 0, "2"),
            ("0", 0, "-1"),
            ("3", 1, "x")
        ])),
        [
            ("min_qty", Some(1), "duplicate_min_qty"),
            ("min_qty", Some(2), "invalid_qty"),
            ("value", Some(2), "invalid_amount"),
            ("value", Some(3), "invalid_rule_value"),
        ]
    );
}

fn controller(permissions: &[&str]) -> Controller {
    let mut c = Controller::new("owner");
    c.session = Some(Session {
        token: "t".into(),
        me: Me {
            user_id: "u".into(),
            permissions_anywhere: permissions.iter().map(|s| (*s).to_owned()).collect(),
            ..Me::default()
        },
        display_name: "Owner".into(),
    });
    c.screen = Some(Screen::Products);
    c.pricing.today = NaiveDate::from_ymd_opt(2026, 11, 11);
    c
}

fn reply(c: &mut Controller, r: Reply) -> Vec<Effect> {
    c.update(Msg::Pricing(Box::new(r))).effects
}

fn act(c: &mut Controller, id: &str) -> Vec<Effect> {
    c.update(Msg::Action(id.into())).effects
}

fn calls(effects: &[Effect]) -> Vec<&Call> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Pricing { call, .. } => Some(call),
            _ => None,
        })
        .collect()
}

fn problem(status: u16, code: &str) -> Problem {
    Problem::new(status, code, "x")
}

fn list(id: &str, kind: &str, target: Option<&str>, name: &str) -> PriceList {
    PriceList {
        id: id.into(),
        kind: kind.into(),
        target_id: target.map(str::to_owned),
        name: name.into(),
        active: true,
        rule: None,
        archived: false,
        archived_at: None,
    }
}

fn entry(id: &str, from: Option<&str>, to: Option<&str>) -> PriceEntry {
    PriceEntry {
        id: id.into(),
        list_id: DEFAULT_PRICE_LIST.into(),
        variant_id: "v1".into(),
        unit_id: "u1".into(),
        valid_from: from.map(str::to_owned),
        valid_to: to.map(str::to_owned),
        tiers: vec![PriceTier::fixed(1000, 5000)],
    }
}

/// A controller with a stored one-variant product open on its Prices tab, and its prices
/// (the default list with `entries`, and a Gold list).
fn on_prices_tab(entries: Vec<PriceEntry>) -> Controller {
    let mut c = controller(&[PRICE_MANAGE]);
    let product: Product = serde_json::from_value(json!({
        "id": "p1", "code": "TEH", "name": "Teh", "type": "stock", "tax_class_id": "tc",
        "units": [{ "id": "u1", "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
        "base_unit_id": "u1", "variants": [{ "id": "v1" }], "barcodes": [],
    }))
    .unwrap();
    let form = ProductForm::from_product(&product, &[], false);
    c.form = Some(FormState::new(form));
    c.screen = Some(Screen::Form);
    let effects = act(&mut c, "tab-3");
    assert_eq!(calls(&effects), [&Call::ProductPrices("p1".into())]);
    reply(
        &mut c,
        Reply::ProductPrices(
            "p1".into(),
            Ok(ProductPrices {
                product_id: "p1".into(),
                lists: vec![
                    list(DEFAULT_PRICE_LIST, "default", None, "Default"),
                    list("gold", "tier", Some("tier-gold"), "Gold"),
                ],
                entries,
            }),
        ),
    );
    c
}

fn pp(c: &Controller) -> &ProductPricing {
    c.pricing.product.as_ref().unwrap()
}

#[test]
fn schedule_rolls_the_old_end_back_when_the_new_price_is_refused() {
    let mut c = on_prices_tab(vec![entry("old", Some("2026-01-01"), None)]);
    assert_eq!(pp(&c).editor.as_ref().unwrap().id, "old");
    assert!(act(&mut c, "price-schedule").is_empty());
    let e = pp(&c).editor.as_ref().unwrap();
    assert_eq!(
        (e.valid_from.as_str(), e.ends.as_deref()),
        ("2026-11-12", Some("old"))
    );
    let effects = act(&mut c, "price-save");
    let [Call::PutEntry { id, body, .. }] = calls(&effects)[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(
        (id.as_str(), body.valid_to.as_deref()),
        ("old", Some("2026-11-11"))
    );
    assert!(act(&mut c, "price-save").is_empty(), "one save at a time");
    // The old entry ended; now the new one, which the server refuses.
    let ended = PriceEntry {
        valid_to: Some("2026-11-11".into()),
        ..entry("old", Some("2026-01-01"), None)
    };
    let effects = reply(&mut c, Reply::EntrySaved(Ok(ended)));
    let [Call::PutEntry { id, body, .. }] = calls(&effects)[..] else {
        panic!("{effects:?}");
    };
    assert_ne!(id, "old");
    assert_eq!(body.valid_from.as_deref(), Some("2026-11-12"));
    let refused = problem(409, "price_overlap").with("entry_id", "old");
    let effects = reply(&mut c, Reply::EntrySaved(Err(refused.into())));
    let [Call::PutEntry { id, body, .. }] = calls(&effects)[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(
        (id.as_str(), body.valid_to.as_deref()),
        ("old", None),
        "rolled back"
    );
    assert_eq!(pp(&c).notice.as_ref().unwrap().code, "price_overlap");
    assert_eq!(pp(&c).notice.as_ref().unwrap().arg, "2026-01-01 – …");
    // The rollback answered: the error stays, the prices reload.
    let effects = reply(
        &mut c,
        Reply::EntrySaved(Ok(entry("old", Some("2026-01-01"), None))),
    );
    assert_eq!(calls(&effects), [&Call::ProductPrices("p1".into())]);
    assert!(!pp(&c).saving);
    assert_eq!(pp(&c).notice.as_ref().unwrap().code, "price_overlap");

    // A refused first step leaves nothing to roll back.
    act(&mut c, "price-schedule");
    act(&mut c, "price-save");
    let effects = reply(
        &mut c,
        Reply::EntrySaved(Err(problem(422, "invalid_rule").into())),
    );
    assert!(effects.is_empty());
    assert_eq!(pp(&c).notice.as_ref().unwrap().code, "invalid_rule");
    assert!(pp(&c).rollback.is_none() && !pp(&c).saving);
}

#[test]
fn entry_errors_go_to_their_fields() {
    let mut c = on_prices_tab(vec![entry("a", None, None)]);
    let cases = [
        (
            problem(422, "invalid_field").with("field", "valid_from"),
            Some(("valid_from", "invalid_date")),
        ),
        (
            problem(422, "invalid_field").with("field", "valid_to"),
            Some(("valid_to", "dates_reversed")),
        ),
        (
            problem(422, "invalid_field").with("field", "tiers.min_qty_thousandths"),
            Some(("tiers", "invalid_tier_qty")),
        ),
        (
            problem(422, "invalid_field").with("field", "tiers.price"),
            Some(("tiers", "invalid_tier_price")),
        ),
        (
            problem(422, "invalid_field").with("field", "tiers"),
            Some(("tiers", "invalid_tiers")),
        ),
        (problem(422, "invalid_field").with("field", "list_id"), None),
        (problem(422, "invalid_reference"), None),
    ];
    for (p, want) in cases {
        assert_eq!(calls(&act(&mut c, "price-save")).len(), 1);
        reply(&mut c, Reply::EntrySaved(Err(p.clone().into())));
        let e = pp(&c).editor.as_ref().unwrap();
        let got = e.issues.first().map(|i| (i.field, i.code));
        assert_eq!(got, want, "{p:?}");
    }
    assert_eq!(pp(&c).notice.as_ref().unwrap().code, "price_reference_gone");
    // Deleting: refused, then done.
    act(&mut c, "price-delete");
    assert_eq!(c.confirm, Some(Confirm::PriceEntryDelete));
    let effects = act(&mut c, "confirm-yes");
    assert_eq!(
        calls(&effects),
        [&Call::DeleteEntry {
            list: DEFAULT_PRICE_LIST.into(),
            id: "a".into()
        }]
    );
    reply(
        &mut c,
        Reply::EntryDeleted(Err(problem(409, "in_use").into())),
    );
    assert_eq!(pp(&c).notice.as_ref().unwrap().code, "in_use");
    // A new entry cannot be deleted or scheduled from.
    act(&mut c, "price-new");
    act(&mut c, "price-delete");
    assert_eq!(pp(&c).notice.as_ref().unwrap().code, "pick_entry_first");
    act(&mut c, "price-schedule");
    assert_eq!(pp(&c).notice.as_ref().unwrap().code, "pick_entry_first");
}

#[test]
fn tiers_lists_and_keys_of_the_prices_tab() {
    let mut c = on_prices_tab(vec![
        entry("a", None, None),
        entry("b", Some("2027-01-01"), None),
    ]);
    let press = |c: &mut Controller, text: &str, ctrl: bool, shift: bool| {
        c.update(Msg::Capture(Press::parse(text, ctrl, shift)))
            .handled
    };
    assert!(press(&mut c, "\u{F701}", false, false));
    assert_eq!(pp(&c).cursor, 1);
    assert!(press(&mut c, "t", true, false));
    assert_eq!(pp(&c).editor.as_ref().unwrap().tiers.len(), 2);
    for _ in 0..9 {
        act(&mut c, "price-add-tier");
    }
    assert_eq!(pp(&c).editor.as_ref().unwrap().tiers.len(), MAX_TIERS);
    assert_eq!(pp(&c).notice.as_ref().unwrap().code, "invalid_tiers");
    for _ in 0..MAX_TIERS {
        c.update(Msg::Pick {
            kind: "remove-tier".into(),
            row: 0,
            sub: 0,
        });
    }
    assert_eq!(pp(&c).editor.as_ref().unwrap().tiers.len(), 1, "one stays");
    c.update(Msg::Choose {
        kind: "price-tier".into(),
        row: 0,
        field: "mode".into(),
        index: RULE_AMOUNT,
    });
    assert_eq!(pp(&c).editor.as_ref().unwrap().tiers[0].mode, RULE_AMOUNT);
    // Ctrl+L: the next list (Gold, empty), then back to the default list.
    assert!(press(&mut c, "l", true, false));
    assert_eq!(pp(&c).list_id, "gold");
    assert!(pp(&c).editor.is_none());
    assert!(press(&mut c, "l", true, false));
    assert_eq!(pp(&c).list_id, DEFAULT_PRICE_LIST);
    // Ctrl+N new, Esc leaves the form.
    assert!(press(&mut c, "n", true, false));
    assert!(pp(&c).editor.as_ref().unwrap().is_new);
    act(&mut c, "price-cancel");
    assert!(pp(&c).editor.is_none());
    // A `price.changed` reloads the tab.
    let event = sultan_client::ServerEvent {
        kind: "price.changed".into(),
        data: json!({}),
    };
    let generation = c.generation;
    let effects = c.update(Msg::Event(generation, event)).effects;
    assert_eq!(calls(&effects), [&Call::ProductPrices("p1".into())]);
    assert!(press(&mut c, "\u{1b}", false, false));
    assert_eq!(c.screen(), Screen::Products);
    assert!(c.pricing.product.is_none());
}

#[test]
fn a_new_product_has_no_prices_until_saved() {
    let mut c = controller(&[PRICE_MANAGE]);
    c.form = Some(FormState::new(ProductForm::new_product("tc")));
    c.screen = Some(Screen::Form);
    assert!(act(&mut c, "tab-3").is_empty());
    assert!(c.pricing.product.is_none());
    assert!(c.prices_after_save().is_empty(), "still new");
    if let Some(f) = &mut c.form {
        f.form.is_new = false;
    }
    assert_eq!(calls(&c.prices_after_save()).len(), 1);
    assert!(c.prices_after_save().is_empty(), "already loaded");
}

#[test]
fn list_editor_errors_and_customer_targets() {
    let mut c = controller(&[PRICE_MANAGE]);
    let effects = act(&mut c, "nav-prices");
    assert_eq!(c.screen(), Screen::PriceLists);
    assert_eq!(calls(&effects), [&Call::Lists]);
    reply(
        &mut c,
        Reply::Lists(Ok(vec![
            list("c1", "customer", Some("cust-1"), "Budi"),
            list(DEFAULT_PRICE_LIST, "default", None, "Default"),
        ])),
    );
    // The customer list's target is looked up for its name.
    let effects = reply(
        &mut c,
        Reply::Lists(Ok(vec![list("c1", "customer", Some("cust-1"), "Budi")])),
    );
    assert_eq!(calls(&effects), [&Call::Customer("cust-1".into())]);
    let budi = Customer {
        id: "cust-1".into(),
        name: "Budi".into(),
        display_code: "K7QH-29XM".into(),
        ..Customer::default()
    };
    reply(&mut c, Reply::Customer("cust-1".into(), Ok(budi.clone())));
    assert_eq!(
        c.target_name("customer", Some("cust-1")),
        "Budi (K7QH-29XM)"
    );
    assert_eq!(c.target_name("tier", Some("gone")), "gone");
    assert_eq!(c.target_name("default", None), "");
    // An existing list: find-customer does nothing; kind and target stay.
    assert!(act(&mut c, "price-find-customer").is_empty());

    // A new customer list: an answer for older text is ignored.
    act(&mut c, "price-list-new");
    c.update(Msg::Choose {
        kind: "price-list".into(),
        row: 0,
        field: "kind".into(),
        index: 3,
    });
    c.update(Msg::Edit {
        kind: "price-list".into(),
        row: 0,
        sub: 0,
        field: "customer".into(),
        value: "08123".into(),
    });
    let effects = act(&mut c, "price-find-customer");
    assert_eq!(calls(&effects), [&Call::FindCustomer("08123".into())]);
    reply(&mut c, Reply::Found("0812".into(), Ok(budi.clone())));
    assert!(c.pricing.editor.target.is_none());
    reply(
        &mut c,
        Reply::Found("08123".into(), Err(problem(404, "not_found").into())),
    );
    assert_eq!(
        c.pricing.editor.notice.as_ref().unwrap().code,
        "partner_not_found"
    );
    reply(&mut c, Reply::Found("08123".into(), Ok(budi)));
    assert_eq!(c.pricing.editor.target.as_deref(), Some("cust-1"));
    assert_eq!(c.pricing.editor.name, "Budi");

    // Server errors.
    let cases = [
        (problem(422, "invalid_reference"), "price_target_gone", None),
        (
            problem(422, "invalid_field").with("field", "name"),
            "fix_errors",
            Some("name"),
        ),
        (problem(422, "invalid_rule"), "fix_errors", Some("rule")),
        (problem(500, "boom"), "error", None),
    ];
    for (p, code, field) in cases {
        assert_eq!(calls(&act(&mut c, "price-list-save")).len(), 1);
        assert!(
            act(&mut c, "price-list-save").is_empty(),
            "one save at a time"
        );
        reply(&mut c, Reply::ListSaved(false, Err(p.into())));
        let e = &c.pricing.editor;
        assert_eq!(e.notice.as_ref().unwrap().code, code);
        assert_eq!(e.issues.first().map(|i| i.field), field);
    }
    reply(&mut c, Reply::Lists(Err(problem(500, "boom").into())));
    assert_eq!(c.pricing.editor.notice.as_ref().unwrap().code, "error");
    // The name is required.
    c.update(Msg::Edit {
        kind: "price-list".into(),
        row: 0,
        sub: 0,
        field: "name".into(),
        value: " ".into(),
    });
    assert!(act(&mut c, "price-list-save").is_empty());
    assert_eq!(
        c.pricing.editor.issue("name").unwrap().code,
        "invalid_list_name"
    );
}

#[test]
#[allow(clippy::too_many_lines)] // one flow across the three screens
fn tax_rates_settings_and_permissions() {
    let mut c = controller(&[PRICE_MANAGE]);
    c.reference.tax_classes = vec![
        TaxClass {
            id: "std".into(),
            name: "Standard".into(),
            archived: false,
        },
        TaxClass {
            id: "ex".into(),
            name: "Exempt".into(),
            archived: false,
        },
    ];
    c.screen = Some(Screen::PriceLists);
    let effects = act(&mut c, "tab-tax-rates");
    assert_eq!(calls(&effects), [&Call::TaxRates("std".into())]);
    assert_eq!(c.pricing.tax.valid_from, "2026-11-11");
    let down = c.update(Msg::Capture(Press::parse("\u{F701}", false, false)));
    assert_eq!(calls(&down.effects), [&Call::TaxRates("ex".into())]);
    c.update(Msg::Edit {
        kind: "tax-rate".into(),
        row: 0,
        sub: 0,
        field: "rate".into(),
        value: "11.5".into(),
    });
    c.update(Msg::Edit {
        kind: "tax-rate".into(),
        row: 0,
        sub: 0,
        field: "valid_from".into(),
        value: "nope".into(),
    });
    assert!(act(&mut c, "tax-rate-add").is_empty());
    assert_eq!(c.pricing.tax.issues[0].field, "valid_from");
    c.pricing.tax.valid_from = "2027-01-01".into();
    let effects = act(&mut c, "tax-rate-add");
    assert_eq!(
        calls(&effects),
        [&Call::PutTaxRate {
            class: "ex".into(),
            rate: TaxRate {
                rate_bp: 1150,
                valid_from: "2027-01-01".into()
            }
        }]
    );
    reply(
        &mut c,
        Reply::TaxRateSaved(
            "ex".into(),
            Err(problem(422, "invalid_field")
                .with("field", "valid_from")
                .into()),
        ),
    );
    assert_eq!(c.pricing.tax.issues[0].code, "invalid_date");
    // A `price.changed` reloads the rates on view; F5 too.
    let generation = c.generation;
    let event = sultan_client::ServerEvent {
        kind: "price.changed".into(),
        data: json!({}),
    };
    assert_eq!(
        calls(&c.update(Msg::Event(generation, event)).effects).len(),
        1
    );
    assert_eq!(calls(&act(&mut c, "price-refresh")).len(), 1);

    // Settings: an error, then a save without a tax-mode change.
    act(&mut c, "tab-pricing-settings");
    assert_eq!(c.screen(), Screen::PricingSettings);
    assert!(
        act(&mut c, "pricing-settings-save").is_empty(),
        "not loaded yet"
    );
    reply(
        &mut c,
        Reply::Settings(false, Err(problem(500, "boom").into())),
    );
    assert_eq!(c.pricing.settings.notice.as_ref().unwrap().code, "error");
    reply(
        &mut c,
        Reply::Settings(false, Ok(PricingSettings::default())),
    );
    c.update(Msg::Choose {
        kind: "pricing-settings".into(),
        row: 0,
        field: "cash_rounding".into(),
        index: 2,
    });
    let effects = act(&mut c, "pricing-settings-save");
    let [Call::PutSettings(s)] = calls(&effects)[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(s.cash_rounding, "down_100");
    assert_eq!(calls(&act(&mut c, "price-refresh")), [&Call::Settings]);

    // Without `price.manage` nothing is written.
    let mut c = controller(&[]);
    c.screen = Some(Screen::PriceLists);
    act(&mut c, "tab-tax-rates");
    assert!(act(&mut c, "tax-rate-add").is_empty());
    assert_eq!(c.pricing.tax.notice.as_ref().unwrap().arg, PRICE_MANAGE);
    assert!(act(&mut c, "pricing-settings-save").is_empty());
    assert!(act(&mut c, "price-list-save").is_empty());
    c.pricing.editor.editing = Some("x".into());
    act(&mut c, "price-list-archive");
    assert!(c.confirm.is_none());
    assert_eq!(
        c.pricing.editor.notice.as_ref().unwrap().code,
        "permission_denied_catalog"
    );
}
