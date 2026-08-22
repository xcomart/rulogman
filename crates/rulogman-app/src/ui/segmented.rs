//! A horizontal segmented control, used instead of a group of radio buttons.

use std::rc::Rc;

use gpui::{App, ElementId, SharedString, Window, div, prelude::*, px, transparent_black};

use super::theme::theme;

/// Callback receiving the index of the segment the user picked.
type SelectHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// A stateless one-of-many selector.
///
/// The control owns no state: the parent view passes the options and the
/// selected index on every render and reacts to [`Segmented::on_select`].
///
/// Every option carries a short machine-readable value alongside its label. The
/// value is only used to build stable element ids, which keeps hover state from
/// following the wrong segment when the option list changes.
///
/// ```ignore
/// Segmented::new("auth")
///     .options(vec![("password", "Password"), ("key", "Private key")])
///     .selected(self.auth.index())
///     .on_select(cx.listener(|this, index, _window, cx| this.set_auth(*index, cx)))
/// ```
#[derive(IntoElement)]
pub struct Segmented {
    id: ElementId,
    options: Vec<(SharedString, SharedString)>,
    selected: usize,
    tab_index: Option<isize>,
    on_select: Option<SelectHandler>,
}

impl Segmented {
    /// Creates an empty segmented control with the first segment selected.
    ///
    /// `id` must be unique among the siblings of the control.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            options: Vec::new(),
            selected: 0,
            tab_index: None,
            on_select: None,
        }
    }

    /// Sets the segments, in display order, as `(value, label)` pairs.
    pub fn options<V, L>(mut self, options: impl IntoIterator<Item = (V, L)>) -> Self
    where
        V: Into<SharedString>,
        L: Into<SharedString>,
    {
        self.options = options
            .into_iter()
            .map(|(value, label)| (value.into(), label.into()))
            .collect();
        self
    }

    /// Sets the index of the highlighted segment.
    ///
    /// An out-of-range index simply highlights nothing.
    pub fn selected(mut self, selected: usize) -> Self {
        self.selected = selected;
        self
    }

    /// Places the whole control at `index` in the window's tab order.
    ///
    /// The group takes a single tab stop rather than one per segment, so `Tab`
    /// steps past the control instead of through it. While focused, `Left` and
    /// `Right` (and `Up`/`Down`) move the selection, wrapping at either end —
    /// the behaviour WAI-ARIA prescribes for a radio group.
    pub fn tab_index(mut self, index: isize) -> Self {
        self.tab_index = Some(index);
        self
    }

    /// Sets the callback invoked with the index of the clicked segment.
    ///
    /// Never fired for the segment that is already selected.
    pub fn on_select(mut self, handler: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Segmented {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = theme(cx);
        let selected = self.selected;
        let count = self.options.len();
        let on_select = self.on_select;
        let container_id = self.id;
        let outer_id = container_id.clone();
        let tab_index = self.tab_index;
        let arrow_handler = on_select.clone();

        let segments = self
            .options
            .into_iter()
            .enumerate()
            .map(move |(index, (value, label))| {
                let is_selected = index == selected;
                let handler = on_select.clone().filter(|_| !is_selected);

                div()
                    .id(ElementId::from((container_id.clone(), value)))
                    .flex()
                    .flex_row()
                    .flex_grow_1()
                    .items_center()
                    .justify_center()
                    .h(px(24.))
                    .px(px(10.))
                    .rounded_sm()
                    .whitespace_nowrap()
                    .text_size(px(12.))
                    .bg(if is_selected {
                        theme.surface_active
                    } else {
                        transparent_black()
                    })
                    .text_color(if is_selected {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .when(!is_selected, |this| {
                        this.cursor_pointer()
                            .hover(|style| style.bg(theme.surface_hover))
                    })
                    .when_some(handler, |this, handler| {
                        this.on_click(move |_, window, cx| handler(index, window, cx))
                    })
                    .child(label)
            });

        div()
            .id(outer_id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.))
            .w_full()
            .p(px(2.))
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .when_some(tab_index.filter(|_| count > 0), |this, index| {
                let accent = theme.accent;
                this.tab_index(index)
                    .focus(move |style| style.border_color(accent))
                    .on_key_down(move |event, window, cx| {
                        if event.keystroke.modifiers.modified() {
                            return;
                        }
                        let delta = match event.keystroke.key.as_str() {
                            "left" | "up" => -1isize,
                            "right" | "down" => 1,
                            _ => return,
                        };
                        let Some(handler) = arrow_handler.as_ref() else {
                            return;
                        };
                        let next = (selected as isize + delta).rem_euclid(count as isize) as usize;
                        if next != selected {
                            cx.stop_propagation();
                            handler(next, window, cx);
                        }
                    })
            })
            .children(segments)
    }
}
