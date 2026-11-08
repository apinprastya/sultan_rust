//! The real register's threading (`Runner::Spawn`): service calls run on the tokio runtime and
//! their results come back to the UI thread through the Slint event loop (`pump`). Runs a real
//! (headless) event loop, so it lives alone in this file: the testing backend with an event loop
//! can be set up once per process.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, Model, SharedString};
use sultan_client::fake::FakeService;
use sultan_ui::{Options, Runner, build_app};

#[test]
fn spawned_calls_come_back_through_the_event_loop() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let fake = Arc::new(FakeService::default());
    let mut options = Options::new("R1 · Kasir 1");
    options.language = Some("en".into());
    let app = build_app(
        fake.clone(),
        Runner::Spawn(runtime.handle().clone()),
        options,
    )
    .unwrap();
    app.ui.show().unwrap();

    // A small script, one step per state the window reaches.
    let step = Rc::new(Cell::new(0));
    let deadline = Instant::now() + Duration::from_secs(10);
    let timer = slint::Timer::default();
    let weak = app.ui.as_weak();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(10), {
        let step = step.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let press = |text: SharedString| {
                ui.window()
                    .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                ui.window()
                    .dispatch_event(WindowEvent::KeyReleased { text });
            };
            match step.get() {
                // The users arrived from the runtime: log in as Ani.
                0 if ui.get_users().row_count() == 4 => {
                    for d in ["1", "2", "3", "4"] {
                        press(d.into());
                    }
                    press(Key::Return.into());
                    step.set(1);
                }
                1 if ui.get_screen() == "open" => {
                    press("7".into());
                    press(Key::Return.into());
                    step.set(2);
                }
                2 if ui.get_screen() == "home" => {
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
    assert_eq!(fake.session().unwrap().opening_cash, 7);
    assert!(!app.ui.get_busy());
}
