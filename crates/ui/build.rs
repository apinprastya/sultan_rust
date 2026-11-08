//! Compiles `ui/app.slint` with the `fluent-dark` style (as in `sultan-spike`) and bundles the
//! Indonesian translation of every `@tr()` string (`translations/id/LC_MESSAGES/sultan-ui.po`).
//!
//! Debug builds (tests, coverage) also carry Slint's element debug info, which the headless tests
//! need to find elements by their accessible label (`i-slint-backend-testing::ElementHandle`);
//! release builds leave it out.

use slint_build::{CompilerConfiguration, DefaultTranslationContext};

fn main() {
    let debug = std::env::var("PROFILE").is_ok_and(|p| p == "debug");
    let config = CompilerConfiguration::new()
        .with_style("fluent-dark".into())
        .with_bundled_translations("translations")
        .with_default_translation_context(DefaultTranslationContext::None)
        .with_debug_info(debug);
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint build failed");
}
