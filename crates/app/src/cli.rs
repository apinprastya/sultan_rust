//! The command line (`clap` derive), plan Phase 7. Run `sultan --help` / `sultan <cmd> --help`.
//!
//! Flags only hold what the person typed; [`crate::config`] merges them with the environment,
//! `sultan.toml` and the defaults.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use sultan_core::doc_number::DEFAULT_TIME_ZONE;

use crate::config::Role;

const DB_HELP: &str = "Database URL, e.g. sqlite://sultan.db or mysql://user:pass@host/sultan \
                       [env: SULTAN_DB] [default: sqlite://<config dir>/sultan.db]";

/// Sultan POS: one binary for standalone, HQ and branch servers and registers.
#[derive(Debug, Parser)]
#[command(name = "sultan", version, about, long_about = None)]
pub struct Cli {
    /// Config file.
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Config file [env: SULTAN_CONFIG] [default: ./sultan.toml]"
    )]
    pub config: Option<PathBuf>,
    /// What to do; none runs this machine as it was set up (single app, server hint or
    /// terminal), or opens the setup wizard on a new machine.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the setup wizard (a window): single app, HQ server or terminal. A bare `sultan` on
    /// a machine that is not set up opens it too.
    Setup,
    /// Set up a new database: company, first branch, its stock location and the Owner account.
    Init(Box<InitArgs>),
    /// Run the API server.
    Serve(ServeArgs),
    /// The register (till): without a subcommand, open the register window of this paired
    /// machine; `status` prints whether it is paired; `pair` pairs it.
    Register(RegisterArgs),
    /// Open the back-office window (catalog: products, categories, brands); log in with a
    /// back-office username and password.
    Backoffice(BackofficeArgs),
}

/// `sultan backoffice`.
#[derive(Debug, Args)]
pub struct BackofficeArgs {
    /// Server URL.
    #[arg(
        long,
        value_name = "URL",
        help = "Server URL, e.g. http://192.168.1.10:8080 [env: SULTAN_SERVER] [default: the \
                paired register's server, else this machine's listen address, else \
                http://127.0.0.1:8080]"
    )]
    pub server: Option<String>,
    /// Username to start with.
    #[arg(long, value_name = "USERNAME")]
    pub user: Option<String>,
}

/// `sultan init`.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Database URL.
    #[arg(long, value_name = "URL", help = DB_HELP)]
    pub db: Option<String>,
    /// Company name.
    #[arg(long)]
    pub company: String,
    /// First branch code, 1-10 letters or digits (used in document numbers), e.g. JKT01.
    #[arg(long, value_name = "CODE")]
    pub branch: String,
    /// First branch name [default: the branch code].
    #[arg(long, value_name = "NAME")]
    pub branch_name: Option<String>,
    /// First branch address.
    #[arg(long, value_name = "TEXT", default_value = "")]
    pub branch_address: String,
    /// IANA time zone of the first branch; document numbers use its local date.
    #[arg(long, value_name = "ZONE", default_value = DEFAULT_TIME_ZONE,
        help = "IANA time zone of the first branch, e.g. Asia/Jakarta (WIB), Asia/Makassar (WITA), \
                Asia/Jayapura (WIT); document numbers use the branch-local date")]
    pub timezone: String,
    /// Name of the branch's default receiving stock location.
    #[arg(long, value_name = "NAME", default_value = "Store floor")]
    pub location_name: String,
    /// Owner username (back-office login).
    #[arg(long, value_name = "USERNAME")]
    pub owner: String,
    /// Owner display name on the register lock screen.
    #[arg(long, value_name = "NAME", default_value = "Owner")]
    pub owner_name: String,
    /// Owner password (scripts only; see the help text).
    #[arg(
        long,
        value_name = "PASSWORD",
        help = "Owner password, at least 8 characters [env: SULTAN_OWNER_PASSWORD]. For scripts \
                only: a flag shows up in the process list and shell history. Asked for (hidden) \
                when omitted"
    )]
    pub owner_password: Option<String>,
    /// Owner PIN (scripts only; see the help text).
    #[arg(
        long,
        value_name = "PIN",
        help = "Owner PIN, 4-6 digits [env: SULTAN_OWNER_PIN]. For scripts only, like \
                --owner-password. Asked for (hidden) when omitted"
    )]
    pub owner_pin: Option<String>,
}

/// `sultan serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Node role.
    #[arg(
        long,
        value_enum,
        help = "Node role [env: SULTAN_ROLE] [default: standalone]. branch is a read-only replica \
                and, until HQ sync exists, serves its local database only"
    )]
    pub role: Option<Role>,
    /// Listen address.
    #[arg(
        long,
        value_name = "ADDR",
        help = "Listen address [env: SULTAN_LISTEN] [default: 127.0.0.1:8080]; use 0.0.0.0:8080 \
                for the shop LAN"
    )]
    pub listen: Option<String>,
    /// Database URL.
    #[arg(long, value_name = "URL", help = DB_HELP)]
    pub db: Option<String>,
}

/// `sultan register`.
#[derive(Debug, Args)]
pub struct RegisterArgs {
    /// What to do; none opens the register window.
    #[command(subcommand)]
    pub action: Option<RegisterAction>,
}

/// `sultan register <action>`.
#[derive(Debug, Subcommand)]
pub enum RegisterAction {
    /// Print whether this machine is paired, and with which register and server.
    Status,
    /// Pair this machine with a register using a one-time pairing code from back office.
    Pair {
        /// Server URL.
        #[arg(
            long,
            value_name = "URL",
            help = "Server URL, e.g. http://192.168.1.10:8080"
        )]
        server: String,
        /// The pairing code (valid 10 minutes, single use).
        #[arg(long)]
        code: String,
    },
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_each_command() {
        let cli = Cli::try_parse_from([
            "sultan",
            "--config",
            "/x/s.toml",
            "init",
            "--company",
            "PT A",
            "--branch",
            "JKT01",
            "--owner",
            "owner",
        ])
        .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/x/s.toml")));
        let Some(Command::Init(init)) = cli.command else {
            panic!("init expected");
        };
        assert_eq!(init.location_name, "Store floor");
        assert_eq!(init.timezone, "Asia/Jakarta");
        assert_eq!(init.owner_name, "Owner");
        assert!(init.owner_pin.is_none() && init.branch_name.is_none());

        let cli =
            Cli::try_parse_from(["sultan", "serve", "--role", "branch", "--config", "c"]).unwrap();
        assert_eq!(
            cli.config,
            Some(PathBuf::from("c")),
            "global flag after the command"
        );
        assert!(matches!(
            cli.command,
            Some(Command::Serve(ServeArgs {
                role: Some(Role::Branch),
                ..
            }))
        ));

        let cli = Cli::try_parse_from(["sultan", "register"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Register(RegisterArgs { action: None }))
        ));
        let cli = Cli::try_parse_from(["sultan", "register", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Register(RegisterArgs {
                action: Some(RegisterAction::Status)
            }))
        ));
        let cli =
            Cli::try_parse_from(["sultan", "register", "pair", "--server", "s", "--code", "c"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Register(RegisterArgs {
                action: Some(RegisterAction::Pair { .. })
            }))
        ));
        let cli = Cli::try_parse_from(["sultan", "backoffice", "--server", "http://h:1"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Backoffice(BackofficeArgs {
                server: Some(_),
                user: None
            }))
        ));
        assert!(Cli::try_parse_from(["sultan"]).unwrap().command.is_none());
        assert!(matches!(
            Cli::try_parse_from(["sultan", "setup"]).unwrap().command,
            Some(Command::Setup)
        ));

        assert!(Cli::try_parse_from(["sultan", "serve", "--role", "register"]).is_err());
        assert!(Cli::try_parse_from(["sultan", "init", "--company", "x"]).is_err());
    }
}
