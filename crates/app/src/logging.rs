//! Logging (plan Phase 7): `tracing` with a `RUST_LOG` env-filter, written to stderr so stdout
//! stays for command output (banners, results).
//!
//! Without `RUST_LOG` the filter is [`DEFAULT_FILTER`]: `info` for Sultan and the request log
//! (`tower_http`), `warn` for the rest (sqlx logs every statement at `info`/`debug`). The request
//! log never contains header values, query strings or bodies (`sultan_server::router`).

use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;

/// The filter used when `RUST_LOG` is not set.
pub const DEFAULT_FILTER: &str =
    "warn,sultan=info,sultan_app=info,sultan_server=info,tower_http=info";

/// The filter from `rust_log` (the value of `RUST_LOG`), or [`DEFAULT_FILTER`] when it is unset or
/// invalid.
#[must_use]
pub fn filter(rust_log: Option<&str>) -> EnvFilter {
    rust_log
        .and_then(|directives| EnvFilter::try_new(directives).ok())
        .unwrap_or_else(|| EnvFilter::new(DEFAULT_FILTER))
}

/// Installs the global subscriber. A second call (e.g. in tests) is ignored.
pub fn init(rust_log: Option<&str>) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter(rust_log))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_falls_back_to_the_default() {
        let cases = [
            (None, DEFAULT_FILTER.to_owned()),
            (Some("debug"), "debug".to_owned()),
            (Some("sultan=[[["), DEFAULT_FILTER.to_owned()),
        ];
        for (rust_log, expected) in cases {
            let want = EnvFilter::new(expected).to_string();
            assert_eq!(filter(rust_log).to_string(), want, "{rust_log:?}");
        }
        // "off" keeps the test output clean; the second call is ignored.
        init(Some("off"));
        init(None);
    }
}
