//! Renders the Start screen (files `0a`–`0d`, `0d` the single app's settings with terminals
//! allowed), every register screen (with the barcode lookup of
//! phase C5 and its prices of spec 2.3 R5, files `5a`–`5d`) and every back-office screen (phase C4,
//! files `bo-*`; the customer screens of spec 2.5 M4 are `bo-10`–`bo-19`, the pricing screens of
//! spec 2.3 R4 `bo-20`–`bo-24`) to PNG
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
use slint::{ComponentHandle, Model, PhysicalSize, PlatformError, SharedString};
use sultan_client::fake::{
    BRANCH_ID, FakeService, PriceCustomer, SEED_BUDI, SEED_GOLD, SEED_SILVER, STANDARD_TAX_CLASS,
};
use sultan_client::pricing::DEFAULT_PRICE_LIST;
use sultan_client::{
    BackOffice, CategoryBody, Customer, PriceEntryBody, PriceListBody, PriceRule, PriceTier,
    Product, Service,
};
use sultan_core::catalog::gtin_check_digit;
use sultan_ui::start::{
    AppKind, Lan, LanChanged, LanFuture, StartError, StartFuture, StartInfo, StartService,
};
use sultan_ui::{
    App, BackOfficeApp, BackOfficeOptions, Options, Runner, build_app, build_backoffice,
    build_start,
};

/// The Start screen's service: the server answers, or not.
struct Reachable(bool);

impl StartService for Reachable {
    fn check(&self) -> StartFuture {
        let answer = if self.0 {
            Ok(None)
        } else {
            Err(StartError::new("network", "192.168.1.10:8080"))
        };
        Box::pin(async move { answer })
    }

    fn set_language(&self, _: &str) -> Result<(), String> {
        Ok(())
    }

    fn set_lan(&self, on: bool) -> LanFuture {
        Box::pin(async move {
            Ok(LanChanged {
                lan: Lan {
                    on,
                    port: 8080,
                    addresses: vec!["192.168.1.10:8080".into()],
                    ..Lan::default()
                },
                server: "http://127.0.0.1:8080".into(),
            })
        })
    }
}

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
    start(&window, &dir, (width, height), &language, &runtime)?;

    let fake = Arc::new(FakeService::default());
    fake.set_cash_sales(1_250_000);
    // The register looks codes up in the same demo catalog; Aqua 600 ml is not sold here.
    let (aqua600, _) = demo_catalog(&fake);
    let owner = runtime
        .block_on(fake.password_login("owner", "owner-pass-1"))?
        .token;
    runtime.block_on(fake.put_branch_blocks(&owner, &aqua600, &[BRANCH_ID.to_owned()]))?;
    demo_prices(&fake, &runtime, &owner)?;
    let mut options = Options::new("R1 · Kasir 1");
    options.language = Some(language.clone());
    options.start = true;
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
    scans(&app, &shot)?;
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

    customers(&app, &shot)?;
    drop(app);

    backoffice(&window, &dir, (width, height), &language, &runtime)
}

type Shot = Result<(), Box<dyn std::error::Error>>;

/// The barcode field (phase C5) with prices (spec 2.3 R5): a variant's barcode, a weight scale
/// label, a blocked item, and the Gold price of an attached customer (detached again after).
fn scans(app: &App, shot: &dyn Fn(&str) -> Shot) -> Shot {
    // The barcode field (phase C5): a variant's barcode, a weight scale label, a blocked item.
    digits(app, &ean13("8992000100000"));
    press(app, Key::Return);
    shot("5a-scan-item")?;
    digits(app, &ean13("210012301250"));
    press(app, Key::Return);
    shot("5b-scan-weight")?;
    digits(app, &ean13("899100000001"));
    press(app, Key::Return);
    shot("5c-scan-not-sold-here")?;
    // A customer attached (Budi, Gold): Teh Botol shows his −5% price next to the store price.
    press(app, Key::Escape);
    press(app, Key::F3);
    digits(app, "0812-3456-789");
    press(app, Key::Return);
    press(app, Key::Return);
    digits(app, &ean13("899100000010"));
    press(app, Key::Return);
    shot("5d-scan-customer-price")?;
    press(app, Key::F4);
    press(app, Key::Escape);
    Ok(())
}

/// The customer step (spec 2.5, phase M5), from the "register in use" screen, as Budi (a
/// manager: name search and payout).
fn customers(app: &App, shot: &dyn Fn(&str) -> Shot) -> Shot {
    press(app, Key::Escape);
    press(app, Key::RightArrow);
    digits(app, "9999");
    press(app, Key::Return);
    press(app, Key::F3);
    press(app, Key::Tab);
    digits(app, "a");
    press(app, Key::Return);
    shot("10-customer-search")?;
    press(app, Key::Return);
    shot("10a-customer")?;
    press(app, Key::Return);
    shot("10b-customer-attached")?;
    press(app, Key::F3);
    press(app, Key::F7);
    digits(app, "2200");
    press(app, Key::Return);
    shot("10c-customer-payout")?;
    press(app, Key::Return);
    press(app, Key::Escape);
    press(app, Key::F6);
    digits(app, "Budi Lagi");
    press(app, Key::Tab);
    digits(app, "0812-3456-789");
    press(app, Key::Return);
    shot("10d-customer-sign-up-taken")?;
    Ok(())
}

/// Store prices for the register shots (spec 2.3 R5): the fake's pricing demo (Teh Botol 5 000,
/// Aqua 600ml 3 500, a Gold tier list −5% that Budi holds), the shirt at 89 000 and the cheese at
/// 129 900 per kg.
fn demo_prices(fake: &FakeService, runtime: &tokio::runtime::Runtime, owner: &str) -> Shot {
    let _ = fake.seed_pricing_demo();
    for (code, price) in [
        (ean13("8992000100000"), 89_000),
        ("123".to_owned(), 129_900),
    ] {
        let item = runtime.block_on(fake.lookup(owner, &code))?;
        fake.seed_price_entry(
            DEFAULT_PRICE_LIST,
            &id(),
            &PriceEntryBody {
                variant_id: item.variant_id,
                unit_id: item.unit_id,
                valid_from: None,
                valid_to: None,
                tiers: vec![PriceTier::fixed(1_000, price)],
            },
        );
    }
    fake.seed_price_customer(
        SEED_BUDI,
        PriceCustomer {
            tier: Some(SEED_GOLD.to_owned()),
            ..PriceCustomer::default()
        },
    );
    Ok(())
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
fn start(
    window: &Rc<MinimalSoftwareWindow>,
    dir: &Path,
    (width, height): (u32, u32),
    language: &str,
    runtime: &tokio::runtime::Runtime,
) -> Result<(), Box<dyn std::error::Error>> {
    let shot = |name: &str| {
        save(
            window,
            &dir.join(format!("{name}-{language}-{width}x{height}.png")),
        )
    };
    let info = StartInfo {
        kind: AppKind::Terminal,
        server: "http://192.168.1.10:8080".into(),
        register_code: "R2".into(),
        register_name: "Kasir 2".into(),
        branch: "JKT01 · Toko Jakarta".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        language: language.to_owned(),
        language_fixed: false,
        lan: None,
    };
    for reachable in [true, false] {
        let start = build_start(
            Arc::new(Reachable(reachable)),
            Runner::Block(runtime.handle().clone()),
            info.clone(),
        )?;
        start.ui.show()?;
        if reachable {
            shot("0a-start")?;
            key(start.ui.window(), &[Key::F3.into()]);
            shot("0b-settings")?;
        } else {
            key(start.ui.window(), &[Key::F1.into()]);
            shot("0c-start-offline")?;
        }
    }
    // A single app: terminals allowed (F7 in the settings).
    let single = StartInfo {
        kind: AppKind::Single,
        server: "http://127.0.0.1:41234".into(),
        register_code: "R1".into(),
        register_name: "Kasir 1".into(),
        lan: Some(Lan {
            port: 8080,
            ..Lan::default()
        }),
        ..info
    };
    let start = build_start(
        Arc::new(Reachable(true)),
        Runner::Block(runtime.handle().clone()),
        single,
    )?;
    start.ui.show()?;
    key(start.ui.window(), &[Key::F3.into()]);
    key(start.ui.window(), &[Key::F7.into()]);
    shot("0d-settings-single")?;
    Ok(())
}

#[allow(clippy::too_many_lines)] // one shot after another, in screen order
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
    options.start = true;
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
    let ctrl_shift = |c: &str| key(w, &[Key::Control.into(), Key::Shift.into(), c.into()]);

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

    // Customers (spec 2.5, M4).
    demo_customers(&fake);
    key(w, &[Key::F6.into()]);
    shot("bo-10-customers")?;
    // Budi Santoso (Gold, balance Rp 2 200) is the third row.
    key(w, &[Key::DownArrow.into()]);
    key(w, &[Key::DownArrow.into()]);
    key(w, &[Key::Return.into()]);
    shot("bo-11-customer-form")?;
    // Siti's phone: taken.
    edit("customer", 0, "phone", "0812-9876-5432");
    ctrl("s");
    shot("bo-12-customer-phone-taken")?;
    edit("customer", 0, "phone", "08123456789");
    ctrl_shift("e");
    edit("erase", 0, "code", "K7QH-29XM");
    shot("bo-13-customer-erase")?;
    key(w, &[Key::Escape.into()]);
    key(w, &[Key::Escape.into()]);
    ctrl("2");
    shot("bo-14-customer-groups")?;
    ctrl("3");
    shot("bo-15-customer-tiers")?;
    ctrl("4");
    shot("bo-16-possible-duplicates")?;
    ctrl("m");
    shot("bo-17-merge-confirm")?;
    key(w, &[Key::Escape.into()]);
    ctrl("5");
    shot("bo-18-negative-balances")?;
    ctrl("6");
    shot("bo-19-balance-settings")?;

    // Pricing (spec 2.3, R4): the demo prices, the Gold tier list and a branch list.
    let demo = fake.seed_pricing_demo();
    fake.seed_price_list(
        "0190a000-0000-7000-8000-000000000400",
        &PriceListBody {
            kind: "branch".into(),
            target_id: Some(BRANCH_ID.into()),
            name: "Harga Jakarta".into(),
            active: true,
            rule: Some(PriceRule::amount(500)),
            archived: false,
        },
    );
    key(w, &[Key::F7.into()]);
    key(w, &[Key::DownArrow.into()]);
    key(w, &[Key::DownArrow.into()]);
    shot("bo-20-price-lists")?;
    ctrl("2");
    // Standard is the second tax class; a future rate.
    key(w, &[Key::DownArrow.into()]);
    edit("tax-rate", 0, "rate", "12");
    edit("tax-rate", 0, "valid_from", "2027-01-01");
    ctrl("s");
    shot("bo-21-tax-rates")?;
    ctrl("3");
    ui.invoke_toggle("pricing-settings".into(), 0, "tax_inclusive".into(), false);
    shot("bo-22-pricing-settings")?;
    key(w, &[Key::F2.into()]);
    key(w, &[Key::F5.into()]);
    let row = ui
        .get_rows()
        .iter()
        .position(|r| r.name == demo.teh.name)
        .unwrap_or_default();
    ui.invoke_pick("open".into(), i32::try_from(row)?, 0);
    ctrl("4");
    shot("bo-23-form-prices")?;
    ctrl_shift("n");
    edit("price-tier", 0, "value", "5.500");
    edit("price-tier", 1, "value", "5.000");
    shot("bo-24-form-prices-schedule")?;
    Ok(())
}

/// More customers for the customer screens: an expired tier, a negative balance and a
/// possible duplicate of Budi.
fn demo_customers(fake: &FakeService) {
    let customer = |id: &str, code: &str, name: &str| Customer {
        id: id.into(),
        code: code.into(),
        display_code: format!("{}-{}", &code[..4], &code[4..]),
        name: name.into(),
        signed_up_at: 1_790_000_000_000,
        created_at: 1_790_000_000_000,
        ..Customer::default()
    };
    fake.seed_customer(Customer {
        phone: Some("+6281377778888".into()),
        tier_id: Some(SEED_SILVER.into()),
        tier_valid_until: Some("2026-06-30".into()),
        ..customer("demo-rina", "R8NA5XK2", "Rina Kartika")
    });
    fake.seed_customer(Customer {
        phone: Some("+6285611112222".into()),
        company_name: Some("CV Maju Jaya".into()),
        ..customer("demo-agus", "A6GS7MJ3", "Agus Salim")
    });
    fake.add_customer_balance("demo-agus", "payment", -1_500);
    // Budi's history: more saved change, then a payment.
    fake.add_customer_balance(SEED_BUDI, "change_saved", 700);
    fake.add_customer_balance(SEED_BUDI, "payment", -500);
    fake.seed_customer(Customer {
        email: Some("budi.s@example.com".into()),
        phone_conflict: Some("+628123456789".into()),
        ..customer("demo-dup", "QW3R7TY9", "Budi S.")
    });
}
