//! Pools, transactions and the backend dispatch used by every repository.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use sqlx::migrate::Migrator;
use sqlx::mysql::{MySql, MySqlPool, MySqlPoolOptions};
use sqlx::sqlite::{
    Sqlite, SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions,
};

use crate::error::{DbError, Result};

static SQLITE_MIGRATIONS: Migrator = sqlx::migrate!("./migrations/sqlite");
static MYSQL_MIGRATIONS: Migrator = sqlx::migrate!("./migrations/mysql");

/// Which database engine a [`Db`] talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// `SQLite` (standalone and small branch nodes).
    Sqlite,
    /// `MySQL` 8 (HQ and larger branch nodes).
    MySql,
}

/// A connection pool for one of the supported backends (`docs/plan.md` §1: `SQLite` + `MySQL`).
#[derive(Debug, Clone)]
pub enum Db {
    /// `SQLite` pool.
    Sqlite(SqlitePool),
    /// `MySQL` pool.
    MySql(MySqlPool),
}

impl Db {
    /// Connects to `url` (`sqlite:...` or `mysql://...`).
    ///
    /// `SQLite` files are created if missing and use WAL; foreign keys are enforced. An in-memory
    /// `SQLite` database (`sqlite::memory:`) gets a pool of exactly one connection that is never
    /// recycled, because each connection would otherwise see its own empty database. With one
    /// connection, do not use the [`Db`] while a [`Tx`] on it is open: go through the `Tx`.
    ///
    /// On Unix a `SQLite` file and its `-wal`, `-shm` and `-journal` side files are readable and
    /// writable by the owner only (`0600`, Phase 9): a missing file is created with that mode
    /// before `SQLite` opens it (`SQLite` gives the side files the main file's mode), and existing
    /// files with group or other bits are tightened, each with a `tracing` warning. See
    /// [`secure_sqlite_files`].
    ///
    /// # Errors
    /// [`DbError::UnsupportedUrl`] for another scheme, or the connection error.
    pub async fn connect(url: &str) -> Result<Self> {
        if url.starts_with("sqlite:") {
            let in_memory = url.contains(":memory:") || url.contains("mode=memory");
            let options = SqliteConnectOptions::from_str(url)?
                .create_if_missing(true)
                .foreign_keys(true)
                .busy_timeout(Duration::from_secs(5));
            let (options, pool) = if in_memory {
                let pool = SqlitePoolOptions::new()
                    .max_connections(1)
                    .min_connections(1)
                    .idle_timeout(None)
                    .max_lifetime(None);
                (options, pool)
            } else {
                let options = options.journal_mode(SqliteJournalMode::Wal);
                (options, SqlitePoolOptions::new())
            };
            let file = (!in_memory).then(|| options.get_filename().to_path_buf());
            if let Some(file) = &file {
                prepare_sqlite_file(file)?;
            }
            let pool = pool.connect_with(options).await?;
            if let Some(file) = &file {
                secure_sqlite_files(file)?;
            }
            Ok(Self::Sqlite(pool))
        } else if url.starts_with("mysql:") {
            Ok(Self::MySql(MySqlPoolOptions::new().connect(url).await?))
        } else {
            Err(DbError::UnsupportedUrl(url.to_owned()))
        }
    }

    /// Applies the embedded migrations of this backend. Idempotent.
    ///
    /// # Errors
    /// The migration error.
    pub async fn migrate(&self) -> Result<()> {
        match self {
            Self::Sqlite(pool) => SQLITE_MIGRATIONS.run(pool).await?,
            Self::MySql(pool) => MYSQL_MIGRATIONS.run(pool).await?,
        }
        Ok(())
    }

    /// Starts a transaction.
    ///
    /// On `SQLite` it is `BEGIN IMMEDIATE`: the write lock is taken up front, waiting up to the
    /// busy timeout for another writer. A deferred transaction that reads first and writes later
    /// would instead fail at once with `SQLITE_BUSY_SNAPSHOT` when another connection committed
    /// in between (WAL cannot upgrade a stale read snapshot, and the busy timeout does not apply).
    /// Readers are not blocked (WAL); transactions are serialized, as `SQLite` writers are anyway.
    /// `MySQL` uses a plain `BEGIN`.
    ///
    /// # Errors
    /// The database error.
    pub async fn begin(&self) -> Result<Tx> {
        Ok(match self {
            Self::Sqlite(pool) => Tx::Sqlite(pool.begin_with("BEGIN IMMEDIATE").await?),
            Self::MySql(pool) => Tx::MySql(pool.begin().await?),
        })
    }

    /// The backend of this pool.
    #[must_use]
    pub const fn backend(&self) -> Backend {
        match self {
            Self::Sqlite(_) => Backend::Sqlite,
            Self::MySql(_) => Backend::MySql,
        }
    }

    /// Closes the pool, waiting for connections to be returned.
    pub async fn close(&self) {
        match self {
            Self::Sqlite(pool) => pool.close().await,
            Self::MySql(pool) => pool.close().await,
        }
    }
}

/// The `SQLite` side files of database `file`: `-wal`, `-shm` and `-journal`.
fn side_files(file: &Path) -> [PathBuf; 3] {
    ["-wal", "-shm", "-journal"].map(|suffix| {
        let mut name = file.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    })
}

/// Creates a missing database file with mode `0600` (Unix), so `SQLite` never creates it with
/// the umask's mode, then tightens what already exists.
fn prepare_sqlite_file(file: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if !file.exists() {
            let created = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(file);
            match created {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(DbError::Io(e)),
            }
        }
    }
    secure_sqlite_files(file)?;
    Ok(())
}

/// Makes `SQLite` database `file` and its existing `-wal`, `-shm` and `-journal` side files
/// readable and writable by the owner only (`0600`) on Unix; a no-op elsewhere. Returns the
/// files whose mode had group or other bits and was tightened (a `tracing` warning is logged for
/// each). Files that do not exist are skipped. [`Db::connect`] calls it for every `SQLite` file.
///
/// # Errors
/// [`DbError::Io`] if a mode cannot be read or changed.
pub fn secure_sqlite_files(file: &Path) -> Result<Vec<PathBuf>> {
    let mut tightened = Vec::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in std::iter::once(file.to_path_buf()).chain(side_files(file)) {
            let metadata = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(DbError::Io(e)),
            };
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .map_err(DbError::Io)?;
                tracing::warn!(
                    file = %path.display(),
                    old_mode = format_args!("{mode:o}"),
                    "database file was readable by others; changed to 0600"
                );
                tightened.push(path);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (file, side_files);
    Ok(tightened)
}

/// An open transaction. Dropping it without [`commit`](Self::commit) rolls it back.
#[derive(Debug)]
pub enum Tx {
    /// `SQLite` transaction.
    Sqlite(sqlx::Transaction<'static, Sqlite>),
    /// `MySQL` transaction.
    MySql(sqlx::Transaction<'static, MySql>),
}

impl Tx {
    /// Commits the transaction.
    ///
    /// # Errors
    /// The database error.
    pub async fn commit(self) -> Result<()> {
        match self {
            Self::Sqlite(tx) => tx.commit().await?,
            Self::MySql(tx) => tx.commit().await?,
        }
        Ok(())
    }

    /// Rolls the transaction back.
    ///
    /// # Errors
    /// The database error.
    pub async fn rollback(self) -> Result<()> {
        match self {
            Self::Sqlite(tx) => tx.rollback().await?,
            Self::MySql(tx) => tx.rollback().await?,
        }
        Ok(())
    }

    /// The backend of this transaction.
    #[must_use]
    pub const fn backend(&self) -> Backend {
        match self {
            Self::Sqlite(_) => Backend::Sqlite,
            Self::MySql(_) => Backend::MySql,
        }
    }
}

/// Where a repository function runs: on a pool (each statement takes a pooled connection) or
/// inside a transaction. Build it with `(&db).into()` or `(&mut tx).into()`; repository functions
/// take `impl Into<Conn>` so callers just pass `&db` or `&mut tx`.
#[derive(Debug)]
pub enum Conn<'c> {
    /// Run on the pool.
    Db(&'c Db),
    /// Run inside the transaction.
    Tx(&'c mut Tx),
}

impl Conn<'_> {
    /// Borrows this `Conn` again for one more statement.
    pub fn reborrow(&mut self) -> Conn<'_> {
        match self {
            Self::Db(db) => Conn::Db(db),
            Self::Tx(tx) => Conn::Tx(tx),
        }
    }

    /// The backend, for the rare statements that differ (see [`crate::auth`]).
    #[must_use]
    pub const fn backend(&self) -> Backend {
        match self {
            Self::Db(db) => db.backend(),
            Self::Tx(tx) => tx.backend(),
        }
    }
}

impl<'c> From<&'c Db> for Conn<'c> {
    fn from(db: &'c Db) -> Self {
        Self::Db(db)
    }
}

impl<'c> From<&'c mut Tx> for Conn<'c> {
    fn from(tx: &'c mut Tx) -> Self {
        Self::Tx(tx)
    }
}

/// Runs `$body` with `$c` bound to a `&mut SqliteConnection` or `&mut MySqlConnection`, taken
/// from the pool or the transaction behind a [`Conn`]. The body is written once and compiled for
/// both backends, which is how all shared SQL avoids a per-backend copy. Only `crate::sql` uses
/// it.
macro_rules! with_connection {
    ($conn:expr, |$c:ident| $body:expr) => {
        match $conn {
            $crate::Conn::Db($crate::Db::Sqlite(pool)) => {
                let mut pooled = pool.acquire().await?;
                let $c = &mut *pooled;
                $body
            }
            $crate::Conn::Db($crate::Db::MySql(pool)) => {
                let mut pooled = pool.acquire().await?;
                let $c = &mut *pooled;
                $body
            }
            $crate::Conn::Tx($crate::Tx::Sqlite(tx)) => {
                let $c = &mut **tx;
                $body
            }
            $crate::Conn::Tx($crate::Tx::MySql(tx)) => {
                let $c = &mut **tx;
                $body
            }
        }
    };
}
pub(crate) use with_connection;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unsupported_url_is_rejected() {
        let err = Db::connect("postgres://x").await.unwrap_err();
        assert!(matches!(err, DbError::UnsupportedUrl(u) if u == "postgres://x"));
    }

    #[tokio::test]
    async fn sqlite_file_database_uses_a_normal_pool() {
        let dir = std::env::temp_dir().join(format!("sultan-db-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}", dir.join("t.db").display());
        let db = Db::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        assert_eq!(db.backend(), Backend::Sqlite);
        let mut tx = db.begin().await.unwrap();
        assert_eq!(tx.backend(), Backend::Sqlite);
        assert_eq!(Conn::from(&mut tx).backend(), Backend::Sqlite);
        tx.rollback().await.unwrap();
        db.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `SQLite` transactions start with `BEGIN IMMEDIATE`: a second one waits for the first
    /// instead of starting a read snapshot that could not be upgraded to a write later.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_transactions_take_the_write_lock_up_front() {
        let dir = std::env::temp_dir().join(format!("sultan-db-imm-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}", dir.join("t.db").display());
        let db = Db::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        let first = db.begin().await.unwrap();
        let waiting = tokio::time::timeout(Duration::from_millis(200), db.begin()).await;
        assert!(
            waiting.is_err(),
            "a second transaction started beside the first"
        );
        first.commit().await.unwrap();
        let second = tokio::time::timeout(Duration::from_secs(5), db.begin())
            .await
            .expect("the write lock was released")
            .unwrap();
        second.rollback().await.unwrap();
        db.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sultan-db-{name}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Phase 9: a new database and its WAL side files are created `0600`.
    #[cfg(unix)]
    #[tokio::test]
    async fn new_sqlite_files_are_owner_only() {
        let dir = temp_dir("perm-new");
        let file = dir.join("sultan.db");
        let db = Db::connect(&format!("sqlite://{}", file.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let [wal, shm, _] = side_files(&file);
        for path in [&file, &wal, &shm] {
            assert!(path.exists(), "{}", path.display());
            assert_eq!(mode(path), 0o600, "{}", path.display());
        }
        db.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Phase 9: existing files that others can read are tightened on connect (and reported).
    #[cfg(unix)]
    #[tokio::test]
    async fn existing_sqlite_files_are_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perm-old");
        let file = dir.join("sultan.db");
        let db = Db::connect(&format!("sqlite://{}", file.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        db.close().await;
        let [wal, shm, journal] = side_files(&file);
        std::fs::write(&journal, b"").unwrap();
        std::fs::write(&wal, b"").unwrap();
        for (path, old) in [(&file, 0o644), (&journal, 0o664), (&wal, 0o600)] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(old)).unwrap();
        }
        assert!(!shm.exists());
        let tightened = secure_sqlite_files(&file).unwrap();
        assert_eq!(tightened, [file.clone(), journal.clone()]);
        assert_eq!(secure_sqlite_files(&file).unwrap(), Vec::<PathBuf>::new());

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
        let db = Db::connect(&format!("sqlite://{}", file.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        for path in [&file, &wal, &shm, &journal] {
            assert_eq!(mode(path), 0o600, "{}", path.display());
        }
        db.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_locations_are_io_errors() {
        // A file inside a missing directory cannot be created.
        let err = prepare_sqlite_file(Path::new("/nonexistent-sultan-dir/x.db")).unwrap_err();
        assert!(matches!(err, DbError::Io(_)), "{err}");
    }

    #[test]
    fn side_file_names() {
        let names = side_files(Path::new("/d/sultan.db"));
        assert_eq!(
            names,
            [
                "/d/sultan.db-wal",
                "/d/sultan.db-shm",
                "/d/sultan.db-journal"
            ]
            .map(PathBuf::from)
        );
    }
}
