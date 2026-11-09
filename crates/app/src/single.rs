//! The single app (`mode = "single"`): one shop on one computer, the server and the register in
//! one process.
//!
//! - [`provision`] (run once by the setup wizard, right after `init`): creates register `R1`
//!   "Kasir 1" at the first branch and its device token directly in the database, as
//!   `POST /registers/pair` would (token hash, `register.pair` audit entry, in one transaction).
//!   The token goes to `register.token` and the details to `[register]` with the server
//!   [`EMBEDDED`], since the port is only known at start.
//! - [`run`] (every bare `sultan`): binds the API server, opens the Start screen against it
//!   ([`crate::start`]: register, back office, settings) and stops the server gracefully when the
//!   Start window closes or "set up again" is chosen (or on Ctrl+C / `SIGTERM`). The server keeps
//!   running while the person switches between the register and the back office. It listens on
//!   loopback with a free port ([`LISTEN`]) unless `listen` (`SULTAN_LISTEN` or `sultan.toml`) is
//!   set, e.g. `0.0.0.0:8080` so that terminals on the shop network can connect too.
//! - The branch shown on the Start screen is stored in `[register]` at provisioning; a single app
//!   provisioned before that reads it from the database at start.

use std::future::Future;
use std::path::Path;

use serde_json::json;
use sultan_core::ids::RegisterId;
use sultan_core::time::Timestamp;
use sultan_db::audit::{self, AuditEntry};
use sultan_db::org::Register;
use sultan_db::{Db, devices, org};
use sultan_server::token;

use crate::config::{self, FileConfig, Mode, RegisterConfig, Role};
use crate::error::{AppError, Result};
use crate::init::InitReport;
use crate::register::{self, PairResponse};
use crate::serve::BoundServer;
use crate::start::{self, Next, Windows};

/// The `server` stored for the single app's register: the real URL is set at each start.
pub const EMBEDDED: &str = "embedded";

/// Where the embedded server listens by default: loopback only, a free port.
pub const LISTEN: &str = "127.0.0.1:0";

/// Code of the single app's register.
pub const REGISTER_CODE: &str = "R1";

/// Name of the single app's register.
pub const REGISTER_NAME: &str = "Kasir 1";

/// The branch as shown on the Start screen: `JKT01 · Toko Jakarta`.
#[must_use]
pub fn branch_label(code: &str, name: &str) -> String {
    format!("{code} · {name}")
}

/// The branch label from the database, for a register stored without one.
async fn branch_from_db(db_url: &str, branch_id: &str) -> Option<String> {
    let id = branch_id.parse().ok()?;
    let db = Db::connect(db_url).await.ok()?;
    let branch = org::get_branch(&db, id).await.ok().flatten();
    db.close().await;
    branch.map(|b| branch_label(&b.code, &b.name))
}

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
        Some(branch_label(&report.branch.code, &report.branch.name)),
        config_path,
    )
}

/// Starts the embedded server and runs the Start screen until it is closed or "set up again"
/// is chosen (see the module docs). The windows block; the server runs on the runtime's other
/// threads meanwhile.
///
/// # Errors
/// [`AppError::NotPaired`], [`AppError::NotInitialised`], [`AppError::Serve`], or a window's
/// error.
pub async fn run<S>(
    config_path: &Path,
    file: &FileConfig,
    env: config::Env<'_>,
    windows: &mut Windows<'_>,
    shutdown: S,
) -> Result<Next>
where
    S: Future<Output = ()> + Send + 'static,
{
    let (mut register, device_token) = register::paired(config_path)?;
    let db_url = config::resolve_db(None, env, file, config_path);
    let listen = env(config::ENV_LISTEN)
        .or_else(|| file.listen.clone())
        .unwrap_or_else(|| LISTEN.to_owned());
    let server = BoundServer::bind(&db_url, &listen, Role::Standalone).await?;
    let url = local_url(server.addr());
    tracing::info!(%url, "single app: embedded server");
    if register.branch.is_none() {
        register.branch = branch_from_db(&db_url, &register.branch_id).await;
    }
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        tokio::select! {
            () = shutdown => {}
            _ = stopped => {}
        }
    }));
    register.server.clone_from(&url);
    let shown = start::run(
        config_path,
        env,
        windows,
        Mode::Single,
        &register,
        &device_token,
    );
    let _ = stop.send(());
    let outcome = task.await.map_err(|e| AppError::Serve {
        addr: url,
        source: std::io::Error::other(e),
    })?;
    let next = shown?;
    outcome?;
    Ok(next)
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
    use std::cell::RefCell;

    use sultan_ui::start::{AppKind, Choice};

    use super::*;
    use crate::init::init_database;
    use crate::init::tests::{TempDir, options};
    use crate::{BackofficeUi, RegisterUi, StartUi};

    const NOW: Timestamp = Timestamp::from_unix_millis(1_800_000_000_000);

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// `GET path` on `server` with the device token; the raw answer.
    pub(crate) fn get(server: &str, path: &str, token: &str) -> String {
        let addr = server.strip_prefix("http://").unwrap().to_owned();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: x\r\nX-Register-Token: {token}\r\n\
             Connection: close\r\n\r\n"
        );
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut stream = std::net::TcpStream::connect(&addr).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).unwrap();
            answer
        })
        .join()
        .unwrap()
    }

    /// What the fake windows saw.
    #[derive(Default)]
    struct Seen {
        starts: Vec<StartUi>,
        registers: Vec<RegisterUi>,
        backoffices: Vec<BackofficeUi>,
    }

    /// Runs the single app with a Start screen that answers `choices` in turn; every register
    /// and back-office window checks that the server answers.
    async fn run_with(
        config_path: &Path,
        env: config::Env<'_>,
        choices: &[Option<Choice>],
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> (Result<Next>, Seen) {
        let seen = RefCell::new(Seen::default());
        let mut choices = choices.iter().copied();
        let mut open_start = |ui: StartUi| -> Result<Option<Choice>> {
            seen.borrow_mut().starts.push(ui);
            Ok(choices.next().expect("a choice for each start"))
        };
        let mut open_ui = |ui: RegisterUi| -> Result<()> {
            let users = get(
                &ui.register.server,
                "/api/v1/registers/me/users",
                &ui.device_token,
            );
            assert!(users.starts_with("HTTP/1.1 200"), "{users}");
            assert!(users.contains("\"display_name\":\"Owner\""), "{users}");
            seen.borrow_mut().registers.push(ui);
            Ok(())
        };
        let mut open_backoffice = |ui: BackofficeUi| -> Result<()> {
            let spec = get(&ui.server, "/api/v1/openapi.json", "");
            assert!(spec.starts_with("HTTP/1.1 200"), "{spec}");
            seen.borrow_mut().backoffices.push(ui);
            Ok(())
        };
        let mut windows = Windows {
            open_start: &mut open_start,
            open_ui: &mut open_ui,
            open_backoffice: &mut open_backoffice,
        };
        let file = config::load(config_path).unwrap();
        let result = run(config_path, &file, env, &mut windows, shutdown).await;
        (result, seen.into_inner())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provisions_a_register_and_keeps_serving_across_windows() {
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
        assert_eq!(register.branch.as_deref(), Some("JKT01 · Toko Jakarta"));

        // Register, back office, register again, then the Start window is closed: one server
        // the whole time.
        let choices = [
            Some(Choice::Register),
            Some(Choice::BackOffice),
            Some(Choice::Register),
            None,
        ];
        let (result, seen) =
            run_with(&config_path, &no_env, &choices, std::future::pending()).await;
        assert_eq!(result.unwrap(), Next::Quit);
        assert_eq!(seen.starts.len(), 4);
        let server = &seen.starts[0].info.server;
        assert!(server.starts_with("http://127.0.0.1:"), "{server}");
        assert!(seen.starts.iter().all(|s| &s.info.server == server));
        assert_eq!(seen.starts[0].info.kind, AppKind::Single);
        assert_eq!(seen.starts[0].info.branch, "JKT01 · Toko Jakarta");
        assert_eq!(seen.starts[0].info.language, "en");
        assert!(!seen.starts[0].info.language_fixed);
        assert_eq!(seen.registers.len(), 2);
        assert!(
            seen.registers
                .iter()
                .all(|r| &r.register.server == server && r.start)
        );
        assert_eq!(seen.registers[0].register.register_name, REGISTER_NAME);
        assert_eq!(seen.backoffices.len(), 1);
        assert_eq!(&seen.backoffices[0].server, server);
        assert!(seen.backoffices[0].start);
        // The server stopped with the Start window.
        let addr = server.strip_prefix("http://").unwrap();
        assert!(std::net::TcpStream::connect(addr).is_err());

        // "Set up again" ends the run too; a register stored without its branch reads it from
        // the database.
        let mut bare = register.clone();
        bare.branch = None;
        config::set_register(&config_path, &bare).unwrap();
        let lan = |k: &str| (k == config::ENV_LISTEN).then(|| "0.0.0.0:0".to_owned());
        let (result, seen) =
            run_with(&config_path, &lan, &[Some(Choice::SetupAgain)], async {}).await;
        assert_eq!(result.unwrap(), Next::SetupAgain);
        assert_eq!(seen.starts[0].info.branch, "JKT01 · Toko Jakarta");

        // A window error is passed on, after the server stopped.
        let mut open_start = |_: StartUi| -> Result<Option<Choice>> { Ok(Some(Choice::Register)) };
        let mut broken = |_: RegisterUi| -> Result<()> { Err(AppError::Ui("no display".into())) };
        let mut no_backoffice = |_: BackofficeUi| -> Result<()> { panic!("no back office") };
        let mut windows = Windows {
            open_start: &mut open_start,
            open_ui: &mut broken,
            open_backoffice: &mut no_backoffice,
        };
        let file = config::load(&config_path).unwrap();
        let err = run(&config_path, &file, &no_env, &mut windows, async {})
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Ui(_)), "{err}");

        // Provisioning a second time works too (another register), but the database must exist.
        let err = provision("postgres://x", &report, &config_path, NOW)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Db(_)), "{err}");
        assert_eq!(branch_from_db("postgres://x", "x").await, None);
        assert_eq!(branch_from_db(&url, "not-an-id").await, None);
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
        let (result, seen) = run_with(&config_path, &no_env, &[], async {}).await;
        assert!(matches!(result, Err(AppError::NotPaired)), "{result:?}");
        assert!(seen.starts.is_empty());
        config::set_register(
            &config_path,
            &RegisterConfig {
                server: EMBEDDED.into(),
                register_id: "r".into(),
                branch_id: "b".into(),
                register_code: "R1".into(),
                register_name: "Kasir 1".into(),
                branch: None,
            },
        )
        .unwrap();
        config::write_private(&register::token_path(&config_path), b"t").unwrap();
        let (result, _) = run_with(&config_path, &no_env, &[], async {}).await;
        assert!(
            matches!(result, Err(AppError::NotInitialised(_))),
            "{result:?}"
        );
    }
}
