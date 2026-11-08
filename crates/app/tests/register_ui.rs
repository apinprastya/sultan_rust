//! End-to-end smoke test of the register window (plan Phase 8): `sultan init` and `sultan serve`
//! in-process on `127.0.0.1:0`, `sultan register pair`, then the real window glue
//! (`sultan_ui::build_app`) with the real HTTP client, driven headlessly
//! (`i-slint-backend-testing`, no window on screen): PIN login, open session, manager override,
//! blind close with report, logout, and a `session.revoked` event over the WebSocket when the
//! back office deactivates the cashier.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, SharedString};
use sultan_app::config::Role;
use sultan_app::init::{InitOptions, init_database};
use sultan_app::register;
use sultan_app::serve::BoundServer;
use sultan_client::HttpService;
use sultan_db::seed;
use sultan_server::{Clock, SystemClock};
use sultan_ui::{App, Options, Runner, build_app};

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn press(app: &App, key: impl Into<SharedString>) {
    let text = key.into();
    let window = app.ui.window();
    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window.dispatch_event(WindowEvent::KeyReleased { text });
}

fn type_digits(app: &App, digits: &str) {
    for c in digits.chars() {
        press(app, c.to_string());
    }
}

/// Picks the tile named `name` with the arrow keys and types the PIN.
fn login(app: &App, name: &str, pin: &str) {
    let index = app
        .ui
        .get_users()
        .iter()
        .position(|u| u.name == name)
        .unwrap_or_else(|| panic!("{name} is on the lock screen"));
    for _ in 0..index {
        press(app, Key::RightArrow);
    }
    type_digits(app, pin);
    press(app, Key::Return);
}

/// The back office: owner login, register `R1`, a cashier and a manager, a pairing code.
async fn back_office(base: &str) -> (String, String, String) {
    let http = reqwest::Client::new();
    let login: Value = http
        .post(format!("{base}/auth/password"))
        .json(&json!({"username": "owner", "password": "owner-pass-1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = login["token"].as_str().unwrap().to_owned();
    let branch = login["branch_id"].clone();
    let register_id = uuid::Uuid::now_v7();
    let put = http
        .put(format!("{base}/registers/{register_id}"))
        .bearer_auth(&token)
        .json(&json!({"branch_id": branch, "code": "R1", "name": "Kasir 1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status().as_u16(), 201);
    let mut ani = String::new();
    for (username, name, pin, role) in [
        ("ani", "Ani", "1111", seed::CASHIER_ROLE),
        ("budi", "Budi", "2222", seed::MANAGER_ROLE),
    ] {
        let id = uuid::Uuid::now_v7().to_string();
        let put = http
            .put(format!("{base}/users/{id}"))
            .bearer_auth(&token)
            .json(&json!({
                "username": username, "display_name": name, "pin": pin,
                "assignments": [{"role_id": role.to_string(), "branch_id": branch}],
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(put.status().as_u16(), 201, "{}", put.text().await.unwrap());
        if username == "ani" {
            ani = id;
        }
    }
    let code: Value = http
        .post(format!("{base}/registers/{register_id}/pairing-code"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (token, ani, code["code"].as_str().unwrap().to_owned())
}

/// A running server with a paired register.
struct Served {
    base: String,
    owner_token: String,
    ani_id: String,
    task: tokio::task::JoinHandle<sultan_app::Result<()>>,
}

/// `sultan init`, `sultan serve` on a free port, the back office, `sultan register pair`.
async fn serve_and_pair(
    db_url: &str,
    till_config: &std::path::Path,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Served {
    let options = InitOptions {
        db_url: db_url.to_owned(),
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
    init_database(&options, SystemClock.now()).await.unwrap();
    let bound = BoundServer::bind(db_url, "127.0.0.1:0", Role::Standalone)
        .await
        .unwrap();
    let root = format!("http://{}", bound.addr());
    let task = tokio::spawn(bound.run(shutdown));
    let base = format!("{root}/api/v1");
    let (owner_token, ani_id, code) = back_office(&base).await;
    register::pair(&root, &code, till_config).await.unwrap();
    Served {
        base,
        owner_token,
        ani_id,
        task,
    }
}

#[test]
fn register_window_against_sultan_serve() {
    i_slint_backend_testing::init_no_event_loop();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = TempDir(std::env::temp_dir().join(format!("sultan-ui-e2e-{}", uuid::Uuid::now_v7())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let db_url = format!("sqlite://{}", dir.0.join("sultan.db").display());
    let till_config = dir.0.join("till/sultan.toml");

    // Server side: init, serve, back office, pairing.
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let served = runtime.block_on(serve_and_pair(&db_url, &till_config, async {
        let _ = stopped.await;
    }));
    let (base, owner_token, ani_id) = (&served.base, &served.owner_token, &served.ani_id);

    // The register: the paired config and token, the real client, the real glue.
    let (paired, device_token) = register::paired(&till_config).unwrap();
    let service = Arc::new(HttpService::new(&paired.server, &device_token));
    let mut options = Options::new(format!(
        "{} · {}",
        paired.register_code, paired.register_name
    ));
    options.language = Some("en".into());
    let app = build_app(service, Runner::Block(runtime.handle().clone()), options).unwrap();
    app.ui.window().set_size(LogicalSize::new(1024.0, 768.0));
    app.ui.show().unwrap();
    let ui = &app.ui;
    assert_eq!(ui.get_register_label(), "R1 · Kasir 1");
    let names: Vec<String> = ui.get_users().iter().map(|u| u.name.into()).collect();
    assert_eq!(names.len(), 3, "{names:?}");

    // Wrong PIN, then login and open the session.
    login(&app, "Ani", "0000");
    assert_eq!(ui.get_lock().note.code, "wrong_pin");
    type_digits(&app, "1111");
    press(&app, Key::Return);
    assert_eq!(ui.get_screen(), "open");
    assert_eq!(ui.get_branch_label(), "JKT01 · Toko Jakarta");
    type_digits(&app, "200000");
    press(&app, Key::Return);
    assert_eq!(ui.get_screen(), "home");
    assert_eq!(ui.get_home().opening, "Rp 200.000");

    // A 15% discount needs Budi's approval; the action is retried with the token.
    press(&app, Key::F2);
    assert!(ui.get_approval().open);
    assert_eq!(ui.get_approval().limit, "10%");
    let budi = ui
        .get_managers()
        .iter()
        .position(|m| m.name == "Budi")
        .unwrap();
    for _ in 0..budi {
        press(&app, Key::DownArrow);
    }
    type_digits(&app, "2222");
    press(&app, Key::Return);
    assert!(!ui.get_approval().open);
    let note = ui.get_home().note;
    assert_eq!(
        (note.code.as_str(), note.arg.as_str()),
        ("override_ok", "Budi")
    );

    // Blind close with the exact amount: balanced.
    press(&app, Key::F10);
    type_digits(&app, "200000");
    press(&app, Key::Return);
    press(&app, Key::Return);
    assert_eq!(ui.get_screen(), "report");
    assert_eq!(
        (ui.get_report().over_short.as_str(), ui.get_report().tone),
        ("Rp 0", 0)
    );
    press(&app, Key::Return);
    assert_eq!(ui.get_screen(), "lock");

    // Again: open a new session, then the back office deactivates Ani.
    login(&app, "Ani", "1111");
    press(&app, Key::Return);
    assert_eq!(ui.get_screen(), "home");
    std::thread::sleep(Duration::from_millis(300)); // the events socket subscribes
    runtime.block_on(async {
        let response = reqwest::Client::new()
            .put(format!("{base}/users/{ani_id}"))
            .bearer_auth(owner_token)
            .json(&json!({"username": "ani", "display_name": "Ani", "active": false}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
    });
    for _ in 0..100 {
        app.pump();
        if ui.get_screen() == "lock" {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        ui.get_screen(),
        "lock",
        "session.revoked returns to the lock screen"
    );
    assert_eq!(ui.get_lock().note.code, "session_ended");

    drop(app);
    stop.send(()).unwrap();
    runtime.block_on(served.task).unwrap().unwrap();
}
