//! The small label that appears when the pointer rests on a control.
//!
//! gpui asks for tooltips as a *builder*: `.tooltip(f)` stores `f`, and calls it
//! to make a fresh view each time the pointer settles. The view has to be an
//! [`AnyView`], so a tooltip cannot be a plain element the way the other widgets
//! here are — it needs an entity behind it. [`tooltip_label`] hides that: it
//! takes the text once and hands back the closure `.tooltip` wants.
//!
//! Nothing here positions anything. gpui lays the view out at the pointer and,
//! when the box would cross a window edge, flips it to the other side of the
//! cursor on that axis — so the widget's only job is to be a box of the right
//! size, and adding an `anchored` or a `deferred` would fight machinery that has
//! already done the work.
//!
//! The styling is the menu panel's, one step quieter: a tooltip is read and
//! dismissed rather than clicked, so it takes [`Theme::surface`] instead of the
//! menu's page background and a softer shadow, which keeps it from reading as
//! something that can be pressed.

use gpui::{AnyView, App, SharedString, Window, div, prelude::*, px};

use super::theme::theme;

/// Horizontal padding of the label, in pixels.
const PADDING_X: f32 = 7.;

/// Vertical padding of the label, in pixels.
const PADDING_Y: f32 = 3.;

/// How far below the pointer the box is pushed, in pixels.
///
/// gpui puts the tooltip one pixel from the mouse position, which is the *tip*
/// of the arrow cursor and therefore underneath the rest of it. This clears the
/// glyph so the first word is not read through the pointer.
const CURSOR_CLEARANCE: f32 = 16.;

/// Builds the callback `.tooltip` takes, showing `label`.
///
/// ```ignore
/// div().id("save").tooltip(tooltip_label("Save")).child(icon)
/// ```
///
/// The text is captured once and cloned per hover, so the caller can hand over
/// a localised string without keeping it alive itself.
pub fn tooltip_label(
    label: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let label = label.into();
    move |_window, cx| {
        let label = label.clone();
        cx.new(|_| TooltipLabel { label }).into()
    }
}

/// The one-line tooltip view [`tooltip_label`] constructs.
struct TooltipLabel {
    /// Text shown in the box. Never wrapped: a tooltip that needs two lines is
    /// documentation, and belongs in the guide rather than under the pointer.
    label: SharedString,
}

impl Render for TooltipLabel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);

        div()
            // Margin rather than an offset passed to gpui: the margin is part of
            // the measured size, so the edge-flipping above still sees the box
            // the user actually sees.
            .mt(px(CURSOR_CLEARANCE))
            .flex_none()
            .px(px(PADDING_X))
            .py(px(PADDING_Y))
            .rounded_sm()
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .shadow_md()
            .text_size(px(11.))
            .text_color(theme.text)
            .whitespace_nowrap()
            .child(self.label.clone())
    }
}
