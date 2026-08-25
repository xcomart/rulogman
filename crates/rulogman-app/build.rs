//! Embeds the application icon into the Windows executable, and re-runs the
//! build when a translation changes.
//!
//! The icon goes in under resource ID 1, which is not arbitrary: gpui's
//! Windows backend loads exactly `LoadImageW(module, MAKEINTRESOURCE(1), ...)`
//! for the window class icon (see `load_icon` in `src/platform.rs` of the
//! vendored `gpui_windows`, which lives in `ruui`). One embedded icon
//! therefore covers Explorer, the taskbar and the running window. Other
//! platforms have no build step: a bare binary carries no icon on macOS (that
//! needs an .app bundle) or Linux (that needs a .desktop entry).

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
    // `rust_i18n::i18n!` reads the YAML at macro expansion time, and a proc
    // macro cannot register the files it read with cargo. Without this line an
    // edited translation would not rebuild the crate.
    println!("cargo:rerun-if-changed=locales");

    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            winresource::WindowsResource::new()
                .set_icon_with_id("../../assets/icon.ico", "1")
                .compile()
                .expect("failed to embed the Windows icon resource");
        }
    }
}
