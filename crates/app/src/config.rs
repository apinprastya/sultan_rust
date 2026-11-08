//! Settings: `sultan.toml` plus environment variables (plan Phase 7).
//!
//! # Precedence
//! For every setting: **command-line flag > environment variable > `sultan.toml` > default**.
//!
//! | Setting | Flag | Environment | `sultan.toml` | Default |
//! |---|---|---|---|---|
//! | config file | `--config` | `SULTAN_CONFIG` | – | `./sultan.toml` |
//! | database URL | `--db` | `SULTAN_DB` | `db` | `sqlite://<absolute config dir>/sultan.db` |
//! | listen address | `--listen` | `SULTAN_LISTEN` | `listen` | `127.0.0.1:8080` |
//! | node role | `--role` | `SULTAN_ROLE` | `role` | `standalone` |
//! | app type (bare `sultan`) | – | – | `mode` | the setup wizard |
//!
//! The database defaults to `sultan.db` **next to the config file**, so one directory holds a
//! node: `sultan.toml`, `sultan.db` and, on a register, `register.token`. The listen address
//! defaults to loopback; pass `--listen 0.0.0.0:8080` to serve registers on the shop LAN.
//!
//! `mode` is written by the setup wizard and decides what a bare `sultan` does ([`Mode`]); an
//! older file without it is read as `client` when it has a `[register]` table and as `server`
//! when it has a `db` ([`resolve_mode`]).
//!
//! A missing config file counts as empty. `sultan init` writes `db`; `sultan register pair` writes
//! the `[register]` table. Both rewrite the file with mode `0600` on Unix, because `db` may hold
//! a `MySQL` password. Keys this version does not know are kept.
//!
//! The register's device token is **not** stored in `sultan.toml` but in `register.token` next to
//! it (mode `0600`), see [`crate::register`].

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sultan_server::NodeRole;

use crate::error::{AppError, Result};

/// Environment variable naming the config file.
pub const ENV_CONFIG: &str = "SULTAN_CONFIG";
/// Environment variable for the database URL.
pub const ENV_DB: &str = "SULTAN_DB";
/// Environment variable for the listen address.
pub const ENV_LISTEN: &str = "SULTAN_LISTEN";
/// Environment variable for the node role.
pub const ENV_ROLE: &str = "SULTAN_ROLE";

/// Default config file, relative to the working directory.
pub const DEFAULT_CONFIG: &str = "sultan.toml";
/// Default database file name, next to the config file.
pub const DEFAULT_DB_FILE: &str = "sultan.db";
/// Default listen address: loopback only.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8080";

/// Reads an environment variable. Injected so tests never touch the process environment.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The real process environment. Empty values count as unset.
#[must_use]
pub fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// The node role (`docs/plan.md` §1). `register` is a client, not a server role, so it is not
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, clap::ValueEnum)]
pub enum Role {
    /// HQ + branch + register in one process.
    #[default]
    Standalone,
    /// Head office: owns master data.
    Hq,
    /// Branch server: read-only replica of master data.
    Branch,
}

impl Role {
    /// The server's node role.
    #[must_use]
    pub const fn node_role(self) -> NodeRole {
        match self {
            Self::Standalone => NodeRole::Standalone,
            Self::Hq => NodeRole::Hq,
            Self::Branch => NodeRole::Branch,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Standalone => "standalone",
            Self::Hq => "hq",
            Self::Branch => "branch",
        })
    }
}

impl FromStr for Role {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standalone" => Ok(Self::Standalone),
            "hq" => Ok(Self::Hq),
            "branch" => Ok(Self::Branch),
            other => Err(AppError::Config(format!(
                "role `{other}` is not one of standalone, hq, branch"
            ))),
        }
    }
}

/// What a bare `sultan` does on this machine (`mode` in `sultan.toml`, written by the setup
/// wizard).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Server and register in one app: start an embedded server and open the register.
    Single,
    /// A server (HQ): started with `sultan serve`.
    Server,
    /// A terminal: open the register window of the paired register.
    Client,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Single => "single",
            Self::Server => "server",
            Self::Client => "client",
        })
    }
}

impl FromStr for Mode {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "single" => Ok(Self::Single),
            "server" => Ok(Self::Server),
            "client" => Ok(Self::Client),
            other => Err(AppError::Config(format!(
                "mode `{other}` is not one of single, server, client"
            ))),
        }
    }
}

/// The app type of this machine: `mode`, or inferred for a file written before the wizard
/// existed (`[register]` → client, `db` → server). `None`: not set up, run the wizard.
///
/// # Errors
/// [`AppError::Config`] for an unknown `mode`.
pub fn resolve_mode(file: &FileConfig) -> Result<Option<Mode>> {
    if let Some(mode) = &file.mode {
        return mode.parse().map(Some);
    }
    Ok(if file.register.is_some() {
        Some(Mode::Client)
    } else if file.db.is_some() {
        Some(Mode::Server)
    } else {
        None
    })
}

/// The paired register, written by `sultan register pair`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterConfig {
    /// Base URL of the branch (or standalone) server, e.g. `http://192.168.1.10:8080`.
    pub server: String,
    /// Register ID.
    pub register_id: String,
    /// Branch ID.
    pub branch_id: String,
    /// Register code, e.g. `R1`.
    pub register_code: String,
    /// Register name.
    pub register_name: String,
}

/// The typed view of `sultan.toml`. Every key is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct FileConfig {
    /// Database URL.
    pub db: Option<String>,
    /// Listen address of `sultan serve`.
    pub listen: Option<String>,
    /// Node role of `sultan serve`.
    pub role: Option<String>,
    /// App type ([`Mode`]), written by the setup wizard.
    pub mode: Option<String>,
    /// The paired register, if any.
    pub register: Option<RegisterConfig>,
}

/// The config file: `flag` > `SULTAN_CONFIG` > `./sultan.toml`.
#[must_use]
pub fn config_path(flag: Option<&Path>, env: Env<'_>) -> PathBuf {
    flag.map(Path::to_path_buf)
        .or_else(|| env(ENV_CONFIG).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG))
}

/// The directory holding the config file (`.` for a bare file name).
#[must_use]
pub fn config_dir(config: &Path) -> PathBuf {
    match config.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The first value that is set: flag, then environment, then file.
fn pick(flag: Option<&str>, env: Option<String>, file: Option<&str>) -> Option<String> {
    flag.map(str::to_owned)
        .or(env)
        .or_else(|| file.map(str::to_owned))
}

/// The database URL (see the module table).
#[must_use]
pub fn resolve_db(flag: Option<&str>, env: Env<'_>, file: &FileConfig, config: &Path) -> String {
    pick(flag, env(ENV_DB), file.db.as_deref()).unwrap_or_else(|| {
        let file = config_dir(config).join(DEFAULT_DB_FILE);
        // Absolute, so the URL `sultan init` writes keeps working from any working directory.
        let file = std::path::absolute(&file).unwrap_or(file);
        format!("sqlite://{}", file.display())
    })
}

/// The listen address (see the module table).
#[must_use]
pub fn resolve_listen(flag: Option<&str>, env: Env<'_>, file: &FileConfig) -> String {
    pick(flag, env(ENV_LISTEN), file.listen.as_deref()).unwrap_or_else(|| DEFAULT_LISTEN.to_owned())
}

/// The node role (see the module table).
///
/// # Errors
/// [`AppError::Config`] for an unknown role in the environment or the file.
pub fn resolve_role(flag: Option<Role>, env: Env<'_>, file: &FileConfig) -> Result<Role> {
    if let Some(role) = flag {
        return Ok(role);
    }
    pick(None, env(ENV_ROLE), file.role.as_deref()).map_or(Ok(Role::default()), |s| s.parse())
}

/// Reads the raw table of `path`; a missing file is an empty table.
fn read_table(path: &Path) -> Result<toml::Table> {
    match fs::read_to_string(path) {
        Ok(text) => text
            .parse::<toml::Table>()
            .map_err(|e| AppError::Config(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(AppError::file("cannot read", path, e)),
    }
}

/// Loads `sultan.toml`; a missing file is an empty config.
///
/// # Errors
/// [`AppError::File`] if it cannot be read, [`AppError::Config`] if it is not valid.
pub fn load(path: &Path) -> Result<FileConfig> {
    let table = read_table(path)?;
    toml::Value::Table(table)
        .try_into()
        .map_err(|e| AppError::Config(format!("{}: {e}", path.display())))
}

/// Applies `change` to the raw table of `path` and writes it back (mode `0600` on Unix). Keys
/// that `change` does not touch are kept.
///
/// # Errors
/// [`AppError::File`] / [`AppError::Config`].
pub fn update(path: &Path, change: impl FnOnce(&mut toml::Table)) -> Result<()> {
    let mut table = read_table(path)?;
    change(&mut table);
    let text = toml::to_string(&table).map_err(|e| AppError::Config(e.to_string()))?;
    write_private(path, text.as_bytes())
}

/// Sets `[register]` in the config file.
///
/// # Errors
/// See [`update`].
pub fn set_register(path: &Path, register: &RegisterConfig) -> Result<()> {
    let value = toml::Value::try_from(register).map_err(|e| AppError::Config(e.to_string()))?;
    update(path, |table| {
        table.insert("register".to_owned(), value);
    })
}

/// Writes `contents` to `path` so that only the owner can read it (mode `0600` on Unix, also
/// when the file already existed). Creates missing parent directories.
///
/// # Errors
/// [`AppError::File`].
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| AppError::file("cannot create", dir, e))?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        // An existing file keeps its old mode on open; tighten it before writing.
        if path.is_file() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .map_err(|e| AppError::file("cannot protect", path, e))?;
        }
    }
    let mut file = options
        .open(path)
        .map_err(|e| AppError::file("cannot write", path, e))?;
    file.write_all(contents)
        .map_err(|e| AppError::file("cannot write", path, e))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    fn file_with(db: Option<&str>, listen: Option<&str>, role: Option<&str>) -> FileConfig {
        FileConfig {
            db: db.map(str::to_owned),
            listen: listen.map(str::to_owned),
            role: role.map(str::to_owned),
            mode: None,
            register: None,
        }
    }

    #[test]
    fn mode_is_read_or_inferred() {
        let mut file = FileConfig::default();
        assert_eq!(resolve_mode(&file).unwrap(), None, "not set up");
        file.db = Some("sqlite://x".into());
        assert_eq!(resolve_mode(&file).unwrap(), Some(Mode::Server));
        file.register = Some(RegisterConfig {
            server: "s".into(),
            register_id: "r".into(),
            branch_id: "b".into(),
            register_code: "R1".into(),
            register_name: "K".into(),
        });
        assert_eq!(resolve_mode(&file).unwrap(), Some(Mode::Client));
        for (text, mode) in [
            ("single", Mode::Single),
            (" Server ", Mode::Server),
            ("CLIENT", Mode::Client),
        ] {
            file.mode = Some(text.into());
            assert_eq!(resolve_mode(&file).unwrap(), Some(mode));
            assert_eq!(mode.to_string().parse::<Mode>().unwrap(), mode);
        }
        file.mode = Some("till".into());
        assert_eq!(
            resolve_mode(&file).unwrap_err().to_string(),
            "invalid setting: mode `till` is not one of single, server, client"
        );
    }

    #[test]
    fn db_precedence_is_flag_env_file_default() {
        let config = Path::new("/etc/sultan/sultan.toml");
        // (flag, env, file, expected)
        let cases = [
            (
                Some("mysql://f"),
                Some("mysql://e"),
                Some("mysql://c"),
                "mysql://f",
            ),
            (None, Some("mysql://e"), Some("mysql://c"), "mysql://e"),
            (None, None, Some("mysql://c"), "mysql://c"),
            (None, None, None, "sqlite:///etc/sultan/sultan.db"),
            (Some("mysql://f"), None, None, "mysql://f"),
            (None, Some("mysql://e"), None, "mysql://e"),
        ];
        for (flag, env, file, expected) in cases {
            let pairs: Vec<_> = env.map(|v| (ENV_DB, v)).into_iter().collect();
            let env = env_of(&pairs);
            let got = resolve_db(flag, &env, &file_with(file, None, None), config);
            assert_eq!(got, expected, "{flag:?} {pairs:?} {file:?}");
        }
        // A bare config file name puts the default database in the working directory, as an
        // absolute path.
        let env = env_of(&[]);
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            resolve_db(None, &env, &FileConfig::default(), Path::new("sultan.toml")),
            format!("sqlite://{}", cwd.join(DEFAULT_DB_FILE).display())
        );
    }

    #[test]
    fn listen_precedence_is_flag_env_file_default() {
        let cases = [
            (
                Some("0.0.0.0:1"),
                Some("0.0.0.0:2"),
                Some("0.0.0.0:3"),
                "0.0.0.0:1",
            ),
            (None, Some("0.0.0.0:2"), Some("0.0.0.0:3"), "0.0.0.0:2"),
            (None, None, Some("0.0.0.0:3"), "0.0.0.0:3"),
            (None, None, None, DEFAULT_LISTEN),
        ];
        for (flag, env, file, expected) in cases {
            let pairs: Vec<_> = env.map(|v| (ENV_LISTEN, v)).into_iter().collect();
            let env = env_of(&pairs);
            assert_eq!(
                resolve_listen(flag, &env, &file_with(None, file, None)),
                expected,
                "{flag:?} {pairs:?} {file:?}"
            );
        }
    }

    #[test]
    fn role_precedence_is_flag_env_file_default() {
        let cases = [
            (Some(Role::Hq), Some("branch"), Some("standalone"), Role::Hq),
            (None, Some("branch"), Some("hq"), Role::Branch),
            (None, None, Some("HQ"), Role::Hq),
            (None, None, None, Role::Standalone),
            (Some(Role::Branch), Some("nonsense"), None, Role::Branch),
        ];
        for (flag, env, file, expected) in cases {
            let pairs: Vec<_> = env.map(|v| (ENV_ROLE, v)).into_iter().collect();
            let env = env_of(&pairs);
            assert_eq!(
                resolve_role(flag, &env, &file_with(None, None, file)).unwrap(),
                expected,
                "{flag:?} {pairs:?} {file:?}"
            );
        }
        let env = env_of(&[(ENV_ROLE, "register")]);
        let err = resolve_role(None, &env, &FileConfig::default()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid setting: role `register` is not one of standalone, hq, branch"
        );
    }

    #[test]
    fn config_path_precedence_is_flag_env_default() {
        let cases = [
            (Some("/a.toml"), Some("/b.toml"), "/a.toml"),
            (None, Some("/b.toml"), "/b.toml"),
            (None, None, DEFAULT_CONFIG),
        ];
        for (flag, env, expected) in cases {
            let pairs: Vec<_> = env.map(|v| (ENV_CONFIG, v)).into_iter().collect();
            let env = env_of(&pairs);
            assert_eq!(
                config_path(flag.map(Path::new), &env),
                PathBuf::from(expected)
            );
        }
        assert_eq!(config_dir(Path::new("sultan.toml")), PathBuf::from("."));
        assert_eq!(config_dir(Path::new("/x/sultan.toml")), PathBuf::from("/x"));
    }

    #[test]
    fn roles_map_to_node_roles_and_print_lowercase() {
        let cases = [
            (Role::Standalone, NodeRole::Standalone, "standalone"),
            (Role::Hq, NodeRole::Hq, "hq"),
            (Role::Branch, NodeRole::Branch, "branch"),
        ];
        for (role, node, text) in cases {
            assert_eq!(role.node_role(), node);
            assert_eq!(role.to_string(), text);
            assert_eq!(text.parse::<Role>().unwrap(), role);
        }
    }

    #[test]
    fn process_env_treats_empty_as_unset() {
        assert_eq!(process_env("SULTAN_SURELY_NOT_SET_ANYWHERE"), None);
        assert!(process_env("PATH").is_some());
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sultan-config-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn load_update_and_keep_unknown_keys() {
        let dir = temp_dir("rw");
        let path = dir.join("sub/sultan.toml");
        assert_eq!(
            load(&path).unwrap(),
            FileConfig::default(),
            "missing = empty"
        );

        write_private(&path, b"listen = \"0.0.0.0:9\"\nfuture_key = 1\n").unwrap();
        update(&path, |t| {
            t.insert("db".into(), "sqlite://x.db".into());
        })
        .unwrap();
        let register = RegisterConfig {
            server: "http://s".into(),
            register_id: "r".into(),
            branch_id: "b".into(),
            register_code: "R1".into(),
            register_name: "Kasir 1".into(),
        };
        set_register(&path, &register).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.db.as_deref(), Some("sqlite://x.db"));
        assert_eq!(loaded.listen.as_deref(), Some("0.0.0.0:9"));
        assert_eq!(loaded.register, Some(register));
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("future_key = 1")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        fs::write(&path, "db = [").unwrap();
        assert!(matches!(load(&path), Err(AppError::Config(_))));
        fs::write(&path, "db = 5").unwrap();
        assert!(matches!(load(&path), Err(AppError::Config(_))));
        // A directory cannot be read as a file nor written over.
        assert!(matches!(load(&dir), Err(AppError::File { .. })));
        assert!(matches!(
            write_private(&dir.join("sub"), b"x"),
            Err(AppError::File { .. })
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn write_private_tightens_an_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"new").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        fs::remove_dir_all(dir).unwrap();
    }
}
