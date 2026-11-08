//! The single app (`mode = "single"`): one shop on one computer, the server and the register in
//! one process.
//!
//! - [`provision`] (run once by the setup wizard, right after `init`): creates register `R1`
//!   "Kasir 1" at the first branch and its device token directly in the database, as
//!   `POST /registers/pair` would (token hash, `register.pair` audit entry, in one transaction).
//!   The token goes to `register.token` and the details to `[register]` with the server
//!   [`EMBEDDED`], since the port is only known at start.
//! - [`run`] (every bare `sultan`): binds the API server, opens the register window against it and
//!   stops the server gracefully when the window closes (or on Ctrl+C / `SIGTERM`). It listens on
//!   loopback with a free port ([`LISTEN`]) unless `listen` (`SULTAN_LISTEN` or `sultan.toml`) is
//!   set, e.g. `0.0.0.0:8080` so that terminals on the shop network can connect too.

use std::future::Future;
use std::path::Path;

use serde_json::json;
use sultan_core::ids::RegisterId;
use sultan_core::time::Timestamp;
use sultan_db::audit::{self, AuditEntry};
use sultan_db::org::Register;
use sultan_db::{Db, devices, org};
use sultan_server::token;

use crate::config::{self, FileConfig, RegisterConfig, Role};
use crate::error::{AppError, Result};
use crate::init::InitReport;
use crate::register::{self, PairResponse};
use crate::serve::BoundServer;
use crate::{ENV_LANG, RegisterUi};

/// The `server` stored for the single app's register: the real URL is set at each start.
pub const EMBEDDED: &str = "embedded";

/// Where the embedded server listens by default: loopback only, a free port.
pub const LISTEN: &str = "127.0.0.1:0";

/// Code of the single app's register.
pub const REGISTER_CODE: &str = "R1";

/// Name of the single app's register.
pub const REGISTER_NAME: &str = "Kasir 1";

/// Creates this machine's register and device token (see the module docs).
///
/// # Errors
/// [`AppError::Db`], [`AppError::File`], [`AppError::Config`].
pub async fn provision(
    db_url: &str,
    report: &InitReport,
    config_path: &Path,
    now: Timestamp,
) -> Result<RegisterConfig> {
    let register = Register {
        id: RegisterId::generate(),
        branch: report.branch.id,
        code: REGISTER_CODE.to_owned(),
        name: REGISTER_NAME.to_owned(),
        default_location: Some(report.location.id),
        archived: false,
    };
    let device_token = token::generate();
    let db = Db::connect(db_url).await?;
    let result = async {
        let mut tx = db.begin().await?;
        org::save_register(&mut tx, &register, now).await?;
        devices::insert_device_token(&mut tx, &token::hash(&device_token), register.id, now)
            .await?;
        let mut entry = AuditEntry::new(now, "register.pair");
        entry.branch = Some(register.branch);
        entry.register = Some(register.id);
        entry.user = Some(report.owner_id);
        entry.target = Some(format!("register:{}", register.id));
        entry.after = Some(json!({ "register_code": register.code, "single_app": true }));
        audit::append(&mut tx, &entry).await?;
        tx.commit().await
    }
    .await;
    db.close().await;
    result?;
    register::store(
        EMBEDDED,
        PairResponse {
            device_token,
            register_id: register.id.to_string(),
            branch_id: register.branch.to_string(),
            register_code: register.code,
            register_name: register.name,
        },
        config_path,
    )
}

/// Starts the embedded server and opens the register window until it is closed (see the module
/// docs). `open_ui` blocks; the server runs on the runtime's other threads meanwhile.
///
/// # Errors
/// [`AppError::NotPaired`], [`AppError::NotInitialised`], [`AppError::Serve`], or the window's
/// error.
pub async fn run<S>(
    config_path: &Path,
    file: &FileConfig,
    env: config::Env<'_>,
    open_ui: &mut dyn FnMut(RegisterUi) -> Result<()>,
    shutdown: S,
) -> Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    let (register, device_token) = register::paired(config_path)?;
    let db_url = config::resolve_db(None, env, file, config_path);
    let listen = env(config::ENV_LISTEN)
        .or_else(|| file.listen.clone())
        .unwrap_or_else(|| LISTEN.to_owned());
    let server = BoundServer::bind(&db_url, &listen, Role::Standalone).await?;
    let url = local_url(server.addr());
    tracing::info!(%url, "single app: embedded server");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        tokio::select! {
            () = shutdown => {}
            _ = stopped => {}
        }
    }));
    let shown = open_ui(RegisterUi {
        register: RegisterConfig {
            server: url.clone(),
            ..register
        },
        device_token,
        language: env(ENV_LANG),
    });
    let _ = stop.send(());
    let outcome = task.await.map_err(|e| AppError::Serve {
        addr: url,
        source: std::io::Error::other(e),
    })?;
    shown.and(outcome)
}

/// The URL this machine reaches a server bound to `addr` at (`0.0.0.0` / `::` → loopback).
#[must_use]
pub fn local_url(addr: std::net::SocketAddr) -> String {
    let ip = match addr.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => std::net::Ipv4Addr::LOCALHOST.into(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => std::net::Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    };
    format!("http://{}", std::net::SocketAddr::new(ip, addr.port()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::init_database;
    use crate::init::tests::{TempDir, options};

    const NOW: Timestamp = Timestamp::from_unix_millis(1_800_000_000_000);

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provisions_a_register_and_serves_it_while_the_window_is_open() {
        let dir = TempDir::new("single");
        let config_path = dir.0.join("sultan.toml");
        let url = dir.sqlite_url();
        let report = init_database(&options(&url), NOW).await.unwrap();
        let register = provision(&url, &report, &config_path, NOW).await.unwrap();
        assert_eq!(
            (register.server.as_str(), register.register_code.as_str()),
            (EMBEDDED, REGISTER_CODE)
        );
        assert_eq!(register.branch_id, report.branch.id.to_string());

        let file = config::load(&config_path).unwrap();
        let mut seen = Vec::new();
        let mut window = |ui: RegisterUi| -> Result<()> {
            // The server answers while the window is open: the device token lists the owner.
            let addr = ui
                .register
                .server
                .strip_prefix("http://")
                .unwrap()
                .to_owned();
            let users = std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut stream = std::net::TcpStream::connect(&addr).unwrap();
                write!(
                    stream,
                    "GET /api/v1/registers/me/users HTTP/1.1\r\nHost: x\r\n\
                     X-Register-Token: {}\r\nConnection: close\r\n\r\n",
                    ui.device_token
                )
                .unwrap();
                let mut answer = String::new();
                stream.read_to_string(&mut answer).unwrap();
                answer
            })
            .join()
            .unwrap();
            assert!(users.starts_with("HTTP/1.1 200"), "{users}");
            assert!(users.contains("\"display_name\":\"Owner\""), "{users}");
            seen.push(ui.register.register_name.clone());
            Ok(())
        };
        run(
            &config_path,
            &file,
            &no_env,
            &mut window,
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(seen, [REGISTER_NAME]);

        // A window error is passed on, after the server stopped.
        let mut broken = |_: RegisterUi| -> Result<()> { Err(AppError::Ui("no display".into())) };
        let lan = |k: &str| (k == config::ENV_LISTEN).then(|| "0.0.0.0:0".to_owned());
        let err = run(&config_path, &file, &lan, &mut broken, async {})
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Ui(_)), "{err}");

        // Provisioning a second time works too (another register), but the database must exist.
        let err = provision("postgres://x", &report, &config_path, NOW)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Db(_)), "{err}");
    }

    #[test]
    fn local_urls() {
        let cases = [
            ("0.0.0.0:8080", "http://127.0.0.1:8080"),
            ("[::]:8080", "http://[::1]:8080"),
            ("192.168.1.10:81", "http://192.168.1.10:81"),
        ];
        for (addr, url) in cases {
            assert_eq!(local_url(addr.parse().unwrap()), url);
        }
    }

    #[tokio::test]
    async fn needs_a_paired_and_initialised_machine() {
        let dir = TempDir::new("single-bad");
        let config_path = dir.0.join("sultan.toml");
        let mut window = |_: RegisterUi| -> Result<()> { panic!("no window expected") };
        let err = run(
            &config_path,
            &FileConfig::default(),
            &no_env,
            &mut window,
            async {},
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::NotPaired), "{err}");
        config::set_register(
            &config_path,
            &RegisterConfig {
                server: EMBEDDED.into(),
                register_id: "r".into(),
                branch_id: "b".into(),
                register_code: "R1".into(),
                register_name: "Kasir 1".into(),
            },
        )
        .unwrap();
        config::write_private(&register::token_path(&config_path), b"t").unwrap();
        let err = run(
            &config_path,
            &FileConfig::default(),
            &no_env,
            &mut window,
            async {},
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::NotInitialised(_)), "{err}");
    }
}
