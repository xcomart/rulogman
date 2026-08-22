//! Minimise / maximise / close buttons for a window that draws its own caption.
//!
//! Only Windows and Linux need these: macOS keeps its native traffic lights
//! even with a transparent title bar, so the caller leaves the strip out there.
//!
//! Which buttons a strip holds, and in which order, is not this module's
//! decision — see [`split`]. A Linux desktop publishes a button layout, and
//! putting the close button on the left is a setting people actually use; the
//! two ends of the title bar therefore each get a strip, either of which may
//! come out empty.
//!
//! The buttons are wired twice over, and deliberately so. Each one marks itself
//! as a [`WindowControlArea`], which is what Windows needs: the hit test then
//! reports the area as a caption button, so the window procedure performs the
//! action natively and — on Windows 11 — the maximise button offers the snap
//! layouts on hover. That path never delivers a click to the app, which is why
//! the `on_click` handlers below exist for everywhere else.
//!
//! Every button also occludes: the strip sits inside the toolbar's drag area,
//! and without occlusion the drag hitbox would answer the hit test first and
//! the buttons would read as "move the window".

use gpui::{
    App, ElementId, Hsla, SharedString, Svg, Window, WindowButton, WindowButtonLayout,
    WindowControlArea, div, prelude::*, px, rgb, svg,
};

use super::theme::theme;

/// Width of one button, matching the caption buttons Windows draws.
const BUTTON_WIDTH: f32 = 46.;

/// Edge length of the glyph inside a button.
///
/// Half a toolbar icon: a caption glyph is meant to read as a hairline mark
/// rather than as a control of its own. It is the smallest thing the app draws,
/// which is why the four assets carry a heavier stroke than the rest of the set
/// (see [`crate::icons::WINDOW_MINIMIZE`]) and why their resting tint is
/// [`Theme::icon`](super::theme::Theme#structfield.icon) rather than the muted
/// text of the label beside them.
const GLYPH_SIZE: f32 = 12.;

/// Hover fill of the close button.
///
/// The one hardcoded color in the widget layer, and the only one that has to
/// be: this exact red is what Windows paints under a close button, so a themed
/// shade would read as a different control.
const CLOSE_HOVER: u32 = 0xE81123;

/// Glyph color on the hovered close button, over [`CLOSE_HOVER`].
const CLOSE_HOVER_GLYPH: u32 = 0xFFFFFF;

/// Style group of the minimise button, so hovering it recolours the glyph.
///
/// One name per button rather than one shared name: a `group_hover` resolves
/// against the nearest ancestor carrying the name, and these three are
/// siblings, so each answers to its own.
const MINIMIZE_GROUP: &str = "window-control-minimize";

/// Style group of the maximise button. See [`MINIMIZE_GROUP`].
const MAXIMIZE_GROUP: &str = "window-control-maximize";

/// Style group of the close button. See [`MINIMIZE_GROUP`].
const CLOSE_GROUP: &str = "window-control-close";

/// Asset paths of the four glyphs [`WindowControls`] draws.
///
/// Passed in rather than named here, the way [`super::TabBar`] takes its
/// dropdown icon: the widget layer carries no assets of its own.
#[derive(Debug, Clone)]
pub struct WindowControlIcons {
    /// Minimise.
    pub minimize: SharedString,
    /// Maximise, drawn while the window is not maximised.
    pub maximize: SharedString,
    /// Restore, drawn while the window is maximised.
    pub restore: SharedString,
    /// Close.
    pub close: SharedString,
}

/// Splits a desktop's button layout into the two strips a title bar draws.
///
/// `layout` is what the platform reports — GNOME's `button-layout` gsetting or
/// the KDE equivalent on Linux, and `None` everywhere else, which is also what
/// a Linux desktop that publishes nothing comes back as. `None` means the
/// familiar minimise / maximise / close on the right, which is what this
/// application drew before it asked at all.
///
/// `supported` is the *window's* answer rather than the desktop's, and the two
/// disagree often enough to matter: a compositor may offer no minimise while
/// the layout still names one. A button the window cannot perform is dropped
/// wherever it appears. Close is never dropped — no platform reports it as
/// unsupported, and a caption without a way to close the window would be a
/// trap.
pub fn split(
    layout: Option<WindowButtonLayout>,
    supported: gpui::WindowControls,
) -> (Vec<WindowButton>, Vec<WindowButton>) {
    let keep = |side: &[Option<WindowButton>]| -> Vec<WindowButton> {
        side.iter()
            .flatten()
            .copied()
            .filter(|button| match button {
                WindowButton::Minimize => supported.minimize,
                WindowButton::Maximize => supported.maximize,
                WindowButton::Close => true,
            })
            .collect()
    };

    match layout {
        Some(layout) => (keep(&layout.left), keep(&layout.right)),
        None => (
            Vec::new(),
            keep(&[
                Some(WindowButton::Minimize),
                Some(WindowButton::Maximize),
                Some(WindowButton::Close),
            ]),
        ),
    }
}

/// The caption buttons of a self-drawn title bar.
///
/// Stateless like every other widget here: it reads the window's own maximised
/// state to pick between the maximise and restore glyphs, and draws exactly the
/// buttons it is handed, in the order it is handed them — [`split`] has already
/// decided both.
#[derive(IntoElement)]
pub struct WindowControls {
    id: ElementId,
    icons: WindowControlIcons,
    buttons: Vec<WindowButton>,
}

impl WindowControls {
    /// Creates the button strip.
    ///
    /// `id` must be unique among the siblings of the strip, and — because a
    /// title bar can carry a strip at each end — among the strips themselves.
    pub fn new(
        id: impl Into<ElementId>,
        icons: WindowControlIcons,
        buttons: Vec<WindowButton>,
    ) -> Self {
        Self {
            id: id.into(),
            icons,
            buttons,
        }
    }
}

/// A caption glyph, sized and tinted.
fn glyph(path: SharedString, color: Hsla) -> Svg {
    svg()
        .size(px(GLYPH_SIZE))
        .flex_none()
        .path(path)
        .text_color(color)
}

impl RenderOnce for WindowControls {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = theme(cx);
        let maximized = window.is_maximized();
        let Self { id, icons, buttons } = self;

        // The frame shared by all three: the group that lets the hover fill
        // reach the glyph, and the area the platform hit test reads.
        let frame = |name: &'static str, group: &'static str, area: WindowControlArea| {
            div()
                .id(ElementId::from((id.clone(), name)))
                .group(group)
                .occlude()
                .window_control_area(area)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(BUTTON_WIDTH))
                .h_full()
                .cursor_pointer()
        };

        let buttons = buttons.into_iter().map(|button| match button {
            WindowButton::Minimize => {
                frame(button.id(), MINIMIZE_GROUP, WindowControlArea::Min)
                    .hover(|style| style.bg(theme.surface_hover))
                    // Never reached on Windows, where the hit test hands the
                    // press to the window procedure before the app sees a click.
                    .on_click(|_, window, _cx| window.minimize_window())
                    .child(
                        glyph(icons.minimize.clone(), theme.icon)
                            .group_hover(MINIMIZE_GROUP, move |style| style.text_color(theme.text)),
                    )
            }
            WindowButton::Maximize => {
                let path = if maximized {
                    icons.restore.clone()
                } else {
                    icons.maximize.clone()
                };
                frame(button.id(), MAXIMIZE_GROUP, WindowControlArea::Max)
                    .hover(|style| style.bg(theme.surface_hover))
                    .on_click(|_, window, _cx| window.zoom_window())
                    .child(
                        glyph(path, theme.icon)
                            .group_hover(MAXIMIZE_GROUP, move |style| style.text_color(theme.text)),
                    )
            }
            WindowButton::Close => frame(button.id(), CLOSE_GROUP, WindowControlArea::Close)
                .hover(|style| style.bg(rgb(CLOSE_HOVER)))
                .on_click(|_, window, _cx| window.remove_window())
                .child(
                    glyph(icons.close.clone(), theme.icon).group_hover(CLOSE_GROUP, |style| {
                        style.text_color(rgb(CLOSE_HOVER_GLYPH))
                    }),
                ),
        });

        div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h_full()
            .bg(theme.surface)
            .border_b_1()
            .border_color(theme.border)
            .children(buttons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window that can do everything, which is [`gpui::WindowControls`]'s own
    /// default and what both platforms that draw their own caption report.
    fn all() -> gpui::WindowControls {
        gpui::WindowControls::default()
    }

    /// A layout built the way a desktop's `button-layout` string parses into
    /// one, without going through the parser: `WindowButtonLayout::parse` is
    /// compiled on Linux alone, and these rules hold on every platform.
    fn layout(left: &[WindowButton], right: &[WindowButton]) -> WindowButtonLayout {
        let side = |buttons: &[WindowButton]| {
            let mut slots = [None; gpui::MAX_BUTTONS_PER_SIDE];
            for (slot, button) in slots.iter_mut().zip(buttons) {
                *slot = Some(*button);
            }
            slots
        };
        WindowButtonLayout {
            left: side(left),
            right: side(right),
        }
    }

    #[test]
    fn no_layout_is_the_familiar_strip_on_the_right() {
        let (left, right) = split(None, all());
        assert!(left.is_empty());
        assert_eq!(
            right,
            [
                WindowButton::Minimize,
                WindowButton::Maximize,
                WindowButton::Close
            ]
        );
    }

    #[test]
    fn a_layout_is_followed_side_by_side_and_in_order() {
        // GNOME's other common setting, `"close,minimize,maximize:"`:
        // everything on the left, close first.
        let all_left = layout(
            &[
                WindowButton::Close,
                WindowButton::Minimize,
                WindowButton::Maximize,
            ],
            &[],
        );
        let (left, right) = split(Some(all_left), all());
        assert_eq!(
            left,
            [
                WindowButton::Close,
                WindowButton::Minimize,
                WindowButton::Maximize
            ]
        );
        assert!(right.is_empty());

        // A split layout keeps each button on the side that named it.
        let both = layout(&[WindowButton::Close], &[WindowButton::Maximize]);
        let (left, right) = split(Some(both), all());
        assert_eq!(left, [WindowButton::Close]);
        assert_eq!(right, [WindowButton::Maximize]);
    }

    #[test]
    fn a_button_the_window_cannot_perform_is_dropped() {
        // A compositor offering neither, under a desktop that asks for both:
        // the window's answer wins, and close survives regardless.
        let supported = gpui::WindowControls {
            maximize: false,
            minimize: false,
            ..all()
        };

        let asked = layout(
            &[WindowButton::Minimize],
            &[WindowButton::Maximize, WindowButton::Close],
        );
        let (left, right) = split(Some(asked), supported);
        assert!(left.is_empty());
        assert_eq!(right, [WindowButton::Close]);

        // The fallback strip is filtered by the same rule.
        let (left, right) = split(None, supported);
        assert!(left.is_empty());
        assert_eq!(right, [WindowButton::Close]);
    }

    #[test]
    fn a_desktop_that_asks_for_no_buttons_gets_none() {
        let (left, right) = split(Some(layout(&[], &[])), all());
        assert!(left.is_empty());
        assert!(right.is_empty());
    }
}
