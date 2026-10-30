//! The small two-answer questions a dialog asks over itself.
//!
//! Deleting a saved connection and walking away from unsaved edits are both one
//! sentence and two buttons, asked by more than one dialog, and the answers
//! have to sit the same way round wherever they are asked: the one that loses
//! something on the left in red, the one that loses nothing last, where a hurried
//! hand lands. Each caller hands in what its two buttons do; how the question
//! looks is decided here once.

use std::rc::Rc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};
use rugpui::{Button, ButtonVariant, modal, theme};

/// What a button of a [`question`] does when it is pressed.
pub(crate) type OnAnswer = Rc<dyn Fn(&mut Window, &mut App)>;

/// What one button of a [`question`] says and does.
pub(crate) struct Answer {
    /// The button's label.
    pub label: SharedString,
    /// How loudly it is drawn.
    pub variant: ButtonVariant,
    /// What pressing it does.
    pub on_click: OnAnswer,
}

impl Answer {
    /// An answer drawn as `variant`, doing `on_click`.
    pub(crate) fn new(
        label: SharedString,
        variant: ButtonVariant,
        on_click: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label,
            variant,
            on_click: Rc::new(on_click),
        }
    }
}

/// A modal asking `lines`, one sentence per line, with `first` and `last` as
/// its buttons in that order.
///
/// A click on the backdrop gives `safe`'s answer, which is whichever of the two
/// loses nothing — the caller says which by passing it, so a stray click can
/// never be the destructive one.
pub(crate) fn question(
    id: &'static str,
    title: SharedString,
    lines: Vec<SharedString>,
    first: Answer,
    last: Answer,
    safe: OnAnswer,
    cx: &App,
) -> AnyElement {
    let theme = theme(cx);
    let button = |index: usize, answer: Answer| {
        let on_click = answer.on_click;
        Button::new((id, index), answer.label)
            .variant(answer.variant)
            .on_click(move |_, window, cx| on_click(window, cx))
    };
    let body = div()
        .flex()
        .flex_col()
        .gap(px(16.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .text_size(px(13.))
                .text_color(theme.text)
                .children(lines.into_iter().map(|line| div().child(line))),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(8.))
                .child(button(0, first))
                .child(button(1, last)),
        );
    div()
        .absolute()
        .inset_0()
        .child(modal(id, title, px(420.), body, move |window, cx| {
            safe(window, cx)
        }))
        .into_any_element()
}
