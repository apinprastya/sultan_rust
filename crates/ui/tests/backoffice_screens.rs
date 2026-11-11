//! Drives the real back-office window headlessly (`i-slint-backend-testing`, like `screens.rs`)
//! against [`FakeService`] (phase C4): keys are window events, buttons are found by their
//! accessible label, text fields are typed into with key events (or set through the window's
//! `edit` callback), and the checks read the window's properties and the fake's stored catalog.

use std::sync::Arc;
use std::time::Duration;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use serde_json::json;
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_client::fake::{FakeService, STANDARD_TAX_CLASS};
use sultan_client::{CategoryBody, Product, ServerEvent};
use sultan_ui::{BackOfficeApp, BackOfficeOptions, Note, Runner, build_backoffice};

struct Office {
    app: BackOfficeApp,
    fake: Arc<FakeService>,
    _runtime: tokio::runtime::Runtime,
}

fn id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// `Kaos Polos` with Size S/M, one barcode per variant, units pcs + box(12).
fn kaos(code: &str, barcode: &str) -> Product {
    let (pcs, box_, s, m, size) = (id(), id(), id(), id(), id());
    let (v1, v2) = (id(), id());
    serde_json::from_value(json!({
        "code": code, "name": "Kaos Polos", "type": "stock", "tax_class_id": STANDARD_TAX_CLASS,
        "options": [{ "id": size, "name": "Size", "values": [
            { "id": s, "name": "Small", "code": "S" }, { "id": m, "name": "Medium", "code": "M" }] }],
        "units": [
            { "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 },
            { "id": box_, "name": "box", "kind": "integer", "factor_thousandths": 12000 }],
        "base_unit_id": pcs,
        "variants": [{ "id": v1, "option_value_ids": [s] }, { "id": v2, "option_value_ids": [m] }],
        "barcodes": [{ "id": id(), "variant_id": v1, "unit_id": pcs, "code": barcode }],
    }))
    .unwrap()
}

fn office_with(fake: FakeService, language: &str) -> Office {
    office_opened(fake, language, false)
}

/// A back office opened from the Start screen (`start`) or alone.
fn office_opened(fake: FakeService, language: &str, start: bool) -> Office {
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
    options.start = start;
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
        _runtime: runtime,
    }
}

fn office() -> Office {
    office_with(FakeService::default(), "en")
}

fn key_text(name: &str) -> SharedString {
    match name {
        "Enter" => Key::Return.into(),
        "Esc" => Key::Escape.into(),
        "Up" => Key::UpArrow.into(),
        "Tab" => Key::Tab.into(),
        "Down" => Key::DownArrow.into(),
        "Control" => Key::Control.into(),
        "Shift" => Key::Shift.into(),
        "F2" => Key::F2.into(),
        "F3" => Key::F3.into(),
        "F4" => Key::F4.into(),
        "F5" => Key::F5.into(),
        "F9" => Key::F9.into(),
        "F12" => Key::F12.into(),
        other => other.into(),
    }
}

impl Office {
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

    /// Types text into whatever has the focus.
    fn type_text(&self, text: &str) {
        for c in text.chars() {
            self.press(&[&c.to_string()]);
        }
    }

    #[track_caller]
    fn click(&self, label: &str) {
        let found: Vec<_> = ElementHandle::find_by_accessible_label(&self.app.ui, label)
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

    fn edit(&self, kind: &str, row: i32, sub: i32, field: &str, value: &str) {
        self.app
            .ui
            .invoke_edit(kind.into(), row, sub, field.into(), value.into());
    }

    fn screen(&self) -> String {
        self.app.ui.get_screen().into()
    }

    fn text(&self, note: Note) -> String {
        self.app.ui.invoke_message(note).into()
    }

    fn login(&self, user: &str, password: &str) {
        self.edit("login", 0, 0, "username", user);
        self.edit("login", 0, 0, "password", password);
        self.app.ui.invoke_action("login".into());
    }

    fn owner(&self) {
        self.login("owner", "owner-pass-1");
        assert_eq!(self.screen(), "products");
    }

    fn rows(&self) -> Vec<String> {
        self.app
            .ui
            .get_rows()
            .iter()
            .map(|r| r.name.to_string())
            .collect()
    }
}

#[test]
fn login_list_open_edit_save() {
    let fake = FakeService::default();
    let shirt = fake.seed_product(&id(), &kaos("KAOS01", "KB-1"));
    fake.seed_product(&id(), &kaos("KAOS02", "KB-2"));
    let o = office_with(fake, "en");
    assert_eq!(o.screen(), "login");
    o.edit("login", 0, 0, "username", "owner");
    o.edit("login", 0, 0, "password", "wrong");
    o.click("Log in");
    assert_eq!(
        o.text(o.app.ui.get_login().notice),
        "Wrong username or password."
    );
    o.owner();
    assert_eq!(o.rows().len(), 2);
    assert_eq!(o.app.ui.get_rows().row_data(0).unwrap().variants, "2");
    assert_eq!(o.app.ui.get_read_only().code, "", "the owner may edit");

    // ↓ then Enter opens the second (older) row: KAOS01.
    o.press(&["Down"]);
    assert_eq!(o.app.ui.get_list().cursor, 1);
    o.press(&["Enter"]);
    assert_eq!(o.screen(), "form");
    let general = o.app.ui.get_general();
    assert_eq!(
        (general.code.as_str(), general.name.as_str()),
        ("KAOS01", "Kaos Polos")
    );
    assert!(!o.app.ui.get_form().dirty);

    o.edit("form", 0, 0, "name", "Kaos Oblong");
    assert!(o.app.ui.get_form().dirty);
    o.press(&["Control", "s"]);
    assert_eq!(o.text(o.app.ui.get_form().notice), "Saved.");
    assert!(!o.app.ui.get_form().dirty);
    let stored = o.fake.stored_product(&shirt.id).unwrap();
    assert_eq!(stored.name, "Kaos Oblong");
    assert_eq!(stored.variants.len(), 2, "variants kept");
    assert_eq!(stored.variants[0].sku, "KAOS01-S");

    // Back to the list (nothing unsaved): it reloads.
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "products");
    assert!(o.rows().contains(&"Kaos Oblong".to_owned()));
}

/// The text a text field shows (found by its accessible label).
fn field_text(o: &Office, label: &str) -> String {
    let found: Vec<_> = ElementHandle::find_by_accessible_label(&o.app.ui, label)
        .filter(|e| e.accessible_role() == Some(AccessibleRole::TextInput))
        .collect();
    assert_eq!(found.len(), 1, "one field labelled {label:?}");
    found[0].accessible_value().unwrap_or_default().to_string()
}

#[test]
fn shortcuts_work_while_a_text_field_has_the_focus() {
    let fake = FakeService::default();
    fake.seed_product(&id(), &kaos("KAOS01", "KB-1"));
    let mut teh = kaos("TEH01", "TB-1");
    teh.name = "Teh Botol".into();
    fake.seed_product(&id(), &teh);
    let o = office_with(fake, "en");
    // The password field has the focus after the username was typed; Enter logs in.
    o.edit("login", 0, 0, "username", "owner");
    o.app.ui.invoke_action("nothing".into());
    o.press(&["Tab"]);
    o.type_text("owner-pass-1");
    o.press(&["Enter"]);
    assert_eq!(
        o.screen(),
        "products",
        "{:?}",
        o.app.ui.get_login().notice.code
    );

    // `/` focuses the search; typing goes into it and filters (debounce 0 here).
    o.press(&["/"]);
    o.type_text("teh");
    assert_eq!(o.app.controller().list.search, "teh");
    assert_eq!(o.rows(), ["Teh Botol"]);
    // Ctrl+N works with the search field focused; the name field gets the focus.
    o.press(&["Control", "n"]);
    assert_eq!(o.screen(), "form");
    o.type_text("Es Teh");
    assert_eq!(
        o.app.controller().form.as_ref().unwrap().form.data.name,
        "Es Teh"
    );
    assert!(o.app.ui.get_form().dirty);
    // Ctrl+1/2/3 switch tabs while typing.
    o.press(&["Control", "2"]);
    assert_eq!(o.app.ui.get_form().tab, 1);
    o.press(&["Control", "3"]);
    assert_eq!(o.app.ui.get_form().tab, 2);
    o.press(&["Control", "1"]);
    assert_eq!(o.app.ui.get_form().tab, 0);
    // Esc with unsaved changes asks; Esc keeps editing; Esc + Enter discards.
    o.press(&["Esc"]);
    assert_eq!(o.app.ui.get_confirm(), "discard");
    o.press(&["Esc"]);
    assert_eq!(
        (o.app.ui.get_confirm().as_str(), o.screen().as_str()),
        ("", "form")
    );
    // Ctrl+S saves: the code and SKU are generated.
    o.press(&["Control", "s"]);
    assert_eq!(o.text(o.app.ui.get_form().notice), "Saved.");
    let code = o.app.ui.get_general().code.to_string();
    assert!(code.starts_with('P'), "{code}");
    assert_eq!(
        field_text(&o, "Code"),
        code,
        "the field shows the generated code"
    );
    o.edit("form", 0, 0, "tags", "dingin");
    o.press(&["Esc"]);
    assert_eq!(o.app.ui.get_confirm(), "discard");
    o.press(&["Enter"]);
    assert_eq!(o.screen(), "products");
    // F3 / F4 / F2 navigate; F12 logs out.
    o.press(&["F3"]);
    assert_eq!(o.screen(), "categories");
    o.press(&["F4"]);
    assert_eq!(o.screen(), "brands");
    o.press(&["F2"]);
    assert_eq!(o.screen(), "products");
    o.press(&["F12"]);
    assert_eq!(o.screen(), "login");
    assert_eq!(o.fake.calls_of("logout").len(), 1);
}

#[test]
fn generated_code_replaces_typed_text() {
    let o = office();
    o.owner();
    o.press(&["Control", "n"]);
    o.press(&["Control", "1"]);
    o.edit("form", 0, 0, "name", "Gula");
    // The user typed into the code field; Generate replaces what it shows.
    o.edit("form", 0, 0, "code", "abc");
    o.click("Generate");
    let code = o.app.ui.get_general().code.to_string();
    assert_eq!(code, "P000001");
    assert_eq!(field_text(&o, "Code"), "P000001");
}

#[test]
fn create_a_product_with_options_and_units() {
    let o = office();
    o.owner();
    o.click("New product");
    o.edit("form", 0, 0, "name", "Kaos Polos");
    o.edit("form", 0, 0, "code", "kaos");
    o.click("Variants");
    o.click("Add option");
    o.edit("option", 0, 0, "name", "Size");
    o.edit("value", 0, 0, "name", "Small");
    o.edit("value", 0, 0, "code", "s");
    o.app.ui.invoke_pick("add-value".into(), 0, 0);
    o.edit("value", 0, 1, "name", "Medium");
    o.edit("value", 0, 1, "code", "m");
    o.click("Add option");
    o.edit("option", 1, 0, "name", "Color");
    o.edit("value", 1, 0, "name", "Red");
    o.edit("value", 1, 0, "code", "red");
    o.app.ui.invoke_pick("add-value".into(), 1, 0);
    o.edit("value", 1, 1, "name", "Blue");
    o.edit("value", 1, 1, "code", "blue");
    let combos = o.app.ui.get_combos();
    assert_eq!(combos.row_count(), 4);
    assert_eq!(combos.row_data(1).unwrap().sku_default, "KAOS-S-BLUE");
    assert_eq!(combos.row_data(3).unwrap().label, "Medium / Blue");
    // Medium / Blue is not sold; a custom SKU for Small / Red.
    o.app
        .ui
        .invoke_toggle("combo".into(), 3, "on".into(), false);
    o.edit("combo", 0, 0, "sku", "kaos-s-merah");
    // Units: a box of 12, bought by the box.
    o.press(&["Control", "3"]);
    o.click("Add unit");
    o.edit("unit", 1, 0, "name", "box");
    o.edit("unit", 1, 0, "factor", "12");
    o.app
        .ui
        .invoke_choose("form".into(), 0, "default_purchase".into(), 1);
    assert_eq!(o.app.ui.get_unit_choices().row_count(), 2);
    // A barcode for Small / Blue in box.
    o.app
        .ui
        .invoke_choose("form".into(), 0, "pick_variant".into(), 1);
    o.app
        .ui
        .invoke_choose("form".into(), 0, "pick_unit".into(), 1);
    o.edit("form", 0, 0, "new_barcode", "BOX-KAOS-S-BLUE");
    o.click("Add barcode");
    assert_eq!(
        o.app.ui.get_barcodes().row_data(0).unwrap().label,
        "Kaos Polos – Small / Blue · box"
    );
    o.click("Generate internal barcode");
    assert_eq!(o.app.ui.get_barcodes().row_count(), 2);
    o.click("Save");
    assert_eq!(o.text(o.app.ui.get_form().notice), "Saved.");

    let id = o.fake.product_ids()[0].clone();
    let p = o.fake.stored_product(&id).unwrap();
    assert_eq!(p.code, "KAOS");
    let skus: Vec<&str> = p.variants.iter().map(|v| v.sku.as_str()).collect();
    assert_eq!(skus, ["KAOS-S-MERAH", "KAOS-S-BLUE", "KAOS-M-RED"]);
    assert_eq!(p.units[1].factor_thousandths, 12_000);
    assert_eq!(p.default_purchase_unit_id, p.units[1].id);
    assert_eq!(p.barcodes.len(), 2);
    assert_eq!(p.barcodes[0].variant_id, p.variants[1].id);
    assert!(p.barcodes[1].code.starts_with("29"));
    // After the save the form is the stored product: clean, not new.
    let form = o.app.ui.get_form();
    assert!(!form.dirty && !form.is_new);
    assert_eq!(
        o.app.ui.get_combos().row_count(),
        4,
        "the switched-off one is still shown"
    );
}

#[test]
fn duplicate_barcode_is_shown_on_its_row() {
    let fake = FakeService::default();
    fake.seed_product(&id(), &kaos("KAOS01", "KB-DUP"));
    let o = office_with(fake, "en");
    o.owner();
    o.press(&["Control", "n"]);
    o.edit("form", 0, 0, "name", "Kaos Lain");
    o.press(&["Control", "3"]);
    o.edit("form", 0, 0, "new_barcode", "KB-DUP");
    o.click("Add barcode");
    o.edit("form", 0, 0, "new_barcode", "OWN-1");
    o.click("Add barcode");
    o.press(&["Control", "1"]);
    o.press(&["Control", "s"]);
    let form = o.app.ui.get_form();
    assert_eq!(form.tab, 2, "the tab with the error");
    assert_eq!(
        o.text(form.notice),
        "Not saved: please fix the marked fields."
    );
    assert!(form.tab_errors.row_data(2).unwrap());
    let rows = o.app.ui.get_barcodes();
    assert_eq!(
        o.text(rows.row_data(0).unwrap().error),
        "The barcode KB-DUP is used by another product."
    );
    assert_eq!(
        rows.row_data(1).unwrap().error.code,
        "",
        "only the duplicate row"
    );
    // Removing it saves.
    o.app.ui.invoke_pick("remove-barcode".into(), 0, 0);
    o.press(&["Control", "s"]);
    assert_eq!(o.text(o.app.ui.get_form().notice), "Saved.");
}

#[test]
fn a_user_without_catalog_manage_reads_only() {
    let fake = FakeService::default();
    fake.seed_product(&id(), &kaos("KAOS01", "KB-1"));
    let o = office_with(fake, "id");
    o.login("kasir", "kasir-pass-1");
    assert_eq!(o.screen(), "products");
    let banner = o.text(o.app.ui.get_read_only());
    assert!(
        banner.starts_with("Hanya baca: mengubah katalog butuh izin catalog.manage"),
        "{banner}"
    );
    assert!(!o.app.ui.get_list().can_edit);
    assert!(!o.app.ui.get_list().show_archived, "no archived toggle");
    o.press(&["Control", "n"]);
    assert_eq!(o.screen(), "products");
    o.press(&["Enter"]);
    assert_eq!(o.screen(), "form");
    assert!(!o.app.ui.get_form().can_edit);
    o.press(&["Control", "s"]);
    assert!(o.fake.calls_of("put_product").is_empty());
    o.press(&["Esc"]);
    assert_eq!(o.screen(), "products");
}

/// C3b (spec 2.2 §11): `catalog.manage` at one branch is enough, so Budi (Manager at one
/// branch) edits the catalog.
#[test]
fn a_branch_manager_may_edit() {
    let o = office_with(FakeService::default(), "en");
    o.login("budi", "budi-pass-1");
    assert_eq!(o.screen(), "products");
    assert_eq!(o.app.ui.get_read_only().code, "");
    assert!(o.app.ui.get_list().can_edit && o.app.ui.get_list().show_archived);
}

/// C3b (spec 2.2 §9): Delete on the product list and in the category and brand editors asks
/// first; a used entry is refused with a translated `in_use` message.
#[test]
fn delete_never_used_entries() {
    let fake = FakeService::default();
    let food = id();
    fake.seed_category(
        &food,
        &CategoryBody {
            name: "Makanan".into(),
            ..CategoryBody::default()
        },
    );
    fake.seed_brand(&id(), "Indomie");
    let sold = fake.seed_product(&id(), &kaos("KAOS01", "KB-1"));
    fake.mark_used(&sold.id);
    let mut typo = kaos("KAOS02", "KB-2");
    typo.category_id = Some(food.clone());
    let typo = fake.seed_product(&id(), &typo);
    let o = office_with(fake, "id");
    o.owner();
    // The newest row (the typo) is highlighted: the button asks, naming it; "Tidak" keeps it.
    o.click("Hapus produk");
    assert_eq!(o.app.ui.get_confirm(), "delete-product");
    assert_eq!(o.app.ui.get_confirm_name(), "Kaos Polos");
    o.press(&["Esc"]);
    assert_eq!(o.app.ui.get_confirm(), "");
    // The category is in use while the typo has it.
    o.press(&["F3"]);
    o.press(&["Control", "Shift", "D"]);
    assert_eq!(o.app.ui.get_confirm(), "delete-category");
    o.press(&["Enter"]);
    assert_eq!(
        o.text(o.app.ui.get_category_editor().notice),
        "Tidak bisa dihapus: masih dipakai produk (termasuk yang diarsipkan). Arsipkan saja."
    );
    // Delete the typo (Ctrl+Shift+D, Enter), then the sold product is refused.
    o.press(&["F2"]);
    o.press(&["Control", "Shift", "D"]);
    o.press(&["Enter"]);
    assert!(o.fake.stored_product(&typo.id).is_none());
    assert_eq!(o.text(o.app.ui.get_list().notice), "Dihapus.");
    assert_eq!(o.rows().len(), 1);
    o.click("Hapus produk");
    o.click("Hapus");
    assert!(o.fake.stored_product(&sold.id).is_some());
    assert_eq!(
        o.text(o.app.ui.get_list().notice),
        "Tidak bisa dihapus: produk sudah pernah dijual, distok, atau diberi harga. Arsipkan saja."
    );
    // Now the category and the brand go.
    o.press(&["F3"]);
    o.click("Hapus kategori");
    o.press(&["Enter"]);
    assert!(o.fake.stored_categories().is_empty());
    assert_eq!(o.app.ui.get_category_rows().row_count(), 0);
    o.press(&["F4"]);
    o.click("Hapus merek");
    assert_eq!(o.app.ui.get_confirm(), "delete-brand");
    o.click("Hapus");
    assert!(o.fake.stored_brands().is_empty());
    assert_eq!(o.text(o.app.ui.get_brand_editor().notice), "Dihapus.");
}

#[test]
fn categories_and_brands() {
    let fake = FakeService::default();
    let food = id();
    fake.seed_category(
        &food,
        &CategoryBody {
            name: "Makanan".into(),
            ..CategoryBody::default()
        },
    );
    fake.seed_brand(&id(), "Indomie");
    let o = office_with(fake, "en");
    o.owner();
    o.click("Categories");
    assert_eq!(o.app.ui.get_category_rows().row_count(), 1);
    assert_eq!(o.app.ui.get_category_editor().name, "Makanan");
    // Ctrl+N adds a child of the selected category; the name field has the focus.
    o.press(&["Control", "n"]);
    o.type_text("Mie");
    o.press(&["Control", "s"]);
    let rows = o.app.ui.get_category_rows();
    assert_eq!(rows.row_count(), 2);
    assert_eq!(
        (
            rows.row_data(1).unwrap().name.as_str(),
            rows.row_data(1).unwrap().depth
        ),
        ("Mie", 1)
    );
    assert_eq!(o.text(o.app.ui.get_category_editor().notice), "Saved.");
    // Move it to the top level and archive it.
    o.app
        .ui
        .invoke_choose("category".into(), 0, "parent".into(), 0);
    o.press(&["Control", "s"]);
    assert!(
        o.fake
            .stored_categories()
            .iter()
            .all(|c| c.parent_id.is_none())
    );
    o.press(&["Control", "Shift", "A"]);
    assert!(o.fake.stored_categories().iter().any(|c| c.archived));
    assert_eq!(o.app.ui.invoke_label("top_level".into()), "(Top level)");

    o.press(&["F4"]);
    assert_eq!(o.app.ui.get_brand_rows().row_count(), 1);
    o.click("Add brand");
    o.type_text("Sedaap");
    o.press(&["Enter"]);
    assert_eq!(o.app.ui.get_brand_rows().row_count(), 2);
    o.app.ui.invoke_pick("brand".into(), 0, 0);
    o.click("Archive");
    assert!(o.fake.stored_brands().iter().any(|b| b.archived));
}

#[test]
fn image_upload_and_bundle_components() {
    let fake = FakeService::default();
    fake.seed_product(&id(), &kaos("KAOS01", "KB-1"));
    let o = office_with(fake, "en");
    o.owner();
    o.press(&["Control", "n"]);
    o.edit("form", 0, 0, "name", "Paket Kaos");
    o.app.ui.invoke_choose("form".into(), 0, "type".into(), 3);
    // An image from a file: shown at once, uploaded on save.
    let path = std::env::temp_dir().join(format!("sultan-bo-{}.png", id()));
    let png = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0xcf, 0x50, 0x0f, 0x00, 0x03, 0x86, 0x01, 0x80, 0x5a, 0x34, 0x7d, 0x6b, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    std::fs::write(&path, png).unwrap();
    o.edit("form", 0, 0, "image_path", path.to_str().unwrap());
    o.click("Use this file");
    assert_eq!(o.app.ui.get_thumbnail().size().width, 1);
    std::fs::remove_file(&path).unwrap();
    // Components: search, pick, quantity.
    o.press(&["Control", "3"]);
    o.edit("form", 0, 0, "component_search", "kaos");
    assert_eq!(o.app.ui.get_hits().row_count(), 2);
    o.click("Kaos Polos – Small");
    assert_eq!(o.app.ui.get_components().row_count(), 1);
    let line = o.app.ui.get_components().row_data(0).unwrap();
    assert_eq!(
        line.units.row_count(),
        2,
        "the component's units were loaded"
    );
    o.edit("component", 0, 0, "qty", "2");
    o.press(&["Control", "s"]);
    assert_eq!(o.text(o.app.ui.get_form().notice), "Saved.");
    let id = o
        .fake
        .product_ids()
        .into_iter()
        .find(|i| o.fake.stored_product(i).unwrap().product_type == "bundle")
        .unwrap();
    let p = o.fake.stored_product(&id).unwrap();
    assert_eq!(p.components[0].qty_thousandths, 2000);
    assert_eq!(o.fake.stored_image(&id).unwrap().len(), png.len());
    // Remove the image, archive the bundle.
    o.press(&["Control", "1"]);
    o.click("Remove image");
    o.press(&["Control", "s"]);
    assert!(o.fake.stored_image(&id).is_none());
    o.press(&["Control", "Shift", "A"]);
    assert_eq!(o.app.ui.get_confirm(), "archive");
    o.press(&["Enter"]);
    assert!(o.fake.stored_product(&id).unwrap().archived);
    assert_eq!(
        o.text(o.app.ui.get_form().notice),
        "The product is archived. It cannot be sold or bought any more."
    );
}

#[test]
fn live_refresh_from_catalog_events() {
    let fake = FakeService::default();
    let shirt = id();
    let stored = fake.seed_product(&shirt, &kaos("KAOS01", "KB-1"));
    let o = office_with(fake, "en");
    o.owner();
    assert!(o.fake.watching());
    let changed = |product: &str| ServerEvent {
        kind: "catalog.changed".into(),
        data: json!({ "scope": "product", "product_id": product }),
    };
    // A product added elsewhere shows up in the list.
    o.fake.seed_product(&id(), &kaos("KAOS02", "KB-2"));
    o.fake.emit(changed("x"));
    o.app.pump();
    assert_eq!(o.rows().len(), 2);
    // The open product changed elsewhere: a warning; reload shows the new version.
    o.press(&["Down"]);
    o.press(&["Enter"]);
    assert_eq!(o.app.ui.get_general().code, "KAOS01");
    o.fake.set_now(1_900_000_000_000);
    let mut newer = stored.clone();
    newer.name = "Kaos Baru".into();
    o.fake.seed_product(&shirt, &newer);
    o.fake.emit(changed(&shirt));
    o.app.pump();
    assert!(o.app.ui.get_form().changed_elsewhere);
    o.click("Reload (drops your changes)");
    assert_eq!(o.app.ui.get_general().name, "Kaos Baru");
    assert!(!o.app.ui.get_form().changed_elsewhere);
}

#[test]
fn f9_on_the_login_screen_goes_back_to_the_start_screen() {
    // Opened alone (`sultan backoffice`): no button, F9 does nothing.
    let o = office();
    assert!(!o.app.ui.get_start());
    o.press(&["F9"]);
    assert!(o.app.ui.window().is_visible());

    // From the Start screen: F9 on the login screen closes the window (also while a field has
    // the focus).
    let o = office_opened(FakeService::default(), "en", true);
    o.type_text("own");
    o.press(&["F9"]);
    assert!(!o.app.ui.window().is_visible());

    // Logged in, F9 does nothing; after logging out (F12) the button closes the window.
    let o = office_opened(FakeService::default(), "id", true);
    o.owner();
    o.press(&["F9"]);
    assert!(o.app.ui.window().is_visible());
    o.press(&["F12"]);
    assert_eq!(o.screen(), "login");
    assert!(o.app.ui.window().is_visible());
    o.click("Layar awal");
    assert!(!o.app.ui.window().is_visible());
}
