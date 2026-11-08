//! End-to-end test of the real `sultan` binary (plan Phase 7): `init` → `serve` (background,
//! random port) → `register pair` → PIN login → shutdown with `SIGTERM`. Secrets come from the
//! environment, so no prompt ever runs.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_sultan");

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Kills the server if the test fails half-way.
struct Server(Option<Child>);

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn sultan(config: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("--config")
        .arg(config)
        .args(args)
        .env_remove("SULTAN_DB")
        .env_remove("SULTAN_LISTEN")
        .env_remove("SULTAN_ROLE")
        .env("SULTAN_OWNER_PASSWORD", "owner-pass-1")
        .env("SULTAN_OWNER_PIN", "1234")
        .env("RUST_LOG", "warn")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn version_and_help() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        text(&out.stdout),
        format!("sultan {}\n", env!("CARGO_PKG_VERSION"))
    );
    let out = Command::new(BIN).arg("--help").output().unwrap();
    let help = text(&out.stdout);
    for command in ["setup", "init", "serve", "register"] {
        assert!(help.contains(command), "{help}");
    }
    let out = Command::new(BIN)
        .args(["setup", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        text(&out.stdout).contains("setup wizard"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn bare_sultan_on_a_server_prints_how_to_start_it() {
    let dir = TempDir(std::env::temp_dir().join(format!("sultan-e2e-bare-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let config = dir.0.join("sultan.toml");
    std::fs::write(&config, "mode = \"server\"\n").unwrap();
    let out = sultan(&config, &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "this machine is set up as a server; start it with: sultan serve (Ctrl+C stops it)\n"
    );
}

#[test]
fn errors_exit_non_zero_with_a_message() {
    let dir = TempDir(std::env::temp_dir().join(format!("sultan-e2e-err-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let config = dir.0.join("sultan.toml");
    let out = sultan(&config, &["serve", "--listen", "127.0.0.1:0"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("is not initialised; run `sultan init` first"));
    // The register window needs a paired machine.
    let out = sultan(&config, &["register"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("not paired: run `sultan register pair"),
        "{}",
        text(&out.stderr)
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn init_serve_pair_login_shutdown() {
    let dir = TempDir(std::env::temp_dir().join(format!("sultan-e2e-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let config = dir.0.join("sultan.toml");

    let init = [
        "init",
        "--company",
        "PT Sultan Demo",
        "--branch",
        "JKT01",
        "--branch-name",
        "Toko Jakarta",
        "--owner",
        "owner",
    ];
    let out = sultan(&config, &init);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("branch JKT01"));
    let out = sultan(&config, &init);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("already initialised"));

    // Serve on a random port; the first stdout line names it.
    let mut child = Command::new(BIN)
        .arg("--config")
        .arg(&config)
        .args(["serve", "--listen", "127.0.0.1:0"])
        .env("RUST_LOG", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut server = Server(Some(child));
    let mut banner = String::new();
    stdout.read_line(&mut banner).unwrap();
    let base = banner
        .split_whitespace()
        .find(|w| w.starts_with("http://"))
        .unwrap()
        .trim_end_matches(',')
        .to_owned();
    assert!(base.ends_with("/api/v1"), "{banner}");

    // Back office: owner login, a register, a pairing code.
    let client = reqwest::Client::new();
    let (login, code) = issue_pairing_code(&client, &base).await;

    // The register machine pairs (its own config directory).
    let till = dir.0.join("till/sultan.toml");
    let server_root = base.trim_end_matches("/api/v1").to_owned();
    let pair = [
        "register",
        "pair",
        "--server",
        server_root.as_str(),
        "--code",
        code.as_str(),
    ];
    let out = sultan(&till, &pair);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = sultan(&till, &["register", "status"]);
    assert!(text(&out.stdout).starts_with("paired: register R1 (Kasir 1)"));
    let device_token = std::fs::read_to_string(dir.0.join("till/register.token")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.0.join("till/register.token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // PIN login at the paired register.
    let pin = client
        .post(format!("{base}/auth/pin"))
        .header("x-register-token", device_token.trim())
        .json(&json!({"user_id": login["user_id"], "pin": "1234"}))
        .send()
        .await
        .unwrap();
    assert_eq!(pin.status().as_u16(), 200);

    // SIGTERM → graceful shutdown, exit 0.
    let mut child = server.0.take().unwrap();
    let pid = child.id().to_string();
    let killed = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    assert!(killed.success());
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}");
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut rest).unwrap();
    assert!(rest.contains("sultan serve: stopped"), "{rest}");
}

/// Logs in as the owner, creates register `R1` at the owner's branch and issues a pairing code.
/// Returns the login answer and the code.
async fn issue_pairing_code(client: &reqwest::Client, base: &str) -> (Value, String) {
    let login: Value = client
        .post(format!("{base}/auth/password"))
        .json(&json!({"username": "owner", "password": "owner-pass-1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = login["token"].as_str().unwrap();
    let register_id = uuid_v7();
    let put = client
        .put(format!("{base}/registers/{register_id}"))
        .bearer_auth(token)
        .json(&json!({"branch_id": login["branch_id"], "code": "R1", "name": "Kasir 1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status().as_u16(), 201);
    let code: Value = client
        .post(format!("{base}/registers/{register_id}/pairing-code"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let code = code["code"].as_str().unwrap().to_owned();
    (login, code)
}

/// A `UUIDv7` (the API wants client-generated v7 IDs).
fn uuid_v7() -> String {
    sultan_core::ids::RegisterId::generate().to_string()
}
