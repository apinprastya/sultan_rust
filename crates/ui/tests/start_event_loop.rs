//! The real Start screen's threading (`Runner::Spawn`): the connection check and a single app's
//! server restart ("Allow terminals to connect") run on the tokio runtime and their answers come
//! back to the UI thread through the Slint event loop (`pump`).
//! Runs a real (headless) event loop, so it lives alone in this file (see `event_loop.rs`).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slint::ComponentHandle;
use slint::platform::{Key, WindowEvent};
use sultan_ui::start::{
    AppKind, Choice, Lan, LanChanged, LanFuture, StartFuture, StartInfo, StartService,
};
use sultan_ui::{Runner, build_start};

/// Answers only once the test opens `gate`, so "still waiting" can be asserted without racing a
/// fixed delay (a busy machine used to let the check finish before the first assert).
struct SlowServer {
    gate: Arc<AtomicBool>,
}

/// Waits until `gate` is open.
async fn wait_for(gate: Arc<AtomicBool>) {
    while !gate.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

impl StartService for SlowServer {
    fn check(&self) -> StartFuture {
        let gate = self.gate.clone();
        Box::pin(async move {
            wait_for(gate).await;
            Ok(None)
        })
    }

    fn set_language(&self, _: &str) -> Result<(), String> {
        Ok(())
    }

    fn set_lan(&self, on: bool) -> LanFuture {
        let gate = self.gate.clone();
        Box::pin(async move {
            wait_for(gate).await;
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

#[test]
fn the_check_comes_back_through_the_event_loop() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let info = StartInfo {
        kind: AppKind::Single,
        server: "http://127.0.0.1:41234".into(),
        register_code: "R1".into(),
        register_name: "Kasir 1".into(),
        branch: String::new(),
        version: "0.1.0".into(),
        language: "en".into(),
        language_fixed: false,
        lan: Some(Lan {
            port: 8080,
            ..Lan::default()
        }),
    };
    let gate = Arc::new(AtomicBool::new(false));
    let start = build_start(
        Arc::new(SlowServer { gate: gate.clone() }),
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

    // Terminals on: the restart runs on the runtime too.
    let window = start.ui.window();
    let press = |key: Key| {
        window.dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    };
    // Settings cancel the waiting choice.
    start.ui.invoke_action("settings".into());
    press(Key::F7);
    assert!(start.ui.get_lan_busy());
    // Both answers may come now.
    gate.store(true, Ordering::SeqCst);

    let done = Rc::new(Cell::new(false));
    let deadline = Instant::now() + Duration::from_secs(10);
    let timer = slint::Timer::default();
    let weak = start.ui.as_weak();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(10), {
        let done = done.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_lan_on() && !ui.get_lan_busy() && ui.get_conn() == "ok" {
                done.set(true);
            }
            if done.get() || Instant::now() > deadline {
                let _ = slint::quit_event_loop();
            }
        }
    });
    slint::run_event_loop_until_quit().unwrap();
    assert!(
        done.get(),
        "the check and the restart answered before the deadline"
    );
    assert_eq!(start.ui.get_lan_addresses(), "192.168.1.10:8080");
    assert_eq!(start.ui.get_server(), "127.0.0.1:8080");
    assert_eq!(start.outcome(), None);
    press(Key::Escape);
    press(Key::F1);
    assert_eq!(start.outcome(), Some(Choice::Register));
    start.pump();
}
