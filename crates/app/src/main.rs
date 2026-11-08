//! `sultan`: the single Sultan POS binary (`docs/plan.md` §1, plan Phase 7). All logic lives in
//! the `sultan_app` library; this file wires it to the real process: arguments, `RUST_LOG`,
//! environment, terminal, stdout, Ctrl+C / `SIGTERM`, the register window and the setup wizard.

use std::process::ExitCode;

use clap::Parser;
use sultan_app::cli::Cli;
use sultan_app::config::process_env;
use sultan_app::prompt::TerminalPrompter;
use sultan_app::{Context, logging, open_register_window, open_setup_window, run, serve};

fn main() -> ExitCode {
    let cli = Cli::parse();
    logging::init(process_env("RUST_LOG").as_deref());
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut prompter = TerminalPrompter;
    let mut stdout = std::io::stdout();
    // The window runs on this (main) thread inside the runtime; its service calls are spawned
    // on the runtime's worker threads.
    let mut window = open_register_window;
    let mut wizard = open_setup_window;
    let ctx = Context {
        env: &process_env,
        prompter: &mut prompter,
        out: &mut stdout,
        shutdown: serve::shutdown_signal(),
        open_ui: &mut window,
        open_setup: &mut wizard,
    };
    match runtime.block_on(run(cli, ctx)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
