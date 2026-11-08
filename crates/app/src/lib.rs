//! The `sultan` binary's logic as a library, so it can be tested without a process (plan Phase 7,
//! `docs/plan.md` §1: one binary for every node role).
//!
//! - [`cli`]: the command line (`setup`, `init`, `serve`, `register`; none = by `mode`).
//! - [`setup`]: the work behind the setup wizard (`sultan_ui::setup`), opened by a bare `sultan`
//!   on a machine that is not set up, through [`Context::open_setup`] ([`open_setup_window`] is
//!   the real one).
//! - [`single`]: the single app: its own register, and an embedded loopback server for the
//!   register window.
//! - [`config`]: `sultan.toml` + `SULTAN_*` environment variables; flag > env > file > default.
//! - [`init`]: first-run setup (company, first branch and location, Owner; spec 2.1 §1, §3, §4).
//! - [`serve`]: the API server for `standalone` / `hq` / `branch`, with graceful shutdown.
//! - [`register`]: register pairing (spec 2.1 §2); the device token lives in `register.token`.
//!   `sultan register` opens the register window (`sultan-ui`, plan Phase 8) for the paired
//!   register through [`Context::open_ui`]; [`open_register_window`] is the real one.
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

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cli::{Cli, Command, InitArgs, RegisterAction, ServeArgs};
use config::{FileConfig, Mode};
use sultan_server::{Clock, SystemClock};

pub use error::{AppError, Result};

/// Environment variable choosing the register window's language (`id` or `en`); default: the
/// system locale.
pub const ENV_LANG: &str = "SULTAN_LANG";

/// What the register window needs: the paired register, its device token and the language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterUi {
    /// The paired register (`[register]` of `sultan.toml`).
    pub register: config::RegisterConfig,
    /// The device token (`register.token`).
    pub device_token: String,
    /// `SULTAN_LANG`, if set.
    pub language: Option<String>,
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
    sultan_ui::run(service, tokio::runtime::Handle::current(), options)
        .map_err(|e| AppError::Ui(e.to_string()))
}

/// What the setup wizard needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupUi {
    /// The config file the wizard writes.
    pub config_path: PathBuf,
    /// `SULTAN_LANG`, if set.
    pub language: Option<String>,
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
        None => bare(&config_path, &file, ctx, false).await,
        Some(Command::Setup) => bare(&config_path, &file, ctx, true).await,
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
        Some(Command::Register(args)) => match args.action {
            None => open_paired(&config_path, ctx.env, ctx.open_ui),
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

fn open_paired(
    config_path: &Path,
    env: config::Env<'_>,
    open_ui: &mut dyn FnMut(RegisterUi) -> Result<()>,
) -> Result<()> {
    let (register, device_token) = register::paired(config_path)?;
    open_ui(RegisterUi {
        register,
        device_token,
        language: env(ENV_LANG),
    })
}

/// A bare `sultan` (or `sultan setup` with `wizard`): the wizard when the machine is not set up,
/// then what its mode says.
async fn bare<S>(
    config_path: &Path,
    file: &FileConfig,
    ctx: Context<'_, S>,
    wizard: bool,
) -> Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    let known = if wizard {
        None
    } else {
        config::resolve_mode(file)?
    };
    let mode = if let Some(mode) = known {
        mode
    } else {
        let ui = SetupUi {
            config_path: config_path.to_path_buf(),
            language: (ctx.env)(ENV_LANG),
        };
        let Some(mode) = (ctx.open_setup)(ui)? else {
            return say(ctx.out, SETUP_NOT_FINISHED);
        };
        mode
    };
    // The wizard rewrote the file.
    let file = config::load(config_path)?;
    match mode {
        Mode::Single => single::run(config_path, &file, ctx.env, ctx.open_ui, ctx.shutdown).await,
        Mode::Server => say(ctx.out, SERVER_HINT),
        Mode::Client => open_paired(config_path, ctx.env, ctx.open_ui),
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
        let ctx = Context {
            env,
            prompter,
            out: &mut out,
            shutdown,
            open_ui: &mut no_window,
            open_setup: &mut no_setup,
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
        let ctx = Context {
            env: &env,
            prompter: &mut quiet,
            out: &mut out,
            shutdown: async {},
            open_ui: &mut window,
            open_setup: &mut no_setup,
        };
        run(cli, ctx).await.unwrap();
        assert_eq!(
            opened,
            [RegisterUi {
                register,
                device_token: "device".into(),
                language: Some("id".into()),
            }]
        );
        assert!(out.is_empty());
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

    /// Runs `args` with fake windows; returns the result, the output, the windows opened and
    /// the wizards opened.
    async fn run_bare(
        args: &[&str],
        mut wizard: impl FnMut(&SetupUi) -> Result<Option<Mode>>,
    ) -> (Result<()>, String, Vec<RegisterUi>, Vec<SetupUi>) {
        let cli = Cli::try_parse_from(args).unwrap();
        let mut out = Vec::new();
        let mut windows = Vec::new();
        let mut wizards = Vec::new();
        let mut window = |ui: RegisterUi| -> Result<()> {
            windows.push(ui);
            Ok(())
        };
        let mut setup = |ui: SetupUi| -> Result<Option<Mode>> {
            let mode = wizard(&ui);
            wizards.push(ui);
            mode
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
        };
        let result = run(cli, ctx).await;
        (result, String::from_utf8(out).unwrap(), windows, wizards)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bare_sultan_opens_the_wizard_then_follows_the_mode() {
        let dir = TempDir::new("run-bare");
        let config_path = dir.0.join("sultan.toml");
        let config = config_path.to_str().unwrap();
        let bare = ["sultan", "--config", config];

        // Closed before the end: a hint, nothing else.
        let (result, out, windows, wizards) = run_bare(&bare, |_| Ok(None)).await;
        result.unwrap();
        assert_eq!(out, format!("{SETUP_NOT_FINISHED}\n"));
        assert!(windows.is_empty());
        assert_eq!(
            wizards,
            [SetupUi {
                config_path: config_path.clone(),
                language: Some("en".into()),
            }]
        );

        // The wizard sets up a single app (as the real one does), then the register opens
        // against the embedded server.
        let (result, out, windows, _) = run_bare(&bare, |ui| {
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
            .unwrap();
            Ok(Some(Mode::Single))
        })
        .await;
        result.unwrap();
        assert!(out.is_empty(), "{out}");
        assert_eq!(windows.len(), 1);
        assert!(windows[0].register.server.starts_with("http://127.0.0.1:"));
        assert_eq!(windows[0].register.register_code, "R1");
        assert_eq!(windows[0].language.as_deref(), Some("en"));

        // Next start: straight to the register, no wizard.
        let (result, _, windows, wizards) = run_bare(&bare, |_| panic!("no wizard expected")).await;
        result.unwrap();
        assert_eq!((windows.len(), wizards.len()), (1, 0));

        // `sultan setup` opens the wizard again.
        let setup_args = ["sultan", "--config", config, "setup"];
        let (result, out, _, wizards) = run_bare(&setup_args, |_| Ok(None)).await;
        result.unwrap();
        assert_eq!(wizards.len(), 1);
        assert!(out.starts_with("setup was not finished"));

        // A wizard that cannot open is an error.
        let (result, ..) = run_bare(&setup_args, |_| Err(AppError::Ui("no display".into()))).await;
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
        let (result, out, windows, wizards) =
            run_bare(&bare, |_| panic!("no wizard expected")).await;
        result.unwrap();
        assert_eq!(out, format!("{SERVER_HINT}\n"));
        assert!(windows.is_empty() && wizards.is_empty());

        // The wizard leaves a server.
        std::fs::remove_file(&config_path).unwrap();
        let (result, out, ..) = run_bare(&bare, |_| Ok(Some(Mode::Server))).await;
        result.unwrap();
        assert_eq!(out, format!("{SERVER_HINT}\n"));

        // A terminal opens the window of its paired register.
        let register = config::RegisterConfig {
            server: "http://10.0.0.1:8080".into(),
            register_id: "r".into(),
            branch_id: "b".into(),
            register_code: "R2".into(),
            register_name: "Kasir 2".into(),
        };
        config::set_register(&config_path, &register).unwrap();
        config::update(&config_path, |t| {
            t.insert("mode".into(), "client".into());
        })
        .unwrap();
        config::write_private(&register::token_path(&config_path), b"dev\n").unwrap();
        let (result, _, windows, _) = run_bare(&bare, |_| panic!("no wizard expected")).await;
        result.unwrap();
        assert_eq!(windows[0].register, register);
        assert_eq!(windows[0].device_token, "dev");

        // An unknown mode is a config error.
        config::update(&config_path, |t| {
            t.insert("mode".into(), "till".into());
        })
        .unwrap();
        let (result, ..) = run_bare(&bare, |_| panic!("no wizard expected")).await;
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
