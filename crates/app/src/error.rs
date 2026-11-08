//! Errors of the `sultan` commands. Every message is meant for the person at the terminal: it says
//! what went wrong and, where there is one, the next step.

use std::path::PathBuf;

use sultan_db::DbError;

/// A failed `sultan` command.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// A setting has an invalid value (flag, environment variable or `sultan.toml`).
    #[error("invalid setting: {0}")]
    Config(String),
    /// Reading or writing a local file failed.
    #[error("{action} {}: {source}", path.display())]
    File {
        /// What was being done, e.g. "cannot read".
        action: &'static str,
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The database failed.
    #[error("database error: {0}")]
    Db(#[from] DbError),
    /// `sultan init` on a database that already has a company.
    #[error(
        "the database is already initialised (company \"{0}\"); `sultan init` only sets up a new database"
    )]
    AlreadyInitialised(String),
    /// `sultan serve` on a database that `sultan init` has not set up.
    #[error("the database {0} is not initialised; run `sultan init` first")]
    NotInitialised(String),
    /// A value given to `sultan init` breaks a rule (PIN, password, codes, names).
    #[error("{0}")]
    Invalid(String),
    /// A secret was neither given nor could be asked for.
    #[error("{0}")]
    MissingSecret(String),
    /// The prompt for a secret failed.
    #[error("cannot read the {what} from the terminal: {source}")]
    Prompt {
        /// Which secret.
        what: &'static str,
        /// The cause.
        source: std::io::Error,
    },
    /// The listener could not be bound or the server failed.
    #[error("server error on {addr}: {source}")]
    Serve {
        /// The listen address.
        addr: String,
        /// The cause.
        source: std::io::Error,
    },
    /// Pairing with the server failed.
    #[error("pairing failed: {0}")]
    Pair(String),
    /// `sultan register` on a machine that is not paired.
    #[error("{NOT_PAIRED}")]
    NotPaired,
    /// The register window could not be opened or failed.
    #[error("register window: {0}")]
    Ui(String),
}

/// What to do on a machine that is not paired.
pub const NOT_PAIRED: &str = "not paired: run `sultan register pair --server <url> --code <code>` \
                              with a pairing code from back office";

impl AppError {
    /// A [`AppError::File`] error.
    pub(crate) fn file(
        action: &'static str,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::File {
            action,
            path: path.into(),
            source,
        }
    }
}

/// Result of a `sultan` command.
pub type Result<T> = std::result::Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_next_step() {
        let cases = [
            (
                AppError::NotInitialised("sqlite://x.db".into()),
                "the database sqlite://x.db is not initialised; run `sultan init` first",
            ),
            (
                AppError::AlreadyInitialised("PT A".into()),
                "the database is already initialised (company \"PT A\"); `sultan init` only sets up a new database",
            ),
            (
                AppError::file(
                    "cannot read",
                    "/x/sultan.toml",
                    std::io::Error::other("boom"),
                ),
                "cannot read /x/sultan.toml: boom",
            ),
            (
                AppError::Prompt {
                    what: "PIN",
                    source: std::io::Error::other("eof"),
                },
                "cannot read the PIN from the terminal: eof",
            ),
            (
                AppError::Serve {
                    addr: "1.2.3.4:5".into(),
                    source: std::io::Error::other("in use"),
                },
                "server error on 1.2.3.4:5: in use",
            ),
            (AppError::Pair("x".into()), "pairing failed: x"),
            (AppError::Config("y".into()), "invalid setting: y"),
            (
                AppError::Ui("no display".into()),
                "register window: no display",
            ),
            (AppError::NotPaired, NOT_PAIRED),
        ];
        for (error, message) in cases {
            assert_eq!(error.to_string(), message);
        }
    }
}
