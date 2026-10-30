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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_file_panel(window, cx);
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

    /// Whether the keyboard is in a terminal pane — a shell or a followed file
    /// — of the active tab, where `F10`, `Shift+F10` and the Menu key belong to
    /// the remote program (mc, htop and a dozen other TUIs answer them).
    fn terminal_has_keyboard(&self, window: &Window, cx: &App) -> bool {
        self.tabs.get(self.active).is_some_and(|tab| {
            matches!(tab.active_view(), PaneView::Terminal(_) | PaneView::Tail(_))
                && tab
                    .active_view()
                    .focus_handle(cx)
                    .contains_focused(window, cx)
        })
    }

    /// `F10`: opens the application menu on its first row — except in a
    /// terminal pane, where the key goes on to the remote program, and on
    /// macOS, whose menu bar is the system's.
    pub(super) fn open_app_menu_action(
        &mut self,
        _: &rugpui::OpenAppMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if cfg!(target_os = "macos")
            || self.terminal_has_keyboard(window, cx)
            || self.dialog_open(cx)
        {
            cx.propagate();
            return;
        }
        self.close_overlays(cx);
        self.menu_by_keyboard = true;
        self.set_menu_open(true, cx);
    }

    /// `Shift+F10` and the Menu key where no surface below answered them: the
    /// active tab's menu, opened under the strip on its first row. A terminal
    /// pane hands the key on to the remote program instead, and the files panel
    /// and an open file answer it themselves with their own menus.
    pub(super) fn open_context_menu_action(
        &mut self,
        _: &rugpui::OpenContextMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal_has_keyboard(window, cx) || self.tabs.is_empty() {
            cx.propagate();
            return;
        }
        let at = point(px(TRAFFIC_LIGHT_GAP), px(TOOLBAR_HEIGHT));
        self.open_tab_context(self.active, at, cx);
        self.tab_context_by_keyboard = true;
    }

    /// Handles *Keyboard shortcuts*, from either menu.
    pub(super) fn show_shortcuts_action(
        &mut self,
        _: &ShowShortcuts,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_shortcuts(cx);
    }

    /// Handles *User guide*, from either menu.
    pub(super) fn open_user_guide_action(
        &mut self,
        _: &OpenUserGuide,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_user_guide(cx);
    }

    /// Handles *Manage dashboards…*, from either menu.
    pub(super) fn manage_dashboards_action(
        &mut self,
        _: &ManageDashboards,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.manage_dashboards(cx);
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

    /// Handles the next-tab chord: the tab after the active one, the first
    /// after the last. Whatever the active tab is split into, which is the
    /// point — `Ctrl+digit` picks a pane there instead.
    pub(super) fn next_tab_action(
        &mut self,
        _: &NextTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = stepped_tab(self.active, self.tabs.len(), true) {
            self.select_tab(index, window, cx);
        }
    }

    /// Handles the previous-tab chord; see [`Workspace::next_tab_action`].
    pub(super) fn prev_tab_action(
        &mut self,
        _: &PrevTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = stepped_tab(self.active, self.tabs.len(), false) {
            self.select_tab(index, window, cx);
        }
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
        if self.pane_context.is_some() {
            self.close_pane_context(cx);
            return;
        }
        if self.panel.update(cx, |panel, cx| panel.close_context(cx)) {
            return;
        }
        // A question under the listing, while the keyboard is in the panel —
        // `Escape` aimed at the terminal is the terminal's.
        if self
            .panel
            .update(cx, |panel, cx| panel.cancel_focused_prompt(window, cx))
        {
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
        // opens over it. Escape is the canceling answer — the pane stays.
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
        if self.color_prompt.is_some() {
            self.close_color_prompt(window, cx);
            return;
        }
        if self.profile_delete.is_some() {
            self.cancel_delete_profile(window, cx);
            return;
        }
        if self.shortcuts_open {
            self.close_shortcuts(window, cx);
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
            // The dialog's own `Escape`: this binding is tried before any key
            // listener of the dialog's, so it is the one that runs, and the
            // dialog peels its own layers — a question, a row menu, a list —
            // before the form goes, asking about unsaved edits on the way.
            // Closing and restoring focus is then the subscription's job.
            self.dialog.update(cx, |dialog, cx| dialog.escape(cx));
            cx.notify();
            return;
        }
        if self.settings.read(cx).is_open() {
            // Through `request_dismiss` for the connection dialog's reason:
            // unsaved edits are asked about, and a plain close comes back as
            // `Dismissed`, which restores the focus.
            self.settings.update(cx, |dialog, cx| dialog.escape(cx));
            cx.notify();
            return;
        }
        cx.propagate();
    }
}
