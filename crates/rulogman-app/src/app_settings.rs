//! The application-wide settings state.
//!
//! [`AppSettings`] loaded from disk lives in a gpui global so that every view
//! reads one consistent snapshot. The settings dialog replaces the global and
//! saves to disk when the user applies changes; everything else only reads.

use gpui::{App, Global, Hsla};
use rulogman_core::AppSettings;

/// Global wrapper holding the current [`AppSettings`].
pub struct CurrentSettings(pub AppSettings);

impl Global for CurrentSettings {}

/// Install the settings global from disk. Call once at start-up.
///
/// A file that cannot be read falls back to defaults; the app must start
/// regardless of what is on disk.
pub fn init(cx: &mut App) {
    let settings = AppSettings::load().unwrap_or_else(|err| {
        log::warn!("starting with default settings: {err:#}");
        AppSettings::default()
    });
    cx.set_global(CurrentSettings(settings));
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
pub fn window_tint(color: Hsla, cx: &App) -> Hsla {
    let opacity = current(cx).window.background_opacity;
    if opacity < 1.0 {
        Hsla {
            a: opacity,
            ..color
        }
    } else {
        color
    }
}
