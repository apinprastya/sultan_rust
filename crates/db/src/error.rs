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
}
