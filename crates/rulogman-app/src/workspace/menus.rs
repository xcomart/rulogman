//! Menus.

use super::*;

impl Workspace {
    /// Whether the file panel is standing beside the body as things are.
    ///
    /// The active tab's flag, and `false` when there is no active tab: the
    /// welcome screen takes the place of the body the panel would be drawn
    /// beside, so a window with nothing open is a window with no panel. Both
    /// render paths that turn on the panel ask here — the body, which puts it on
    /// screen, and the toolbar, whose button lights up to say that it is there —
    /// so that what the strip claims and what stands under it cannot come apart.
    pub(super) fn panel_showing(&self) -> bool {
        self.tabs.get(self.active).is_some_and(|tab| tab.panel_open)
    }

    /// Tells the file panel which session it is looking at.
    ///
    /// Called from the render pass rather than from each of the eight places
    /// that can change the active pane, so there is no site left to forget. The
    /// panel compares the session against the one it already holds and returns
    /// without repainting when they match, which is every frame but the ones
    /// that actually switch.
    ///
    /// [`SessionTab::panel_session`] rather than the session the tab speaks for,
    /// so that switching to an open file does not empty the panel: the file came
    /// from a filesystem, and that filesystem is what the panel goes on showing
    /// beside it — which is how the next file is opened.
    pub(super) fn sync_file_panel(&self, cx: &mut Context<Self>) {
        let session = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.panel_session(cx));
        self.panel
            .update(cx, |panel, cx| panel.set_session(session, cx));
    }

    /// Drops a closed session's browsing state from the file panel.
    pub(super) fn forget_panel_session(&self, session: EntityId, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.forget_session(session, cx));
    }

    /// Shows or hides the application dropdown menu.
    pub(super) fn set_menu_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.menu_open == open {
            return;
        }
        self.menu_open = open;
        cx.notify();
    }

    /// Shows or hides the tab strip's dropdown tab list.
    pub(super) fn set_tab_menu_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.tab_menu_open == open {
            return;
        }
        self.tab_menu_open = open;
        cx.notify();
    }

    /// Opens the context menu of the tab at `index`, with its corner at `at`.
    ///
    /// The right-click that gets here does not change the active tab, so `index`
    /// and [`Workspace::active`] are independent — which is what the menu's
    /// commands are built around.
    pub(super) fn open_tab_context(
        &mut self,
        index: usize,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if index >= self.tabs.len() || self.dialog_open(cx) {
            return;
        }
        // Not `close_overlays`: a modal dialog outranks the strip — the guard
        // above leaves it alone — while the other dropdowns are simply mutually
        // exclusive with this menu.
        self.menu_open = false;
        self.tab_menu_open = false;
        self.language_menu = None;
        self.charset_menu = None;
        self.tab_context = Some((index, at));
        cx.notify();
    }

    /// Puts the tab context menu away, if one is open.
    pub(super) fn close_tab_context(&mut self, cx: &mut Context<Self>) {
        if self.tab_context.take().is_some() {
            cx.notify();
        }
    }

    /// Opens the status bar's file-type picker, with its foot at `at`.
    ///
    /// Guarded like [`Workspace::open_tab_context`]: a modal outranks the bar
    /// underneath it, and the other dropdowns are mutually exclusive with this
    /// one. Refused outright when the active pane is not a file, which is also
    /// when the trigger is not drawn — the guard is for the frame between a
    /// press and the pane changing under it.
    pub(super) fn open_language_menu(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        if self.dialog_open(cx) || self.active_editor().is_none() {
            return;
        }
        self.menu_open = false;
        self.tab_menu_open = false;
        self.tab_context = None;
        self.charset_menu = None;
        self.language_menu = Some(at);
        cx.notify();
    }

    /// Puts the file-type picker away, if it is open.
    pub(super) fn close_language_menu(&mut self, cx: &mut Context<Self>) {
        if self.language_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Opens the status bar's character-encoding picker, with its foot at `at`.
    ///
    /// Guarded exactly like [`Workspace::open_language_menu`], and it closes
    /// that one: the two triggers sit side by side on the bar, so opening this
    /// list while the other stood would leave two menus overlapping the button
    /// they both hang off.
    pub(super) fn open_charset_menu(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        if self.dialog_open(cx) || self.active_editor().is_none() {
            return;
        }
        self.menu_open = false;
        self.tab_menu_open = false;
        self.tab_context = None;
        self.language_menu = None;
        self.charset_menu = Some(at);
        cx.notify();
    }

    /// Puts the character-encoding picker away, if it is open.
    pub(super) fn close_charset_menu(&mut self, cx: &mut Context<Self>) {
        if self.charset_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Colours the active file as `language`.
    ///
    /// A no-op on a tab whose active pane is not a file, which is only reachable
    /// from a menu that outlived the pane it was opened over.
    pub(super) fn set_active_language(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else {
            return;
        };
        editor.update(cx, |editor, cx| editor.set_language(id, cx));
        cx.notify();
    }

    /// Re-reads the active file in `charset`.
    ///
    /// A no-op on a tab whose active pane is not a file, for the same reason
    /// [`Workspace::set_active_language`] is. Unlike the language, this one can
    /// decline — an unsaved buffer, or bytes that are not text in the charset
    /// asked for — and it says so on the pane itself, where the file is.
    pub(super) fn set_active_charset(&mut self, charset: Charset, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else {
            return;
        };
        editor.update(cx, |editor, cx| editor.set_charset(charset, cx));
        cx.notify();
    }

    /// The open file the keyboard is in, if the active pane is one.
    pub(super) fn active_editor(&self) -> Option<&Entity<EditorPane>> {
        match self.tabs.get(self.active)?.active_view() {
            PaneView::Editor(editor) => Some(editor),
            PaneView::Terminal(_) | PaneView::Tail(_) => None,
        }
    }

    /// Opens the context menu of the saved profile `id`, with its corner at
    /// `at`.
    ///
    /// Guarded like [`Workspace::open_tab_context`], and for the same reasons:
    /// a modal outranks the empty state behind it, while the two dropdowns are
    /// simply mutually exclusive with this menu.
    pub(super) fn open_empty_context(
        &mut self,
        id: Uuid,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.dialog_open(cx) {
            return;
        }
        self.menu_open = false;
        self.tab_menu_open = false;
        self.language_menu = None;
        self.charset_menu = None;
        self.empty_context = Some((id, at));
        cx.notify();
    }

    /// Puts the empty state's context menu away, if one is open.
    pub(super) fn close_empty_context(&mut self, cx: &mut Context<Self>) {
        if self.empty_context.take().is_some() {
            cx.notify();
        }
    }

    /// Renders the context menu of a right-clicked tab, if one is open.
    ///
    /// The commands depend on which tab was clicked, because both of them are
    /// about the active tab:
    ///
    /// * on another tab, the menu merges *that* tab into the active one as a
    ///   split — the only way to bring an existing session in, and the reason
    ///   that half of splitting has no shortcut;
    /// * on the active tab, it splits the active pane off into a second
    ///   connection to the same host, and offers the reverse of a merge: moving
    ///   the active pane back out into a tab of its own, which needs the tab to
    ///   actually be split.
    ///
    /// One row is the exception and acts on the clicked tab wherever it sits:
    /// moving that tab into a window of its own, which is how a tab is dragged
    /// out of a crowded window without first bringing it to the front.
    ///
    /// A row whose command would be refused is left out rather than shown doing
    /// nothing, so the menu can come down to nothing but "close this tab".
    ///
    /// The rows come in three groups, separated in that order: rearranging the
    /// panes of the strip, opening a connection, and closing tabs. A group whose
    /// every row was left out contributes no rule of its own.
    pub(super) fn render_tab_context(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let (index, position) = self.tab_context?;
        // The strip and the stored index are a frame apart: a tab can be gone by
        // now — closed from the menu itself, or by the session that owned it.
        let tab = self.tabs.get(index)?;
        let this = cx.entity();

        let mut splits = Vec::new();
        let mut break_out = Vec::new();
        if index == self.active {
            // A split that would leave an unusably small pane is refused, so the
            // row asking for it is left out rather than offered and ignored.
            if self.can_split_active(Axis::Horizontal, cx) {
                splits.push(
                    MenuEntry::new(ts!("tab.duplicate_right"))
                        .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+D"))
                        .on_activate(|window, cx| {
                            window.dispatch_action(Box::new(DuplicateSplitRight), cx)
                        }),
                );
            }
            if self.can_split_active(Axis::Vertical, cx) {
                splits.push(
                    MenuEntry::new(ts!("tab.duplicate_below"))
                        .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+S"))
                        .on_activate(|window, cx| {
                            window.dispatch_action(Box::new(DuplicateSplitBelow), cx)
                        }),
                );
            }
            // In the split group rather than a group of their own: they are
            // about the shape of the panes the splits above made.
            for (label, axis) in [
                (ts!("menu.equalize_widths"), Axis::Horizontal),
                (ts!("menu.equalize_heights"), Axis::Vertical),
            ] {
                if !self.can_equalize(axis) {
                    continue;
                }
                let this = this.clone();
                splits.push(MenuEntry::new(label).on_activate(move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.equalize_panes(axis, cx));
                }));
            }
            if self.can_break_out_active() {
                break_out.push(
                    MenuEntry::new(ts!("menu.break_out_pane"))
                        .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+B"))
                        .on_activate(|window, cx| {
                            window.dispatch_action(Box::new(BreakOutPane), cx)
                        }),
                );
            }
        } else {
            // A split that would leave an unusably small pane is refused, so the
            // row asking for it is left out rather than offered and ignored.
            for (label, axis) in [
                (ts!("tab.split_right"), Axis::Horizontal),
                (ts!("tab.split_below"), Axis::Vertical),
            ] {
                if !self.can_split_active(axis, cx) {
                    continue;
                }
                let this = this.clone();
                splits.push(MenuEntry::new(label).on_activate(move |window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.merge_tab_into_active(index, axis, window, cx);
                    });
                }));
            }
        }

        // The one command in this menu that acts on the clicked tab whether or
        // not it is the active one, which is what it is for: a tab is sent off
        // to a window of its own by right-clicking *it*, wherever the keyboard
        // happens to be. It closes whichever group is about rearranging the
        // strip — the break-out on the active tab, the splits on any other — and
        // it is left out on a window's only tab, for which see
        // [`tab_can_move_out`]. The chord is named only on the active tab,
        // because that is the tab it would act on.
        if tab_can_move_out(self.tabs.len()) {
            let this = this.clone();
            let mut row =
                MenuEntry::new(ts!("menu.tab_to_window")).on_activate(move |window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.move_tab_to_new_window(index, window, cx);
                    });
                });
            if index == self.active {
                row = row.shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+N"));
                break_out.push(row);
            } else {
                splits.push(row);
            }
        }

        // The one dashboard-specific command, and the primary way to reach it:
        // capture what this tab looks like now back onto the dashboard it was
        // opened from. Offered only on a dashboard tab — the only tab with
        // somewhere to save to — and acting on the clicked tab whether or not it
        // is the active one. The chord is named only on the active tab, because
        // that is the tab the shortcut would save.
        let mut dashboard_actions = Vec::new();
        if tab.dashboard.is_some() {
            let this = this.clone();
            let mut row = MenuEntry::new(ts!("tab.save_layout")).on_activate(move |_window, cx| {
                this.update(cx, |workspace, cx| workspace.save_tab_layout(index, cx));
            });
            if index == self.active {
                row = row.shortcut(format!("{SHORTCUT_MODIFIER}+Shift+L"));
            }
            dashboard_actions.push(row);
        }

        // Both rows speak for the session the tab label already names, which on
        // a split tab is the active pane's rather than the tab's first. A tab
        // holding nothing but open files names no session, and neither row means
        // anything without one.
        let mut connect = Vec::new();
        if let Some(entity) = tab.active_session(cx) {
            let session = entity.read(cx);
            connect.push(MenuEntry::new(ts!("tab.duplicate")).on_activate({
                let this = this.clone();
                move |window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.duplicate_tab(index, window, cx);
                    });
                }
            }));
            if !session.status().is_live() {
                // The same command the connection overlay's button carries,
                // worded the way that button words it: a local shell is started
                // again, not reconnected to.
                let label = if session.is_local() {
                    ts!("session.restart")
                } else {
                    ts!("session.reconnect")
                };
                let session = entity.clone();
                let this = this.clone();
                connect.push(MenuEntry::new(label).on_activate(move |window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.reconnect_session(&session, window, cx);
                    });
                }));
            }
            // The followed files of the profile this session came from, worded
            // and ordered exactly as the profile row's own menu words them:
            // a shell on a host is where the user is standing when they want a
            // log off that host, and having to go back to the welcome screen
            // for it would be a trip through a screen this window is not even
            // showing.
            //
            // Read out of the profile store rather than off the session, which
            // holds the profile as it was when the tab was opened: a file added
            // to the connection since then is a file the user has just asked
            // for, and a store lookup is what the empty state does too. A
            // session whose profile has since been forgotten simply offers no
            // rows.
            if let Some(profile) = session
                .profile_id()
                .and_then(|id| self.profile(id, cx))
                .filter(|profile| !profile.tails.is_empty())
            {
                for rule in &profile.tails {
                    let path = rule.path.clone();
                    let label = ts!(
                        "empty.menu_tail",
                        name = session::remote_file_name(&path).to_owned()
                    );
                    let this = this.clone();
                    let profile = profile.clone();
                    connect.push(MenuEntry::new(label).on_activate(move |window, cx| {
                        let (profile, path) = (profile.clone(), path.clone());
                        this.update(cx, |workspace, cx| {
                            workspace.open_tail(&profile, path, window, cx);
                        });
                    }));
                }
            }
        }

        // One row per followed file of every saved connection, which is the
        // group that closes the authoring loop: adding panes to a tab, dragging
        // the dividers between them and then *Save layout to dashboard* is how
        // a dashboard is composed by hand, and this is the only thing in the
        // application that can add the pane. Every connection rather than this
        // tab's — unlike the `connect` group above, which offers the files of
        // the session the tab already holds — because an arrangement worth
        // saving is usually one that spans hosts: the point of a dashboard is
        // the deploy watched across all of them at once. The list is as long as
        // the user's own configuration makes it and is not capped; a menu of
        // twenty rows is a configuration of twenty followed files, and hiding
        // some of them would only make the missing ones unreachable.
        //
        // Offered on the active tab alone, and gated exactly as the split rows
        // are: the size check can only answer for the *active* pane, so on any
        // other tab it would be measuring one pane and splitting another. A
        // pane already too small to split in half does not offer to be.
        let mut add_tails = Vec::new();
        if index == self.active && self.can_split_active(Axis::Vertical, cx) {
            for profile in self.dialog.read(cx).profiles() {
                for rule in &profile.tails {
                    let path = rule.path.clone();
                    let label = ts!(
                        "tab.add_tail",
                        file = session::remote_file_name(&path).to_owned(),
                        connection = profile.name.clone()
                    );
                    let this = this.clone();
                    let profile_id = profile.id;
                    add_tails.push(MenuEntry::new(label).on_activate(move |window, cx| {
                        let path = path.clone();
                        this.update(cx, |workspace, cx| {
                            workspace.add_tail_to_tab(index, profile_id, path, window, cx);
                        });
                    }));
                }
            }
        }

        let mut close = vec![MenuEntry::new(ts!("tab.close")).on_activate({
            let this = this.clone();
            move |window, cx| {
                this.update(cx, |workspace, cx| workspace.close_tab(index, window, cx));
            }
        })];
        if self.tabs.len() > 1 {
            close.push(MenuEntry::new(ts!("tab.close_others")).on_activate({
                let this = this.clone();
                move |window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.close_other_tabs(index, window, cx);
                    });
                }
            }));
        }
        if index + 1 < self.tabs.len() {
            close.push(MenuEntry::new(ts!("tab.close_right")).on_activate({
                let this = this.clone();
                move |window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.close_tabs_right(index, window, cx);
                    });
                }
            }));
        }

        let mut entries = Vec::new();
        for group in [
            splits,
            break_out,
            dashboard_actions,
            connect,
            add_tails,
            close,
        ] {
            if group.is_empty() {
                continue;
            }
            if !entries.is_empty() {
                entries.push(MenuEntry::separator());
            }
            entries.extend(group);
        }

        Some(
            ContextMenu::new("tab-context")
                .position(position)
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.close_tab_context(cx));
                }),
        )
    }

    /// Renders the context menu of an empty-state profile row, if one is open.
    ///
    /// Four rows and no conditions on them: every saved profile can be
    /// connected to, edited, copied and forgotten, whatever it holds. What can
    /// go is the profile itself — the store is re-read whenever the dialog
    /// opens, and this menu can outlive the row that opened it — in which case
    /// there is nothing left for the menu to speak for and it draws nothing.
    pub(super) fn render_empty_context(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let (id, position) = self.empty_context?;
        let profile = self.profile(id, cx)?;
        let this = cx.entity();

        let mut entries = vec![MenuEntry::new(ts!("connection.connect")).on_activate({
            let this = this.clone();
            let profile = profile.clone();
            move |window, cx| {
                let profile = profile.clone();
                this.update(cx, |workspace, cx| {
                    workspace.open_profile(&profile, window, cx);
                });
            }
        })];

        // One row per file the profile follows, straight under *Connect*,
        // because that is what they are: a second way to open this connection,
        // pointed at a file rather than at a shell. They are named after the
        // file rather than after the path — a menu row is one line wide and the
        // last component is what tells two logs apart — and the whole path is
        // read in the pane the row opens, where there is room for it.
        for rule in &profile.tails {
            let path = rule.path.clone();
            let label = ts!(
                "empty.menu_tail",
                name = session::remote_file_name(&path).to_owned()
            );
            let this = this.clone();
            let profile = profile.clone();
            entries.push(MenuEntry::new(label).on_activate(move |window, cx| {
                let (profile, path) = (profile.clone(), path.clone());
                this.update(cx, |workspace, cx| {
                    workspace.open_tail(&profile, path, window, cx);
                });
            }));
        }

        entries.extend([
            // The ellipsis the dialog's own Edit button does without: from here
            // the form is not on screen yet, so this row promises it.
            MenuEntry::new(ts!("empty.menu_edit")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.edit_profile(id, cx));
                }
            }),
            MenuEntry::new(ts!("connection.duplicate")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.duplicate_profile(id, cx));
                }
            }),
            MenuEntry::separator(),
            MenuEntry::new(ts!("connection.delete")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.delete_profile(id, cx));
                }
            }),
        ]);

        Some(
            ContextMenu::new("empty-profile-context")
                .position(position)
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.close_empty_context(cx));
                }),
        )
    }
}
