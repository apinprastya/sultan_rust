//! Renders every register screen (with the barcode lookup of phase C5, files `5a`–`5c`) and
//! every back-office screen (phase C4, files `bo-*`) to PNG
//! with Slint's software renderer (no display needed), driven by keys against the fake service,
//! like the headless tests.
//!
//! ```sh
//! cargo run -p sultan-ui --example screenshots -- docs/screenshots [WIDTHxHEIGHT] [en|id]
//! ```

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use serde_json::json;
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Key, Platform, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, PhysicalSize, PlatformError, SharedString};
use sultan_client::fake::{BRANCH_ID, FakeService, STANDARD_TAX_CLASS};
use sultan_client::{BackOffice, CategoryBody, Product};
use sultan_core::catalog::gtin_check_digit;
use sultan_ui::{
    App, BackOfficeApp, BackOfficeOptions, Options, Runner, build_app, build_backoffice,
};

struct Shots(Rc<MinimalSoftwareWindow>);

impl Platform for Shots {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn press(app: &App, text: impl Into<SharedString>) {
    key(app.ui.window(), &[text.into()]);
}

/// Presses a combination on `window`: all parts down, released in reverse.
fn key(window: &slint::Window, parts: &[SharedString]) {
    for p in parts {
        window.dispatch_event(WindowEvent::KeyPressed { text: p.clone() });
    }
    for p in parts.iter().rev() {
        window.dispatch_event(WindowEvent::KeyReleased { text: p.clone() });
    }
}

fn digits(app: &App, text: &str) {
    for c in text.chars() {
        press(app, c.to_string());
    }
}

fn save(window: &MinimalSoftwareWindow, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    slint::platform::update_timers_and_animations();
    let size = window.size();
    let (width, height) = (size.width as usize, size.height as usize);
    let mut pixels = vec![PremultipliedRgbaColor::default(); width * height];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width);
    });
    let bytes: Vec<u8> = pixels
        .iter()
        .flat_map(|p| [p.red, p.green, p.blue, p.alpha])
        .collect();
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, size.width, size.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    encoder.write_header()?.write_image_data(&bytes)?;
    println!("{}", path.display());
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().unwrap_or_else(|| "docs/screenshots".into()));
    let (width, height) = args
        .next()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1366_u32, 768_u32));
    let language = args.next().unwrap_or_else(|| "en".into());
    std::fs::create_dir_all(&dir)?;

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Shots(window.clone())))?;
    window.set_size(PhysicalSize::new(width, height));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let fake = Arc::new(FakeService::default());
    fake.set_cash_sales(1_250_000);
    // The register looks codes up in the same demo catalog; Aqua 600 ml is not sold here.
    let (aqua600, _) = demo_catalog(&fake);
    let owner = runtime
        .block_on(fake.password_login("owner", "owner-pass-1"))?
        .token;
    runtime.block_on(fake.put_branch_blocks(&owner, &aqua600, &[BRANCH_ID.to_owned()]))?;
    let mut options = Options::new("R1 · Kasir 1");
    options.language = Some(language.clone());
    let app = build_app(
        fake.clone(),
        Runner::Block(runtime.handle().clone()),
        options,
    )?;
    app.ui.show()?;
    let shot = |name: &str| {
        save(
            &window,
            &dir.join(format!("{name}-{language}-{width}x{height}.png")),
        )
    };

    shot("1-lock")?;
    press(&app, Key::RightArrow);
    digits(&app, "12");
    shot("2-pin-pad")?;
    press(&app, Key::Escape);
    press(&app, Key::LeftArrow);
    digits(&app, "0000");
    press(&app, Key::Return);
    shot("3-wrong-pin")?;
    digits(&app, "1234");
    press(&app, Key::Return);
    digits(&app, "500000");
    shot("4-open-session")?;
    press(&app, Key::Return);
    shot("5-home")?;
    // The barcode field (phase C5): a variant's barcode, a weight scale label, a blocked item.
    digits(&app, &ean13("8992000100000"));
    press(&app, Key::Return);
    shot("5a-scan-item")?;
    digits(&app, &ean13("210012301250"));
    press(&app, Key::Return);
    shot("5b-scan-weight")?;
    digits(&app, &ean13("899100000001"));
    press(&app, Key::Return);
    shot("5c-scan-not-sold-here")?;
    press(&app, Key::Escape);
    press(&app, Key::F2);
    press(&app, Key::DownArrow);
    digits(&app, "5555");
    press(&app, Key::Return);
    digits(&app, "55");
    shot("6-override")?;
    press(&app, Key::Escape);
    press(&app, Key::F10);
    press(&app, Key::DownArrow);
    digits(&app, "17");
    press(&app, Key::DownArrow);
    digits(&app, "3");
    press(&app, Key::DownArrow);
    press(&app, Key::DownArrow);
    digits(&app, "4");
    shot("7-blind-close")?;
    press(&app, Key::Return);
    press(&app, Key::Return);
    shot("8-report")?;
    press(&app, Key::Return);

    // Someone else's session blocks the register.
    fake.open_for("u-budi", 300_000);
    press(&app, Key::RightArrow);
    press(&app, Key::RightArrow);
    digits(&app, "5555");
    press(&app, Key::Return);
    shot("9-register-in-use")?;
    drop(app);

    backoffice(&window, &dir, (width, height), &language, &runtime)
}

fn id() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn ean13(body: &str) -> String {
    format!("{body}{}", gtin_check_digit(body).unwrap_or(0))
}

/// A product document from JSON (units, variants and barcodes get IDs here).
fn product(value: serde_json::Value) -> Product {
    serde_json::from_value(value).expect("demo product")
}

/// A 160×160 PNG: a shirt-blue square with a lighter band (the demo thumbnail).
fn demo_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 160, 160);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        let mut data = Vec::with_capacity(160 * 160 * 3);
        for y in 0..160_u32 {
            for x in 0..160_u32 {
                let band = (50..110).contains(&y) && (30..130).contains(&x);
                let (r, g, b): (u8, u8, u8) = if band {
                    (230, 236, 250)
                } else {
                    (70, 110, 220)
                };
                let shade = u8::try_from((x + y) / 16).unwrap_or(0);
                data.extend_from_slice(&[r, g.saturating_sub(shade), b.saturating_sub(shade)]);
            }
        }
        writer.write_image_data(&data).expect("png data");
    }
    bytes
}

/// The demo catalog of the back-office screenshots.
#[allow(clippy::too_many_lines)] // one demo catalog, product by product
fn demo_catalog(fake: &FakeService) -> (String, String) {
    let (drinks, water, tea, clothes) = (id(), id(), id(), id());
    let category = |name: &str, parent: Option<&String>, code: &str| CategoryBody {
        parent_id: parent.cloned(),
        name: name.to_owned(),
        code: code.to_owned(),
        archived: false,
    };
    fake.seed_category(&drinks, &category("Minuman", None, "MNM"));
    fake.seed_category(&water, &category("Air Mineral", Some(&drinks), "AIR"));
    fake.seed_category(&tea, &category("Teh", Some(&drinks), ""));
    fake.seed_category(&clothes, &category("Pakaian", None, "PKN"));
    let (aqua, sosro, sultan) = (id(), id(), id());
    fake.seed_brand(&aqua, "Aqua");
    fake.seed_brand(&sosro, "Sosro");
    fake.seed_brand(&sultan, "Sultan Wear");

    let simple = |code: &str, name: &str, cat: &str, brand: &str, barcode: &str| {
        let (pcs, ctn, v) = (id(), id(), id());
        product(json!({
            "code": code, "name": name, "type": "stock", "tax_class_id": STANDARD_TAX_CLASS,
            "category_id": cat, "brand_id": brand,
            "units": [
                { "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 },
                { "id": ctn, "name": "karton", "kind": "integer", "factor_thousandths": 24000 }],
            "base_unit_id": pcs, "default_purchase_unit_id": ctn,
            "variants": [{ "id": v }],
            "barcodes": [{ "id": id(), "variant_id": v, "unit_id": pcs, "code": barcode }],
        }))
    };
    let first = id();
    fake.seed_product(
        &first,
        &simple(
            "AQ600",
            "Aqua Air Mineral 600 ml",
            &water,
            &aqua,
            &ean13("899100000001"),
        ),
    );
    fake.seed_product(
        &id(),
        &simple(
            "AQ1500",
            "Aqua Air Mineral 1500 ml",
            &water,
            &aqua,
            &ean13("899100000002"),
        ),
    );
    fake.seed_product(
        &id(),
        &simple(
            "TB350",
            "Teh Botol Sosro 350 ml",
            &tea,
            &sosro,
            &ean13("899100000003"),
        ),
    );
    let (kg, v) = (id(), id());
    fake.seed_product(
        &id(),
        &product(json!({
            "code": "KEJU", "name": "Keju Cheddar (timbang)", "type": "weighed",
            "tax_class_id": STANDARD_TAX_CLASS,
            "units": [{ "id": kg, "name": "kg", "kind": "decimal", "factor_thousandths": 1000 }],
            "base_unit_id": kg, "variants": [{ "id": v, "plu": 123 }],
        })),
    );

    // Kaos Polos: Size S/M/L × Color Red/Blue, L/Blue not sold; pcs + pack(3).
    let (pcs, pack) = (id(), id());
    let sizes = [
        (id(), "Small", "S"),
        (id(), "Medium", "M"),
        (id(), "Large", "L"),
    ];
    let colors = [(id(), "Red", "RED"), (id(), "Blue", "BLUE")];
    let mut variants = Vec::new();
    let mut barcodes = Vec::new();
    for (i, s) in sizes.iter().enumerate() {
        for (j, c) in colors.iter().enumerate() {
            if i == 2 && j == 1 {
                continue;
            }
            let v = id();
            barcodes.push(json!({ "id": id(), "variant_id": v, "unit_id": pcs,
                "code": ean13(&format!("8992000{i}{j}0000")) }));
            variants.push(json!({ "id": v, "option_value_ids": [s.0, c.0] }));
        }
    }
    let values = |list: &[(String, &str, &str)]| -> Vec<serde_json::Value> {
        list.iter()
            .map(|(id, name, code)| json!({ "id": id, "name": name, "code": code }))
            .collect()
    };
    let shirt = id();
    fake.seed_product(
        &shirt,
        &product(json!({
            "code": "KAOS01", "name": "Kaos Polos Katun", "type": "stock",
            "tax_class_id": STANDARD_TAX_CLASS, "category_id": clothes, "brand_id": sultan,
            "tags": ["katun", "basic"],
            "options": [
                { "id": id(), "name": "Size", "values": values(&sizes) },
                { "id": id(), "name": "Color", "values": values(&colors) }],
            "units": [
                { "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 },
                { "id": pack, "name": "pack", "short_name": "pk", "kind": "integer",
                  "factor_thousandths": 3000, "purchasable": true }],
            "base_unit_id": pcs, "default_purchase_unit_id": pack,
            "variants": variants, "barcodes": barcodes,
        })),
    );
    (first, shirt)
}

/// Renders the back-office screens (`bo-*`).
fn backoffice(
    window: &Rc<MinimalSoftwareWindow>,
    dir: &Path,
    (width, height): (u32, u32),
    language: &str,
    runtime: &tokio::runtime::Runtime,
) -> Result<(), Box<dyn std::error::Error>> {
    let fake = Arc::new(FakeService::default());
    let (_, shirt) = demo_catalog(&fake);
    let mut options = BackOfficeOptions::new("http://192.168.1.10:8080");
    options.language = Some(language.to_owned());
    options.debounce = std::time::Duration::ZERO;
    options.username = "owner".into();
    let app: BackOfficeApp = build_backoffice(
        fake.clone(),
        Runner::Block(runtime.handle().clone()),
        options,
    )?;
    app.ui.show()?;
    let ui = &app.ui;
    let shot = |name: &str| {
        save(
            window,
            &dir.join(format!("{name}-{language}-{width}x{height}.png")),
        )
    };
    let edit = |kind: &str, row: i32, field: &str, value: &str| {
        ui.invoke_edit(kind.into(), row, 0, field.into(), value.into());
    };
    let w = ui.window();
    let ctrl = |c: &str| key(w, &[Key::Control.into(), c.into()]);

    shot("bo-1-login")?;
    edit("login", 0, "password", "owner-pass-1");
    ui.invoke_action("login".into());
    // An image on the shirt for the thumbnail.
    let token = runtime
        .block_on(fake.password_login("owner", "owner-pass-1"))?
        .token;
    runtime.block_on(fake.put_product_image(&token, &shirt, demo_png()))?;
    key(w, &[Key::DownArrow.into()]);
    shot("bo-2-products")?;

    // Open the shirt (the newest row is first).
    key(w, &[Key::UpArrow.into()]);
    key(w, &[Key::Return.into()]);
    shot("bo-3-form-general")?;
    ctrl("2");
    shot("bo-4-form-variants")?;
    ctrl("3");
    shot("bo-5-form-units")?;

    // A new product whose barcode is taken: the error is on the barcode row.
    key(w, &[Key::Escape.into()]);
    ctrl("n");
    edit("form", 0, "name", "Aqua Air Mineral 600 ml (promo)");
    ctrl("3");
    edit("form", 0, "new_barcode", &ean13("899100000001"));
    ui.invoke_action("add-barcode".into());
    ctrl("s");
    shot("bo-6-form-duplicate-barcode")?;
    key(w, &[Key::Escape.into()]);
    shot("bo-7-discard-changes")?;
    key(w, &[Key::Return.into()]);

    key(w, &[Key::F3.into()]);
    key(w, &[Key::DownArrow.into()]);
    shot("bo-8-categories")?;
    key(w, &[Key::F4.into()]);
    shot("bo-9-brands")?;
    Ok(())
}
