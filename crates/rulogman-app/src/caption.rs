//! Keeps the native title bar readable and in step with the app theme.
//!
//! Windows and macOS both draw the caption themselves, and both drive it from
//! the *system* theme. A light app on a dark desktop — or the reverse — thus
//! gets a title bar that clashes with its own chrome. The remedy differs per
//! platform.
//!
//! On Windows, rulogman uses the standard DWM caption (no custom-drawn title
//! bar), so the title text and the minimise / maximise / close glyphs are
//! painted by Windows, not by us. Two things make that go wrong:
//!
//! * gpui only ever tells DWM which system theme is in use
//!   (`DWMWA_USE_IMMERSIVE_DARK_MODE`, set once at window creation and again
//!   when the OS theme changes).
//! * With `background_blur` on, gpui asks for `ACCENT_ENABLE_ACRYLICBLURBEHIND`
//!   via the undocumented `SetWindowCompositionAttribute`. That accent policy
//!   covers the *whole* window, caption included, and DWM then paints the
//!   caption glyphs as if the caption were glass: near-white text and buttons.
//!   On a light desktop the acrylic surface is also near-white, so the title
//!   vanishes and the buttons fade to almost nothing.
//!
//! Pinning `DWMWA_CAPTION_COLOR` and `DWMWA_TEXT_COLOR` to our own palette
//! takes the caption out of the accent policy's hands entirely: the caption
//! becomes an opaque strip in the app's surface color with the app's text
//! color on it, in every focus state and whatever the desktop is doing. Those
//! two attributes need Windows 11 (build 22000); on older builds the calls
//! fail harmlessly and the immersive-dark-mode flag, which we also set from
//! the *app* theme rather than the system one, still gets the contrast right.
//!
//! On macOS none of our colors are wanted: AppKit already draws the title and
//! the traffic lights correctly for whichever `NSAppearance` is in force, and
//! the wrong one is in force only because it is inherited from the system.
//! Overriding it from the app theme is the whole fix, and gpui does the
//! overriding: [`App::set_window_appearance`] sets `NSApplication.appearance`,
//! which every window of the process then takes its chrome from. That is
//! coarser than the per-window pinning this module used to do by hand, and it
//! costs nothing here — rulogman has one theme at a time, for every window it
//! owns.
//!
//! Nothing on Linux: the caption there is the compositor's, themed by the
//! desktop rather than by the window, and when rulogman draws its own it draws
//! it out of the same palette as the rest of the chrome.

use gpui::{App, Window};

use crate::ui::Theme;

#[cfg(target_os = "windows")]
mod platform {
    use std::mem::size_of;

    use gpui::{Hsla, Rgba, Window};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{COLORREF, HWND};
    use windows::Win32::Graphics::Dwm::{
        DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute,
    };
    use windows::core::BOOL;

    use crate::ui::Theme;

    /// Packs a theme color into the `0x00BBGGRR` `COLORREF` DWM expects.
    ///
    /// The alpha channel is dropped: DWM has no use for it here, and the
    /// caption is deliberately opaque so the acrylic behind it cannot bleed
    /// through and wash the glyphs out again.
    fn colorref(color: Hsla) -> COLORREF {
        let rgba = Rgba::from(color);
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
        COLORREF(channel(rgba.r) | (channel(rgba.g) << 8) | (channel(rgba.b) << 16))
    }

    /// Extracts the Win32 handle backing `window`, if there is one.
    ///
    /// Spelled as an explicit trait call because gpui's `Window` also has an
    /// inherent `window_handle()` — a gpui-internal id, not the OS handle —
    /// which would otherwise win name resolution.
    fn hwnd(window: &Window) -> Option<HWND> {
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(HWND(handle.hwnd.get() as *mut _)),
            _ => None,
        }
    }

    /// Sets one DWM window attribute, logging rather than propagating failures.
    ///
    /// Every attribute used here is optional decoration; a Windows 10 host that
    /// rejects the caption colors should still run.
    fn set_attribute<T>(hwnd: HWND, attribute: u32, value: &T) {
        let result = unsafe {
            DwmSetWindowAttribute(
                hwnd,
                windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(attribute as i32),
                value as *const T as *const _,
                size_of::<T>() as u32,
            )
        };
        if let Err(error) = result {
            log::debug!("DwmSetWindowAttribute({attribute}) failed: {error}");
        }
    }

    /// Repaints the caption in the app's own colors.
    pub fn apply(window: &Window, theme: &Theme) {
        let Some(hwnd) = hwnd(window) else {
            return;
        };

        // Windows 10 only understands this one, and there it is the whole fix:
        // it flips the caption between the light and dark system presets. On
        // Windows 11 the explicit colors below win outright — measured — so
        // gpui re-asserting this attribute from the *system* theme when the
        // desktop switches light/dark cannot undo the caption there.
        let dark: BOOL = theme.dark.into();
        set_attribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE.0 as u32, &dark);
        // Windows 11 (build 22000) and up. DWM picks the caption glyph color
        // from the luminance of the caption color, so these two cover the
        // title, the minimise/maximise/close buttons and the strip behind them.
        set_attribute(hwnd, DWMWA_CAPTION_COLOR.0 as u32, &colorref(theme.surface));
        set_attribute(hwnd, DWMWA_TEXT_COLOR.0 as u32, &colorref(theme.text));
    }
}

/// Repaints the window caption to match `theme`.
///
/// A no-op on Linux, whose windows here have no separately themed caption to
/// correct.
#[cfg(target_os = "windows")]
pub fn apply_caption_theme(window: &Window, theme: &Theme, _cx: &App) {
    platform::apply(window, theme);
}

/// Repaints the window caption to match `theme`.
///
/// A no-op on Linux, whose windows here have no separately themed caption to
/// correct.
///
/// Only the theme's darkness is handed over, never its colors: those are
/// AppKit's to choose, and it chooses them well once told which side of
/// light/dark the app is on. Safe to call from inside a gpui update even
/// though `-setAppearance:` synchronously delivers
/// `viewDidChangeEffectiveAppearance` back into gpui's view — gpui's own
/// appearance-changed hook defers the work it does to the next foreground
/// turn precisely so that this cannot re-enter an `App` that is already
/// borrowed.
///
/// `window` goes unread: the override is app-wide, and every window of the
/// process picks it up. It stays in the signature so the call sites hand the
/// same three arguments over on every platform.
#[cfg(target_os = "macos")]
pub fn apply_caption_theme(_window: &Window, theme: &Theme, cx: &App) {
    cx.set_window_appearance(Some(if theme.dark {
        gpui::WindowAppearance::Dark
    } else {
        gpui::WindowAppearance::Light
    }));
}

/// Repaints the window caption to match `theme`.
///
/// A no-op on Linux, whose windows here have no separately themed caption to
/// correct.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn apply_caption_theme(_window: &Window, _theme: &Theme, _cx: &App) {}
