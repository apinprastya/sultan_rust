//! `sultan register`: the register (till) side of pairing (spec 2.1 §2, plan Phase 7).
//!
//! `sultan register pair --server <url> --code <code>` sends the one-time pairing code to
//! `POST /api/v1/registers/pair` and keeps the long-lived device token it gets back:
//! - the **token** goes to `register.token` next to `sultan.toml`, written with mode `0600` on Unix
//!   (owner-only). The spec only says the server stores a hash; on the register the token is a
//!   bearer secret, so it is kept out of `sultan.toml`, which people copy around and paste into
//!   support tickets;
//! - the **non-secret details** (server URL, register and branch IDs, code, name) go to the
//!   `[register]` table of `sultan.toml`.
//!
//! Pairing again replaces both. `sultan register status` prints the paired status; `sultan
//! register` opens the register window (plan Phase 8, `crates/ui`) with [`paired`].
//!
//! The client speaks plain HTTP (the server has no TLS yet), which is acceptable on the shop LAN
//! only.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::{self, RegisterConfig};
use crate::error::{AppError, Result};

/// File holding the device token, next to `sultan.toml`.
pub const TOKEN_FILE: &str = "register.token";

/// Where the device token of the register configured by `config` lives.
#[must_use]
pub fn token_path(config: &Path) -> PathBuf {
    config::config_dir(config).join(TOKEN_FILE)
}

pub use sultan_client::api_base;

/// The server URL without the API suffix, as stored in `sultan.toml`.
fn server_root(server: &str) -> String {
    let base = api_base(server);
    base.strip_suffix("/api/v1").unwrap_or(&base).to_owned()
}

/// Answer of `POST /registers/pair`.
#[derive(Debug, Deserialize)]
pub(crate) struct PairResponse {
    pub(crate) device_token: String,
    pub(crate) register_id: String,
    pub(crate) branch_id: String,
    pub(crate) register_code: String,
    pub(crate) register_name: String,
}

#[derive(Debug, Default, Deserialize)]
struct Problem {
    code: Option<String>,
    detail: Option<String>,
}

/// Pairs this machine with a register (see the module docs) and saves the result.
///
/// # Errors
/// [`AppError::Pair`] when the server cannot be reached or refuses the code, [`AppError::File`] /
/// [`AppError::Config`] when saving fails.
pub async fn pair(server: &str, code: &str, config_path: &Path) -> Result<RegisterConfig> {
    let url = format!("{}/registers/pair", api_base(server));
    let response = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "code": code.trim() }))
        .send()
        .await
        .map_err(|e| AppError::Pair(format!("cannot reach {url}: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        let problem: Problem = response.json().await.unwrap_or_default();
        let code = problem.code.unwrap_or_else(|| status.to_string());
        let detail = problem
            .detail
            .unwrap_or_else(|| "unexpected answer from the server".to_owned());
        return Err(AppError::Pair(format!("{detail} ({code})")));
    }
    let paired: PairResponse = response
        .json()
        .await
        .map_err(|e| AppError::Pair(format!("unexpected answer from {url}: {e}")))?;
    store(&server_root(server), paired, config_path)
}

/// Saves a pairing: the device token to `register.token`, the details to `[register]` with
/// `server` as the server URL.
///
/// # Errors
/// [`AppError::File`] / [`AppError::Config`].
pub(crate) fn store(
    server: &str,
    paired: PairResponse,
    config_path: &Path,
) -> Result<RegisterConfig> {
    config::write_private(&token_path(config_path), paired.device_token.as_bytes())?;
    let register = RegisterConfig {
        server: server.to_owned(),
        register_id: paired.register_id,
        branch_id: paired.branch_id,
        register_code: paired.register_code,
        register_name: paired.register_name,
    };
    config::set_register(config_path, &register)?;
    Ok(register)
}

/// The server URL to store for `server` (without `/api/v1`).
#[must_use]
pub fn root_of(server: &str) -> String {
    server_root(server)
}

/// The stored device token, if this machine is paired.
///
/// # Errors
/// [`AppError::File`] if the token file exists but cannot be read.
pub fn device_token(config_path: &Path) -> Result<Option<String>> {
    let path = token_path(config_path);
    match fs::read_to_string(&path) {
        Ok(token) => Ok(Some(token.trim().to_owned()).filter(|t| !t.is_empty())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AppError::file("cannot read", path, e)),
    }
}

/// The paired register and its device token.
///
/// # Errors
/// [`AppError::NotPaired`] when either is missing, [`AppError::File`] / [`AppError::Config`].
pub fn paired(config_path: &Path) -> Result<(RegisterConfig, String)> {
    let file = config::load(config_path)?;
    let token = device_token(config_path)?;
    match (file.register, token) {
        (Some(register), Some(token)) => Ok((register, token)),
        _ => Err(AppError::NotPaired),
    }
}

/// One line saying whether and to what this machine is paired.
///
/// # Errors
/// [`AppError::File`] / [`AppError::Config`].
pub fn status(config_path: &Path) -> Result<String> {
    match paired(config_path) {
        Ok((r, _)) => Ok(format!(
            "paired: register {} ({}) at {}, register id {}, branch id {}; device token in {}; \
             open the register with `sultan register`",
            r.register_code,
            r.register_name,
            r.server,
            r.register_id,
            r.branch_id,
            token_path(config_path).display()
        )),
        Err(AppError::NotPaired) => Ok(crate::error::NOT_PAIRED.to_owned()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use sultan_core::time::Timestamp;

    use super::*;
    use crate::config::Role;
    use crate::init::init_database;
    use crate::init::tests::{TempDir, options};
    use crate::serve::BoundServer;

    #[test]
    fn api_base_accepts_common_spellings() {
        let cases = [
            ("http://h:8080", "http://h:8080/api/v1"),
            ("http://h:8080/", "http://h:8080/api/v1"),
            ("http://h:8080/api/v1", "http://h:8080/api/v1"),
            (" http://h:8080/api/v1/ ", "http://h:8080/api/v1"),
        ];
        for (server, base) in cases {
            assert_eq!(api_base(server), base, "{server}");
            assert_eq!(server_root(server), "http://h:8080");
        }
        assert_eq!(
            token_path(Path::new("/x/sultan.toml")),
            Path::new("/x/register.token")
        );
    }

    /// Logs in as the owner and issues a pairing code for a new register at `JKT01`.
    async fn issue_code(base: &str) -> String {
        let client = reqwest::Client::new();
        let login: serde_json::Value = client
            .post(format!("{base}/auth/password"))
            .json(&serde_json::json!({"username": "owner", "password": "owner-pass-1"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = login["token"].as_str().unwrap().to_owned();
        let branch_id = login["branch_id"].as_str().unwrap().to_owned();
        let register_id = sultan_core::ids::RegisterId::generate();
        let created = client
            .put(format!("{base}/registers/{register_id}"))
            .bearer_auth(&token)
            .json(&serde_json::json!({"branch_id": branch_id, "code": "R1", "name": "Kasir 1"}))
            .send()
            .await
            .unwrap();
        assert_eq!(created.status().as_u16(), 201);
        let code: serde_json::Value = client
            .post(format!("{base}/registers/{register_id}/pairing-code"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        code["code"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn pairs_against_a_running_server_and_stores_the_token_privately() {
        let dir = TempDir::new("pair");
        let url = dir.sqlite_url();
        init_database(
            &options(&url),
            Timestamp::from_unix_millis(1_800_000_000_000),
        )
        .await
        .unwrap();
        let server = BoundServer::bind(&url, "127.0.0.1:0", Role::Standalone)
            .await
            .unwrap();
        let root = format!("http://{}", server.addr());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));

        let config_path = dir.0.join("till/sultan.toml");
        assert!(status(&config_path).unwrap().starts_with("not paired"));

        let code = issue_code(&format!("{root}/api/v1")).await;
        let register = pair(&format!("{root}/"), &code, &config_path)
            .await
            .unwrap();
        assert_eq!(register.register_code, "R1");
        assert_eq!(register.server, root);
        let token = device_token(&config_path).unwrap().unwrap();
        assert!(!fs::read_to_string(&config_path).unwrap().contains(&token));
        assert_eq!(config::load(&config_path).unwrap().register, Some(register));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(token_path(&config_path))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let line = status(&config_path).unwrap();
        assert!(line.starts_with("paired: register R1 (Kasir 1)"), "{line}");

        // The token works: the lock screen lists the owner.
        let users: serde_json::Value = reqwest::Client::new()
            .get(format!("{root}/api/v1/registers/me/users"))
            .header("x-register-token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(users[0]["display_name"], "Owner");

        // The code is single use.
        let err = pair(&root, &code, &config_path).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "pairing failed: the pairing code is unknown, already used or expired \
             (invalid_pairing_code)"
        );
        // A non-problem error answer.
        let err = pair(&format!("{root}/nowhere"), "x", &config_path)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("(not_found)"), "{err}");

        stop.send(()).unwrap();
        task.await.unwrap().unwrap();

        let err = pair(&root, "x", &config_path).await.unwrap_err();
        assert!(
            err.to_string().starts_with("pairing failed: cannot reach"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn unexpected_answers_are_reported() {
        // A server that answers 200 with something else, and 500 without a problem body.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route(
                "/ok/api/v1/registers/pair",
                axum::routing::post(async || "hello"),
            )
            .route(
                "/err/api/v1/registers/pair",
                axum::routing::post(async || (axum::http::StatusCode::BAD_GATEWAY, "oops")),
            );
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        let dir = TempDir::new("pair-odd");
        let config_path = dir.0.join("sultan.toml");

        let err = pair(&format!("http://{addr}/ok"), "c", &config_path)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unexpected answer from"), "{err}");
        let err = pair(&format!("http://{addr}/err"), "c", &config_path)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "pairing failed: unexpected answer from the server (502 Bad Gateway)"
        );
        assert!(device_token(&config_path).unwrap().is_none());
        task.abort();

        // A token path that is a directory cannot be read.
        fs::create_dir_all(token_path(&config_path)).unwrap();
        assert!(matches!(
            device_token(&config_path),
            Err(AppError::File { .. })
        ));
    }
}
