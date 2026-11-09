//! The work behind the setup wizard (`sultan_ui::setup`): [`AppSetupService`] implements
//! [`SetupService`] with the same code as the CLI commands.
//!
//! - `check_db`: connects (a `SQLite` file is created) and reports whether a company exists.
//! - `init_shop`: [`init::init_database`] (all of `sultan init`'s rules), then `sultan.toml`:
//!   `db`, `mode` and, for an HQ server, `listen` and `role = "hq"`. A single app also gets its
//!   own register ([`single::provision`]).
//! - `check_server`: `GET /api/v1/openapi.json` must answer with an `OpenAPI` document.
//! - `manager_login`: `POST /auth/password`, then `GET /branches` (the active ones).
//! - `register_terminal`: `PUT /registers/{new id}`, `POST /registers/{id}/pairing-code`,
//!   `POST /registers/pair`, then `POST /auth/logout`. The manager's token is never stored. A
//!   branch server refuses with `read_only_replica` (registers are HQ data), a user without
//!   `org.manage` with `permission_denied`.
//! - `pair_with_code`: `POST /registers/pair`.
//!
//! Pairing saves like `sultan register pair` ([`register::store`]) and sets `mode = "client"`.
//! Errors become [`SetupError`] codes the wizard translates; server problem codes it knows are
//! passed on, anything else is `failed` with the detail.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use sultan_core::ids::RegisterId;
use sultan_db::{Db, org};
use sultan_server::{Clock, SystemClock};
use sultan_ui::setup::{
    AppType, BranchChoice, DbCheck, ManagerSession, Paired, SetupError, SetupFuture, SetupService,
    ShopReport, ShopRequest, TerminalRequest,
};

use crate::config::{self, FileConfig, Mode};
use crate::error::AppError;
use crate::init::{self, InitOptions};
use crate::register::{self, PairResponse, api_base};
use crate::single;

/// Server problem codes the wizard has a message for.
const KNOWN_PROBLEMS: [&str; 6] = [
    "invalid_credentials",
    "locked_out",
    "permission_denied",
    "read_only_replica",
    "invalid_pairing_code",
    "conflict",
];

/// [`SetupService`] for the machine whose config file is `config_path`.
#[derive(Debug, Clone)]
pub struct AppSetupService {
    config_path: PathBuf,
    client: reqwest::Client,
}

impl AppSetupService {
    /// The service for `config_path`.
    #[must_use]
    pub fn new(config_path: &Path) -> Self {
        Self {
            config_path: config_path.to_path_buf(),
            client: reqwest::Client::new(),
        }
    }
}

/// The database a single app uses: `sultan.db` next to the config file.
#[must_use]
pub fn default_db(config_path: &Path) -> String {
    config::resolve_db(None, &|_| None, &FileConfig::default(), config_path)
}

/// The mode a finished wizard leaves the machine in.
#[must_use]
pub const fn mode_of(app_type: AppType) -> Mode {
    match app_type {
        AppType::Single => Mode::Single,
        AppType::Hq => Mode::Server,
        AppType::Branch | AppType::Terminal => Mode::Client,
    }
}

/// An [`AppError`] as a wizard message.
#[must_use]
pub fn setup_error(e: &AppError) -> SetupError {
    match e {
        AppError::Invalid(detail) => SetupError::new("invalid", detail.as_str()),
        AppError::AlreadyInitialised(company) => {
            SetupError::new("already_initialised", company.as_str())
        }
        AppError::Db(e) => SetupError::new("db", e.to_string()),
        AppError::File { .. } | AppError::Config(_) => SetupError::new("file", e.to_string()),
        other => SetupError::new("failed", other.to_string()),
    }
}

fn network(e: &reqwest::Error) -> SetupError {
    SetupError::new("network", e.to_string())
}

fn not_sultan(what: &impl ToString) -> SetupError {
    SetupError::new("not_sultan", what.to_string())
}

/// Sends `request`; a success answer's JSON body (`null` when empty), or the problem as a
/// [`SetupError`].
async fn call(request: reqwest::RequestBuilder) -> Result<Value, SetupError> {
    let response = request.send().await.map_err(|e| network(&e))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(body);
    }
    let detail = body["detail"].as_str().unwrap_or_default();
    Err(match body["code"].as_str() {
        Some(code) if KNOWN_PROBLEMS.contains(&code) => SetupError::new(code, detail),
        Some("invalid_session") => SetupError::new("session_ended", detail),
        Some(code) => SetupError::new("failed", format!("{detail} ({code})")),
        None => not_sultan(&status),
    })
}

fn parse<T: for<'de> Deserialize<'de>>(body: Value) -> Result<T, SetupError> {
    serde_json::from_value(body).map_err(|e| not_sultan(&e))
}

#[derive(Debug, Deserialize)]
struct LoginAnswer {
    token: String,
    branch_id: String,
}

#[derive(Debug, Deserialize)]
struct BranchAnswer {
    id: String,
    code: String,
    name: String,
    archived: bool,
}

#[derive(Debug, Deserialize)]
struct CodeAnswer {
    code: String,
}

/// Exchanges a pairing code and saves the pairing; the machine becomes a terminal.
async fn pair(
    client: &reqwest::Client,
    server: &str,
    code: &str,
    branch: Option<String>,
    config_path: &Path,
) -> Result<Paired, SetupError> {
    let base = api_base(server);
    let body = call(
        client
            .post(format!("{base}/registers/pair"))
            .json(&json!({ "code": code.trim() })),
    )
    .await?;
    let answer: PairResponse = parse(body)?;
    let stored = register::store(&register::root_of(server), answer, branch, config_path)
        .map_err(|e| setup_error(&e))?;
    set_mode(config_path, Mode::Client)?;
    Ok(Paired {
        register_code: stored.register_code,
        register_name: stored.register_name,
        server: stored.server,
    })
}

fn set_mode(config_path: &Path, mode: Mode) -> Result<(), SetupError> {
    config::update(config_path, |table| {
        table.insert("mode".to_owned(), mode.to_string().into());
    })
    .map_err(|e| setup_error(&e))
}

async fn init_shop(config_path: PathBuf, request: ShopRequest) -> Result<ShopReport, SetupError> {
    let db_url = request
        .db_url
        .clone()
        .unwrap_or_else(|| default_db(&config_path));
    let options = InitOptions {
        db_url: db_url.clone(),
        company: request.company,
        branch_code: request.branch_code,
        branch_name: request.branch_name,
        branch_address: String::new(),
        timezone: request.timezone,
        location_name: request.location_name,
        owner_username: request.owner_username,
        owner_display_name: request.owner_display_name,
        owner_password: request.owner_password,
        owner_pin: request.owner_pin,
    };
    let now = SystemClock.now();
    let report = init::init_database(&options, now)
        .await
        .map_err(|e| setup_error(&e))?;
    let hq = request.app_type == AppType::Hq;
    config::update(&config_path, |table| {
        table.insert("db".to_owned(), db_url.clone().into());
        if hq {
            if let Some(listen) = &request.listen {
                table.insert("listen".to_owned(), listen.clone().into());
            }
            table.insert("role".to_owned(), "hq".into());
        }
    })
    .map_err(|e| setup_error(&e))?;
    if !hq {
        single::provision(&db_url, &report, &config_path, now)
            .await
            .map_err(|e| setup_error(&e))?;
    }
    // Last: a machine with a mode is set up.
    set_mode(&config_path, mode_of(request.app_type))?;
    Ok(ShopReport {
        company: report.company.name,
        branch_code: report.branch.code,
        db_url,
    })
}

impl SetupService for AppSetupService {
    fn check_db(&self, url: &str) -> SetupFuture<DbCheck> {
        let url = url.to_owned();
        Box::pin(async move {
            let db = Db::connect(&url)
                .await
                .map_err(|e| SetupError::new("db", e.to_string()))?;
            // A new database has no `company` table yet: that is empty too.
            let company = org::get_company(&db).await.ok().flatten();
            db.close().await;
            Ok(company.map_or(DbCheck::Empty, |c| DbCheck::Initialised(c.name)))
        })
    }

    fn init_shop(&self, request: &ShopRequest) -> SetupFuture<ShopReport> {
        Box::pin(init_shop(self.config_path.clone(), request.clone()))
    }

    fn check_server(&self, server: &str) -> SetupFuture<()> {
        let request = self
            .client
            .get(format!("{}/openapi.json", api_base(server)));
        Box::pin(async move {
            let body = call(request).await?;
            if body.get("openapi").is_some() {
                Ok(())
            } else {
                Err(not_sultan(&"no OpenAPI document"))
            }
        })
    }

    fn manager_login(
        &self,
        server: &str,
        username: &str,
        password: &str,
    ) -> SetupFuture<ManagerSession> {
        let base = api_base(server);
        let client = self.client.clone();
        let login = client
            .post(format!("{base}/auth/password"))
            .json(&json!({ "username": username, "password": password }));
        Box::pin(async move {
            let answer: LoginAnswer = parse(call(login).await?)?;
            let branches: Vec<BranchAnswer> = parse(
                call(
                    client
                        .get(format!("{base}/branches"))
                        .bearer_auth(&answer.token),
                )
                .await?,
            )?;
            Ok(ManagerSession {
                token: answer.token,
                branch_id: answer.branch_id,
                branches: branches
                    .into_iter()
                    .filter(|b| !b.archived)
                    .map(|b| BranchChoice {
                        id: b.id,
                        code: b.code,
                        name: b.name,
                    })
                    .collect(),
            })
        })
    }

    fn register_terminal(
        &self,
        server: &str,
        token: &str,
        request: &TerminalRequest,
    ) -> SetupFuture<Paired> {
        let server = server.to_owned();
        let token = token.to_owned();
        let request = request.clone();
        let client = self.client.clone();
        let config_path = self.config_path.clone();
        Box::pin(async move {
            let base = api_base(&server);
            let id = RegisterId::generate();
            call(
                client
                    .put(format!("{base}/registers/{id}"))
                    .bearer_auth(&token)
                    .json(&json!({
                        "branch_id": request.branch_id,
                        "code": request.code,
                        "name": request.name,
                    })),
            )
            .await?;
            let code: CodeAnswer = parse(
                call(
                    client
                        .post(format!("{base}/registers/{id}/pairing-code"))
                        .bearer_auth(&token),
                )
                .await?,
            )?;
            let branch = Some(request.branch_label).filter(|b| !b.is_empty());
            let paired = pair(&client, &server, &code.code, branch, &config_path).await;
            // The manager's session is not needed any more, whatever happened.
            let _ = call(
                client
                    .post(format!("{base}/auth/logout"))
                    .bearer_auth(&token)
                    .header("idempotency-key", RegisterId::generate().to_string()),
            )
            .await;
            paired
        })
    }

    fn pair_with_code(&self, server: &str, code: &str) -> SetupFuture<Paired> {
        let server = server.to_owned();
        let code = code.to_owned();
        let client = self.client.clone();
        let config_path = self.config_path.clone();
        Box::pin(async move { pair(&client, &server, &code, None, &config_path).await })
    }
}

#[cfg(test)]
mod tests {
    use sultan_core::time::Timestamp;

    use super::*;
    use crate::config::Role;
    use crate::init::tests::{TempDir, options};
    use crate::serve::BoundServer;

    fn shop(app_type: AppType, db_url: Option<String>) -> ShopRequest {
        let o = options("unused");
        ShopRequest {
            app_type,
            db_url,
            listen: Some("0.0.0.0:9000".into()),
            company: o.company,
            branch_code: o.branch_code,
            branch_name: o.branch_name,
            timezone: o.timezone,
            location_name: o.location_name,
            owner_username: o.owner_username,
            owner_display_name: o.owner_display_name,
            owner_password: o.owner_password,
            owner_pin: o.owner_pin,
        }
    }

    #[test]
    fn modes_and_errors() {
        assert_eq!(mode_of(AppType::Single), Mode::Single);
        assert_eq!(mode_of(AppType::Hq), Mode::Server);
        assert_eq!(mode_of(AppType::Terminal), Mode::Client);
        assert_eq!(mode_of(AppType::Branch), Mode::Client);
        let cases = [
            (AppError::Invalid("x".into()), "invalid", "x"),
            (
                AppError::AlreadyInitialised("PT".into()),
                "already_initialised",
                "PT",
            ),
            (AppError::Config("c".into()), "file", "invalid setting: c"),
            (AppError::NotPaired, "failed", crate::error::NOT_PAIRED),
        ];
        for (error, code, arg) in cases {
            assert_eq!(setup_error(&error), SetupError::new(code, arg));
        }
        assert!(default_db(Path::new("/x/sultan.toml")).ends_with("/x/sultan.db"));
    }

    #[tokio::test]
    async fn single_app_and_hq_server_set_up_the_database_and_the_config() {
        let dir = TempDir::new("setup-shop");
        let config_path = dir.0.join("sultan.toml");
        let service = AppSetupService::new(&config_path);
        assert_eq!(
            service.check_db(&dir.sqlite_url()).await.unwrap(),
            DbCheck::Empty
        );
        let report = service
            .init_shop(&shop(AppType::Single, None))
            .await
            .unwrap();
        assert_eq!(
            report.db_url,
            dir.sqlite_url(),
            "the file next to the config"
        );
        assert_eq!(report.branch_code, "JKT01");
        let file = config::load(&config_path).unwrap();
        assert_eq!(file.mode.as_deref(), Some("single"));
        assert_eq!((file.listen, file.role), (None, None));
        assert_eq!(file.register.unwrap().server, single::EMBEDDED);
        assert!(register::device_token(&config_path).unwrap().is_some());
        assert_eq!(
            service.check_db(&dir.sqlite_url()).await.unwrap(),
            DbCheck::Initialised("PT Sultan Demo".into())
        );
        // Twice: refused, the wizard shows it.
        let err = service
            .init_shop(&shop(AppType::Single, None))
            .await
            .unwrap_err();
        assert_eq!(err.code, "already_initialised");

        let hq = TempDir::new("setup-hq");
        let config_path = hq.0.join("sultan.toml");
        let service = AppSetupService::new(&config_path);
        let db = format!("sqlite://{}", hq.0.join("data/hq.db").display());
        std::fs::create_dir_all(hq.0.join("data")).unwrap();
        service
            .init_shop(&shop(AppType::Hq, Some(db.clone())))
            .await
            .unwrap();
        let file = config::load(&config_path).unwrap();
        assert_eq!(file.mode.as_deref(), Some("server"));
        assert_eq!(file.db, Some(db));
        assert_eq!(file.listen.as_deref(), Some("0.0.0.0:9000"));
        assert_eq!(file.role.as_deref(), Some("hq"));
        assert!(file.register.is_none());

        let mut bad = shop(AppType::Hq, Some("sqlite://x".into()));
        bad.owner_pin = "1".into();
        assert_eq!(service.init_shop(&bad).await.unwrap_err().code, "invalid");
        let err = service.check_db("postgres://x").await.unwrap_err();
        assert_eq!(err.code, "db");
    }

    /// An initialised server on a random port, stopped when dropped.
    struct Running {
        root: String,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        _dir: TempDir,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
        }
    }

    async fn running(role: Role) -> Running {
        let dir = TempDir::new("setup-server");
        let url = dir.sqlite_url();
        init::init_database(
            &options(&url),
            Timestamp::from_unix_millis(1_800_000_000_000),
        )
        .await
        .unwrap();
        let server = BoundServer::bind(&url, "127.0.0.1:0", role).await.unwrap();
        let root = format!("http://{}", server.addr());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        Running {
            root,
            stop: Some(stop),
            _dir: dir,
        }
    }

    #[tokio::test]
    async fn terminal_registers_itself_with_a_manager_login() {
        let server = running(Role::Standalone).await;
        let dir = TempDir::new("setup-terminal");
        let config_path = dir.0.join("sultan.toml");
        let service = AppSetupService::new(&config_path);
        service.check_server(&server.root).await.unwrap();

        let err = service
            .manager_login(&server.root, "owner", "wrong-pass")
            .await
            .unwrap_err();
        assert_eq!(err.code, "invalid_credentials");
        let session = service
            .manager_login(&server.root, "owner", "owner-pass-1")
            .await
            .unwrap();
        assert_eq!(session.branches.len(), 1);
        assert_eq!(session.branches[0].code, "JKT01");
        assert_eq!(session.branches[0].id, session.branch_id);

        let request = TerminalRequest {
            branch_id: session.branch_id.clone(),
            code: "R2".into(),
            name: "Kasir 2".into(),
            branch_label: "JKT01 · Toko".into(),
        };
        let paired = service
            .register_terminal(&server.root, &session.token, &request)
            .await
            .unwrap();
        assert_eq!(paired.register_code, "R2");
        assert_eq!(paired.server, server.root);
        let file = config::load(&config_path).unwrap();
        assert_eq!(file.mode.as_deref(), Some("client"));
        let (register, token) = register::paired(&config_path).unwrap();
        assert_eq!(register.register_name, "Kasir 2");
        assert_eq!(register.branch.as_deref(), Some("JKT01 · Toko"));
        assert!(!token.is_empty());

        // The manager's session was ended.
        let err = service
            .register_terminal(&server.root, &session.token, &request)
            .await
            .unwrap_err();
        assert_eq!(err.code, "session_ended");
        // The same code twice at the branch.
        let again = service
            .manager_login(&server.root, "owner", "owner-pass-1")
            .await
            .unwrap();
        let err = service
            .register_terminal(&server.root, &again.token, &request)
            .await
            .unwrap_err();
        assert_eq!(err.code, "conflict");
        // A used or unknown pairing code.
        let err = service
            .pair_with_code(&server.root, "AAAA-BBBB")
            .await
            .unwrap_err();
        assert_eq!(err.code, "invalid_pairing_code");
    }

    #[tokio::test]
    async fn a_branch_server_cannot_register_terminals() {
        let server = running(Role::Branch).await;
        let dir = TempDir::new("setup-branch");
        let service = AppSetupService::new(&dir.0.join("sultan.toml"));
        let session = service
            .manager_login(&server.root, "owner", "owner-pass-1")
            .await
            .unwrap();
        let request = TerminalRequest {
            branch_id: session.branch_id.clone(),
            code: "R2".into(),
            name: "Kasir 2".into(),
            branch_label: String::new(),
        };
        let err = service
            .register_terminal(&server.root, &session.token, &request)
            .await
            .unwrap_err();
        assert_eq!(err.code, "read_only_replica");
    }

    #[tokio::test]
    async fn servers_that_are_not_sultan() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route(
                "/a/api/v1/openapi.json",
                axum::routing::get(async || axum::Json(json!({"hello": 1}))),
            )
            .route(
                "/b/api/v1/openapi.json",
                axum::routing::get(async || (axum::http::StatusCode::BAD_GATEWAY, "oops")),
            )
            .route(
                "/c/api/v1/auth/password",
                axum::routing::post(async || {
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(json!({"code": "invalid_body", "detail": "nope"})),
                    )
                }),
            )
            .route(
                "/d/api/v1/auth/password",
                axum::routing::post(async || axum::Json(json!({"unexpected": true}))),
            );
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        let dir = TempDir::new("setup-odd");
        let service = AppSetupService::new(&dir.0.join("sultan.toml"));
        let root = format!("http://{addr}");
        let err = service
            .check_server(&format!("{root}/a"))
            .await
            .unwrap_err();
        assert_eq!(err.code, "not_sultan");
        let err = service
            .check_server(&format!("{root}/b"))
            .await
            .unwrap_err();
        assert_eq!(err, SetupError::new("not_sultan", "502 Bad Gateway"));
        let err = service
            .manager_login(&format!("{root}/c"), "u", "p")
            .await
            .unwrap_err();
        assert_eq!(err, SetupError::new("failed", "nope (invalid_body)"));
        let err = service
            .manager_login(&format!("{root}/d"), "u", "p")
            .await
            .unwrap_err();
        assert_eq!(err.code, "not_sultan");
        task.abort();
        let err = service
            .check_server("http://127.0.0.1:1")
            .await
            .unwrap_err();
        assert_eq!(err.code, "network");
    }
}
