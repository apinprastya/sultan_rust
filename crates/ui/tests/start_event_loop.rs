//! The real Start screen's threading (`Runner::Spawn`): the connection check runs on the tokio
//! runtime and its answer comes back to the UI thread through the Slint event loop (`pump`).
//! Runs a real (headless) event loop, so it lives alone in this file (see `event_loop.rs`).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::ComponentHandle;
use slint::platform::{Key, WindowEvent};
use sultan_ui::start::{AppKind, Choice, StartError, StartFuture, StartInfo, StartService};
use sultan_ui::{Runner, build_start};

struct SlowServer;

impl StartService for SlowServer {
    fn check(&self) -> StartFuture {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<(), StartError>(())
        })
    }

    fn set_language(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn the_check_comes_back_through_the_event_loop() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let info = StartInfo {
        kind: AppKind::Terminal,
        server: "http://10.0.0.1:8080".into(),
        register_code: "R2".into(),
        register_name: "Kasir 2".into(),
        branch: String::new(),
        version: "0.1.0".into(),
        language: "en".into(),
        language_fixed: false,
    };
    let start = build_start(
        Arc::new(SlowServer),
        Runner::Spawn(runtime.handle().clone()),
        info,
    )
    .unwrap();
    start.ui.show().unwrap();
    assert_eq!(start.ui.get_conn(), "checking");
    // F1 while the check runs: the register opens once the answer is in.
    start.ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::F1.into(),
    });
    assert_eq!(start.outcome(), None);

    let done = Rc::new(Cell::new(false));
    let deadline = Instant::now() + Duration::from_secs(10);
    let timer = slint::Timer::default();
    let weak = start.ui.as_weak();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(10), {
        let done = done.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_conn() == "ok" {
                done.set(true);
            }
            if done.get() || Instant::now() > deadline {
                let _ = slint::quit_event_loop();
            }
        }
    });
    slint::run_event_loop_until_quit().unwrap();
    assert!(done.get(), "the check answered before the deadline");
    assert_eq!(start.outcome(), Some(Choice::Register));
    start.pump();
}
