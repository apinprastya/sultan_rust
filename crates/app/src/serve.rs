//! `sultan serve`: runs the API server of a `standalone`, `hq` or `branch` node (plan Phase 7,
//! `docs/plan.md` §1).
//!
//! Start-up:
//! 1. Refuses a database that `sultan init` has not set up: a missing `SQLite` file (it is not
//!    created), or a database without a company.
//! 2. Runs the migrations (upgrades an older database) and re-seeds the default roles, which
//!    `sultan_db::seed` says to do at every start. On Unix a `SQLite` file and its `-wal` /
//!    `-shm` / `-journal` side files are made owner-only (`0600`) when the database is opened
//!    (`sultan_db::Db::connect`, Phase 9): files that others could read are **fixed and
//!    reported** with a warning in the log, not refused, so an upgraded node keeps starting.
//! 3. Binds the listener and prints the API URL, the role and the events WebSocket URL.
//! 4. Serves until Ctrl+C or `SIGTERM`, then stops accepting, lets running requests finish and
//!    closes the database ([`shutdown_signal`]).
//!
//! **Branch role:** the node answers master-data writes with `403 read_only_replica` (spec 2.1
//! §1). HQ↔branch sync is not implemented yet, so a branch node serves its **local** database
//! only; its master data comes from `sultan init` until the sync worker exists.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use sultan_db::{Db, org, seed};
use sultan_server::{AppState, Clock, SystemClock, router};
use tokio::net::TcpListener;

use crate::config::Role;
use crate::error::{AppError, Result};

/// The local file of a `sqlite:` URL, or `None` for in-memory databases and other backends.
#[must_use]
pub fn sqlite_file(url: &str) -> Option<PathBuf> {
    let rest = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))?;
    let path = rest.split('?').next().unwrap_or_default();
    if path.is_empty() || path.contains(":memory:") || url.contains("mode=memory") {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// Opens an initialised database for serving (steps 1–2 of the module docs).
///
/// # Errors
/// [`AppError::NotInitialised`], or [`AppError::Db`].
pub async fn open_initialised(db_url: &str) -> Result<Db> {
    let not_initialised = || AppError::NotInitialised(db_url.to_owned());
    if sqlite_file(db_url).is_some_and(|file| !file.exists()) {
        return Err(not_initialised());
    }
    let db = Db::connect(db_url).await?;
    // Before migrating: an uninitialised database has no `company` table (or no row in it).
    if !matches!(org::get_company(&db).await, Ok(Some(_))) {
        db.close().await;
        return Err(not_initialised());
    }
    db.migrate().await?;
    seed::seed_defaults(&db, SystemClock.now()).await?;
    Ok(db)
}

/// The start-up banner (also the line the end-to-end test reads to find the port).
#[must_use]
pub fn banner(addr: SocketAddr, role: Role) -> String {
    let mut text = format!(
        "sultan serve: role {role}, API http://{addr}/api/v1, events ws://{addr}/api/v1/events"
    );
    if role == Role::Branch {
        text.push_str(
            "\nbranch node: master data is a read-only replica; HQ sync is not implemented yet, \
             so this node serves its local database only",
        );
    }
    text
}

/// A server with its database open and its listener bound, not yet serving.
#[derive(Debug)]
pub struct BoundServer {
    listener: TcpListener,
    state: AppState,
    addr: SocketAddr,
    role: Role,
}

impl BoundServer {
    /// Opens the database and binds `listen` (steps 1–3 of the module docs, without printing).
    ///
    /// # Errors
    /// [`AppError::NotInitialised`], [`AppError::Db`] or [`AppError::Serve`] (bind failed).
    pub async fn bind(db_url: &str, listen: &str, role: Role) -> Result<Self> {
        let db = open_initialised(db_url).await?;
        let listener = match TcpListener::bind(listen).await {
            Ok(listener) => listener,
            Err(source) => {
                db.close().await;
                return Err(AppError::Serve {
                    addr: listen.to_owned(),
                    source,
                });
            }
        };
        let addr = listener.local_addr().map_err(|source| AppError::Serve {
            addr: listen.to_owned(),
            source,
        })?;
        let state = AppState::new(db, role.node_role(), Arc::new(SystemClock));
        Ok(Self {
            listener,
            state,
            addr,
            role,
        })
    }

    /// The bound address (the real port when listening on port 0).
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The start-up banner for this server.
    #[must_use]
    pub fn banner(&self) -> String {
        banner(self.addr, self.role)
    }

    /// Serves until `shutdown` completes, then drains running requests and closes the database.
    ///
    /// # Errors
    /// [`AppError::Serve`].
    pub async fn run(self, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        let db = self.state.db.clone();
        if self.role == Role::Branch {
            tracing::warn!(
                "branch node: HQ sync is not implemented yet, serving the local database only"
            );
        }
        tracing::info!(addr = %self.addr, role = %self.role, "listening");
        let result = axum::serve(self.listener, router(self.state))
            .with_graceful_shutdown(shutdown)
            .await;
        db.close().await;
        tracing::info!("stopped");
        result.map_err(|source| AppError::Serve {
            addr: self.addr.to_string(),
            source,
        })
    }
}

/// Completes on Ctrl+C, or on `SIGTERM` on Unix (what `systemd` and `kill` send).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use sultan_core::time::Timestamp;

    use super::*;
    use crate::init::init_database;
    use crate::init::tests::{TempDir, options};

    #[test]
    fn sqlite_file_of_urls() {
        let cases = [
            ("sqlite:///tmp/a.db", Some("/tmp/a.db")),
            ("sqlite://rel.db?mode=rwc", Some("rel.db")),
            ("sqlite:x.db", Some("x.db")),
            ("sqlite::memory:", None),
            ("sqlite://file.db?mode=memory", None),
            ("sqlite://", None),
            ("mysql://u:p@h/db", None),
        ];
        for (url, file) in cases {
            assert_eq!(sqlite_file(url), file.map(PathBuf::from), "{url}");
        }
    }

    #[test]
    fn banner_names_urls_and_role() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(
            banner(addr, Role::Standalone),
            "sultan serve: role standalone, API http://127.0.0.1:8080/api/v1, \
             events ws://127.0.0.1:8080/api/v1/events"
        );
        assert!(banner(addr, Role::Branch).contains("serves its local database only"));
    }

    #[tokio::test]
    async fn refuses_an_uninitialised_database() {
        let dir = TempDir::new("serve-uninit");
        let missing = dir.sqlite_url();
        let err = BoundServer::bind(&missing, "127.0.0.1:0", Role::Standalone)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("the database {missing} is not initialised; run `sultan init` first")
        );
        assert!(
            sqlite_file(&missing).is_some_and(|f| !f.exists()),
            "not created"
        );

        // An existing but empty database (migrated or not) is refused too.
        let empty = format!("sqlite://{}", dir.0.join("empty.db").display());
        let db = Db::connect(&empty).await.unwrap();
        db.close().await;
        assert!(matches!(
            open_initialised(&empty).await,
            Err(AppError::NotInitialised(_))
        ));
        let db = Db::connect(&empty).await.unwrap();
        db.migrate().await.unwrap();
        db.close().await;
        assert!(matches!(
            open_initialised(&empty).await,
            Err(AppError::NotInitialised(_))
        ));
        assert!(matches!(
            open_initialised("postgres://x").await,
            Err(AppError::Db(_))
        ));
    }

    /// Phase 9: an existing database that others can read is tightened to `0600` at start.
    #[cfg(unix)]
    #[tokio::test]
    async fn tightens_database_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("serve-perm");
        let url = dir.sqlite_url();
        init_database(
            &options(&url),
            Timestamp::from_unix_millis(1_800_000_000_000),
        )
        .await
        .unwrap();
        let file = sqlite_file(&url).unwrap();
        let journal = dir.0.join("sultan.db-journal");
        std::fs::write(&journal, b"").unwrap();
        for path in [&file, &journal] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let db = open_initialised(&url).await.unwrap();
        for name in [
            "sultan.db",
            "sultan.db-wal",
            "sultan.db-shm",
            "sultan.db-journal",
        ] {
            let mode = std::fs::metadata(dir.0.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
        db.close().await;
    }

    #[tokio::test]
    async fn serves_openapi_and_shuts_down_gracefully() {
        let dir = TempDir::new("serve");
        let url = dir.sqlite_url();
        init_database(
            &options(&url),
            Timestamp::from_unix_millis(1_800_000_000_000),
        )
        .await
        .unwrap();

        let server = BoundServer::bind(&url, "127.0.0.1:0", Role::Hq)
            .await
            .unwrap();
        let addr = server.addr();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_ne!(addr.port(), 0);
        assert!(server.banner().contains("role hq"));

        // The port is taken now.
        let err = BoundServer::bind(&url, &addr.to_string(), Role::Hq)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Serve { .. }), "{err}");

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        let response = reqwest::get(format!("http://{addr}/api/v1/openapi.json"))
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let doc: serde_json::Value = response.json().await.unwrap();
        assert!(doc["paths"]["/api/v1/registers/pair"].is_object());

        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert!(
            reqwest::get(format!("http://{addr}/api/v1/openapi.json"))
                .await
                .is_err(),
            "no longer listening"
        );
    }

    #[tokio::test]
    async fn branch_role_is_a_read_only_replica() {
        let dir = TempDir::new("serve-branch");
        let url = dir.sqlite_url();
        init_database(
            &options(&url),
            Timestamp::from_unix_millis(1_800_000_000_000),
        )
        .await
        .unwrap();
        let server = BoundServer::bind(&url, "127.0.0.1:0", Role::Branch)
            .await
            .unwrap();
        let addr = server.addr();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        let client = reqwest::Client::new();
        let login: serde_json::Value = client
            .post(format!("http://{addr}/api/v1/auth/password"))
            .json(&serde_json::json!({"username": "owner", "password": "owner-pass-1"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = login["token"].as_str().unwrap();
        let response = client
            .put(format!(
                "http://{addr}/api/v1/branches/{}",
                sultan_core::ids::BranchId::generate()
            ))
            .bearer_auth(token)
            .json(&serde_json::json!({"code": "BDG01", "name": "Bandung"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 403);
        let problem: serde_json::Value = response.json().await.unwrap();
        assert_eq!(problem["code"], "read_only_replica");
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}
