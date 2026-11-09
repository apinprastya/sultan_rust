//! The error type of the persistence layer.

/// Result alias for this crate.
pub type Result<T, E = DbError> = std::result::Result<T, E>;

/// Errors of the repositories.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// A unique constraint was violated: a duplicate code, username or role name, or a second
    /// open cashier session for a register or user (the safety net of spec 2.1 §5).
    #[error("conflict: {0}")]
    Conflict(String),
    /// A foreign key points at a row that does not exist.
    #[error("referenced row does not exist: {0}")]
    MissingReference(String),
    /// The row the operation needs does not exist or is not in the required state.
    #[error("not found: {0}")]
    NotFound(&'static str),
    /// A stored value cannot be mapped to the domain type (e.g. an unknown permission key).
    #[error("invalid stored data: {0}")]
    Corrupt(String),
    /// The URL scheme is neither `sqlite:` nor `mysql:`.
    #[error("unsupported database url `{0}` (expected sqlite: or mysql:)")]
    UnsupportedUrl(String),
    /// A database file could not be created or its permissions changed.
    #[error("database file: {0}")]
    Io(#[source] std::io::Error),
    /// Applying migrations failed.
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// Any other database error.
    #[error(transparent)]
    Sqlx(sqlx::Error),
}

impl DbError {
    /// Whether retrying the whole transaction may succeed: a unique-key race
    /// ([`DbError::Conflict`]), a `MySQL` deadlock (1213) or lock wait timeout (1205), or a busy /
    /// locked `SQLite` database (`SQLITE_BUSY` with its extended codes such as `BUSY_SNAPSHOT`,
    /// and `SQLITE_LOCKED`). Used where a transaction allocates a sequence number (Phase 9).
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Conflict(_) => true,
            Self::Sqlx(sqlx::Error::Database(db)) => {
                if let Some(mysql) = db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>() {
                    matches!(mysql.number(), 1205 | 1213)
                } else {
                    db.code()
                        .and_then(|code| code.parse::<i32>().ok())
                        .is_some_and(|code| matches!(code & 0xff, 5 | 6))
                }
            }
            _ => false,
        }
    }
}

/// `SQLITE_BUSY` (primary result code 5), as `sqlx`'s `SqliteError::code` reports it.
#[cfg(any(test, feature = "test-util"))]
const SQLITE_BUSY: &str = "5";

#[cfg(any(test, feature = "test-util"))]
impl DbError {
    /// A `SQLite` "database is locked" error (`SQLITE_BUSY`) as a lost write-lock race produces
    /// it: a database error whose code is `5`, which [`DbError::is_retryable`] accepts. For tests
    /// of retry handling that cannot easily provoke a real lock race (`sqlx` does not let other
    /// crates build its `SqliteError`). The `tests` module checks that a real busy error has the
    /// same code.
    #[must_use]
    pub fn sqlite_busy() -> Self {
        Self::Sqlx(sqlx::Error::Database(Box::new(SqliteBusy)))
    }
}

/// The database error behind [`DbError::sqlite_busy`].
#[cfg(any(test, feature = "test-util"))]
#[derive(Debug)]
struct SqliteBusy;

#[cfg(any(test, feature = "test-util"))]
impl std::fmt::Display for SqliteBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Like `SqliteError`'s own message.
        let message = sqlx::error::DatabaseError::message(self);
        write!(f, "(code: {SQLITE_BUSY}) {message}")
    }
}

#[cfg(any(test, feature = "test-util"))]
impl std::error::Error for SqliteBusy {}

#[cfg(any(test, feature = "test-util"))]
impl sqlx::error::DatabaseError for SqliteBusy {
    fn message(&self) -> &'static str {
        "database is locked"
    }

    fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some(SQLITE_BUSY.into())
    }

    fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self
    }

    fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
        self
    }

    fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        self
    }

    fn kind(&self) -> sqlx::error::ErrorKind {
        sqlx::error::ErrorKind::Other
    }
}

impl From<sqlx::Error> for DbError {
    fn from(error: sqlx::Error) -> Self {
        match &error {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                Self::Conflict(db.message().to_owned())
            }
            sqlx::Error::Database(db) if db.is_foreign_key_violation() => {
                Self::MissingReference(db.message().to_owned())
            }
            _ => Self::Sqlx(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages() {
        assert_eq!(DbError::Conflict("x".into()).to_string(), "conflict: x");
        assert_eq!(
            DbError::MissingReference("y".into()).to_string(),
            "referenced row does not exist: y"
        );
        assert_eq!(DbError::NotFound("role").to_string(), "not found: role");
        assert_eq!(
            DbError::Corrupt("z".into()).to_string(),
            "invalid stored data: z"
        );
        assert_eq!(
            DbError::UnsupportedUrl("pg://".into()).to_string(),
            "unsupported database url `pg://` (expected sqlite: or mysql:)"
        );
        let other: DbError = sqlx::Error::RowNotFound.into();
        assert!(matches!(other, DbError::Sqlx(sqlx::Error::RowNotFound)));
    }

    #[test]
    fn retryable_errors() {
        assert!(DbError::Conflict("x".into()).is_retryable());
        let busy = DbError::sqlite_busy();
        assert!(busy.is_retryable());
        assert_eq!(
            busy.to_string(),
            "error returned from database: (code: 5) database is locked"
        );
        let DbError::Sqlx(sqlx::Error::Database(db)) = &busy else {
            panic!("{busy:?}");
        };
        assert_eq!(db.kind(), sqlx::error::ErrorKind::Other);
        assert!(std::error::Error::source(db.as_error()).is_none());
        for other in [
            DbError::Sqlx(sqlx::Error::PoolTimedOut),
            DbError::NotFound("x"),
            DbError::Corrupt("x".into()),
        ] {
            assert!(!other.is_retryable(), "{other:?}");
        }
    }

    /// [`DbError::sqlite_busy`] looks like what `SQLite` really answers when the write lock is
    /// taken and the busy timeout is zero.
    #[tokio::test]
    async fn sqlite_busy_matches_a_real_lock_race() {
        use sqlx::ConnectOptions;
        use sqlx::sqlite::SqliteConnectOptions;
        use std::str::FromStr;

        let dir = std::env::temp_dir().join(format!("sultan-db-busy-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}", dir.join("t.db").display());
        let options = SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true)
            .busy_timeout(std::time::Duration::ZERO);
        let mut holder = options.connect().await.unwrap();
        let mut loser = options.connect().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut holder)
            .await
            .unwrap();
        let real: DbError = sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut loser)
            .await
            .unwrap_err()
            .into();
        assert!(real.is_retryable(), "{real:?}");
        let code = |e: &DbError| match e {
            DbError::Sqlx(sqlx::Error::Database(db)) => db.code().map(std::borrow::Cow::into_owned),
            _ => None,
        };
        assert_eq!(code(&real), code(&DbError::sqlite_busy()));
        assert_eq!(real.to_string(), DbError::sqlite_busy().to_string());
        drop((holder, loser));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
