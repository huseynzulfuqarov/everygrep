fn main() {
    // The translations in translations/<lang>/LC_MESSAGES/everygrep-gui.po
    // are compiled into the program; texts in the .slint files are marked
    // with @tr() and have no context, so one catalog serves the .slint files
    // and the Rust code (see src/i18n.rs).
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        .with_bundled_translations("translations")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/app.slint", config).expect("UI compiles");
}
