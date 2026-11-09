//! The Start screen flow of a single app or a terminal (`docs/plan.md` §3 "UI only, except
//! HQ"): a bare `sultan` opens the Start window ([`crate::Context::open_start`]) and, until it
//! is closed, opens what was chosen there and comes back to it:
//!
//! - **Register** → the register window ([`crate::Context::open_ui`]) at the lock screen, with
//!   F9 back to the Start screen;
//! - **Back office** → the back-office window ([`crate::Context::open_backoffice`]) against the
//!   same server, with F9 on its login screen back to the Start screen;
//! - **Set up again** → [`Next::SetupAgain`]: the caller opens the setup wizard;
//! - the window closed → [`Next::Quit`].
//!
//! Closing the register or the back office returns to the Start screen too. The language is
//! read again before each window, since the settings may have changed it ([`StartUi`]).
//!
//! [`AppStartService`] is the real [`StartService`]: the connection check is
//! `GET /registers/me/users` with the device token (an answer means the server is up and knows
//! this register; `invalid_device_token` means it was unpaired), and the language is saved as
//! `language` in `sultan.toml`.

use std::path::{Path, PathBuf};

use sultan_client::{HttpService, Service};
use sultan_ui::start::{AppKind, Choice, StartError, StartFuture, StartInfo, StartService};
use sultan_ui::start_app::server_text;

use crate::config::{self, Env, FileConfig, Mode, RegisterConfig};
use crate::error::Result;
use crate::{BackofficeUi, RegisterUi, StartUi, register};

/// What happens after the Start screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// The Start window was closed: quit.
    Quit,
    /// "Set up again": open the setup wizard.
    SetupAgain,
}

/// The windows the Start screen flow opens (see [`crate::Context`]).
pub struct Windows<'a> {
    /// The Start window.
    pub open_start: &'a mut dyn FnMut(StartUi) -> Result<Option<Choice>>,
    /// The register window.
    pub open_ui: &'a mut dyn FnMut(RegisterUi) -> Result<()>,
    /// The back-office window.
    pub open_backoffice: &'a mut dyn FnMut(BackofficeUi) -> Result<()>,
}

/// [`StartService`] for this machine.
#[derive(Debug, Clone)]
pub struct AppStartService {
    config_path: PathBuf,
    server: String,
    http: HttpService,
}

impl AppStartService {
    /// The service for the register paired at `server` with `device_token`, saving to
    /// `config_path`.
    #[must_use]
    pub fn new(config_path: &Path, server: &str, device_token: &str) -> Self {
        Self {
            config_path: config_path.to_path_buf(),
            server: server.to_owned(),
            http: HttpService::new(server, device_token),
        }
    }
}

impl StartService for AppStartService {
    fn check(&self) -> StartFuture {
        let users = self.http.lock_users();
        let server = server_text(&self.server);
        Box::pin(async move {
            match users.await {
                Ok(_) => Ok(()),
                Err(e) if e.code() == Some("invalid_device_token") => {
                    Err(StartError::new("unpaired", e.to_string()))
                }
                Err(_) => Err(StartError::new("network", server)),
            }
        })
    }

    fn set_language(&self, language: &str) -> std::result::Result<(), String> {
        config::set_language(&self.config_path, language).map_err(|e| e.to_string())
    }
}

/// The language of the system locale (`LC_ALL` > `LC_MESSAGES` > `LANG`): `id` for
/// Indonesian, otherwise `en`.
#[must_use]
pub fn system_language(env: Env<'_>) -> String {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(env)
        .unwrap_or_default();
    if locale.to_ascii_lowercase().starts_with("id") {
        "id".to_owned()
    } else {
        "en".to_owned()
    }
}

/// The language the windows use: `SULTAN_LANG` > `language` in the file > the system locale.
#[must_use]
pub fn language(env: Env<'_>, file: &FileConfig) -> String {
    config::resolve_language(env, file).unwrap_or_else(|| system_language(env))
}

/// The Start screen's view of this machine.
fn info(env: Env<'_>, file: &FileConfig, mode: Mode, register: &RegisterConfig) -> StartInfo {
    StartInfo {
        kind: if mode == Mode::Single {
            AppKind::Single
        } else {
            AppKind::Terminal
        },
        server: register.server.clone(),
        register_code: register.register_code.clone(),
        register_name: register.register_name.clone(),
        branch: register.branch.clone().unwrap_or_default(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        language: language(env, file),
        language_fixed: env(crate::ENV_LANG).is_some(),
    }
}

/// Runs the Start screen until it is closed or "set up again" is chosen (see the module docs).
/// `register.server` is where the server answers (a single app's own loopback URL).
///
/// # Errors
/// [`crate::AppError::File`] / [`crate::AppError::Config`] when `sultan.toml` cannot be read,
/// or a window's error.
pub fn run(
    config_path: &Path,
    env: Env<'_>,
    windows: &mut Windows<'_>,
    mode: Mode,
    register: &RegisterConfig,
    device_token: &str,
) -> Result<Next> {
    loop {
        let file = config::load(config_path)?;
        let ui = StartUi {
            config_path: config_path.to_path_buf(),
            info: info(env, &file, mode, register),
            device_token: device_token.to_owned(),
        };
        let language = Some(ui.info.language.clone());
        match (windows.open_start)(ui)? {
            None => return Ok(Next::Quit),
            Some(Choice::SetupAgain) => return Ok(Next::SetupAgain),
            Some(Choice::Register) => (windows.open_ui)(RegisterUi {
                register: register.clone(),
                device_token: device_token.to_owned(),
                language,
                start: true,
            })?,
            Some(Choice::BackOffice) => (windows.open_backoffice)(BackofficeUi {
                server: register.server.clone(),
                username: String::new(),
                language,
                start: true,
            })?,
        }
    }
}

/// A terminal: the Start screen against the paired server.
///
/// # Errors
/// [`crate::AppError::NotPaired`], see [`run`].
pub fn terminal(config_path: &Path, env: Env<'_>, windows: &mut Windows<'_>) -> Result<Next> {
    let (register, device_token) = register::paired(config_path)?;
    run(
        config_path,
        env,
        windows,
        Mode::Client,
        &register,
        &device_token,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::tests::TempDir;

    #[test]
    fn languages_come_from_env_file_or_locale() {
        let file = FileConfig::default();
        let cases: [(&[(&str, &str)], &str); 5] = [
            (&[], "en"),
            (&[("LANG", "id_ID.UTF-8")], "id"),
            (&[("LC_ALL", "en_US.UTF-8"), ("LANG", "id_ID.UTF-8")], "en"),
            (&[("LC_MESSAGES", "ID")], "id"),
            (&[("SULTAN_LANG", "en"), ("LANG", "id_ID")], "en"),
        ];
        for (pairs, expected) in cases {
            let env = |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| (*v).to_owned())
            };
            assert_eq!(language(&env, &file), expected, "{pairs:?}");
        }
        let saved = FileConfig {
            language: Some("id".into()),
            ..FileConfig::default()
        };
        assert_eq!(language(&|_| None, &saved), "id");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_check_knows_paired_and_unpaired_registers() {
        use sultan_core::time::Timestamp;

        let dir = TempDir::new("start-check");
        let config_path = dir.0.join("sultan.toml");
        let url = dir.sqlite_url();
        let now = Timestamp::from_unix_millis(1_800_000_000_000);
        let report = crate::init::init_database(&crate::init::tests::options(&url), now)
            .await
            .unwrap();
        crate::single::provision(&url, &report, &config_path, now)
            .await
            .unwrap();
        let (_, token) = register::paired(&config_path).unwrap();
        let server = crate::serve::BoundServer::bind(&url, "127.0.0.1:0", config::Role::Standalone)
            .await
            .unwrap();
        let root = format!("http://{}", server.addr());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        let paired = AppStartService::new(&config_path, &root, &token);
        paired.check().await.unwrap();
        let unpaired = AppStartService::new(&config_path, &root, "not-a-token");
        assert_eq!(unpaired.check().await.unwrap_err().code, "unpaired");
        let _ = stop.send(());
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn the_service_checks_the_server_and_saves_the_language() {
        let dir = TempDir::new("start-service");
        let config_path = dir.0.join("sultan.toml");
        // Nothing listens on port 1.
        let service = AppStartService::new(&config_path, "http://127.0.0.1:1", "t");
        let err = service.check().await.unwrap_err();
        assert_eq!(err, StartError::new("network", "127.0.0.1:1"));
        service.set_language("id").unwrap();
        assert_eq!(
            config::load(&config_path).unwrap().language.as_deref(),
            Some("id")
        );
        // A file that cannot be written.
        let blocked = AppStartService::new(&dir.0, "http://127.0.0.1:1", "t");
        assert!(blocked.set_language("en").is_err());
    }
}
