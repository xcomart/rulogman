//! A dropdown that picks one string out of a list.
//!
//! Like every other widget here the control is stateless: the parent view owns
//! the selected value, the open flag and the list's [`ScrollHandle`], passes
//! them in on every render, and reacts to [`Select::on_select`] and
//! [`Select::on_open_change`].
//!
//! The list is drawn with [`deferred`] rather than inline, for the same reason
//! [`MenuButton`](super::MenuButton) does it: a trigger that sits inside a
//! scrolling form would otherwise have its list clipped by that form.

use std::rc::Rc;

use gpui::{
    Anchor, AnchoredPositionMode, App, ElementId, MouseButton, Pixels, ScrollHandle, SharedString,
    Window, anchored, deferred, div, point, prelude::*, px, transparent_black,
};

use super::scrollbar::Scrollbar;
use super::theme::theme;

/// Height of the trigger, matching [`TextInput`](super::TextInput) so the two
/// line up when a form mixes them.
const TRIGGER_HEIGHT: f32 = 32.;

/// Vertical distance from the top of the trigger to the top of the list, so the
/// list clears the button it hangs from.
const DROP_OFFSET: f32 = TRIGGER_HEIGHT + 4.;

/// Width of the list when the caller sets no width of its own.
///
/// An `anchored` element is positioned absolutely and therefore cannot inherit
/// the trigger's width, so the list always needs a width in pixels.
const DEFAULT_WIDTH: f32 = 320.;

/// Height at which the list starts scrolling.
const LIST_MAX_HEIGHT: f32 = 260.;

/// Height of one option row.
const ROW_HEIGHT: f32 = 26.;

/// Distance the list keeps from the window edges when it would overflow.
const WINDOW_MARGIN: f32 = 6.;

/// Draw order of the click-catching backdrop, relative to other deferred draws.
const BACKDROP_PRIORITY: usize = 1;

/// Draw order of the list; above [`BACKDROP_PRIORITY`] so that the backdrop
/// never eats clicks meant for an option row.
const LIST_PRIORITY: usize = 2;

/// Glyph drawn at the right edge of the trigger.
const CHEVRON: &str = "\u{25be}";

/// Callback fired with the index and the text of the option the user picked.
type SelectHandler = Rc<dyn Fn(usize, &str, &mut Window, &mut App)>;

/// Callback fired when the list wants to open or close itself.
type OpenChangeHandler = Rc<dyn Fn(bool, &mut Window, &mut App)>;

/// A stateless one-of-many dropdown.
///
/// Options are plain strings: the text of an option is also its identity, which
/// keeps the widget usable for lists the caller discovers at runtime — font
/// families, for one — without inventing ids for them.
///
/// The control takes a single tab stop. `Enter` and `Space` toggle the list, as
/// they do for any focusable element in gpui, and while the list is open the
/// arrow keys move the selection without wrapping. Closing on `Escape` is left
/// to the parent, so that a dialog can decide whether the key belongs to the
/// dropdown or to itself.
///
/// ```ignore
/// Select::new("font")
///     .options(font_names)
///     .selected(self.font.clone())
///     .placeholder("System default")
///     .open(self.font_open)
///     .scroll_handle(self.font_scroll.clone())
///     .on_select(..)
///     .on_open_change(..)
/// ```
#[derive(IntoElement)]
pub struct Select {
    id: ElementId,
    options: Vec<SharedString>,
    selected: Option<SharedString>,
    placeholder: SharedString,
    open: bool,
    width: Option<Pixels>,
    tab_index: Option<isize>,
    scroll_handle: Option<ScrollHandle>,
    scrollbar: Option<Scrollbar>,
    on_select: Option<SelectHandler>,
    on_open_change: Option<OpenChangeHandler>,
}

impl Select {
    /// Creates an empty, closed dropdown with nothing selected.
    ///
    /// `id` must be unique among the siblings of the control.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            options: Vec::new(),
            selected: None,
            placeholder: SharedString::default(),
            open: false,
            width: None,
            tab_index: None,
            scroll_handle: None,
            scrollbar: None,
            on_select: None,
            on_open_change: None,
        }
    }

    /// Sets the options, in display order.
    pub fn options(mut self, options: impl IntoIterator<Item = SharedString>) -> Self {
        self.options = options.into_iter().collect();
        self
    }

    /// Sets the picked option. An option the list does not contain still shows
    /// on the trigger, it just highlights no row.
    pub fn selected(mut self, selected: Option<impl Into<SharedString>>) -> Self {
        self.selected = selected.map(Into::into);
        self
    }

    /// Sets the text shown muted on the trigger while nothing is selected.
    ///
    /// A list that offers an explicit "no choice" row should spell it the same
    /// way as the placeholder: that row is then highlighted while the selection
    /// is empty, so the open list always shows where the user stands.
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Sets whether the list is currently shown.
    pub fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    /// Sets the width of the trigger and the list.
    ///
    /// Without it the trigger fills its parent and the list falls back to a
    /// fixed width, because an absolutely positioned list cannot measure the
    /// trigger it hangs from.
    pub fn width(mut self, width: Pixels) -> Self {
        self.width = Some(width);
        self
    }

    /// Places the control at `index` in the window's tab order.
    pub fn tab_index(mut self, index: isize) -> Self {
        self.tab_index = Some(index);
        self
    }

    /// Attaches the scroll handle of the list.
    ///
    /// The handle belongs to the parent so that it can reveal the current
    /// option — with [`ScrollHandle::scroll_to_item`] — when it opens the list.
    /// Keyboard navigation scrolls through the same handle.
    pub fn scroll_handle(mut self, handle: ScrollHandle) -> Self {
        self.scroll_handle = Some(handle);
        self
    }

    /// Draws `bar` down the open list as its overlay scroll indicator.
    ///
    /// Passed in rather than built here, and only while it should be on screen,
    /// for the same reason the handle above is: a bar comes and goes with the
    /// scrolling, and this control keeps no state between renders. The owner
    /// answers drags of it too, since the id it built the bar with is what tells
    /// that drag from any other.
    pub fn scrollbar(mut self, bar: Scrollbar) -> Self {
        self.scrollbar = Some(bar);
        self
    }

    /// Sets the callback invoked with the option the user picked.
    ///
    /// Receives both the zero-based index of the option and its text. The index
    /// is what a caller should key off when the list has a fixed shape — a
    /// leading "no choice" row, say — because the text is translated and
    /// comparing against it would break in every language but one.
    ///
    /// Fired by a click on a row and by the arrow keys; the list closes itself
    /// after a click, so the callback does not have to.
    pub fn on_select(
        mut self,
        handler: impl Fn(usize, &str, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }

    /// Called with the open state the control would like to be in.
    ///
    /// Fires with `true` when the trigger is activated while closed, and with
    /// `false` when it is activated again, when a row is clicked, or when the
    /// pointer goes down anywhere outside the list.
    pub fn on_open_change(
        mut self,
        handler: impl Fn(bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_open_change = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Select {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = theme(cx);
        let viewport = window.viewport_size();
        let open = self.open;
        let id = self.id;
        let options = self.options;
        let placeholder = self.placeholder;
        let selected = self.selected;
        let on_select = self.on_select;
        let on_open_change = self.on_open_change;
        let scroll_handle = self.scroll_handle;
        let list_width = self.width.unwrap_or(px(DEFAULT_WIDTH));

        // With nothing selected the row that repeats the placeholder counts as
        // the current one, so a list whose first entry means "no choice" still
        // marks itself while the selection is empty.
        let current = options.iter().position(|option| match &selected {
            Some(selected) => option == selected,
            None => *option == placeholder,
        });
        let label = selected.clone().unwrap_or_else(|| placeholder.clone());

        let trigger = div()
            .id(ElementId::from((id.clone(), "trigger")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .w_full()
            .h(px(TRIGGER_HEIGHT))
            .px(px(8.))
            .rounded_md()
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .text_size(px(14.))
            .line_height(px(20.))
            .cursor_pointer()
            .hover(|style| style.bg(theme.surface_hover))
            .when_some(self.tab_index, |this, index| {
                let accent = theme.accent;
                this.tab_index(index)
                    .focus(move |style| style.border_color(accent))
            })
            .when_some(on_open_change.clone(), |this, handler| {
                this.on_click(move |_, window, cx| handler(!open, window, cx))
            })
            .on_key_down({
                let options = options.clone();
                let on_select = on_select.clone();
                let scroll_handle = scroll_handle.clone();
                move |event, window, cx| {
                    if !open || event.keystroke.modifiers.modified() || options.is_empty() {
                        return;
                    }
                    let delta: isize = match event.keystroke.key.as_str() {
                        "up" => -1,
                        "down" => 1,
                        _ => return,
                    };
                    let last = options.len() - 1;
                    let next = match current {
                        Some(current) => {
                            (current as isize + delta).clamp(0, last as isize) as usize
                        }
                        None if delta > 0 => 0,
                        None => last,
                    };
                    cx.stop_propagation();
                    if let Some(handle) = scroll_handle.as_ref() {
                        handle.scroll_to_item(next);
                    }
                    if Some(next) != current
                        && let Some(handler) = on_select.as_ref()
                    {
                        handler(next, &options[next], window, cx);
                    }
                }
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(if selected.is_some() {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .child(label),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(CHEVRON),
            );

        // A full-window sheet under the list: a pointer press anywhere it can
        // see closes the dropdown. It is deferred so that it covers the whole
        // window rather than just the row the trigger sits in.
        let backdrop = div()
            .id(ElementId::from((id.clone(), "backdrop")))
            .w(viewport.width)
            .h(viewport.height)
            .occlude()
            .when_some(on_open_change.clone(), |this, handler| {
                this.on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    handler(false, window, cx)
                })
            });

        let row_theme = theme.clone();
        let rows = options.into_iter().enumerate().map(move |(index, option)| {
            let theme = &row_theme;
            let is_current = Some(index) == current;
            let on_select = on_select.clone();
            let on_open_change = on_open_change.clone();
            let value = option.clone();

            div()
                .id(ElementId::from(("select-option", index)))
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .h(px(ROW_HEIGHT))
                .px(px(10.))
                .mx(px(4.))
                .rounded_sm()
                .text_size(px(13.))
                .text_color(if is_current { theme.accent } else { theme.text })
                .bg(if is_current {
                    theme.surface_active
                } else {
                    transparent_black()
                })
                .cursor_pointer()
                .hover(|style| style.bg(theme.surface_hover))
                .on_click(move |_, window, cx| {
                    if let Some(handler) = on_select.as_ref() {
                        handler(index, &value, window, cx);
                    }
                    if let Some(handler) = on_open_change.as_ref() {
                        handler(false, window, cx);
                    }
                })
                .child(div().flex_1().min_w_0().truncate().child(option))
        });

        let list = div()
            .id(ElementId::from((id.clone(), "list")))
            .occlude()
            .flex()
            .flex_col()
            .flex_none()
            .w(list_width)
            .max_h(px(LIST_MAX_HEIGHT))
            .py(px(4.))
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .bg(theme.background)
            .border_1()
            .border_color(theme.border)
            .rounded_lg()
            .shadow_lg()
            .text_color(theme.text)
            .when_some(scroll_handle.as_ref(), |this, handle| {
                this.track_scroll(handle)
            })
            .children(rows);

        // The bar cannot go inside the list, whose children are what scroll
        // away underneath it; this box is the list's own size, so it is what the
        // thumb is placed against.
        let list = div()
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .child(list)
            .children(self.scrollbar.and_then(|bar| bar.render(&theme)));

        // The list hangs off a zero-sized box laid out *before* the trigger,
        // not off the trigger itself: an `anchored` element is positioned
        // absolutely, and an absolutely positioned box is placed by its
        // parent's alignment, so giving it a box of its own is what pins it to
        // the trigger's top-left corner.
        let overlays = div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(0.))
            .h(px(0.))
            .child(
                deferred(
                    anchored()
                        .position(point(px(0.), px(0.)))
                        .position_mode(AnchoredPositionMode::Window)
                        .child(backdrop),
                )
                .with_priority(BACKDROP_PRIORITY),
            )
            .child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .offset(point(px(0.), px(DROP_OFFSET)))
                        .snap_to_window_with_margin(px(WINDOW_MARGIN))
                        .child(list),
                )
                .with_priority(LIST_PRIORITY),
            );

        div()
            .id(id)
            .flex()
            .flex_col()
            .w_full()
            .when_some(self.width, |this, width| this.w(width))
            .children(open.then_some(overlays))
            .child(trigger)
    }
}
