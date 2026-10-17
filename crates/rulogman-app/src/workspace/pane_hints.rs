//! Pane numbers.
//!
//! Holding <kbd>Ctrl</kbd> over a split tab draws a number in the top-left
//! corner of every pane, counted in layout order — the order the focus cycle
//! walks. With <kbd>Ctrl</kbd> still down, a digit focuses that pane and
//! <kbd>Tab</kbd> (<kbd>Shift</kbd>+<kbd>Tab</kbd>) the next (previous) one.
//!
//! `Ctrl` on every platform, because `Tab` is the one key the gesture cannot do
//! without and `Ctrl` is the only modifier the desktop leaves it to: `Alt+Tab`
//! and `Cmd+Tab` switch applications before the window ever sees them. Off
//! macOS that puts the digits on the tab chord, so a digit picks a pane only
//! while the active tab is split, and the tab it always picked otherwise — see
//! [`Workspace::focus_pane_action`].

use std::time::Duration;

use gpui::{AnyWindowHandle, Keystroke, Modifiers, ModifiersChangedEvent, Task};

use super::*;

/// How long `Ctrl` has to be held on its own before the numbers appear.
///
/// Long enough that the `Ctrl` of a `Ctrl+C` typed into a split tab has met its
/// letter before anything is drawn, so a chord the shell is meant to have does
/// not flash the numbers on its way past.
const PANE_HINT_DELAY: Duration = Duration::from_millis(250);

/// Whether the pane numbers are up, and what decides when they go up.
pub(super) struct PaneHints {
    /// Whether the numbers are drawn.
    pub(super) shown: bool,
    /// The delay running before they are, while `Ctrl` is held on its own.
    pending: Option<Task<()>>,
    /// Set once something other than a pane shortcut was pressed while `Ctrl`
    /// was down: that `Ctrl` belonged to another chord, so the numbers stay
    /// down until it is let go.
    suppressed: bool,
    /// Sees every keystroke first, to notice the chords that are not pane
    /// ones.
    _keystrokes: Subscription,
}

/// Whether `modifiers` are the ones the pane numbers answer to: `Ctrl`, with
/// `Shift` allowed for the backwards step and nothing else.
fn holds_pane_chord(modifiers: &Modifiers) -> bool {
    modifiers.control && !modifiers.alt && !modifiers.platform && !modifiers.function
}

/// Whether `keystroke` is one of the pane shortcuts the numbers belong to:
/// `Ctrl` with `Tab`, or with a digit that numbers a pane.
///
/// Read off the key rather than off the action it resolved to, because the
/// keystroke is looked at before any binding is: a chord the terminal takes
/// for itself — `Ctrl+C` — stops there, and nothing after the terminal ever
/// hears of it.
fn is_pane_shortcut(keystroke: &Keystroke) -> bool {
    holds_pane_chord(&keystroke.modifiers)
        && (keystroke.key == "tab"
            || keystroke
                .key
                .parse::<usize>()
                .is_ok_and(|digit| (1..=QUICK_FOCUS_PANES).contains(&digit)))
}

impl PaneHints {
    /// Starts hidden, listening for keystrokes in `window` only.
    pub(super) fn new(window: &Window, cx: &mut Context<Workspace>) -> Self {
        // Keystroke observers are application-wide; a chord typed in another
        // window says nothing about the `Ctrl` held over this one.
        let own: AnyWindowHandle = window.window_handle();
        let workspace = cx.weak_entity();
        let keystrokes = cx.intercept_keystrokes(move |event, window, cx| {
            let keystroke = &event.keystroke;
            if window.window_handle() != own
                || !keystroke.modifiers.control
                || is_pane_shortcut(keystroke)
            {
                return;
            }
            workspace
                .update(cx, |workspace, cx| workspace.suppress_pane_hints(cx))
                .ok();
        });
        Self {
            shown: false,
            pending: None,
            suppressed: false,
            _keystrokes: keystrokes,
        }
    }
}

impl Workspace {
    /// Whether the active tab has more than one pane to number.
    fn active_tab_split(&self) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| tab.panes.leaf_count() > 1)
    }

    /// The panes of the active tab in the order they are numbered, while the
    /// numbers are to be drawn; `None` otherwise.
    ///
    /// The modifiers are asked again here rather than trusted to the last
    /// change: a window that lost the keyboard with `Ctrl` down never hears it
    /// come up.
    pub(super) fn pane_hint_order(&self, window: &Window, cx: &App) -> Option<Vec<PaneId>> {
        if !self.pane_hints.shown
            || !holds_pane_chord(&window.modifiers())
            || !window.is_window_active()
            || self.dialog_open(cx)
        {
            return None;
        }
        let tab = self.tabs.get(self.active)?;
        (tab.panes.leaf_count() > 1).then(|| tab.panes.leaf_ids())
    }

    /// Follows `Ctrl` going down and coming up.
    pub(super) fn on_pane_hint_modifiers(
        &mut self,
        event: &ModifiersChangedEvent,
        cx: &mut Context<Self>,
    ) {
        let modifiers = event.modifiers;
        if !modifiers.control {
            self.reset_pane_hints(cx);
            return;
        }
        if !holds_pane_chord(&modifiers) {
            self.suppress_pane_hints(cx);
            return;
        }
        let hints = &self.pane_hints;
        if hints.shown || hints.pending.is_some() || hints.suppressed || !self.active_tab_split() {
            return;
        }
        self.pane_hints.pending = Some(cx.spawn(async move |workspace, cx| {
            cx.background_executor().timer(PANE_HINT_DELAY).await;
            workspace
                .update(cx, |workspace, cx| {
                    workspace.pane_hints.pending = None;
                    workspace.pane_hints.shown = true;
                    cx.notify();
                })
                .ok();
        }));
    }

    /// Puts the numbers up at once, after a pane shortcut was pressed with
    /// `Ctrl`, so the next digit can be read off the screen without waiting
    /// out the delay.
    fn show_pane_hints(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !holds_pane_chord(&window.modifiers()) || self.pane_hints.suppressed {
            return;
        }
        self.pane_hints.pending = None;
        if !self.pane_hints.shown {
            self.pane_hints.shown = true;
            cx.notify();
        }
    }

    /// Takes the numbers down until `Ctrl` is next let go.
    fn suppress_pane_hints(&mut self, cx: &mut Context<Self>) {
        self.pane_hints.suppressed = true;
        self.hide_pane_hints(cx);
    }

    /// Takes the numbers down and forgets everything about the `Ctrl` that
    /// was held.
    fn reset_pane_hints(&mut self, cx: &mut Context<Self>) {
        self.pane_hints.suppressed = false;
        self.hide_pane_hints(cx);
    }

    fn hide_pane_hints(&mut self, cx: &mut Context<Self>) {
        self.pane_hints.pending = None;
        if self.pane_hints.shown {
            self.pane_hints.shown = false;
            cx.notify();
        }
    }

    /// Focuses the pane at `index` in layout order; `false` when the active
    /// tab has no such pane.
    pub(super) fn focus_pane_at(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return false;
        };
        let Some(pane) = tab.panes.leaf_ids().get(index).copied() else {
            return false;
        };
        tab.focus(pane);
        self.focus_active(window, cx);
        cx.notify();
        true
    }

    /// Handles <kbd>Ctrl</kbd> + a digit.
    ///
    /// On an unsplit tab there is no pane to pick, and the key is handed on:
    /// off macOS to [`SelectTab`], bound to the same chord, so the tab
    /// shortcut is exactly what it was wherever there are no panes to number;
    /// on macOS, where the tab chord is `Cmd`, on to the terminal as before.
    /// A digit past the last pane of a split tab does nothing — passing it on
    /// would switch tabs from under a user counting panes.
    pub(super) fn focus_pane_action(
        &mut self,
        action: &FocusPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.active_tab_split() {
            cx.propagate();
            return;
        }
        self.focus_pane_at(action.0, window, cx);
        self.show_pane_hints(window, cx);
    }

    /// Handles the pane focus shortcut for the next pane.
    pub(super) fn focus_next_pane_action(
        &mut self,
        _: &FocusNextPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_next_pane(window, cx);
        self.show_pane_hints(window, cx);
    }

    /// Handles the pane focus shortcut for the previous pane.
    pub(super) fn focus_prev_pane_action(
        &mut self,
        _: &FocusPrevPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_prev_pane(window, cx);
        self.show_pane_hints(window, cx);
    }
}

/// The number drawn in the top-left corner of a pane while `Ctrl` is held.
///
/// The active pane's badge is filled with the accent, as its frame is, so the
/// number to press and the pane the keyboard is in can be told apart at once.
pub(super) fn pane_hint_badge(number: usize, active: bool, theme: &Theme) -> AnyElement {
    let (fill, text) = if active {
        (theme.accent, theme.background)
    } else {
        (theme.surface, theme.text)
    };
    div()
        .absolute()
        .top(px(8.))
        .left(px(8.))
        .min_w(px(28.))
        .h(px(28.))
        .px(px(8.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .border_1()
        .border_color(theme.accent)
        .bg(fill)
        .text_color(text)
        .text_size(px(16.))
        .font_weight(gpui::FontWeight::BOLD)
        .shadow_md()
        .child(number.to_string())
        .into_any_element()
}
