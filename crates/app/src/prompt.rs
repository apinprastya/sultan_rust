//! Secrets for `sultan init`: the Owner's password and PIN (spec 2.1 §4).
//!
//! Order: **flag > environment variable > hidden prompt**. The prompt only runs on a terminal and
//! asks twice; without a terminal (scripts, CI) a missing secret is an error that names the flag
//! and the variable. Flags and variables exist for scripting only: a flag is visible to other
//! users in the process list and in shell history, so prefer the prompt or the variable.

use std::io::IsTerminal;

use crate::config::Env;
use crate::error::{AppError, Result};

/// Asks for a secret without echoing it. Injected so tests never prompt.
pub trait Prompter {
    /// Whether a person can answer (stdin is a terminal).
    fn is_interactive(&self) -> bool;
    /// Reads one line without echo.
    ///
    /// # Errors
    /// The terminal error.
    fn secret(&mut self, prompt: &str) -> std::io::Result<String>;
}

/// The real terminal (`rpassword`, no echo).
#[derive(Debug, Default, Clone, Copy)]
pub struct TerminalPrompter;

impl Prompter for TerminalPrompter {
    fn is_interactive(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn secret(&mut self, prompt: &str) -> std::io::Result<String> {
        rpassword::prompt_password(prompt)
    }
}

/// One secret `sultan init` needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretSpec {
    /// Name in messages, e.g. "Owner password".
    pub what: &'static str,
    /// The flag that sets it.
    pub flag: &'static str,
    /// The environment variable that sets it.
    pub env: &'static str,
}

/// The Owner's password.
pub const OWNER_PASSWORD: SecretSpec = SecretSpec {
    what: "Owner password",
    flag: "--owner-password",
    env: "SULTAN_OWNER_PASSWORD",
};

/// The Owner's PIN.
pub const OWNER_PIN: SecretSpec = SecretSpec {
    what: "Owner PIN",
    flag: "--owner-pin",
    env: "SULTAN_OWNER_PIN",
};

/// The secret from the flag, the environment or the prompt (asked twice, must match).
///
/// # Errors
/// [`AppError::MissingSecret`] without a terminal, [`AppError::Invalid`] when the two answers
/// differ, [`AppError::Prompt`] when the terminal fails.
pub fn obtain(
    spec: SecretSpec,
    flag: Option<String>,
    env: Env<'_>,
    prompter: &mut dyn Prompter,
) -> Result<String> {
    if let Some(value) = flag.or_else(|| env(spec.env)) {
        return Ok(value);
    }
    if !prompter.is_interactive() {
        return Err(AppError::MissingSecret(format!(
            "the {} is required: pass {} or set {} (no terminal to ask on)",
            spec.what, spec.flag, spec.env
        )));
    }
    let read = |prompter: &mut dyn Prompter, prompt: String| {
        prompter.secret(&prompt).map_err(|source| AppError::Prompt {
            what: spec.what,
            source,
        })
    };
    let first = read(prompter, format!("{}: ", spec.what))?;
    let second = read(prompter, format!("{} (again): ", spec.what))?;
    if first == second {
        Ok(first)
    } else {
        Err(AppError::Invalid(format!(
            "the two {} entries differ",
            spec.what
        )))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// Answers prompts from a queue; `None` answers are terminal errors.
    #[derive(Debug, Default)]
    pub struct FakePrompter {
        pub interactive: bool,
        pub answers: VecDeque<Option<String>>,
        pub asked: Vec<String>,
    }

    impl FakePrompter {
        pub fn answering(answers: &[&str]) -> Self {
            Self {
                interactive: true,
                answers: answers.iter().map(|a| Some((*a).to_owned())).collect(),
                asked: Vec::new(),
            }
        }
    }

    impl Prompter for FakePrompter {
        fn is_interactive(&self) -> bool {
            self.interactive
        }

        fn secret(&mut self, prompt: &str) -> std::io::Result<String> {
            self.asked.push(prompt.to_owned());
            self.answers
                .pop_front()
                .flatten()
                .ok_or_else(|| std::io::Error::other("closed"))
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn flag_then_env_then_prompt() {
        let env = |k: &str| (k == "SULTAN_OWNER_PIN").then(|| "2222".to_owned());
        let mut prompter = FakePrompter::answering(&["3333", "3333"]);
        let cases = [
            (Some("1111"), &env as Env<'_>, "1111"),
            (None, &env, "2222"),
            (None, &no_env, "3333"),
        ];
        for (flag, env, expected) in cases {
            let got = obtain(OWNER_PIN, flag.map(str::to_owned), env, &mut prompter).unwrap();
            assert_eq!(got, expected);
        }
        assert_eq!(prompter.asked, ["Owner PIN: ", "Owner PIN (again): "]);
    }

    #[test]
    fn prompt_failures() {
        let mut quiet = FakePrompter::default();
        let err = obtain(OWNER_PASSWORD, None, &no_env, &mut quiet).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the Owner password is required: pass --owner-password or set SULTAN_OWNER_PASSWORD \
             (no terminal to ask on)"
        );
        assert!(quiet.asked.is_empty(), "never prompts without a terminal");

        let mut differ = FakePrompter::answering(&["a-password", "b-password"]);
        let err = obtain(OWNER_PASSWORD, None, &no_env, &mut differ).unwrap_err();
        assert_eq!(err.to_string(), "the two Owner password entries differ");

        let mut broken = FakePrompter::answering(&[]);
        let err = obtain(OWNER_PIN, None, &no_env, &mut broken).unwrap_err();
        assert!(matches!(
            err,
            AppError::Prompt {
                what: "Owner PIN",
                ..
            }
        ));
    }

    #[test]
    fn terminal_prompter_is_not_interactive_under_test() {
        // `cargo test` runs without a terminal on stdin in CI; just exercise the check.
        let _ = TerminalPrompter.is_interactive();
    }
}
