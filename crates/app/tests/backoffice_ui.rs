//! End-to-end test of the back office (phase C4): `sultan init` and `sultan serve` in-process on
//! `127.0.0.1:0`, then the real window glue (`sultan_ui::build_backoffice`) with the real HTTP
//! client (`HttpService::user`, no device token), driven headlessly: password login, a category
//! and a brand, a product with an option and a box unit, a duplicate barcode answered `409` and
//! shown on its row, the list, archive, and `catalog.changed` from another client over the
//! WebSocket.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_app::config::Role;
use sultan_app::init::{InitOptions, init_database};
use sultan_app::serve::BoundServer;
use sultan_client::HttpService;
use sultan_server::{Clock, SystemClock};
use sultan_ui::{BackOfficeApp, BackOfficeOptions, Runner, build_backoffice};

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn press(app: &BackOfficeApp, parts: &[SharedString]) {
    let window = app.ui.window();
    for p in parts {
        window.dispatch_event(WindowEvent::KeyPressed { text: p.clone() });
    }
    for p in parts.iter().rev() {
        window.dispatch_event(WindowEvent::KeyReleased { text: p.clone() });
    }
}

fn ctrl(app: &BackOfficeApp, c: &str) {
    press(app, &[Key::Control.into(), c.into()]);
}

fn edit(app: &BackOfficeApp, kind: &str, row: i32, sub: i32, field: &str, value: &str) {
    app.ui
        .invoke_edit(kind.into(), row, sub, field.into(), value.into());
}

fn note(app: &BackOfficeApp, note: sultan_ui::Note) -> String {
    app.ui.invoke_message(note).into()
}

#[test]
#[allow(clippy::too_many_lines)] // one end-to-end story
fn back_office_against_sultan_serve() {
    i_slint_backend_testing::init_no_event_loop();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = TempDir(std::env::temp_dir().join(format!("sultan-bo-e2e-{}", uuid::Uuid::now_v7())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let db_url = format!("sqlite://{}", dir.0.join("sultan.db").display());
    let options = InitOptions {
        db_url: db_url.clone(),
        company: "PT Sultan Demo".into(),
        branch_code: "JKT01".into(),
        branch_name: "Toko Jakarta".into(),
        branch_address: String::new(),
        timezone: "Asia/Jakarta".into(),
        location_name: "Store floor".into(),
        owner_username: "owner".into(),
        owner_display_name: "Owner".into(),
        owner_password: "owner-pass-1".into(),
        owner_pin: "1234".into(),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (root, task) = runtime.block_on(async {
        init_database(&options, SystemClock.now()).await.unwrap();
        let bound = BoundServer::bind(&db_url, "127.0.0.1:0", Role::Standalone)
            .await
            .unwrap();
        let root = format!("http://{}", bound.addr());
        let task = tokio::spawn(bound.run(async {
            let _ = stopped.await;
        }));
        (root, task)
    });

    let service = Arc::new(HttpService::user(&root));
    let mut options = BackOfficeOptions::new(&root);
    options.language = Some("en".into());
    options.username = "owner".into();
    options.debounce = Duration::ZERO;
    let app = build_backoffice(service, Runner::Block(runtime.handle().clone()), options).unwrap();
    app.ui.window().set_size(LogicalSize::new(1366.0, 768.0));
    app.ui.show().unwrap();
    let ui = &app.ui;

    // Wrong password, then the owner.
    edit(&app, "login", 0, 0, "password", "nope-nope");
    ui.invoke_action("login".into());
    assert_eq!(
        note(&app, ui.get_login().notice),
        "Wrong username or password."
    );
    edit(&app, "login", 0, 0, "password", "owner-pass-1");
    ui.invoke_action("login".into());
    assert_eq!(ui.get_screen(), "products");
    assert_eq!(ui.get_user_name(), "Owner");
    assert_eq!(
        ui.get_read_only().code,
        "",
        "the owner may edit the catalog"
    );
    assert!(ui.get_list().show_archived);

    // A category and a brand.
    press(&app, &[Key::F3.into()]);
    ui.invoke_action("cat-add-root".into());
    edit(&app, "category", 0, 0, "name", "Pakaian");
    ctrl(&app, "s");
    assert_eq!(ui.get_category_rows().row_count(), 1);
    press(&app, &[Key::F4.into()]);
    ctrl(&app, "n");
    edit(&app, "brand", 0, 0, "name", "Sultan Wear");
    ctrl(&app, "s");
    assert_eq!(ui.get_brand_rows().row_count(), 1);

    // A product with Size S/M and a box of 12; the code is generated.
    press(&app, &[Key::F2.into()]);
    ctrl(&app, "n");
    edit(&app, "form", 0, 0, "name", "Kaos Polos");
    ui.invoke_choose("form".into(), 0, "category".into(), 1);
    ui.invoke_choose("form".into(), 0, "brand".into(), 1);
    ctrl(&app, "2");
    ui.invoke_action("add-option".into());
    edit(&app, "option", 0, 0, "name", "Size");
    edit(&app, "value", 0, 0, "name", "Small");
    edit(&app, "value", 0, 0, "code", "s");
    ui.invoke_pick("add-value".into(), 0, 0);
    edit(&app, "value", 0, 1, "name", "Medium");
    edit(&app, "value", 0, 1, "code", "m");
    ctrl(&app, "3");
    ui.invoke_action("add-unit".into());
    edit(&app, "unit", 1, 0, "name", "box");
    edit(&app, "unit", 1, 0, "factor", "12");
    edit(&app, "form", 0, 0, "new_barcode", "KAOS-BOX-S");
    ui.invoke_choose("form".into(), 0, "pick_unit".into(), 1);
    ui.invoke_action("add-barcode".into());
    ui.invoke_action("generate-barcode".into());
    ctrl(&app, "s");
    assert_eq!(note(&app, ui.get_form().notice), "Saved.");
    let code = ui.get_general().code.to_string();
    assert!(code.starts_with('P'), "{code}");
    let product_id = app.controller().form.as_ref().unwrap().form.id.clone();

    // The server has the document.
    let stored: Value = runtime.block_on(async {
        let client = reqwest::Client::new();
        let login: Value = client
            .post(format!("{root}/api/v1/auth/password"))
            .json(&json!({"username": "owner", "password": "owner-pass-1"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = login["token"].as_str().unwrap().to_owned();
        client
            .get(format!("{root}/api/v1/products/{product_id}"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    });
    assert_eq!(stored["variants"][1]["sku"], format!("{code}-M"));
    assert_eq!(stored["units"][1]["factor_thousandths"], 12_000);
    assert_eq!(stored["barcodes"].as_array().unwrap().len(), 2);
    assert!(stored["category_id"].is_string() && stored["brand_id"].is_string());

    // A second product with the same barcode: 409, shown on the barcode row.
    press(&app, &[Key::Escape.into()]);
    ctrl(&app, "n");
    edit(&app, "form", 0, 0, "name", "Kaos Lain");
    ctrl(&app, "3");
    edit(&app, "form", 0, 0, "new_barcode", "KAOS-BOX-S");
    ui.invoke_action("add-barcode".into());
    ctrl(&app, "1");
    ctrl(&app, "s");
    assert_eq!(ui.get_form().tab, 2);
    assert_eq!(
        note(&app, ui.get_barcodes().row_data(0).unwrap().error),
        "The barcode KAOS-BOX-S is used by another product."
    );
    press(&app, &[Key::Escape.into()]);
    assert_eq!(ui.get_confirm(), "discard");
    press(&app, &[Key::Return.into()]);

    // The list: search and open, archive.
    edit(&app, "list", 0, 0, "search", "kaos");
    let rows = ui.get_rows();
    assert_eq!(rows.row_count(), 1);
    assert_eq!(rows.row_data(0).unwrap().variants, "2");
    assert_eq!(rows.row_data(0).unwrap().category, "Pakaian");
    press(&app, &[Key::Return.into()]);
    assert_eq!(ui.get_screen(), "form");
    press(&app, &[Key::Control.into(), Key::Shift.into(), "A".into()]);
    press(&app, &[Key::Return.into()]);
    assert!(ui.get_form().archived);

    // Another client changes the open product: the form warns (events over the WebSocket).
    runtime.block_on(async {
        let client = reqwest::Client::new();
        let login: Value = client
            .post(format!("{root}/api/v1/auth/password"))
            .json(&json!({"username": "owner", "password": "owner-pass-1"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = login["token"].as_str().unwrap();
        let answer = client
            .post(format!("{root}/api/v1/products/{product_id}/unarchive"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status().as_u16(), 200);
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ui.get_form().changed_elsewhere && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        app.pump();
    }
    assert!(ui.get_form().changed_elsewhere, "catalog.changed arrived");
    ui.invoke_action("reload".into());
    assert!(!ui.get_form().archived);

    press(&app, &[Key::F12.into()]);
    assert_eq!(ui.get_screen(), "login");
    stop.send(()).unwrap();
    runtime.block_on(task).unwrap().unwrap();
}
