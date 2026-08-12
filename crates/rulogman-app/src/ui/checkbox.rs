//! A labelled on/off checkbox.

use gpui::{App, ClickEvent, ElementId, SharedString, Window, div, prelude::*, px};

use super::theme::theme;

/// Glyph painted inside the box while it is checked.
const CHECK_GLYPH: &str = "\u{2713}";

/// Callback fired with the value the checkbox is about to take.
type ToggleHandler = Box<dyn Fn(bool, &mut Window, &mut App)>;

/// A stateless checkbox with a clickable label.
///
/// Like [`Button`](super::Button) it owns no state: the parent view passes the
/// current value on every render and updates its own state from
/// [`Checkbox::on_toggle`], which receives the *new* value. Clicking anywhere on
/// the row — box or label — toggles it.
///
/// ```ignore
/// Checkbox::new("remember", "Remember password")
///     .checked(self.remember)
///     .on_toggle(cx.listener(|this, checked, _window, cx| {
///         this.remember = *checked;
///         cx.notify();
///     }))
/// ```
#[derive(IntoElement)]
pub struct Checkbox {
    id: ElementId,
    label: SharedString,
    checked: bool,
    tab_index: Option<isize>,
    on_toggle: Option<ToggleHandler>,
}

impl Checkbox {
    /// Creates an unchecked checkbox.
    ///
    /// `id` must be unique among the siblings of the checkbox.
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            checked: false,
            tab_index: None,
            on_toggle: None,
        }
    }

    /// Sets whether the box is ticked.
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    /// Places the checkbox at `index` in the window's tab order.
    ///
    /// A focused checkbox draws an accent outline and toggles on `Space` or
    /// `Enter`, which gpui delivers as an ordinary click.
    pub fn tab_index(mut self, index: isize) -> Self {
        self.tab_index = Some(index);
        self
    }

    /// Sets the callback invoked with the value the checkbox is toggling to.
    pub fn on_toggle(mut self, handler: impl Fn(bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_toggle = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for Checkbox {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = theme(cx);
        let checked = self.checked;
        let next = !checked;

        let (box_bg, box_border, glyph_color) = if checked {
            (theme.accent, theme.accent, theme.background)
        } else {
            (theme.surface, theme.border, gpui::transparent_black())
        };

        div()
            .id(self.id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .py(px(1.))
            .rounded_sm()
            // Transparent until focused, so the ring costs no layout.
            .border_1()
            .border_color(gpui::transparent_black())
            .cursor_pointer()
            .text_size(px(13.))
            .text_color(theme.text)
            .when_some(self.tab_index, |this, index| {
                let accent = theme.accent;
                this.tab_index(index)
                    .focus(move |style| style.border_color(accent))
            })
            .when_some(self.on_toggle, |this, handler| {
                this.on_click(move |_: &ClickEvent, window, cx| handler(next, window, cx))
            })
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(16.))
                    .rounded_sm()
                    .border_1()
                    .border_color(box_border)
                    .bg(box_bg)
                    .text_size(px(11.))
                    .text_color(glyph_color)
                    .child(if checked { CHECK_GLYPH } else { "" }),
            )
            .child(div().child(self.label))
    }
}
