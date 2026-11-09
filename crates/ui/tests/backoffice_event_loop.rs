//! The back office's real threading (`Runner::Spawn`): calls run on the tokio runtime, results
//! come back through the Slint event loop (`pump`), and the search waits for its debounce timer.
//! Runs a real (headless) event loop, so it lives alone in this file (one per process).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use slint::{ComponentHandle, Model};
use sultan_client::Product;
use sultan_client::fake::{FakeService, STANDARD_TAX_CLASS};
use sultan_ui::{BackOfficeOptions, Runner, build_backoffice};

fn product(code: &str, name: &str) -> Product {
    let (pcs, v) = (
        uuid::Uuid::now_v7().to_string(),
        uuid::Uuid::now_v7().to_string(),
    );
    serde_json::from_value(json!({
        "code": code, "name": name, "type": "stock", "tax_class_id": STANDARD_TAX_CLASS,
        "units": [{ "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
        "base_unit_id": pcs, "variants": [{ "id": v }],
    }))
    .unwrap()
}

#[test]
fn spawned_calls_and_the_search_debounce() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let fake = Arc::new(FakeService::default());
    for (code, name) in [
        ("TEH", "Teh Botol"),
        ("KOPI", "Kopi Susu"),
        ("AIR", "Air Mineral"),
    ] {
        fake.seed_product(&uuid::Uuid::now_v7().to_string(), &product(code, name));
    }
    let mut options = BackOfficeOptions::new("http://127.0.0.1:8080");
    options.language = Some("en".into());
    options.username = "owner".into();
    options.debounce = Duration::from_millis(30);
    let app = build_backoffice(
        fake.clone(),
        Runner::Spawn(runtime.handle().clone()),
        options,
    )
    .unwrap();
    app.ui.show().unwrap();

    let step = Rc::new(Cell::new(0));
    let deadline = Instant::now() + Duration::from_secs(10);
    let timer = slint::Timer::default();
    let weak = app.ui.as_weak();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(10), {
        let step = step.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let edit = |kind: &str, field: &str, value: &str| {
                ui.invoke_edit(kind.into(), 0, 0, field.into(), value.into());
            };
            match step.get() {
                0 => {
                    edit("login", "password", "owner-pass-1");
                    ui.invoke_action("login".into());
                    step.set(1);
                }
                // The list arrived from the runtime: search (debounced).
                1 if ui.get_rows().row_count() == 3 => {
                    edit("list", "search", "k");
                    edit("list", "search", "kopi");
                    step.set(2);
                }
                2 if ui.get_rows().row_count() == 1 => {
                    step.set(3);
                    let _ = slint::quit_event_loop();
                }
                _ if Instant::now() > deadline => {
                    let _ = slint::quit_event_loop();
                }
                _ => {}
            }
        }
    });
    slint::run_event_loop().unwrap();
    assert_eq!(step.get(), 3, "the flow finished before the deadline");
    assert_eq!(app.ui.get_rows().row_data(0).unwrap().name, "Kopi Susu");
    // Only the last keystroke searched: the login's list plus one search.
    assert_eq!(fake.calls_of("products").len(), 2);
}
