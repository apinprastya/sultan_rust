//! The `sultan` binary's logic as a library, so it can be tested without a process (plan Phase 7,
//! `docs/plan.md` §1: one binary for every node role).
//!
//! - [`cli`]: the command line (`setup`, `init`, `serve`, `register`, `backoffice`; none = by
//!   `mode`). On a single app or a terminal the commands are for scripts only: everything is
//!   reachable from the windows (`docs/plan.md` §3 "UI only, except HQ").
//! - [`setup`]: the work behind the setup wizard (`sultan_ui::setup`), opened by a bare `sultan`
//!   on a machine that is not set up, or by "set up again" on the Start screen, through
//!   [`Context::open_setup`] ([`open_setup_window`] is the real one).
//! - [`start`]: the Start screen of a single app or a terminal (register, back office,
//!   settings), opened by a bare `sultan` through [`Context::open_start`]
//!   ([`open_start_window`] is the real one); the register and the back office go back to it.
//! - [`single`]: the single app: its own register, and an embedded loopback server that runs
//!   while the Start screen is open (across register and back-office windows).
//! - [`config`]: `sultan.toml` + `SULTAN_*` environment variables; flag > env > file > default.
//! - [`init`]: first-run setup (company, first branch and location, Owner; spec 2.1 §1, §3, §4).
//! - [`serve`]: the API server for `standalone` / `hq` / `branch`, with graceful shutdown.
//! - [`register`]: register pairing (spec 2.1 §2); the device token lives in `register.token`.
//!   `sultan register` opens the register window (`sultan-ui`, plan Phase 8) for the paired
//!   register through [`Context::open_ui`]; [`open_register_window`] is the real one.
//! - `sultan backoffice` opens the back-office window (`sultan-ui`, phase C4) against the server
//!   of [`config::resolve_server`] through [`Context::open_backoffice`];
//!   [`open_backoffice_window`] is the real one.
//! - [`prompt`]: hidden prompts for the Owner's password and PIN.
//! - [`logging`]: `tracing` subscriber with a `RUST_LOG` filter.
//!
//! `main.rs` only parses the arguments, installs logging and calls [`run`] with the real
//! environment, terminal, shutdown signal and window.

pub mod cli;
pub mod config;
pub mod error;
pub mod init;
pub mod logging;
pub mod prompt;
pub mod register;
pub mod serve;
pub mod setup;
pub mod single;
pub mod start;

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cli::{BackofficeArgs, Cli, Command, InitArgs, RegisterAction, ServeArgs};
use config::{FileConfig, Mode};
use sultan_server::{Clock, SystemClock};
use sultan_ui::start::{Choice, StartInfo};

pub use error::{AppError, Result};

/// Environment variable choosing the windows' language (`id` or `en`); it overrides `language`
/// in `sultan.toml` (Start screen → Settings); default: the system locale.
pub const ENV_LANG: &str = "SULTAN_LANG";

/// What the register window needs: the paired register, its device token and the language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterUi {
    /// The paired register (`[register]` of `sultan.toml`).
    pub register: config::RegisterConfig,
    /// The device token (`register.token`).
    pub device_token: String,
    /// The language (`SULTAN_LANG` or `language` in `sultan.toml`), if set.
    pub language: Option<String>,
    /// Opened from the Start screen: F9 on the lock screen goes back to it.
    pub start: bool,
}

/// Opens the register window against the paired server and blocks until it is closed. Must be
/// called on the main thread, inside the tokio runtime (service calls are spawned on it).
///
/// # Errors
/// [`AppError::Ui`] when the window cannot be opened (no display) or fails.
pub fn open_register_window(ui: RegisterUi) -> Result<()> {
    let service = Arc::new(sultan_client::HttpService::new(
        &ui.register.server,
        &ui.device_token,
    ));
    let mut options = sultan_ui::Options::new(format!(
        "{} · {}",
        ui.register.register_code, ui.register.register_name
    ));
    options.language = ui.language;
    options.start = ui.start;
    sultan_ui::run(service, tokio::runtime::Handle::current(), options)
        .map_err(|e| AppError::Ui(e.to_string()))
}

/// What the back-office window needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackofficeUi {
    /// The server URL.
    pub server: String,
    /// The username to start with (empty: none).
    pub username: String,
    /// The language (`SULTAN_LANG` or `language` in `sultan.toml`), if set.
    pub language: Option<String>,
    /// Opened from the Start screen: F9 on the login screen goes back to it.
    pub start: bool,
}

/// Opens the back-office window against `ui.server` and blocks until it is closed. Must be
/// called on the main thread, inside the tokio runtime (service calls are spawned on it).
///
/// # Errors
/// [`AppError::Ui`] when the window cannot be opened (no display) or fails.
pub fn open_backoffice_window(ui: BackofficeUi) -> Result<()> {
    let service = Arc::new(sultan_client::HttpService::user(&ui.server));
    let mut options = sultan_ui::BackOfficeOptions::new(register::root_of(&ui.server));
    options.username = ui.username;
    options.language = ui.language;
    options.start = ui.start;
    sultan_ui::run_backoffice(service, tokio::runtime::Handle::current(), options)
        .map_err(|e| AppError::Ui(e.to_string()))
}

/// What the setup wizard needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupUi {
    /// The config file the wizard writes.
    pub config_path: PathBuf,
    /// The language (`SULTAN_LANG` or `language` in `sultan.toml`), if set.
    pub language: Option<String>,
}

/// What the Start screen needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartUi {
    /// The config file (the language is saved there).
    pub config_path: PathBuf,
    /// What the screen shows; `info.server` is where the server answers.
    pub info: StartInfo,
    /// The device token, for the connection check.
    pub device_token: String,
}

/// Opens the Start window and blocks until a choice is made (`Some`) or it is closed (`None`).
/// Must be called on the main thread, inside the tokio runtime.
///
/// # Errors
/// [`AppError::Ui`] when the window cannot be opened.
pub fn open_start_window(ui: StartUi) -> Result<Option<Choice>> {
    let service = Arc::new(start::AppStartService::new(
        &ui.config_path,
        &ui.info.server,
        &ui.device_token,
    ));
    sultan_ui::start_app::run(service, tokio::runtime::Handle::current(), ui.info)
        .map_err(|e| AppError::Ui(e.to_string()))
}

/// What a bare `sultan` prints on a machine set up as a server.
pub const SERVER_HINT: &str =
    "this machine is set up as a server; start it with: sultan serve (Ctrl+C stops it)";

/// What a bare `sultan` prints when the wizard was closed before the end.
pub const SETUP_NOT_FINISHED: &str =
    "setup was not finished; run `sultan` again to continue (or `sultan --help` for the commands)";

/// Opens the setup wizard and blocks until it is finished or closed. Returns the mode the
/// machine is now in, or `None` when the window was closed first. Must be called on the main
/// thread, inside the tokio runtime.
///
/// # Errors
/// [`AppError::Ui`] when the window cannot be opened.
pub fn open_setup_window(ui: SetupUi) -> Result<Option<Mode>> {
    let SetupUi {
        config_path,
        language,
    } = ui;
    let service = Arc::new(setup::AppSetupService::new(&config_path));
    let default_db = setup::default_db(&config_path);
    let path = default_db
        .strip_prefix("sqlite://")
        .unwrap_or(&default_db)
        .to_owned();
    sultan_ui::setup_app::run(
        service,
        tokio::runtime::Handle::current(),
        &path,
        language.as_deref(),
    )
    .map(|outcome| outcome.map(setup::mode_of))
    .map_err(|e| AppError::Ui(e.to_string()))
}

/// What a command needs from the outside world; tests pass fakes.
pub struct Context<'a, S> {
    /// Environment variables.
    pub env: config::Env<'a>,
    /// Asks for secrets `sultan init` was not given.
    pub prompter: &'a mut dyn prompt::Prompter,
    /// Where command output goes (stdout in the binary).
    pub out: &'a mut dyn Write,
    /// Completes when `sultan serve` should stop.
    pub shutdown: S,
    /// Opens the register window and returns when it is closed ([`open_register_window`] in the
    /// binary).
    pub open_ui: &'a mut dyn FnMut(RegisterUi) -> Result<()>,
    /// Opens the setup wizard and returns when it is closed ([`open_setup_window`] in the
    /// binary).
    pub open_setup: &'a mut dyn FnMut(SetupUi) -> Result<Option<Mode>>,
    /// Opens the back-office window and returns when it is closed
    /// ([`open_backoffice_window`] in the binary).
    pub open_backoffice: &'a mut dyn FnMut(BackofficeUi) -> Result<()>,
    /// Opens the Start screen and returns the choice, or `None` when it was closed
    /// ([`open_start_window`] in the binary).
    pub open_start: &'a mut dyn FnMut(StartUi) -> Result<Option<Choice>>,
}

fn say(out: &mut dyn Write, text: &str) -> Result<()> {
    writeln!(out, "{text}")
        .and_then(|()| out.flush())
        .map_err(|e| AppError::file("cannot write", "<stdout>", e))
}

/// Runs one command.
///
/// # Errors
/// The command's [`AppError`].
pub async fn run<S>(cli: Cli, ctx: Context<'_, S>) -> Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    let config_path = config::config_path(cli.config.as_deref(), ctx.env);
    let file = config::load(&config_path)?;
    match cli.command {
        None => bare(&config_path, ctx, false).await,
        Some(Command::Setup) => bare(&config_path, ctx, true).await,
        Some(Command::Init(args)) => {
            let secrets = (args.owner_password.clone(), args.owner_pin.clone());
            let password =
                prompt::obtain(prompt::OWNER_PASSWORD, secrets.0, ctx.env, ctx.prompter)?;
            let pin = prompt::obtain(prompt::OWNER_PIN, secrets.1, ctx.env, ctx.prompter)?;
            let db_url = config::resolve_db(args.db.as_deref(), ctx.env, &file, &config_path);
            let options = init_options(*args, db_url, password, pin);
            let report = init::init_database(&options, SystemClock.now()).await?;
            config::update(&config_path, |table| {
                table.insert("db".to_owned(), options.db_url.clone().into());
            })?;
            say(
                ctx.out,
                &format!(
                    "initialised {}: company \"{}\", branch {} ({}), stock location \"{}\", \
                     owner \"{}\"\nconfig written to {}\nnext: sultan serve",
                    options.db_url,
                    report.company.name,
                    report.branch.code,
                    report.branch.id,
                    report.location.name,
                    options.owner_username,
                    config_path.display()
                ),
            )
        }
        Some(Command::Serve(ServeArgs { role, listen, db })) => {
            let role = config::resolve_role(role, ctx.env, &file)?;
            let listen = config::resolve_listen(listen.as_deref(), ctx.env, &file);
            let db_url = config::resolve_db(db.as_deref(), ctx.env, &file, &config_path);
            let server = serve::BoundServer::bind(&db_url, &listen, role).await?;
            say(ctx.out, &server.banner())?;
            server.run(ctx.shutdown).await?;
            say(ctx.out, "sultan serve: stopped")
        }
        Some(Command::Backoffice(BackofficeArgs { server, user })) => {
            (ctx.open_backoffice)(BackofficeUi {
                server: config::resolve_server(server.as_deref(), ctx.env, &file),
                username: user.unwrap_or_default(),
                language: config::resolve_language(ctx.env, &file),
                start: false,
            })
        }
        Some(Command::Register(args)) => match args.action {
            None => open_paired(&config_path, ctx.env, &file, ctx.open_ui),
            Some(RegisterAction::Status) => say(ctx.out, &register::status(&config_path)?),
            Some(RegisterAction::Pair { server, code }) => {
                let paired = register::pair(&server, &code, &config_path).await?;
                say(
                    ctx.out,
                    &format!(
                        "paired: register {} ({}) at {}; details in {}, device token in {}",
                        paired.register_code,
                        paired.register_name,
                        paired.server,
                        config_path.display(),
                        register::token_path(&config_path).display()
                    ),
                )
            }
        },
    }
}

/// `sultan register`: the register window alone (no Start screen).
fn open_paired(
    config_path: &Path,
    env: config::Env<'_>,
    file: &FileConfig,
    open_ui: &mut dyn FnMut(RegisterUi) -> Result<()>,
) -> Result<()> {
    let (register, device_token) = register::paired(config_path)?;
    open_ui(RegisterUi {
        register,
        device_token,
        language: config::resolve_language(env, file),
        start: false,
    })
}

/// Completes once `shutdown` has, for any number of waiters (the single app's server is
/// started again after "set up again").
fn shared_shutdown<S>(shutdown: S) -> tokio::sync::watch::Receiver<bool>
where
    S: Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        let _ = tx.send(true);
    });
    rx
}

/// A bare `sultan` (or `sultan setup` with `wizard`): the wizard when the machine is not set up,
/// then what its mode says: the Start screen on a single app or a terminal (until it is closed;
/// "set up again" opens the wizard, and closing the wizard then returns to the Start screen),
/// the `sultan serve` hint on a server.
async fn bare<S>(config_path: &Path, ctx: Context<'_, S>, wizard: bool) -> Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    let Context {
        env,
        out,
        shutdown,
        open_ui,
        open_setup,
        open_backoffice,
        open_start,
        ..
    } = ctx;
    let stop = shared_shutdown(shutdown);
    let mut windows = start::Windows {
        open_start,
        open_ui,
        open_backoffice,
    };
    let mut wizard = wizard;
    let mut from_start = false;
    loop {
        let file = config::load(config_path)?;
        let known = if wizard {
            None
        } else {
            config::resolve_mode(&file)?
        };
        let mode = if let Some(mode) = known {
            mode
        } else {
            let ui = SetupUi {
                config_path: config_path.to_path_buf(),
                language: config::resolve_language(env, &file),
            };
            match (open_setup)(ui)? {
                Some(mode) => mode,
                // Closed early after "set up again": back to the Start screen.
                None => match config::resolve_mode(&config::load(config_path)?)? {
                    Some(mode) if from_start && mode != Mode::Server => mode,
                    _ => return say(out, SETUP_NOT_FINISHED),
                },
            }
        };
        // The wizard rewrote the file.
        let file = config::load(config_path)?;
        let next = match mode {
            Mode::Single => {
                let mut stop = stop.clone();
                let stopped = async move {
                    let _ = stop.wait_for(|stopped| *stopped).await;
                };
                single::run(config_path, &file, env, &mut windows, stopped).await?
            }
            Mode::Server => return say(out, SERVER_HINT),
            Mode::Client => start::terminal(config_path, env, &mut windows)?,
        };
        match next {
            start::Next::Quit => return Ok(()),
            start::Next::SetupAgain => {
                wizard = true;
                from_start = true;
            }
        }
    }
}

fn init_options(
    args: InitArgs,
    db_url: String,
    password: String,
    pin: String,
) -> init::InitOptions {
    init::InitOptions {
        db_url,
        company: args.company,
        branch_name: args.branch_name.unwrap_or_else(|| args.branch.clone()),
        branch_code: args.branch,
        branch_address: args.branch_address,
        timezone: args.timezone,
        location_name: args.location_name,
        owner_username: args.owner,
        owner_display_name: args.owner_name,
        owner_password: password,
        owner_pin: pin,
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::init::tests::TempDir;
    use crate::prompt::tests::FakePrompter;

    async fn run_args<S>(
        args: &[&str],
        env: config::Env<'_>,
        prompter: &mut FakePrompter,
        shutdown: S,
    ) -> (Result<()>, String)
    where
        S: Future<Output = ()> + Send + 'static,
    {
        let cli = Cli::try_parse_from(args).unwrap();
        let mut out = Vec::new();
        let mut no_window = |_: RegisterUi| -> Result<()> { panic!("no window expected") };
        let mut no_setup = |_: SetupUi| -> Result<Option<Mode>> { panic!("no wizard expected") };
        let mut no_backoffice =
            |_: BackofficeUi| -> Result<()> { panic!("no back office expected") };
        let mut no_start = |_: StartUi| -> Result<Option<Choice>> { panic!("no start expected") };
        let ctx = Context {
            env,
            prompter,
            out: &mut out,
            shutdown,
            open_ui: &mut no_window,
            open_setup: &mut no_setup,
            open_backoffice: &mut no_backoffice,
            open_start: &mut no_start,
        };
        let result = run(cli, ctx).await;
        (result, String::from_utf8(out).unwrap())
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[tokio::test]
    async fn init_then_serve_then_status_through_run() {
        let dir = TempDir::new("run");
        let config = dir.0.join("sultan.toml");
        let config = config.to_str().unwrap();
        let mut prompter =
            FakePrompter::answering(&["owner-pass-1", "owner-pass-1", "1234", "1234"]);
        let init = [
            "sultan",
            "--config",
            config,
            "init",
            "--company",
            "PT A",
            "--branch",
            "JKT01",
            "--owner",
            "owner",
        ];
        let (result, out) = run_args(&init, &no_env, &mut prompter, async {}).await;
        result.unwrap();
        assert!(out.contains("branch JKT01"), "{out}");
        assert_eq!(prompter.asked.len(), 4, "password and PIN, twice each");
        let written = config::load(std::path::Path::new(config)).unwrap();
        assert_eq!(
            written.db,
            Some(dir.sqlite_url()),
            "default db next to the config"
        );

        // Second init: refused (secrets come from the environment this time).
        let env = |k: &str| match k {
            "SULTAN_OWNER_PASSWORD" => Some("owner-pass-1".to_owned()),
            "SULTAN_OWNER_PIN" => Some("1234".to_owned()),
            _ => None,
        };
        let mut quiet = FakePrompter::default();
        let (result, _) = run_args(&init, &env, &mut quiet, async {}).await;
        assert!(matches!(result, Err(AppError::AlreadyInitialised(_))));

        // Serve with an immediate shutdown; the db comes from the file, the listen address from
        // the environment.
        let env = |k: &str| (k == "SULTAN_LISTEN").then(|| "127.0.0.1:0".to_owned());
        let serve = ["sultan", "--config", config, "serve", "--role", "hq"];
        let (result, out) = run_args(&serve, &env, &mut quiet, async {}).await;
        result.unwrap();
        assert!(
            out.starts_with("sultan serve: role hq, API http://127.0.0.1:"),
            "{out}"
        );
        assert!(out.ends_with("sultan serve: stopped\n"), "{out}");

        let status = ["sultan", "--config", config, "register", "status"];
        let (result, out) = run_args(&status, &no_env, &mut quiet, async {}).await;
        result.unwrap();
        assert!(out.starts_with("not paired"), "{out}");
        // Not paired: no window, the same advice as an error.
        let open = ["sultan", "--config", config, "register"];
        let (result, _) = run_args(&open, &no_env, &mut quiet, async {}).await;
        assert!(matches!(result, Err(AppError::NotPaired)));

        let pair = [
            "sultan",
            "--config",
            config,
            "register",
            "pair",
            "--server",
            "http://127.0.0.1:1",
            "--code",
            "x",
        ];
        let (result, _) = run_args(&pair, &no_env, &mut quiet, async {}).await;
        assert!(matches!(result, Err(AppError::Pair(_))));
    }

    #[tokio::test]
    async fn bad_settings_stop_before_any_work() {
        let dir = TempDir::new("run-bad");
        let config = dir.0.join("sultan.toml");
        std::fs::write(&config, "role = \"till\"\n").unwrap();
        let config = config.to_str().unwrap();
        let mut quiet = FakePrompter::default();
        let serve = ["sultan", "--config", config, "serve"];
        let (result, _) = run_args(&serve, &no_env, &mut quiet, async {}).await;
        assert!(matches!(result, Err(AppError::Config(_))));

        let init = [
            "sultan",
            "--config",
            config,
            "init",
            "--company",
            "PT A",
            "--branch",
            "JKT01",
            "--owner",
            "owner",
            "--owner-password",
            "owner-pass-1",
        ];
        let (result, _) = run_args(&init, &no_env, &mut quiet, async {}).await;
        assert!(matches!(result, Err(AppError::MissingSecret(_))));

        std::fs::write(dir.0.join("sultan.toml"), "db = [").unwrap();
        let (result, _) = run_args(&serve, &no_env, &mut quiet, async {}).await;
        assert!(matches!(result, Err(AppError::Config(_))));
    }

    #[tokio::test]
    async fn register_opens_the_window_of_the_paired_register() {
        let dir = TempDir::new("run-ui");
        let config_path = dir.0.join("sultan.toml");
        let register = config::RegisterConfig {
            server: "http://127.0.0.1:1".into(),
            register_id: "r".into(),
            branch_id: "b".into(),
            register_code: "R1".into(),
            register_name: "Kasir 1".into(),
            branch: None,
        };
        config::set_register(&config_path, &register).unwrap();
        config::write_private(&register::token_path(&config_path), b"device\n").unwrap();

        let mut opened = Vec::new();
        let mut window = |ui: RegisterUi| -> Result<()> {
            opened.push(ui);
            Ok(())
        };
        let mut out = Vec::new();
        let mut quiet = FakePrompter::default();
        let env = |k: &str| (k == ENV_LANG).then(|| "id".to_owned());
        let cli = Cli::try_parse_from([
            "sultan",
            "--config",
            config_path.to_str().unwrap(),
            "register",
        ])
        .unwrap();
        let mut no_setup = |_: SetupUi| -> Result<Option<Mode>> { panic!("no wizard expected") };
        let mut no_backoffice =
            |_: BackofficeUi| -> Result<()> { panic!("no back office expected") };
        let mut no_start = |_: StartUi| -> Result<Option<Choice>> { panic!("no start expected") };
        let ctx = Context {
            env: &env,
            prompter: &mut quiet,
            out: &mut out,
            shutdown: async {},
            open_ui: &mut window,
            open_setup: &mut no_setup,
            open_backoffice: &mut no_backoffice,
            open_start: &mut no_start,
        };
        run(cli, ctx).await.unwrap();
        assert_eq!(
            opened,
            [RegisterUi {
                register,
                device_token: "device".into(),
                language: Some("id".into()),
                start: false,
            }]
        );
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn backoffice_opens_its_window_with_the_resolved_server() {
        let dir = TempDir::new("backoffice");
        let config_path = dir.0.join("sultan.toml");
        config::update(&config_path, |t| {
            t.insert("listen".into(), "0.0.0.0:9090".into());
        })
        .unwrap();
        let mut opened = Vec::new();
        let mut backoffice = |ui: BackofficeUi| -> Result<()> {
            opened.push(ui);
            Ok(())
        };
        let env = |k: &str| (k == ENV_LANG).then(|| "en".to_owned());
        for args in [
            vec!["backoffice", "--user", "owner"],
            vec!["backoffice", "--server", "http://hq:8080/api/v1"],
        ] {
            let mut all = vec!["sultan", "--config", config_path.to_str().unwrap()];
            all.extend(args);
            let cli = Cli::try_parse_from(all).unwrap();
            let mut out = Vec::new();
            let mut quiet = FakePrompter::default();
            let mut no_window = |_: RegisterUi| -> Result<()> { panic!("no register expected") };
            let mut no_setup =
                |_: SetupUi| -> Result<Option<Mode>> { panic!("no wizard expected") };
            let mut no_start =
                |_: StartUi| -> Result<Option<Choice>> { panic!("no start expected") };
            let ctx = Context {
                env: &env,
                prompter: &mut quiet,
                out: &mut out,
                shutdown: async {},
                open_ui: &mut no_window,
                open_setup: &mut no_setup,
                open_backoffice: &mut backoffice,
                open_start: &mut no_start,
            };
            run(cli, ctx).await.unwrap();
        }
        assert_eq!(
            opened,
            [
                BackofficeUi {
                    server: "http://127.0.0.1:9090".into(),
                    username: "owner".into(),
                    language: Some("en".into()),
                    start: false,
                },
                BackofficeUi {
                    server: "http://hq:8080/api/v1".into(),
                    username: String::new(),
                    language: Some("en".into()),
                    start: false,
                }
            ]
        );
    }

    #[test]
    fn init_options_default_the_branch_name_to_its_code() {
        let cli = Cli::try_parse_from([
            "sultan",
            "init",
            "--company",
            "C",
            "--branch",
            "B1",
            "--owner",
            "o",
        ])
        .unwrap();
        let Some(Command::Init(args)) = cli.command else {
            panic!()
        };
        let options = init_options(*args, "db".into(), "p".into(), "1".into());
        assert_eq!(options.branch_name, "B1");
    }

    /// What the fake windows saw.
    #[derive(Default)]
    struct Seen {
        out: String,
        starts: Vec<StartUi>,
        windows: Vec<RegisterUi>,
        backoffices: Vec<BackofficeUi>,
        wizards: Vec<SetupUi>,
    }

    /// Runs `args` with fake windows: the wizard is `wizard`, the Start screen answers `choices`
    /// in turn (and quits when they run out), the register and back office just close.
    async fn run_bare(
        args: &[&str],
        mut wizard: impl FnMut(&SetupUi) -> Result<Option<Mode>>,
        choices: &[Option<Choice>],
    ) -> (Result<()>, Seen) {
        let cli = Cli::try_parse_from(args).unwrap();
        let mut out = Vec::new();
        let seen = std::cell::RefCell::new(Seen::default());
        let mut choices = choices.iter().copied();
        let mut window = |ui: RegisterUi| -> Result<()> {
            seen.borrow_mut().windows.push(ui);
            Ok(())
        };
        let mut backoffice = |ui: BackofficeUi| -> Result<()> {
            seen.borrow_mut().backoffices.push(ui);
            Ok(())
        };
        let mut setup = |ui: SetupUi| -> Result<Option<Mode>> {
            let mode = wizard(&ui);
            seen.borrow_mut().wizards.push(ui);
            mode
        };
        let mut start = |ui: StartUi| -> Result<Option<Choice>> {
            seen.borrow_mut().starts.push(ui);
            Ok(choices.next().flatten())
        };
        let mut quiet = FakePrompter::default();
        let env = |k: &str| (k == ENV_LANG).then(|| "en".to_owned());
        let ctx = Context {
            env: &env,
            prompter: &mut quiet,
            out: &mut out,
            shutdown: async {},
            open_ui: &mut window,
            open_setup: &mut setup,
            open_backoffice: &mut backoffice,
            open_start: &mut start,
        };
        let result = run(cli, ctx).await;
        let mut seen = seen.into_inner();
        seen.out = String::from_utf8(out).unwrap();
        (result, seen)
    }

    /// What the real wizard does for a single app.
    fn set_up_single(ui: &SetupUi) -> Result<Option<Mode>> {
        let service = setup::AppSetupService::new(&ui.config_path);
        let request = sultan_ui::setup::ShopRequest {
            app_type: sultan_ui::setup::AppType::Single,
            db_url: None,
            listen: None,
            company: "PT A".into(),
            branch_code: "JKT01".into(),
            branch_name: "Toko".into(),
            timezone: "Asia/Jakarta".into(),
            location_name: "Store floor".into(),
            owner_username: "owner".into(),
            owner_display_name: "Owner".into(),
            owner_password: "owner-pass-1".into(),
            owner_pin: "1234".into(),
        };
        let handle = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            handle.block_on(sultan_ui::setup::SetupService::init_shop(
                &service, &request,
            ))
        })
        .join()
        .unwrap()
        .map_err(|e| AppError::Invalid(e.code))?;
        Ok(Some(Mode::Single))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bare_sultan_opens_the_wizard_then_the_start_screen() {
        let dir = TempDir::new("run-bare");
        let config_path = dir.0.join("sultan.toml");
        let config = config_path.to_str().unwrap();
        let bare = ["sultan", "--config", config];

        // Closed before the end: a hint, nothing else.
        let (result, seen) = run_bare(&bare, |_| Ok(None), &[]).await;
        result.unwrap();
        assert_eq!(seen.out, format!("{SETUP_NOT_FINISHED}\n"));
        assert!(seen.starts.is_empty() && seen.windows.is_empty());
        assert_eq!(
            seen.wizards,
            [SetupUi {
                config_path: config_path.clone(),
                language: Some("en".into()),
            }]
        );

        // The wizard sets up a single app, then the Start screen opens against the embedded
        // server: the register, then the back office, then it is closed.
        let choices = [Some(Choice::Register), Some(Choice::BackOffice), None];
        let (result, seen) = run_bare(&bare, set_up_single, &choices).await;
        result.unwrap();
        assert!(seen.out.is_empty(), "{}", seen.out);
        assert_eq!(seen.starts.len(), 3);
        let server = seen.starts[0].info.server.clone();
        assert!(server.starts_with("http://127.0.0.1:"), "{server}");
        assert!(seen.starts.iter().all(|s| s.info.server == server));
        assert_eq!(seen.starts[0].info.register_code, "R1");
        assert_eq!(seen.starts[0].info.branch, "JKT01 · Toko");
        assert!(seen.starts[0].info.language_fixed, "SULTAN_LANG is set");
        assert_eq!(seen.windows.len(), 1);
        assert_eq!(seen.windows[0].register.server, server);
        assert_eq!(seen.windows[0].language.as_deref(), Some("en"));
        assert!(seen.windows[0].start);
        assert_eq!(seen.backoffices[0].server, server);

        // Next start: straight to the Start screen, no wizard.
        let (result, seen) = run_bare(&bare, |_| panic!("no wizard expected"), &[None]).await;
        result.unwrap();
        assert_eq!((seen.starts.len(), seen.wizards.len()), (1, 0));

        // "Set up again": the wizard opens; closed early, the Start screen comes back (with the
        // embedded server started again).
        let choices = [Some(Choice::SetupAgain), None];
        let (result, seen) = run_bare(&bare, |_| Ok(None), &choices).await;
        result.unwrap();
        assert_eq!((seen.starts.len(), seen.wizards.len()), (2, 1));
        assert!(seen.out.is_empty(), "{}", seen.out);
        assert_ne!(seen.starts[0].info.server, seen.starts[1].info.server);

        // The wizard refuses the database that is already set up (it is never overwritten); an
        // error there is passed on.
        let (result, seen) = run_bare(&bare, set_up_single, &[Some(Choice::SetupAgain)]).await;
        assert!(
            matches!(&result, Err(AppError::Invalid(code)) if code == "already_initialised"),
            "{result:?}"
        );
        assert_eq!(seen.wizards.len(), 1);

        // `sultan setup` opens the wizard again; closed early it says so.
        let setup_args = ["sultan", "--config", config, "setup"];
        let (result, seen) = run_bare(&setup_args, |_| Ok(None), &[]).await;
        result.unwrap();
        assert_eq!(seen.wizards.len(), 1);
        assert!(seen.out.starts_with("setup was not finished"));

        // A wizard that cannot open is an error.
        let (result, _) =
            run_bare(&setup_args, |_| Err(AppError::Ui("no display".into())), &[]).await;
        assert!(matches!(result, Err(AppError::Ui(_))));
    }

    #[tokio::test]
    async fn bare_sultan_on_a_server_and_on_a_terminal() {
        let dir = TempDir::new("run-bare-modes");
        let config_path = dir.0.join("sultan.toml");
        let config = config_path.to_str().unwrap();
        let bare = ["sultan", "--config", config];

        // An older config with only `db`: a server.
        std::fs::write(&config_path, "db = \"sqlite:///x.db\"\n").unwrap();
        let (result, seen) = run_bare(&bare, |_| panic!("no wizard expected"), &[]).await;
        result.unwrap();
        assert_eq!(seen.out, format!("{SERVER_HINT}\n"));
        assert!(seen.windows.is_empty() && seen.wizards.is_empty() && seen.starts.is_empty());

        // The wizard leaves a server.
        std::fs::remove_file(&config_path).unwrap();
        let (result, seen) = run_bare(&bare, |_| Ok(Some(Mode::Server)), &[]).await;
        result.unwrap();
        assert_eq!(seen.out, format!("{SERVER_HINT}\n"));

        // A terminal opens the Start screen of its paired register (also when its server is
        // unreachable: the screen checks); the register and the back office go to that server.
        let register = config::RegisterConfig {
            server: "http://10.0.0.1:8080".into(),
            register_id: "r".into(),
            branch_id: "b".into(),
            register_code: "R2".into(),
            register_name: "Kasir 2".into(),
            branch: Some("JKT01 · Toko".into()),
        };
        config::set_register(&config_path, &register).unwrap();
        config::update(&config_path, |t| {
            t.insert("mode".into(), "client".into());
        })
        .unwrap();
        config::write_private(&register::token_path(&config_path), b"dev\n").unwrap();
        let choices = [Some(Choice::Register), Some(Choice::BackOffice), None];
        let (result, seen) = run_bare(&bare, |_| panic!("no wizard expected"), &choices).await;
        result.unwrap();
        let info = &seen.starts[0].info;
        assert_eq!(info.kind, sultan_ui::start::AppKind::Terminal);
        assert_eq!(info.server, "http://10.0.0.1:8080");
        assert_eq!(
            (info.register_code.as_str(), info.branch.as_str()),
            ("R2", "JKT01 · Toko")
        );
        assert_eq!(seen.starts[0].device_token, "dev");
        assert_eq!(seen.starts[0].config_path, config_path);
        assert_eq!(seen.windows[0].register, register);
        assert_eq!(seen.windows[0].device_token, "dev");
        assert_eq!(seen.backoffices[0].server, "http://10.0.0.1:8080");
        assert_eq!(seen.starts.len(), 3);

        // "Set up again" on a terminal: the wizard pairs it with another server.
        let moved = config::RegisterConfig {
            server: "http://10.0.0.2:8080".into(),
            ..register.clone()
        };
        let choices = [Some(Choice::SetupAgain), None];
        let (result, seen) = run_bare(
            &bare,
            |ui| {
                config::set_register(&ui.config_path, &moved).unwrap();
                Ok(Some(Mode::Client))
            },
            &choices,
        )
        .await;
        result.unwrap();
        assert_eq!(seen.starts[1].info.server, "http://10.0.0.2:8080");

        // A terminal whose pairing is gone.
        std::fs::remove_file(register::token_path(&config_path)).unwrap();
        let (result, _) = run_bare(&bare, |_| panic!("no wizard expected"), &[]).await;
        assert!(matches!(result, Err(AppError::NotPaired)));

        // An unknown mode is a config error.
        config::update(&config_path, |t| {
            t.insert("mode".into(), "till".into());
        })
        .unwrap();
        let (result, _) = run_bare(&bare, |_| panic!("no wizard expected"), &[]).await;
        assert!(matches!(result, Err(AppError::Config(_))));
    }

    #[test]
    fn say_reports_write_errors() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("closed"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(say(&mut Broken, "x").is_err());
    }
}
