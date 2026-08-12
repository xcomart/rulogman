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
//! the traffic lights correctly for whichever `NSAppearance` the window
//! carries, and the window carries the wrong one only because it inherits the
//! system's. Pinning the window's appearance to `NSAppearanceNameDarkAqua` or
//! `NSAppearanceNameAqua` from the app theme is the whole fix.

use gpui::Window;

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

#[cfg(target_os = "macos")]
// objc 0.2's `msg_send!` and `class!` expand to a `cfg(feature =
// "cargo-clippy")` test, and the feature belongs to objc, not to us — so the
// check-cfg lint fires at every call site here. CI builds with `-D warnings`.
#[allow(unexpected_cfgs)]
mod platform {
    use gpui::Window;
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    type Id = *mut Object;

    /// Extracts the `NSWindow` backing `window`, if it has one yet.
    ///
    /// gpui hands out the `NSView`, not the window, so this hops one link up
    /// the responder chain; a view that has not been installed in a window
    /// answers `nil`. Spelled as an explicit trait call because gpui's
    /// `Window` also has an inherent `window_handle()` — a gpui-internal id,
    /// not the OS handle — which would otherwise win name resolution.
    fn ns_window(window: &Window) -> Option<Id> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return None;
        };
        let view = handle.ns_view.as_ptr() as Id;
        let ns_window: Id = unsafe { msg_send![view, window] };
        (!ns_window.is_null()).then_some(ns_window)
    }

    /// Pins the window's appearance to the app theme.
    ///
    /// Only the theme's darkness is wanted, not its colors: those are AppKit's
    /// to choose, and it chooses them well once told which side of light/dark
    /// the window is on. Without this the appearance is inherited from the
    /// system.
    pub fn apply(window: &Window, dark: bool) {
        let Some(ns_window) = ns_window(window) else {
            return;
        };
        unsafe {
            let name = if dark {
                NSAppearanceNameDarkAqua
            } else {
                NSAppearanceNameAqua
            };
            let appearance: Id = msg_send![class!(NSAppearance), appearanceNamed: name];
            if appearance.is_null() {
                return;
            }
            // Scheduled on the run loop rather than sent directly:
            // `-setAppearance:` synchronously delivers
            // `viewDidChangeEffectiveAppearance`, which gpui's view hooks to
            // re-enter the app — and this function is always called from
            // inside a gpui update, where that re-entry finds the App borrow
            // already taken and the appearance observers are dropped with a
            // "RefCell already borrowed" error in the log. A zero delay runs
            // it on the next run-loop turn, after the update has released the
            // borrow. The receiver and argument are retained by the
            // scheduling, so a window closed in between stays sound.
            let _: () = msg_send![
                ns_window,
                performSelector: sel!(setAppearance:)
                withObject: appearance
                afterDelay: 0.0f64
            ];
        }
    }

    // The appearance names are AppKit globals with no binding in the crates we
    // already depend on, so they are linked directly. Both have existed since
    // 10.14, well below anything this app targets.
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {
        static NSAppearanceNameAqua: Id;
        static NSAppearanceNameDarkAqua: Id;
    }
}

/// Repaints the window caption to match `theme`.
///
/// A no-op on Linux, whose windows here have no separately themed caption to
/// correct.
#[cfg(target_os = "windows")]
pub fn apply_caption_theme(window: &Window, theme: &Theme) {
    platform::apply(window, theme);
}

/// Repaints the window caption to match `theme`.
///
/// A no-op on Linux, whose windows here have no separately themed caption to
/// correct.
#[cfg(target_os = "macos")]
pub fn apply_caption_theme(window: &Window, theme: &Theme) {
    platform::apply(window, theme.dark);
}

/// Repaints the window caption to match `theme`.
///
/// A no-op on Linux, whose windows here have no separately themed caption to
/// correct.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn apply_caption_theme(_window: &Window, _theme: &Theme) {}
