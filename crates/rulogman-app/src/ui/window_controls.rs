//! Minimise / maximise / close buttons for a window that draws its own caption.
//!
//! Only Windows and Linux need these: macOS keeps its native traffic lights
//! even with a transparent title bar, so the caller leaves the strip out there.
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
    App, ElementId, Hsla, SharedString, Svg, Window, WindowControlArea, div, prelude::*, px, rgb,
    svg,
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

/// The caption buttons of a self-drawn title bar.
///
/// Stateless like every other widget here: it reads the window's own maximised
/// state to pick between the maximise and restore glyphs, and leaves out any
/// button the platform reports it does not support — which is how a Wayland
/// compositor that offers no minimise gets a strip without one.
#[derive(IntoElement)]
pub struct WindowControls {
    id: ElementId,
    icons: WindowControlIcons,
}

impl WindowControls {
    /// Creates the button strip.
    ///
    /// `id` must be unique among the siblings of the strip.
    pub fn new(id: impl Into<ElementId>, icons: WindowControlIcons) -> Self {
        Self {
            id: id.into(),
            icons,
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
        let supported = window.window_controls();
        let maximized = window.is_maximized();

        // The frame shared by all three: the group that lets the hover fill
        // reach the glyph, and the area the platform hit test reads.
        let button = |name: &'static str, group: &'static str, area: WindowControlArea| {
            div()
                .id(ElementId::from((self.id.clone(), name)))
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

        let minimize = supported.minimize.then(|| {
            button("minimize", MINIMIZE_GROUP, WindowControlArea::Min)
                .hover(|style| style.bg(theme.surface_hover))
                // Never reached on Windows, where the hit test hands the press
                // to the window procedure before the app sees a click.
                .on_click(|_, window, _cx| window.minimize_window())
                .child(
                    glyph(self.icons.minimize.clone(), theme.icon)
                        .group_hover(MINIMIZE_GROUP, move |style| style.text_color(theme.text)),
                )
        });

        let maximize = supported.maximize.then(|| {
            let path = if maximized {
                self.icons.restore.clone()
            } else {
                self.icons.maximize.clone()
            };
            button("maximize", MAXIMIZE_GROUP, WindowControlArea::Max)
                .hover(|style| style.bg(theme.surface_hover))
                .on_click(|_, window, _cx| window.zoom_window())
                .child(
                    glyph(path, theme.icon)
                        .group_hover(MAXIMIZE_GROUP, move |style| style.text_color(theme.text)),
                )
        });

        let close = button("close", CLOSE_GROUP, WindowControlArea::Close)
            .hover(|style| style.bg(rgb(CLOSE_HOVER)))
            .on_click(|_, window, _cx| window.remove_window())
            .child(
                glyph(self.icons.close.clone(), theme.icon).group_hover(CLOSE_GROUP, |style| {
                    style.text_color(rgb(CLOSE_HOVER_GLYPH))
                }),
            );

        div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h_full()
            .bg(theme.surface)
            .border_b_1()
            .border_color(theme.border)
            .children(minimize)
            .children(maximize)
            .child(close)
    }
}
