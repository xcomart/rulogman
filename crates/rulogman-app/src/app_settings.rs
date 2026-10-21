//! The application-wide settings state.
//!
//! [`AppSettings`] loaded from disk lives in a gpui global so that every view
//! reads one consistent snapshot. The settings dialog replaces the global and
//! saves to disk when the user applies changes; everything else only reads.
//!
//! The window geometry is the one part that flows the other way: every
//! workspace window records where it is as it is moved and resized (see
//! [`record_window_geometry`]), and [`save_on_exit`] writes the result out once,
//! when the last window closes or the app quits. Writing it as it changes would
//! put a file write in the middle of a resize drag. With several windows open,
//! the one moved or resized last is what the next launch comes back as.
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
use rugpui_shell::WindowGeometry;
use rulogman_core::{AppSettings, WindowSettings};

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

/// Writes `geometry` into `state`, leaving its appearance alone.
///
/// The two halves of a [`WindowSettings`] answer to different owners: the
/// placement follows the live window and is written back from it, while the
/// opacity, the blur and the title bar style are the user's choices and must
/// survive being recorded over.
fn apply_geometry(geometry: WindowGeometry, state: &mut WindowSettings) {
    state.x = Some(geometry.x);
    state.y = Some(geometry.y);
    state.width = geometry.width;
    state.height = geometry.height;
    state.maximized = geometry.maximized;
}

/// Whether `state` already records `geometry`.
fn records_geometry(geometry: WindowGeometry, state: &WindowSettings) -> bool {
    saved_geometry(state) == Some(geometry)
}

/// The placement `state` records, or `None` when it carries no position.
///
/// `None` is a first run: the caller centres the saved *size* on the active
/// display rather than guessing at coordinates.
pub fn saved_geometry(state: &WindowSettings) -> Option<WindowGeometry> {
    WindowGeometry::saved(state.x, state.y, state.width, state.height, state.maximized)
}

/// Records where a window is, without touching the disk.
///
/// Called from each workspace's window-bounds observer, so it runs on every
/// move and every step of a resize drag. The global is left alone unless a
/// value actually changed: the observer fires far more often than the rounded
/// geometry differs.
pub fn record_window_geometry(geometry: WindowGeometry, cx: &mut App) {
    let Some(settings) = cx.try_global::<CurrentSettings>() else {
        return;
    };
    if records_geometry(geometry, &settings.0.window) {
        return;
    }
    apply_geometry(geometry, &mut cx.global_mut::<CurrentSettings>().0.window);
}

/// Copies the geometry the settings global records into `settings`.
///
/// For a writer holding a snapshot taken earlier — the settings dialog keeps
/// the one it opened on — so that saving it does not put back a window size
/// the user has since dragged away from.
pub fn carry_window_geometry(settings: &mut AppSettings, cx: &App) {
    let current = current(cx).window;
    settings.window.x = current.x;
    settings.window.y = current.y;
    settings.window.width = current.width;
    settings.window.height = current.height;
    settings.window.maximized = current.maximized;
}

/// Writes the settings global out at shutdown, for the window geometry.
///
/// Reports rather than propagates: there is no longer a window to show a
/// failure in. Writes nothing while [`load_error`] answers.
pub fn save_on_exit(cx: &App) {
    if load_error(cx).is_some() {
        return;
    }
    if let Err(error) = save(&current(cx), cx) {
        log::warn!("could not save the window geometry: {error:#}");
    }
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

    /// A placement that is nothing like the defaults, so a value left behind by
    /// mistake shows up as itself.
    fn geometry() -> WindowGeometry {
        WindowGeometry {
            x: 120,
            y: -30,
            width: 1600,
            height: 980,
            maximized: true,
        }
    }

    #[gpui::test]
    fn a_recorded_placement_is_what_the_next_launch_opens_at(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut settings = AppSettings::default();
            settings.window.background_opacity = 0.7;
            replace(settings, cx);
            assert_eq!(saved_geometry(&current(cx).window), None);

            record_window_geometry(geometry(), cx);
            let window = current(cx).window;
            assert_eq!(saved_geometry(&window), Some(geometry()));
            // The appearance beside it is the user's and is left alone.
            assert_eq!(window.background_opacity, 0.7);
        });
    }

    #[gpui::test]
    fn a_dialog_snapshot_keeps_the_size_the_window_has_now(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            replace(AppSettings::default(), cx);
            let mut snapshot = current(cx);
            record_window_geometry(geometry(), cx);

            snapshot.window.background_blur = true;
            carry_window_geometry(&mut snapshot, cx);
            assert_eq!(saved_geometry(&snapshot.window), Some(geometry()));
            assert!(snapshot.window.background_blur);
        });
    }
}
