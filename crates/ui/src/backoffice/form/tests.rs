use serde_json::json;
use sultan_client::Problem;
use sultan_core::catalog::{ProductType, UnitKind};

use super::*;

const TAX: &str = "00000000-0000-7000-8000-000000000101";

/// A stored `KAOS` with Size S/M, units pcs + box(12), one barcode; SKUs default except M.
fn stored(used: bool) -> Product {
    let ids: Vec<String> = (0..8).map(|_| new_id()).collect();
    serde_json::from_value(json!({
        "id": ids[0], "code": "KAOS", "name": "Kaos Polos", "receipt_name": "Kaos Polos",
        "type": "stock", "tax_class_id": TAX, "tags": ["promo", "basic"],
        "options": [{ "id": ids[1], "name": "Size", "values": [
            { "id": ids[2], "name": "Small", "code": "S" },
            { "id": ids[3], "name": "Medium", "code": "M" }] }],
        "units": [
            { "id": ids[4], "name": "pcs", "kind": "integer", "factor_thousandths": 1000 },
            { "id": ids[5], "name": "box", "kind": "integer", "factor_thousandths": 12000 }],
        "base_unit_id": ids[4], "default_sale_unit_id": ids[4], "default_purchase_unit_id": ids[5],
        "variants": [
            { "id": ids[6], "sku": "KAOS-S", "option_value_ids": [ids[2]] },
            { "id": ids[7], "sku": "SPECIAL-M", "option_value_ids": [ids[3]] }],
        "barcodes": [{ "id": new_id(), "variant_id": ids[6], "unit_id": ids[4], "code": "899" }],
        "used": used, "updated_at": 5,
    }))
    .unwrap()
}

#[test]
fn tabs_of_fields() {
    let cases = [
        ("name", Tab::General),
        ("code", Tab::General),
        ("category_id", Tab::General),
        ("options.values.code", Tab::Variants),
        ("variants.sku", Tab::Variants),
        ("units.factor_thousandths", Tab::Units),
        ("base_unit", Tab::Units),
        ("default_sale_unit_id", Tab::Units),
        ("barcodes.code", Tab::Units),
        ("components", Tab::Units),
    ];
    for (field, tab) in cases {
        assert_eq!(Tab::of_field(field), tab, "{field}");
    }
    assert_eq!(Tab::ALL.map(Tab::index), [0, 1, 2]);
}

#[test]
fn small_helpers() {
    let cases = [
        (1000, "1"),
        (24_000, "24"),
        (500, "0.5"),
        (1250, "1.25"),
        (-1500, "-1.5"),
        (0, "0"),
    ];
    for (t, text) in cases {
        assert_eq!(qty_text(t), text);
    }
    assert_eq!(backticked("invalid SKU `a b`"), Some("a b"));
    assert_eq!(backticked("no ticks"), None);
    let rules = [
        ("the base unit must be decimal", "rule_weighed_decimal"),
        ("no units besides the base unit", "rule_single_unit"),
        ("a bundle has no options", "rule_bundle_options"),
        ("the base unit must be integer", "rule_bundle_integer"),
        ("only a bundle has components", "rule_components"),
        ("something else", "product_type_rule"),
    ];
    for (reason, code) in rules {
        assert_eq!(type_rule_code(reason), code);
    }
    assert_eq!(product_type("nope"), ProductType::Stock);
}

#[test]
fn a_new_product_has_one_variant_and_pcs() {
    let mut f = ProductForm::new_product(TAX);
    assert!(f.is_new && !f.is_dirty());
    let body = f.to_body();
    assert_eq!(body.variants.len(), 1);
    assert_eq!(
        body.variants[0].sku, "",
        "the code is generated, so is the SKU"
    );
    assert_eq!(body.units[0].factor_thousandths, 1000);
    assert_eq!(f.default_sku(0), "…");
    // A name is all a stock product needs.
    assert!(!f.validate());
    assert_eq!(f.errors[0].code, "invalid_name");
    assert_eq!(f.tab, Tab::General);
    f.data.name = " Teh Botol ".into();
    assert!(f.is_dirty());
    assert!(f.validate(), "{:?}", f.errors);
    assert_eq!(f.to_body().name, "Teh Botol");
    f.data.code = "tb01".into();
    assert_eq!(
        f.to_body().variants[0].sku,
        "TB01",
        "no options: SKU = code"
    );
    assert_eq!(f.receipt_placeholder(), "Teh Botol");
    f.data.tags = "promo, , baru".into();
    assert_eq!(f.to_body().tags, ["promo", "baru"]);
    f.mark_clean();
    assert!(!f.is_dirty());
}

#[test]
fn default_skus_follow_the_code_and_the_grid() {
    let mut f = ProductForm::new_product(TAX);
    f.data.name = "Kaos".into();
    f.add_option();
    f.data.options[0].name = "Size".into();
    f.data.options[0].values[0].name = "Small".into();
    f.data.options[0].values[0].code = "s".into();
    f.add_value(0);
    f.data.options[0].values[1].name = "Medium".into();
    f.data.options[0].values[1].code = "m".into();
    f.add_option();
    f.data.options[1].name = "Color".into();
    f.data.options[1].values[0].name = "Red".into();
    f.data.options[1].values[0].code = "red".into();
    assert_eq!(f.data.combos.len(), 2);
    assert!(f.data.combos.iter().all(|c| c.on));
    assert_eq!(f.default_sku(1), "…-M-RED");
    f.data.code = "kaos".into();
    assert_eq!(f.effective_sku(0), "KAOS-S-RED");
    f.data.code = "tee".into();
    assert_eq!(f.effective_sku(1), "TEE-M-RED", "follows the code");
    f.data.combos[1].sku = "special".into();
    assert_eq!(f.effective_sku(1), "SPECIAL", "a typed SKU stays");
    let combo = f.data.combos[1].clone();
    assert_eq!(f.combo_label(&combo), "Medium / Red");
    assert_eq!(f.variant_label(&combo.variant_id), "Kaos – Medium / Red");
    assert_eq!(f.variant_label("nope"), "");
    assert_eq!(f.effective_sku(9), "");
    assert_eq!(f.default_sku(9), "");

    // Switching a new combination off leaves it out of the body.
    f.set_combo_on(0, false);
    let body = f.to_body();
    assert_eq!(body.variants.len(), 1);
    assert_eq!(body.variants[0].sku, "SPECIAL");
    assert_eq!(body.options[1].values[0].code, "RED");
    assert!(f.validate(), "{:?}", f.errors);

    // A third option, then none can be added; removing one rebuilds the grid.
    f.add_option();
    assert!(!f.can_add_option());
    f.add_option();
    assert_eq!(f.data.options.len(), 3);
    f.remove_option(2);
    f.remove_option(9);
    assert_eq!(f.data.combos.len(), 2);
    // Values: an option keeps at least one.
    f.remove_value(1, 0);
    assert_eq!(f.data.options[1].values.len(), 1);
    f.remove_value(0, 1);
    assert_eq!(f.data.options[0].values.len(), 1);
    assert_eq!(f.data.combos.len(), 1);
    f.add_value(7);
}

#[test]
fn a_stored_product_round_trips() {
    let p = stored(false);
    let f = ProductForm::from_product(&p, &["b1".to_owned()], true);
    assert!(!f.is_new && !f.is_dirty() && f.has_image);
    assert_eq!(f.updated_at, 5);
    assert_eq!(f.data.combos[0].sku, "", "the default SKU follows the code");
    assert_eq!(f.data.combos[1].sku, "SPECIAL-M");
    assert_eq!(f.data.receipt_name, "", "the default receipt name");
    assert_eq!(f.data.tags, "promo, basic");
    assert_eq!(f.data.units[1].factor, "12");
    assert!(f.data.blocked.contains("b1"));
    let body = f.to_body();
    let skus: Vec<&str> = body.variants.iter().map(|v| v.sku.as_str()).collect();
    assert_eq!(skus, ["KAOS-S", "SPECIAL-M"]);
    assert_eq!(body.units[1].factor_thousandths, 12_000);
    assert_eq!(body.default_purchase_unit_id, p.default_purchase_unit_id);
    assert_eq!(body.barcodes, p.barcodes);
    assert_eq!(f.active_units().len(), 2);
    assert_eq!(f.active_combos(), [0, 1]);

    // Combinations the product never had start off when the grid grows.
    let mut grown = p.clone();
    grown.options[0]
        .values
        .push(sultan_client::catalog::OptionValue {
            id: new_id(),
            name: "Large".into(),
            code: "L".into(),
        });
    let f = ProductForm::from_product(&grown, &[], false);
    assert_eq!(f.data.combos.len(), 3);
    assert!(!f.data.combos[2].on && !f.data.combos[2].stored);
    assert_eq!(f.to_body().variants.len(), 2);

    // A stored variant switched off is archived, not dropped.
    let mut f = ProductForm::from_product(&p, &[], false);
    f.set_combo_on(1, false);
    let body = f.to_body();
    assert_eq!(body.variants.len(), 2);
    assert!(body.variants[1].archived);
}

#[test]
fn after_use_some_fields_are_frozen() {
    let p = stored(true);
    let mut f = ProductForm::from_product(&p, &[], false);
    assert!(f.used);
    assert!(f.factor_locked(0) && f.factor_locked(1));
    assert!(!f.can_add_option());
    assert!(!f.can_remove_value(0, 0), "stored values stay");
    f.add_option();
    f.remove_option(0);
    assert_eq!(f.data.options.len(), 1);
    f.add_value(0);
    assert!(f.can_remove_value(0, 2), "a new value can go again");
    f.set_type(ProductType::Service);
    assert_eq!(
        f.data.product_type,
        ProductType::Stock,
        "the type is frozen"
    );
    // A stored unit is archived instead of removed, and back.
    f.remove_unit(1);
    assert!(f.data.units[1].archived);
    f.remove_unit(1);
    assert!(!f.data.units[1].archived);
    // A new unit is removable and its factor editable.
    f.add_unit();
    assert!(!f.factor_locked(2));
    f.remove_unit(2);
    assert_eq!(f.data.units.len(), 2);
}

#[test]
fn units_and_numbers_are_checked() {
    let mut f = ProductForm::new_product(TAX);
    f.data.name = "Air".into();
    f.add_unit();
    f.data.units[1].name = "box".into();
    let cases = [
        ("abc", Some("invalid_number")),
        ("2.5", Some("factor_whole")),
        ("1", Some("factor_above_one")),
        ("24", None),
    ];
    for (factor, code) in cases {
        f.data.units[1].factor = factor.into();
        let ok = f.validate();
        assert_eq!(ok, code.is_none(), "{factor}: {:?}", f.errors);
        if let Some(code) = code {
            let e = f.error_for("units", Some(1)).unwrap();
            assert_eq!(e.code, code);
            assert_eq!(f.tab, Tab::Units);
            assert!(f.tab_has_errors(Tab::Units) && !f.tab_has_errors(Tab::General));
        }
    }
    // A decimal base takes a decimal factor.
    f.data.base_kind = UnitKind::Decimal;
    f.data.units[1].factor = "2.5".into();
    assert!(f.validate(), "{:?}", f.errors);
    // Two units with one name: the core rule names the row.
    f.data.base_kind = UnitKind::Integer;
    f.data.units[1].factor = "6".into();
    f.data.units[1].name = "pcs".into();
    assert!(!f.validate());
    let e = f.error_for("units", None).unwrap();
    assert_eq!((e.code.as_str(), e.row), ("duplicate_unit_name", Some(0)));
    // Removing the unit drops its barcodes and resets the defaults.
    f.data.units[1].name = "box".into();
    let (variant, box_) = (
        f.data.combos[0].variant_id.clone(),
        f.data.units[1].id.clone(),
    );
    f.add_barcode(&variant, &box_, " 123 ");
    assert_eq!(f.data.barcodes[0].code, "123");
    f.data.default_purchase.clone_from(&box_);
    f.data.default_sale.clone_from(&box_);
    f.remove_unit(1);
    f.remove_unit(0);
    assert!(f.data.barcodes.is_empty());
    assert_eq!(f.data.default_purchase, f.data.units[0].id);
    assert_eq!(f.data.default_sale, f.data.units[0].id);
    // At most 5 extra units.
    for _ in 0..7 {
        f.add_unit();
    }
    assert_eq!(f.data.units.len(), 6);
    assert!(!f.can_add_unit());
}

#[test]
fn types_change_units_options_and_components() {
    let mut f = ProductForm::new_product(TAX);
    f.data.name = "Keju".into();
    f.add_unit();
    f.add_option();
    f.set_type(ProductType::Weighed);
    assert_eq!(f.data.base_kind, UnitKind::Decimal);
    assert_eq!(f.data.units.len(), 1);
    assert_eq!(f.data.units[0].name, "kg");
    assert!(!f.can_add_unit());
    // PLU: digits only, at most 6.
    f.data.options.clear();
    f.regenerate_grid(true);
    let cases = [
        ("12a", false),
        ("1234567", false),
        ("000", false),
        ("123", true),
        ("", true),
    ];
    for (plu, ok) in cases {
        f.data.combos[0].plu = plu.into();
        assert_eq!(f.validate(), ok, "{plu}: {:?}", f.errors);
    }
    assert_eq!(f.to_body().variants[0].plu, None, "empty PLU");
    f.data.combos[0].plu = "77".into();
    assert_eq!(f.to_body().variants[0].plu, Some(77));
    f.set_type(ProductType::Weighed);
    f.set_type(ProductType::Bundle);
    assert!(f.data.combos[0].plu.is_empty());
    assert_eq!(f.data.base_kind, UnitKind::Integer);
    assert!(!f.data.purchasable, "a bundle is not bought");
    assert!(!f.can_add_option());
    // Components: a bundle needs one; quantities are checked; duplicates are refused.
    assert!(!f.validate());
    assert_eq!(f.errors[0].tab, Tab::Units);
    let line = ComponentRow {
        variant_id: new_id(),
        unit_id: new_id(),
        qty: "0".into(),
        product_id: Some("p".into()),
        name: "Teh".into(),
        sku: "TEH".into(),
        units: Vec::new(),
    };
    f.add_component(line.clone());
    f.add_component(line);
    assert_eq!(f.data.components.len(), 1);
    assert!(!f.validate());
    assert_eq!(
        f.error_for("components", Some(0)).unwrap().code,
        "invalid_number"
    );
    f.data.components[0].qty = "2".into();
    assert!(f.validate(), "{:?}", f.errors);
    assert_eq!(f.to_body().components[0].qty_thousandths, 2000);
    f.set_component_units("p", &[("u".into(), "pcs".into())]);
    assert_eq!(f.data.components[0].units.len(), 1);
    f.set_type(ProductType::Service);
    assert!(f.data.components.is_empty());
    assert!(!f.data.purchasable);
}

#[test]
fn server_failures_land_on_their_field_and_tab() {
    let p = stored(false);
    let mut f = ProductForm::from_product(&p, &[], false);
    // 409 duplicate barcode: the barcode row with that code.
    let dup = Problem::new(409, "duplicate_barcode", "taken")
        .with("product_id", "other")
        .with("variant_id", "v")
        .with("value", "899")
        .with("field", "barcodes.code");
    assert!(f.apply_problem(&dup));
    assert_eq!(f.tab, Tab::Units);
    let e = f.error_for("barcodes", Some(0)).unwrap();
    assert_eq!(
        (e.code.as_str(), e.arg.as_str()),
        ("duplicate_barcode", "899")
    );
    // 409 duplicate SKU: the combination with that SKU.
    let dup = Problem::new(409, "duplicate_sku", "taken")
        .with("product_id", "other")
        .with("value", "SPECIAL-M");
    f.apply_problem(&dup);
    assert_eq!(f.tab, Tab::Variants);
    assert_eq!(
        f.error_for("variants", Some(1)).unwrap().code,
        "duplicate_sku"
    );
    // 409 duplicate code: no field in the answer → `code`.
    let dup = Problem::new(409, "duplicate_code", "taken")
        .with("product_id", "other")
        .with("value", "KAOS");
    f.apply_problem(&dup);
    assert_eq!((f.tab, f.errors[0].field.as_str()), (Tab::General, "code"));
    // 422 with several errors: every one, the first tab (in tab order) wins.
    let rules = Problem::new(422, "invalid_unit_factor", "x").with(
        "errors",
        json!([
            {"code": "invalid_unit_factor", "field": "units.factor_thousandths",
             "message": "invalid factor for unit `box`: must be greater than 1"},
            {"code": "frozen_after_use", "field": "type", "message": "`type` cannot change"},
            {"code": "product_type_rule", "field": "type",
             "message": "weighed product: the base unit must be decimal"},
            {"code": "invalid_field", "field": "name", "message": "invalid name"},
        ]),
    );
    assert!(f.apply_problem(&rules));
    assert_eq!(f.tab, Tab::General);
    assert_eq!(f.errors.len(), 4);
    assert_eq!(f.error_for("units", Some(1)).unwrap().arg, "box");
    assert_eq!(f.errors[1].code, "frozen_type");
    assert_eq!(f.errors[2].code, "rule_weighed_decimal");
    assert_eq!(f.errors[3].code, "invalid_name");
    // A reference that no longer exists goes to the category picker.
    f.apply_problem(&Problem::new(422, "invalid_reference", "gone"));
    assert_eq!(f.errors[0].field, "category_id");
    // Not a field problem.
    assert!(!f.apply_problem(&Problem::new(403, "permission_denied", "no")));
}

#[test]
fn core_rules_point_at_rows() {
    let p = stored(false);
    let mut f = ProductForm::from_product(&p, &[], false);
    f.data.combos[1].sku = "a b".into();
    assert!(!f.validate());
    let e = f.error_for("variants", Some(1)).unwrap();
    assert_eq!((e.code.as_str(), e.arg.as_str()), ("invalid_sku", "A B"));
    f.data.combos[1].sku.clear();
    f.data.options[0].values[1].code = "S".into();
    assert!(!f.validate());
    assert_eq!(
        f.error_for("options", Some(0)).unwrap().code,
        "duplicate_option_value"
    );
    f.data.options[0].values[1].code = "M".into();
    f.data.barcodes[0].code = "8991234567890".into();
    assert!(!f.validate());
    assert_eq!(
        f.error_for("barcodes", Some(0)).unwrap().code,
        "invalid_check_digit"
    );
    f.data.barcodes[0].code = "x y".into();
    assert!(!f.validate());
    assert_eq!(f.errors[0].code, "invalid_barcode");
    f.data.barcodes[0].code = "899".into();
    f.data.code = "bad code".into();
    assert!(!f.validate());
    assert_eq!(f.errors[0].code, "invalid_product_code");
    f.data.code = "KAOS".into();
    f.data.options[0].name = String::new();
    assert!(!f.validate());
    assert_eq!(f.errors[0].code, "invalid_options_name");
    f.data.options[0].name = "Size".into();
    f.add_option();
    f.data.options[1].name = "size".into();
    f.data.options[1].values[0].name = "X".into();
    f.data.options[1].values[0].code = "X".into();
    assert!(!f.validate());
    assert_eq!(f.errors[0].code, "duplicate_option");
    // A bad ID in the body is reported, not a panic.
    f.data.options[1].id = "not-a-uuid".into();
    assert!(!f.validate());
    assert!(
        f.errors.iter().any(|e| e.code == "invalid_body"),
        "{:?}",
        f.errors
    );
}

#[test]
fn archiving_the_default_unit_resets_the_defaults() {
    let p = stored(true);
    let mut f = ProductForm::from_product(&p, &[], false);
    let (base, box_) = (f.data.units[0].id.clone(), f.data.units[1].id.clone());
    assert_eq!(f.data.default_purchase, box_);
    f.data.default_sale.clone_from(&box_);
    f.remove_unit(1);
    assert!(f.data.units[1].archived);
    assert_eq!(f.data.default_purchase, base);
    assert_eq!(f.data.default_sale, base);
    let body = f.to_body();
    assert_eq!(body.default_purchase_unit_id, base);
    // Restoring the unit leaves the defaults alone.
    f.remove_unit(1);
    assert!(!f.data.units[1].archived);
    assert_eq!(f.data.default_purchase, base);
}

#[test]
fn barcodes_keep_their_flag_and_follow_their_variants() {
    let mut p = stored(false);
    p.barcodes[0].allow_invalid_check_digit = true;
    let mut f = ProductForm::from_product(&p, &[], false);
    assert!(f.to_body().barcodes[0].allow_invalid_check_digit);
    // A barcode whose variant is no longer in the body is not sent.
    let unit = f.data.units[0].id.clone();
    f.add_barcode(&new_id(), &unit, "777");
    assert_eq!(f.data.barcodes.len(), 2);
    let body = f.to_body();
    assert_eq!(body.barcodes.len(), 1);
    assert!(body.barcodes[0].allow_invalid_check_digit);
    // Switching a new combination off drops its barcode as well.
    f.data.combos[0].stored = false;
    f.data.combos[0].on = false;
    assert!(f.to_body().barcodes.is_empty());
}

#[test]
fn changing_the_type_drops_barcodes_of_removed_units() {
    let mut f = ProductForm::new_product(TAX);
    f.add_unit();
    let (variant, base, box_) = (
        f.data.combos[0].variant_id.clone(),
        f.data.units[0].id.clone(),
        f.data.units[1].id.clone(),
    );
    f.add_barcode(&variant, &base, "111");
    f.add_barcode(&variant, &box_, "222");
    f.set_type(ProductType::Weighed);
    assert_eq!(f.data.units.len(), 1);
    assert_eq!(f.data.barcodes.len(), 1);
    assert_eq!(f.data.barcodes[0].code, "111");
}
