//! The application-wide settings state.
//!
//! [`AppSettings`] loaded from disk lives in a gpui global so that every view
//! reads one consistent snapshot. The settings dialog replaces the global and
//! saves to disk when the user applies changes; everything else only reads.
//!
//! # A file that could not be read
//!
//! The app starts on defaults when `settings.json` cannot be read, but it never
//! writes them: they are not the user's settings, and saving them would replace
//! a file the user may still be able to repair with nothing they chose.
//! [`load_error`] says why, the settings dialog shows it, and every write
//! refuses until a restart reads the file again.

use anyhow::Result;
use gpui::{App, Global, Hsla, SharedString};
use rulogman_core::AppSettings;

/// Global wrapper holding the current [`AppSettings`].
pub struct CurrentSettings(pub AppSettings);

impl Global for CurrentSettings {}

/// Why `settings.json` could not be read at start-up; see [`load_error`].
struct UnreadableSettings(SharedString);

impl Global for UnreadableSettings {}

/// Install the settings global from disk. Call once at start-up.
///
/// A file that cannot be read falls back to defaults; the app must start
/// regardless of what is on disk. Those defaults are never written over it:
/// see [`load_error`].
pub fn init(cx: &mut App) {
    install(AppSettings::load(), cx);
}

/// Installs what reading the settings file came to.
pub(crate) fn install(loaded: Result<AppSettings>, cx: &mut App) {
    match loaded {
        Ok(settings) => {
            cx.set_global(CurrentSettings(settings));
            if cx.has_global::<UnreadableSettings>() {
                cx.remove_global::<UnreadableSettings>();
            }
        }
        Err(err) => {
            log::warn!("starting with default settings: {err:#}");
            cx.set_global(CurrentSettings(AppSettings::default()));
            cx.set_global(UnreadableSettings(format!("{err:#}").into()));
        }
    }
}

/// Why `settings.json` could not be read at start-up, if it could not.
///
/// While this answers, the settings are defaults standing in for a file that
/// is still there, and nothing writes them: [`save`] refuses and the settings
/// dialog refuses to apply, showing this instead.
pub fn load_error(cx: &App) -> Option<SharedString> {
    cx.try_global::<UnreadableSettings>()
        .map(|unreadable| unreadable.0.clone())
}

/// Writes `settings` to `settings.json`, unless [`load_error`] answers.
///
/// # Errors
///
/// Fails without writing while the file could not be read at start-up, and
/// when the write itself fails.
pub fn save(settings: &AppSettings, cx: &App) -> Result<()> {
    write(settings, cx, AppSettings::save)
}

/// Hands `settings` to `save`, unless the settings global stands in for a
/// file that could not be read.
fn write(
    settings: &AppSettings,
    cx: &App,
    save: impl FnOnce(&AppSettings) -> Result<()>,
) -> Result<()> {
    if let Some(error) = load_error(cx) {
        anyhow::bail!("settings.json could not be read at start-up and is left as it is: {error}");
    }
    save(settings)
}

/// A snapshot of the current settings.
pub fn current(cx: &App) -> AppSettings {
    cx.try_global::<CurrentSettings>()
        .map(|g| g.0.clone())
        .unwrap_or_default()
}

/// Replace the settings global. The caller is responsible for persistence and
/// for re-applying the settings to open windows and sessions.
pub fn replace(settings: AppSettings, cx: &mut App) {
    cx.set_global(CurrentSettings(settings));
}

/// Applies the configured window opacity to a background fill.
///
/// Only a fill that covers the window edge to edge — the empty state, the
/// terminal surface — may use this, and **at most one such fill may cover any
/// given pixel**. The window surface starts out fully transparent, so a single
/// translucent fill lets the desktop (or the acrylic blur behind the window)
/// show through. A second one on top does not: gpui's Windows renderer blends
/// the alpha channel additively (`SrcBlendAlpha = ONE, DestBlendAlpha = ONE`),
/// so two fills of, say, 0.75 and 0.62 saturate the surface alpha at 1.0 and
/// the window goes opaque. That is what an opaque fill on the workspace root
/// used to do to every translucent fill under it, and it is why the connection
/// overlay — which dims the terminal while a session is not live — turns the
/// window opaque until the session connects.
///
/// The opacity itself lives in a widget-layer global rather than being read
/// off [`current`] here, so that the leaves which have to agree with this can
/// reach it — `rugpui`'s own widgets ask [`rugpui::window_translucent`] and paint no
/// background of their own while the window is see-through. [`set_tint`] is what
/// pushes it there, at start-up and on a settings *save*, which is also what
/// keeps this from following an unsaved edit: the fill is only half of what
/// makes a window translucent, and the other half — the platform surface being
/// told to permit alpha — happens only when the settings are saved.
pub fn window_tint(color: Hsla, cx: &App) -> Hsla {
    rugpui_shell::window_tint(color, cx)
}

/// Publishes the configured opacity to the widget layer.
///
/// Call after [`rugpui::init`], which installs a fully opaque default of its own,
/// and again whenever the settings are saved — in the same breath as the window
/// is told what background appearance to wear.
pub fn set_tint(settings: &AppSettings, cx: &mut App) {
    rugpui::set_window_tint(settings.window.background_opacity, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn settings_that_could_not_be_read_are_not_saved_over(cx: &mut gpui::TestAppContext) {
        // Saving the defaults the app started on, with one choice made since,
        // would replace every setting the user has with nothing they chose.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        let damaged = b"{ \"theme\": \"dracula\", ";
        std::fs::write(&path, damaged).unwrap();

        cx.update(|cx| {
            install(AppSettings::load_from(&path), cx);
            assert!(load_error(cx).is_some());
            let settings = AppSettings {
                ignored_update: Some("v9.9.9".to_owned()),
                ..current(cx)
            };
            assert!(write(&settings, cx, |settings| settings.save_to(&path)).is_err());
        });
        assert_eq!(std::fs::read(&path).unwrap(), damaged);

        // A file that reads — here, none at all — is written as it always was.
        std::fs::remove_file(&path).unwrap();
        cx.update(|cx| {
            install(AppSettings::load_from(&path), cx);
            assert!(load_error(cx).is_none());
            write(&current(cx), cx, |settings| settings.save_to(&path))
                .expect("the file is written");
        });
        assert!(path.exists());
    }
}
