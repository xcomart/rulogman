//! The small two-answer questions a dialog asks over itself.
//!
//! Deleting a saved connection and walking away from unsaved edits are both one
//! sentence and two buttons, asked by more than one dialog, and the answers
//! have to sit the same way round wherever they are asked: the one that loses
//! something on the left in red, the one that loses nothing last, where a hurried
//! hand lands. Each caller hands in what its two buttons do; how the question
//! looks is decided here once.

use std::rc::Rc;

use gpui::{AnyElement, App, FocusHandle, SharedString, Window, div, prelude::*, px};
use rugpui::{Button, ButtonVariant, TextSize, modal, theme};

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
/// `safe` is the answer that loses nothing, and the question gives it every
/// way out that is not a deliberate press: its button — `first` or `last`,
/// whichever the caller says by `safe_is_last` — takes the keyboard when the
/// question opens, so `Enter` answers it too, and `Escape` and a click on the
/// backdrop both give it. `focus` is the caller's handle for that button,
/// kept across frames so the modal can hand the keyboard back when it goes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn question(
    id: &'static str,
    title: SharedString,
    lines: Vec<SharedString>,
    first: Answer,
    last: Answer,
    safe_is_last: bool,
    focus: &FocusHandle,
    cx: &App,
) -> AnyElement {
    let theme = theme(cx);
    let safe = if safe_is_last {
        last.on_click.clone()
    } else {
        first.on_click.clone()
    };
    let button = |index: usize, answer: Answer, focused: bool| {
        let on_click = answer.on_click;
        let button = Button::new((id, index), answer.label)
            .variant(answer.variant)
            .on_click(move |_, window, cx| on_click(window, cx));
        if focused {
            button.focus_handle(focus)
        } else {
            button
        }
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
                .text_size(TextSize::Body.size(cx))
                .text_color(theme.text)
                .children(lines.into_iter().map(|line| div().child(line))),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(8.))
                .child(button(0, first, !safe_is_last))
                .child(button(1, last, safe_is_last)),
        );
    div()
        .absolute()
        .inset_0()
        .child(
            modal(id, title, px(420.), body, move |window, cx| {
                safe(window, cx)
            })
            .initial_focus(focus)
            .escape_dismisses(true),
        )
        .into_any_element()
}
