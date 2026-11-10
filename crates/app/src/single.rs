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
//! - [`Embedded`] is that server. Start screen → Settings → "Allow terminals to connect" calls
//!   [`Embedded::set_lan`]: the new address (`0.0.0.0:<port>`, or loopback) is bound first, then
//!   saved as `listen`, then the new server starts and the old one stops gracefully (a running
//!   request may take [`STOP_GRACE`]). When the old and the new address share the port, the old
//!   server stops first, and is started again on its address if the new one cannot be bound.
//!   Either way a failure leaves the previous setting in force and in the file. `SULTAN_LISTEN`
//!   locks the setting. When a saved `listen` cannot be bound at start (another program took the
//!   port), the app still opens on loopback and the Start screen says so ([`Lan::failed_port`]).
//! - The branch shown on the Start screen is stored in `[register]` at provisioning; a single app
//!   provisioned before that reads it from the database at start.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::json;
use sultan_core::ids::RegisterId;
use sultan_core::time::Timestamp;
use sultan_db::audit::{self, AuditEntry};
use sultan_db::org::Register;
use sultan_db::{Db, devices, org};
use sultan_server::token;
use sultan_ui::start::{Lan, LanChanged, StartError};
use tokio::sync::watch;
use tokio::task::JoinHandle;

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

/// The port terminals connect to when nothing else is configured.
pub const LAN_PORT: u16 = 8080;

/// How long a restart waits for the old server's running requests before it cuts them off.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

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
            branch_code: None,
            branch_name: None,
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
    let fixed = env(config::ENV_LISTEN);
    let listen = fixed
        .clone()
        .or_else(|| file.listen.clone())
        .unwrap_or_else(|| LISTEN.to_owned());
    let (stopping, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        let _ = stopping.send(true);
    });
    let embedded = Embedded::start(&db_url, config_path, &listen, fixed.is_some(), stop_rx).await?;
    tracing::info!(url = %embedded.url(), "single app: embedded server");
    if register.branch.is_none() {
        register.branch = branch_from_db(&db_url, &register.branch_id).await;
    }
    let shown = start::run(
        config_path,
        env,
        windows,
        Mode::Single,
        &register,
        &device_token,
        Some(&embedded),
    );
    let stopped = embedded.stop().await;
    let next = shown?;
    stopped?;
    Ok(next)
}

/// A running server: where it listens, how to stop it, its task.
#[derive(Debug)]
struct Running {
    addr: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    task: JoinHandle<Result<()>>,
}

impl Running {
    /// Serves `server` until stopped or until the app shuts down.
    fn spawn(server: BoundServer, mut stopping: watch::Receiver<bool>) -> Self {
        let addr = server.addr();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async move {
            tokio::select! {
                _ = stopping.wait_for(|stop| *stop) => {}
                _ = stopped => {}
            }
        }));
        Self { addr, stop, task }
    }

    /// Stops gracefully; after `grace` the remaining requests are cut off.
    async fn stop(self, grace: Option<Duration>) -> Result<()> {
        let _ = self.stop.send(());
        let mut task = self.task;
        let joined = match grace {
            None => (&mut task).await,
            Some(grace) => {
                if let Ok(joined) = tokio::time::timeout(grace, &mut task).await {
                    joined
                } else {
                    tracing::warn!(addr = %self.addr, "requests still running; stopped anyway");
                    task.abort();
                    return Ok(());
                }
            }
        };
        joined.map_err(|e| AppError::Serve {
            addr: self.addr.to_string(),
            source: std::io::Error::other(e),
        })?
    }
}

/// The single app's embedded API server, which the Start screen's settings can move to another
/// address (see the module docs). Shared with the Start screen's service.
#[derive(Debug)]
pub struct Embedded {
    db_url: String,
    config_path: PathBuf,
    /// `SULTAN_LISTEN` is set: the address cannot change here.
    fixed: bool,
    /// The port of the saved `listen` that could not be bound at start, until a change.
    failed_port: Mutex<Option<u16>>,
    stopping: watch::Receiver<bool>,
    /// The server; a restart holds the lock throughout.
    running: tokio::sync::Mutex<Option<Running>>,
    /// Where it listens now.
    addr: Mutex<SocketAddr>,
}

impl Embedded {
    /// Binds `listen` and starts serving. A saved address that cannot be bound (not one fixed
    /// by `SULTAN_LISTEN`) falls back to loopback, remembered as [`Lan::failed_port`].
    ///
    /// # Errors
    /// [`AppError::NotInitialised`], [`AppError::Db`], [`AppError::Serve`].
    pub async fn start(
        db_url: &str,
        config_path: &Path,
        listen: &str,
        fixed: bool,
        stopping: watch::Receiver<bool>,
    ) -> Result<std::sync::Arc<Self>> {
        let mut failed_port = None;
        let server = match BoundServer::bind(db_url, listen, Role::Standalone).await {
            Err(AppError::Serve { addr, source }) if !fixed && listen != LISTEN => {
                tracing::warn!(%addr, %source, "cannot listen there; loopback only");
                failed_port = Some(port_of(listen).unwrap_or(LAN_PORT));
                BoundServer::bind(db_url, LISTEN, Role::Standalone).await?
            }
            bound => bound?,
        };
        let addr = server.addr();
        Ok(std::sync::Arc::new(Self {
            db_url: db_url.to_owned(),
            config_path: config_path.to_path_buf(),
            fixed,
            failed_port: Mutex::new(failed_port),
            running: tokio::sync::Mutex::new(Some(Running::spawn(server, stopping.clone()))),
            stopping,
            addr: Mutex::new(addr),
        }))
    }

    /// Where the server listens now.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        *self.addr.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The URL this computer reaches the server at.
    #[must_use]
    pub fn url(&self) -> String {
        local_url(self.addr())
    }

    /// The "Terminals in this shop" setting as it is in force, with this computer's addresses.
    #[must_use]
    pub fn lan(&self) -> Lan {
        let addr = self.addr();
        let on = !addr.ip().is_loopback();
        let port = if on { addr.port() } else { self.lan_port() };
        let addresses = if !on {
            Vec::new()
        } else if addr.ip().is_unspecified() {
            lan_addresses(&interfaces(), port)
        } else {
            vec![addr.to_string()]
        };
        Lan {
            on,
            port,
            addresses,
            fixed: self.fixed,
            failed_port: *self
                .failed_port
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// The port terminals use when switched on: the saved `listen`'s, else [`LAN_PORT`].
    fn lan_port(&self) -> u16 {
        config::load(&self.config_path)
            .ok()
            .and_then(|file| file.listen)
            .and_then(|listen| port_of(&listen))
            .filter(|port| *port != 0)
            .unwrap_or(LAN_PORT)
    }

    /// Allows terminals to connect (`on`) or not: restarts the server on the new address and
    /// saves it (see the module docs).
    ///
    /// # Errors
    /// Code `in_use` (detail: the port) when the address is taken, `failed` otherwise; the
    /// previous setting stays.
    pub async fn set_lan(&self, on: bool) -> std::result::Result<LanChanged, StartError> {
        if self.fixed {
            return Err(StartError::new("failed", "SULTAN_LISTEN"));
        }
        let port = self.lan_port();
        let listen = if on {
            format!("0.0.0.0:{port}")
        } else if port == LAN_PORT {
            LISTEN.to_owned()
        } else {
            format!("127.0.0.1:{port}")
        };
        let saved = (listen != LISTEN).then(|| listen.clone());
        let mut running = self.running.lock().await;
        let Some(old) = running.take() else {
            return Err(StartError::new("failed", "the server has stopped"));
        };
        let old_addr = old.addr;
        let result = self.restart(old, &listen, saved.as_deref()).await;
        let (new, result) = match result {
            Ok(new) => (new, Ok(())),
            Err((back, e)) => (back, Err(e)),
        };
        *self.addr.lock().unwrap_or_else(PoisonError::into_inner) = new.addr;
        *running = Some(new);
        drop(running);
        result?;
        tracing::info!(from = %old_addr, to = %self.addr(), "single app: server moved");
        *self
            .failed_port
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        Ok(LanChanged {
            lan: self.lan(),
            server: self.url(),
        })
    }

    /// Moves `old` to `listen` and saves `saved` as `listen` (`None`: removed). On an error the
    /// server that runs afterwards (the old one, or the old one started again) comes back too.
    async fn restart(
        &self,
        old: Running,
        listen: &str,
        saved: Option<&str>,
    ) -> std::result::Result<Running, (Running, StartError)> {
        let same_port = port_of(listen).is_some_and(|port| port == old.addr.port());
        if !same_port {
            let server = match BoundServer::bind(&self.db_url, listen, Role::Standalone).await {
                Ok(server) => server,
                Err(e) => return Err((old, lan_error(&e, listen))),
            };
            if let Err(e) = self.save(saved) {
                return Err((old, e));
            }
            let new = Running::spawn(server, self.stopping.clone());
            let _ = old.stop(Some(STOP_GRACE)).await;
            return Ok(new);
        }
        // The same port: the old listener must go first.
        let old_addr = old.addr;
        let _ = old.stop(Some(STOP_GRACE)).await;
        let bound = BoundServer::bind(&self.db_url, listen, Role::Standalone).await;
        let failure = match bound {
            Ok(server) => match self.save(saved) {
                Ok(()) => return Ok(Running::spawn(server, self.stopping.clone())),
                Err(e) => e,
            },
            Err(e) => lan_error(&e, listen),
        };
        match BoundServer::bind(&self.db_url, &old_addr.to_string(), Role::Standalone).await {
            Ok(server) => Err((Running::spawn(server, self.stopping.clone()), failure)),
            Err(e) => {
                // Nothing listens any more: loopback on a free port keeps the app working.
                tracing::warn!(%e, "cannot listen on the previous address again");
                match BoundServer::bind(&self.db_url, LISTEN, Role::Standalone).await {
                    Ok(server) => Err((Running::spawn(server, self.stopping.clone()), failure)),
                    Err(e) => {
                        let (stop, _) = tokio::sync::oneshot::channel();
                        let task = tokio::spawn(async move { Err(e) });
                        let dead = Running {
                            addr: old_addr,
                            stop,
                            task,
                        };
                        Err((dead, failure))
                    }
                }
            }
        }
    }

    /// Saves `listen` (`None`: removes it, i.e. loopback).
    fn save(&self, listen: Option<&str>) -> std::result::Result<(), StartError> {
        config::update(&self.config_path, |table| match listen {
            Some(listen) => {
                table.insert("listen".to_owned(), listen.into());
            }
            None => {
                table.remove("listen");
            }
        })
        .map_err(|e| StartError::new("failed", e.to_string()))
    }

    /// Stops the server gracefully (the app is closing or setting up again).
    ///
    /// # Errors
    /// [`AppError::Serve`].
    pub async fn stop(&self) -> Result<()> {
        match self.running.lock().await.take() {
            Some(running) => running.stop(None).await,
            None => Ok(()),
        }
    }
}

/// The port of a listen address such as `0.0.0.0:8080`.
fn port_of(listen: &str) -> Option<u16> {
    listen.rsplit_once(':')?.1.parse().ok()
}

/// A failed bind as the Start screen shows it.
fn lan_error(e: &AppError, listen: &str) -> StartError {
    match e {
        AppError::Serve { source, .. } if source.kind() == std::io::ErrorKind::AddrInUse => {
            StartError::new(
                "in_use",
                port_of(listen).map_or_else(|| listen.to_owned(), |p| p.to_string()),
            )
        }
        e => StartError::new("failed", e.to_string()),
    }
}

/// This computer's network interfaces: name, IPv4 address, up.
fn interfaces() -> Vec<(String, Ipv4Addr, bool)> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.ip() {
            IpAddr::V4(ip) => {
                // Some drivers report `Unknown` for a working interface.
                let up = !matches!(
                    i.oper_status,
                    if_addrs::IfOperStatus::Down | if_addrs::IfOperStatus::NotPresent
                );
                Some((i.name, ip, up))
            }
            IpAddr::V6(_) => None,
        })
        .collect()
}

/// Name prefixes of virtual interfaces (containers, VMs) that terminals cannot reach.
const VIRTUAL: [&str; 6] = ["docker", "br-", "veth", "virbr", "vboxnet", "vmnet"];

/// What terminals type to reach a server listening on every interface at `port`: the IPv4
/// addresses of the interfaces that are up, without loopback, link-local and virtual ones;
/// private (shop network) addresses first.
#[must_use]
pub fn lan_addresses(interfaces: &[(String, Ipv4Addr, bool)], port: u16) -> Vec<String> {
    let mut ips: Vec<Ipv4Addr> = interfaces
        .iter()
        .filter(|(name, ip, up)| {
            *up && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_unspecified()
                && !VIRTUAL.iter().any(|v| name.starts_with(v))
        })
        .map(|(_, ip, _)| *ip)
        .collect();
    ips.sort_by_key(|ip| (!ip.is_private(), *ip));
    ips.dedup();
    ips.into_iter()
        .map(|ip| SocketAddr::new(ip.into(), port).to_string())
        .collect()
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

    /// A port nothing listens on (right now).
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Whether something accepts connections at `addr`.
    fn listening(addr: &str) -> bool {
        std::net::TcpStream::connect(addr).is_ok()
    }

    /// Awaits `future` from inside a (blocking) window hook.
    fn wait<T>(future: impl Future<Output = T>) -> T {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
    }

    /// The Start screen's service as the real window builds it.
    fn service(ui: &StartUi) -> crate::start::AppStartService {
        crate::start::AppStartService::new(&ui.config_path, &ui.info.server, &ui.device_token)
            .with_embedded(ui.embedded.clone().map(|e| e.0))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::too_many_lines)] // one scenario: on, off, a port in use, quit
    async fn settings_move_the_server_to_the_shop_network_and_back() {
        use sultan_ui::start::StartService;

        let dir = TempDir::new("single-lan");
        let config_path = dir.0.join("sultan.toml");
        let url = dir.sqlite_url();
        let report = init_database(&options(&url), NOW).await.unwrap();
        provision(&url, &report, &config_path, NOW).await.unwrap();
        let port = free_port();
        let linux = cfg!(target_os = "linux");
        // On Linux all of 127/8 is loopback: a server on 0.0.0.0 answers at 127.0.0.2, one on
        // 127.0.0.1 does not.
        let wildcard = format!("127.0.0.2:{port}");
        let local = format!("127.0.0.1:{port}");
        let mut blocker = None;

        let seen = RefCell::new(Seen::default());
        let mut step = 0;
        let mut open_start = |ui: StartUi| -> Result<Option<Choice>> {
            step += 1;
            let service = service(&ui);
            let lan = ui.info.lan.clone().expect("a single app has the setting");
            let choice = match step {
                1 => {
                    // Off: loopback on a free port; the saved port is used when switched on.
                    assert!(!lan.on && !lan.fixed && lan.addresses.is_empty());
                    assert_eq!(lan.port, LAN_PORT);
                    assert_eq!(lan.failed_port, None);
                    let old = ui.info.server.strip_prefix("http://").unwrap().to_owned();
                    assert!(listening(&old));
                    config::update(&ui.config_path, |t| {
                        t.insert("listen".into(), format!("127.0.0.1:{port}").into());
                    })
                    .unwrap();
                    let changed = wait(service.set_lan(true)).unwrap();
                    assert!(changed.lan.on);
                    assert_eq!(changed.lan.port, port);
                    assert!(
                        changed
                            .lan
                            .addresses
                            .iter()
                            .all(|a| a.ends_with(&format!(":{port}")))
                    );
                    assert_eq!(changed.server, format!("http://{local}"));
                    assert_eq!(
                        config::load(&ui.config_path).unwrap().listen,
                        Some(format!("0.0.0.0:{port}"))
                    );
                    // The old listener is closed, the new one answers on every interface.
                    assert!(!listening(&old));
                    assert!(listening(&local));
                    if linux {
                        assert!(listening(&wildcard));
                    }
                    assert!(wait(service.check()).unwrap().is_some());
                    Some(Choice::Register)
                }
                2 => {
                    assert!(lan.on);
                    assert_eq!(ui.info.server, format!("http://{local}"));
                    // Off again on the same port: the old listener goes first.
                    let changed = wait(service.set_lan(false)).unwrap();
                    assert!(!changed.lan.on && changed.lan.addresses.is_empty());
                    assert_eq!(changed.server, format!("http://{local}"));
                    assert_eq!(
                        config::load(&ui.config_path).unwrap().listen,
                        Some(format!("127.0.0.1:{port}"))
                    );
                    assert!(listening(&local));
                    if linux {
                        assert!(!listening(&wildcard));
                    }
                    Some(Choice::BackOffice)
                }
                3 => {
                    // Another program holds the port on another address: switching on fails,
                    // the server comes back where it was, the file is unchanged.
                    if linux {
                        blocker = Some(std::net::TcpListener::bind(&wildcard).unwrap());
                        let err = wait(service.set_lan(true)).unwrap_err();
                        assert_eq!(err, StartError::new("in_use", port.to_string()));
                        assert_eq!(
                            config::load(&ui.config_path).unwrap().listen,
                            Some(format!("127.0.0.1:{port}"))
                        );
                    }
                    assert!(wait(service.check()).is_ok());
                    Some(Choice::Register)
                }
                _ => None,
            };
            seen.borrow_mut().starts.push(ui);
            Ok(choice)
        };
        let mut open_ui = |ui: RegisterUi| -> Result<()> {
            let users = get(
                &ui.register.server,
                "/api/v1/registers/me/users",
                &ui.device_token,
            );
            assert!(users.starts_with("HTTP/1.1 200"), "{users}");
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
        let file = config::load(&config_path).unwrap();
        let next = run(
            &config_path,
            &file,
            &no_env,
            &mut windows,
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(next, Next::Quit);
        let seen = seen.into_inner();
        assert_eq!(seen.starts.len(), 4);
        // The register and the back office use the server where it is at that moment.
        assert_eq!(seen.registers[0].register.server, format!("http://{local}"));
        assert_eq!(seen.backoffices[0].server, format!("http://{local}"));
        assert_eq!(seen.starts[3].info.server, format!("http://{local}"));
        // Stopped with the Start window.
        assert!(!listening(&local));
        drop(blocker);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_saved_port_in_use_falls_back_to_loopback_and_sultan_listen_locks_it() {
        let dir = TempDir::new("single-lan-start");
        let config_path = dir.0.join("sultan.toml");
        let url = dir.sqlite_url();
        init_database(&options(&url), NOW).await.unwrap();
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let (_stop, stopping) = watch::channel(false);
        let listen = format!("127.0.0.1:{port}");
        config::update(&config_path, |t| {
            t.insert("listen".into(), listen.clone().into());
        })
        .unwrap();
        let embedded = Embedded::start(&url, &config_path, &listen, false, stopping.clone())
            .await
            .unwrap();
        assert!(embedded.addr().ip().is_loopback());
        assert_ne!(embedded.addr().port(), port);
        assert_eq!(embedded.lan().failed_port, Some(port));
        assert!(!embedded.lan().on);
        // Both settings need the saved port, which is still taken: the server stays where it is.
        let before = embedded.addr();
        for on in [true, false] {
            let err = embedded.set_lan(on).await.unwrap_err();
            assert_eq!(err, StartError::new("in_use", port.to_string()));
            assert_eq!(embedded.addr(), before);
            assert!(listening(&before.to_string()));
        }
        assert_eq!(embedded.lan().failed_port, Some(port));
        assert_eq!(config::load(&config_path).unwrap().listen, Some(listen));
        // Freed: switching off now works and clears the problem.
        drop(taken);
        let changed = embedded.set_lan(false).await.unwrap();
        assert_eq!(changed.lan.failed_port, None);
        assert_eq!(embedded.addr().port(), port);
        embedded.stop().await.unwrap();
        embedded.stop().await.unwrap();
        // Stopped: nothing to move.
        assert_eq!(embedded.set_lan(true).await.unwrap_err().code, "failed");

        // SULTAN_LISTEN: a failed bind is an error, a specific address is shown as is, and the
        // setting is locked.
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = blocker.local_addr().unwrap().to_string();
        let err = Embedded::start(&url, &config_path, &busy, true, stopping.clone())
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Serve { .. }), "{err}");
        let fixed = Embedded::start(&url, &config_path, "127.0.0.1:0", true, stopping)
            .await
            .unwrap();
        let lan = fixed.lan();
        assert!(lan.fixed && !lan.on);
        assert_eq!(fixed.set_lan(true).await.unwrap_err().code, "failed");
        fixed.stop().await.unwrap();
    }

    #[test]
    fn lan_addresses_skip_loopback_link_local_and_virtual_interfaces() {
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        let interfaces = [
            ("lo".to_owned(), ip("127.0.0.1"), true),
            ("eth0".to_owned(), ip("203.0.113.7"), true),
            ("wlan0".to_owned(), ip("192.168.1.10"), true),
            ("eth1".to_owned(), ip("10.0.0.5"), true),
            ("eth2".to_owned(), ip("192.168.2.1"), false),
            ("eth3".to_owned(), ip("169.254.10.1"), true),
            ("docker0".to_owned(), ip("172.17.0.1"), true),
            ("wlan0".to_owned(), ip("192.168.1.10"), true),
        ];
        assert_eq!(
            lan_addresses(&interfaces, 8080),
            ["10.0.0.5:8080", "192.168.1.10:8080", "203.0.113.7:8080"]
        );
        assert!(lan_addresses(&interfaces[..1], 8080).is_empty());
        assert_eq!(port_of("0.0.0.0:8080"), Some(8080));
        assert_eq!(port_of("nonsense"), None);
        let other = lan_error(&AppError::Config("x".into()), "0.0.0.0:1");
        assert_eq!(other.code, "failed");
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
