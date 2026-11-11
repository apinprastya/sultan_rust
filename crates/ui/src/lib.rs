//! Sultan POS register UI (plan Phase 8; spec 2.1 §4–§6): lock screen with user tiles and PIN
//! pad, opening a cashier session, the session home with the barcode lookup (spec 2.2 §6, phase
//! C5), blind close with its report, and the manager-override dialog. Built with Slint (`fluent-dark` style, the `sultan-spike` theme
//! tokens) and keyboard-first, as proven by the spike.
//!
//! - [`controller`]: the screen flow as a plain state machine (all decisions, unit-tested).
//! - [`app`]: the glue to the Slint window; depends on [`sultan_client::Service`] only.
//! - [`amount`], [`count`], [`input`]: Rupiah formatting, the denomination count, keys, PIN and
//!   time texts.
//! - [`scan`]: the session home's barcode field (phase C5): typed code, waiting lookup, shown
//!   item.
//! - [`customer`]: the session home's customer step (spec 2.5, phase M5): find, sign up,
//!   attach, balance payout.
//! - [`setup`] and [`setup_app`]: the first-run setup wizard (single app, HQ server, branch
//!   server (not available yet), terminal), a separate window with its own controller.
//! - [`start`] and [`start_app`]: the Start screen of a single app or a terminal (register,
//!   back office, settings), a window of its own with its own controller.
//! - [`backoffice`] and [`backoffice_app`]: the back office (phase C4, spec 2.2): login,
//!   product list, product form, category and brand editors, and the customer screens (spec 2.5,
//!   phase M4: list, form, groups, tiers, duplicates and merge, negative balances, erase, balance
//!   settings; [`backoffice::customers`]), a window of its own with its own controller, driven
//!   through [`sultan_client::BackOffice`].
//! - `ui/*.slint`: the window, mostly declarative. Every user-facing string is `@tr()` in
//!   `ui/theme.slint` or `ui/app.slint`; the Indonesian translation is bundled from
//!   `translations/id/LC_MESSAGES/sultan-ui.po` and picked from the system locale or
//!   [`app::Options::language`].

pub mod amount;
pub mod app;
pub mod backoffice;
pub mod backoffice_app;
pub mod controller;
pub mod count;
pub mod customer;
pub mod input;
pub mod scan;
pub mod setup;
pub mod setup_app;
pub mod start;
pub mod start_app;

#[allow(clippy::all, clippy::pedantic, missing_docs)]
mod generated {
    slint::include_modules!();
}

pub use app::{App, Options, Runner, build_app, run};
pub use backoffice_app::{BackOfficeApp, BackOfficeOptions, build_backoffice, run_backoffice};
pub use generated::*;
pub use setup_app::{Setup, build_setup};
pub use start_app::{Start, build_start};
