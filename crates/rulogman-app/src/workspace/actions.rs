//! Actions.

use super::*;

impl Workspace {
    /// Handles <kbd>Ctrl</kbd>/<kbd>Cmd</kbd> + <kbd>T</kbd>.
    pub(super) fn new_session_action(
        &mut self,
        _: &NewSession,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_dialog(cx);
    }

    /// Handles <kbd>Ctrl</kbd>/<kbd>Cmd</kbd> + <kbd>W</kbd>.
    ///
    /// Closes the active pane rather than the whole tab, the way a split editor
    /// or terminal does: on an unsplit tab the two are the same thing, and on a
    /// split one closing every pane in turn ends up closing the tab.
    pub(super) fn close_session_action(
        &mut self,
        _: &CloseSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_active_pane(window, cx);
    }

    /// Handles the pane focus shortcut for the next pane.
    pub(super) fn focus_next_pane_action(
        &mut self,
        _: &FocusNextPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_next_pane(window, cx);
    }

    /// Handles the pane focus shortcut for the previous pane.
    pub(super) fn focus_prev_pane_action(
        &mut self,
        _: &FocusPrevPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_prev_pane(window, cx);
    }

    /// Handles the shortcut that pulls the active pane out into its own tab.
    pub(super) fn break_out_pane_action(
        &mut self,
        _: &BreakOutPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.break_out_active_pane(window, cx);
    }

    /// Handles the shortcut that sends the active tab off into a window of its
    /// own.
    ///
    /// The active tab, where the tab context menu's row acts on the tab that was
    /// right-clicked; both end in the same call.
    pub(super) fn move_tab_to_new_window_action(
        &mut self,
        _: &MoveTabToNewWindow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_tab_to_new_window(self.active, window, cx);
    }

    /// Handles the shortcut that splits the active pane to the right.
    pub(super) fn duplicate_split_right_action(
        &mut self,
        _: &DuplicateSplitRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.duplicate_split(Axis::Horizontal, window, cx);
    }

    /// Handles the shortcut that splits the active pane downwards.
    pub(super) fn duplicate_split_below_action(
        &mut self,
        _: &DuplicateSplitBelow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.duplicate_split(Axis::Vertical, window, cx);
    }

    /// Handles the command that squares the columns of the active tab up.
    pub(super) fn equalize_widths_action(
        &mut self,
        _: &EqualizeWidths,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.equalize_panes(Axis::Horizontal, cx);
    }

    /// Handles the command that squares the rows of the active tab up.
    pub(super) fn equalize_heights_action(
        &mut self,
        _: &EqualizeHeights,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.equalize_panes(Axis::Vertical, cx);
    }

    /// Handles the command that saves the active tab's arrangement to its
    /// dashboard.
    ///
    /// The active tab, where the tab context menu's row acts on the tab that
    /// was right-clicked; both end in the same call, which is a no-op on a tab
    /// that is not a dashboard.
    pub(super) fn save_dashboard_layout_action(
        &mut self,
        _: &SaveDashboardLayout,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_tab_layout(self.active, cx);
    }

    /// Handles the shortcut that shows and hides the remote file panel.
    pub(super) fn toggle_file_panel_action(
        &mut self,
        _: &ToggleFilePanel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_file_panel(cx);
    }

    /// Handles <kbd>Ctrl</kbd>/<kbd>Cmd</kbd> + <kbd>,</kbd>.
    pub(super) fn open_settings_action(
        &mut self,
        _: &OpenSettings,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings(cx);
    }

    /// Handles the "About rulogman" menu item.
    pub(super) fn show_about_action(
        &mut self,
        _: &ShowAbout,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_about(cx);
    }

    /// Handles the "Check for updates" menu item.
    pub(super) fn check_updates_action(
        &mut self,
        _: &CheckUpdates,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.check_updates(window, cx);
    }

    /// Handles <kbd>Ctrl</kbd>/<kbd>Cmd</kbd> + a digit.
    pub(super) fn select_tab_action(
        &mut self,
        action: &SelectTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_tab(action.0, window, cx);
    }

    /// Handles <kbd>Ctrl</kbd>/<kbd>Cmd</kbd> + <kbd>Alt</kbd> + a digit.
    ///
    /// A digit past the end of the store does nothing at all — no log, no
    /// beep. Nine chords are bound whatever the user has saved, so most of them
    /// name nothing on most installations, and a shortcut that names nothing is
    /// not a mistake to report: it is a key that is simply not in use yet.
    pub(super) fn open_dashboard_action(
        &mut self,
        action: &OpenDashboard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self
            .dashboards
            .dashboards()
            .get(action.0)
            .map(|dashboard| dashboard.id)
        else {
            return;
        };
        self.open_dashboard(id, window, cx);
    }

    /// Handles <kbd>Esc</kbd>: closes whichever overlay is open, or lets the key
    /// through to the terminal when none is.
    pub(super) fn dismiss_dialog_action(
        &mut self,
        _: &DismissDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The dropdown menus paint above everything else, so they are dismissed
        // first. The file panel's menu is one of them even though the panel
        // owns it: it is drawn over the window like the rest, and the key
        // reaches this handler rather than the panel, which binds nothing.
        if self.tab_context.is_some() {
            self.close_tab_context(cx);
            return;
        }
        if self.empty_context.is_some() {
            self.close_empty_context(cx);
            return;
        }
        if self.panel.update(cx, |panel, cx| panel.close_context(cx)) {
            return;
        }
        if self.menu_open {
            self.set_menu_open(false, cx);
            return;
        }
        if self.tab_menu_open {
            self.set_tab_menu_open(false, cx);
            return;
        }
        // Ahead of the dialogs, because none of them can be open at the same
        // time as this one: `dialog_open` counts the question, so nothing else
        // opens over it. Escape is the cancelling answer — the pane stays.
        if self.close_confirm.is_some() {
            self.cancel_close_editor(cx);
            self.focus_active(window, cx);
            return;
        }
        // Beside it, and for the same reasons: nothing can be open over this
        // one either, and `Escape` is the answer that leaves the pane as it
        // stands — locked, or unsaved.
        if self.sudo_prompt.is_some() {
            self.cancel_sudo_password(window, cx);
            return;
        }
        if self.about.read(cx).is_open() {
            self.about.update(cx, |dialog, cx| dialog.close(cx));
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        if self.update.read(cx).is_open() {
            // Swallowed rather than propagated while an install runs: the key
            // must not reach the terminal, but nothing may take the screen from
            // a swap either, so `Escape` simply does nothing until it is over.
            if !self.update.read(cx).is_busy() {
                self.update.update(cx, |dialog, cx| dialog.close(cx));
                self.focus_active(window, cx);
                cx.notify();
            }
            return;
        }
        if self.dialog.read(cx).is_open() {
            // Route through `dismiss` rather than closing directly: the dialog
            // also binds `Escape` internally, and going through one path keeps
            // `Dismissed` firing exactly once no matter which handler wins the
            // dispatch. Closing and restoring focus is then the subscription's
            // job.
            self.dialog.update(cx, |dialog, cx| dialog.dismiss(cx));
            cx.notify();
            return;
        }
        if self.settings.read(cx).is_open() {
            self.settings.update(cx, |dialog, cx| dialog.close(cx));
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        cx.propagate();
    }
}
