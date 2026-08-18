//! A dropdown that picks one color scheme out of a list.
//!
//! The same control as [`Select`](super::Select) — trigger, deferred list,
//! backdrop, arrow keys — over entries that carry colors as well as a name. A
//! grid of preview cards shows more of each scheme at once, but it costs a form
//! several rows of height per catalogue; a dialog that has two catalogues, and a
//! dozen other settings besides, is better served by one line each with the
//! colors carried along on the right of every row.
//!
//! The widget knows nothing about terminals: callers hand it plain colors, so
//! the same dropdown can preview anything that has a background, a foreground
//! and a handful of accents.
//!
//! Entries are identified by id rather than by their label, unlike the plain
//! string dropdown: a scheme's name is what the user reads and its id is what
//! `settings.json` stores, and the two are not the same string.

use std::rc::Rc;

use gpui::{
    AnchoredPositionMode, App, Corner, Div, ElementId, Hsla, MouseButton, Pixels, ScrollHandle,
    SharedString, Window, anchored, deferred, div, point, prelude::*, px, transparent_black,
};

use super::scrollbar::{Scrollbar, WheelStaysOnAxis};
use super::theme::{Theme, theme};

/// Height of the trigger, matching [`Select`](super::Select) so a form that
/// mixes the two lines up.
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

/// Height of one option row. Taller than the plain dropdown's, because a row
/// carries a color preview and not only a line of text.
const ROW_HEIGHT: f32 = 30.;

/// Height of the color preview pill drawn at the right of a row.
const PREVIEW_HEIGHT: f32 = 20.;

/// Diameter of one accent chip inside that pill.
const CHIP_SIZE: f32 = 8.;

/// Distance the list keeps from the window edges when it would overflow.
const WINDOW_MARGIN: f32 = 6.;

/// Draw order of the click-catching backdrop, relative to other deferred draws.
const BACKDROP_PRIORITY: usize = 1;

/// Draw order of the list; above [`BACKDROP_PRIORITY`] so that the backdrop
/// never eats clicks meant for an option row.
const LIST_PRIORITY: usize = 2;

/// Glyph drawn at the right edge of the trigger.
const CHEVRON: &str = "\u{25be}";

/// Text drawn on the pill of an entry that has no colors of its own.
///
/// English, because the widget layer has no locale of its own; every caller in
/// the application overrides it with [`SchemeSwatch::placeholder_label`].
const DEFAULT_PLACEHOLDER_LABEL: &str = "inherits";

/// Callback fired with the id of the newly picked scheme.
type SelectHandler = Rc<dyn Fn(&str, &mut Window, &mut App)>;

/// Callback fired when the list wants to open or close itself.
type OpenChangeHandler = Rc<dyn Fn(bool, &mut Window, &mut App)>;

/// The colors drawn inside one entry's preview pill.
#[derive(Debug, Clone)]
pub struct SchemePreview {
    /// Background the pill is filled with.
    pub background: Hsla,
    /// Color of the sample text drawn on the background.
    pub foreground: Hsla,
    /// Accent chips drawn next to the sample text, in display order.
    pub ansi: Vec<Hsla>,
}

/// One entry of a [`SchemeSelect`].
///
/// The fields are closed to the application, which builds a swatch through the
/// builders below and never takes one apart.
#[derive(Debug, Clone)]
pub struct SchemeSwatch {
    /// Stable id reported to [`SchemeSelect::on_select`].
    id: SharedString,
    /// Label shown at the left of the row.
    name: SharedString,
    /// Colors to preview. `None` renders a muted placeholder pill instead,
    /// which is how a per-session picker offers "use the global scheme".
    preview: Option<SchemePreview>,
    /// Text drawn on that placeholder pill. Taken from the caller so the widget
    /// needs no translations of its own.
    placeholder_label: SharedString,
}

impl SchemeSwatch {
    /// Creates an entry with no preview, drawn as a muted placeholder pill.
    pub fn new(id: impl Into<SharedString>, name: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            preview: None,
            placeholder_label: SharedString::new_static(DEFAULT_PLACEHOLDER_LABEL),
        }
    }

    /// Attaches the colors to draw in this entry's preview pill.
    pub fn preview(mut self, preview: SchemePreview) -> Self {
        self.preview = Some(preview);
        self
    }

    /// Sets the text of the placeholder pill shown when there is no preview.
    ///
    /// Callers pass a translated string; the built-in default is English.
    pub fn placeholder_label(mut self, label: impl Into<SharedString>) -> Self {
        self.placeholder_label = label.into();
        self
    }
}

/// The color pill that stands in for one entry.
///
/// The scheme's own background, its foreground on a sample of text, and the
/// accents as chips. An entry with no colors of its own gets an outlined pill
/// carrying its placeholder label, so that "inherit the global scheme" reads as
/// an absence of color rather than as a scheme that happens to be transparent.
fn preview_pill(preview: Option<&SchemePreview>, placeholder: SharedString, theme: &Theme) -> Div {
    match preview {
        Some(preview) => div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(3.))
            .h(px(PREVIEW_HEIGHT))
            .px(px(6.))
            .rounded_sm()
            .bg(preview.background)
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.))
                    .text_color(preview.foreground)
                    .child("Aa"),
            )
            .children(preview.ansi.iter().map(|color| {
                div()
                    .flex_none()
                    .size(px(CHIP_SIZE))
                    .rounded_full()
                    .bg(*color)
            })),
        None => div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h(px(PREVIEW_HEIGHT))
            .px(px(6.))
            .rounded_sm()
            .border_1()
            .border_color(theme.border)
            .text_size(px(10.))
            .text_color(theme.text_muted)
            .child(placeholder),
    }
}

/// A stateless one-of-many dropdown over color schemes.
///
/// The control owns no state: the parent view passes the entries, the selected
/// id, the open flag and the list's [`ScrollHandle`] on every render, and reacts
/// to [`SchemeSelect::on_select`] and [`SchemeSelect::on_open_change`].
///
/// The control takes a single tab stop. `Enter` and `Space` toggle the list, as
/// they do for any focusable element in gpui, and while the list is open the
/// arrow keys move the selection without wrapping. Closing on `Escape` is left
/// to the parent, so that a dialog can decide whether the key belongs to the
/// dropdown or to itself.
///
/// ```ignore
/// SchemeSelect::new("scheme")
///     .options(scheme_swatches())
///     .selected(Some(self.scheme.clone()))
///     .open(self.scheme_open)
///     .scroll_handle(self.scheme_scroll.clone())
///     .on_select(..)
///     .on_open_change(..)
/// ```
#[derive(IntoElement)]
pub struct SchemeSelect {
    id: ElementId,
    options: Vec<SchemeSwatch>,
    selected: Option<SharedString>,
    open: bool,
    width: Option<Pixels>,
    tab_index: Option<isize>,
    scroll_handle: Option<ScrollHandle>,
    scrollbar: Option<Scrollbar>,
    on_select: Option<SelectHandler>,
    on_open_change: Option<OpenChangeHandler>,
}

impl SchemeSelect {
    /// Creates an empty, closed dropdown with nothing selected.
    ///
    /// `id` must be unique among the siblings of the control.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            options: Vec::new(),
            selected: None,
            open: false,
            width: None,
            tab_index: None,
            scroll_handle: None,
            scrollbar: None,
            on_select: None,
            on_open_change: None,
        }
    }

    /// Sets the entries, in display order.
    pub fn options(mut self, options: impl IntoIterator<Item = SchemeSwatch>) -> Self {
        self.options = options.into_iter().collect();
        self
    }

    /// Sets the id of the picked entry.
    ///
    /// An id no entry answers to still shows on the trigger — spelled as the id
    /// itself, since there is no name to show — and highlights no row: a
    /// hand-edited `settings.json` naming a scheme that has since gone should
    /// say so rather than look like nothing was ever chosen.
    pub fn selected(mut self, selected: Option<impl Into<SharedString>>) -> Self {
        self.selected = selected.map(Into::into);
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
    /// entry — with [`ScrollHandle::scroll_to_item`] — when it opens the list.
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

    /// Sets the callback invoked with the id of the picked entry.
    ///
    /// Never fired for the entry that is already selected — clicking it only
    /// puts the list away — which spares the parent an update that changes
    /// nothing.
    pub fn on_select(mut self, handler: impl Fn(&str, &mut Window, &mut App) + 'static) -> Self {
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

impl RenderOnce for SchemeSelect {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = theme(cx);
        let viewport = window.viewport_size();
        let open = self.open;
        let id = self.id;
        let options = self.options;
        let selected = self.selected;
        let on_select = self.on_select;
        let on_open_change = self.on_open_change;
        let scroll_handle = self.scroll_handle;
        let list_width = self.width.unwrap_or(px(DEFAULT_WIDTH));

        let ids: Rc<[SharedString]> = options.iter().map(|entry| entry.id.clone()).collect();
        let current = selected
            .as_ref()
            .and_then(|id| ids.iter().position(|candidate| candidate == id));

        // With no entry answering to the selected id there is no name to show,
        // so the id stands in for it and the pill is left off entirely.
        let (label, pill) = match current.map(|index| &options[index]) {
            Some(entry) => (
                entry.name.clone(),
                Some(preview_pill(
                    entry.preview.as_ref(),
                    entry.placeholder_label.clone(),
                    &theme,
                )),
            ),
            None => (selected.clone().unwrap_or_default(), None),
        };

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
                let ids = ids.clone();
                let on_select = on_select.clone();
                let scroll_handle = scroll_handle.clone();
                move |event, window, cx| {
                    if !open || event.keystroke.modifiers.modified() || ids.is_empty() {
                        return;
                    }
                    let delta: isize = match event.keystroke.key.as_str() {
                        "up" => -1,
                        "down" => 1,
                        _ => return,
                    };
                    let last = ids.len() - 1;
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
                        handler(&ids[next], window, cx);
                    }
                }
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.text)
                    .child(label),
            )
            .children(pill)
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
        let rows = options.into_iter().enumerate().map(move |(index, entry)| {
            let theme = &row_theme;
            let is_current = Some(index) == current;
            let on_select = on_select.clone().filter(|_| !is_current);
            let on_open_change = on_open_change.clone();
            let value = entry.id.clone();

            div()
                .id(ElementId::from(("scheme-option", index)))
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .gap(px(8.))
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
                        handler(&value, window, cx);
                    }
                    if let Some(handler) = on_open_change.as_ref() {
                        handler(false, window, cx);
                    }
                })
                .child(div().flex_1().min_w_0().truncate().child(entry.name))
                .child(preview_pill(
                    entry.preview.as_ref(),
                    entry.placeholder_label,
                    theme,
                ))
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
            .wheel_stays_on_axis()
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
                        .anchor(Corner::TopLeft)
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
