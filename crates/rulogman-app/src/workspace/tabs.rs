//! Tabs.

use super::*;

impl Workspace {
    /// Activates the tab at `index`, if it exists.
    ///
    /// Selecting the tab that is already active is not a no-op: it scrolls the
    /// strip back to it, which is the point of picking it from the tab list.
    pub(super) fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        // See [`Workspace::on_pane_focused`]: a picker opened over one file must
        // not be answered against another. The shortcuts reach here without a
        // press for the menu's backdrop to catch.
        self.language_menu = None;
        self.charset_menu = None;
        self.active = index;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Takes the tab at `index` out of the strip and hangs up everything in it.
    ///
    /// The half of closing a tab that is the same however many tabs are going.
    /// It deliberately leaves [`Workspace::active`], the strip scroll and the
    /// focus alone: which tab should be active afterwards depends on how many
    /// more are still about to be removed, so only the caller can decide it.
    ///
    /// `index` must be in range; every caller has already checked it.
    pub(super) fn retire_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let tab = self.tabs.remove(index);
        for session in tab.sessions(cx) {
            self.forget_panel_session(session.entity_id(), cx);
            session.update(cx, |session, cx| session.disconnect(cx));
        }
    }

    /// Disconnects and removes the tab at `index`, panes and all — asking first
    /// if the tab is one file with unsaved changes in it.
    ///
    /// This is the tab strip's close button and the tab menu's close row: a tab
    /// that was split closes as a unit. Closing one pane at a time is
    /// [`Workspace::close_active_pane`].
    ///
    /// The question is [`tab_close_asks`]'s to decide. Answering it comes back
    /// through [`Workspace::confirm_close_editor`] and lands in
    /// [`Workspace::remove_pane`], which takes the last pane of a tab down by
    /// calling [`Workspace::close_tab_now`] — the unguarded half of this — so the
    /// question is asked once rather than again by the close it authorised.
    pub(super) fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if let Some(pane) = tab.unsaved_lone_editor(cx) {
            self.ask_before_closing(pane, window, cx);
            return;
        }
        self.close_tab_now(index, window, cx);
    }

    /// Disconnects and removes the tab at `index` without asking about anything.
    ///
    /// Every caller has already settled whatever question the tab raised, or
    /// there was none to raise; see [`Workspace::close_tab`].
    pub(super) fn close_tab_now(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.tabs.len() {
            return;
        }

        self.retire_tab(index, cx);

        // Removing a tab in front of the active one shifts it down a slot.
        if index < self.active {
            self.active -= 1;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Closes every tab except the one at `index` — and except any tab holding
    /// unsaved edits.
    ///
    /// A bulk close asks nothing, because there is no honest way to ask: a
    /// command aimed at a dozen tabs would have to put a dozen questions up in
    /// turn, and a user working through them has no way back to the one they
    /// already answered. So a tab with an edited file in it is simply left
    /// standing — the command still empties the strip of everything that had
    /// nothing to lose, and what is left is exactly what would have been lost.
    /// Closing one of those tabs afterwards asks about it the ordinary way.
    ///
    /// The focus ends on the tab the close was aimed from unless it was already
    /// on one of the survivors, in which case it stays where it is.
    pub(super) fn close_other_tabs(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.tabs.len() {
            return;
        }

        // Both indices follow the strip as it shrinks: `kept` is the tab this
        // was aimed from, which never goes, and `active` is where the focus is.
        let mut kept = index;
        let mut active = self.active;
        // Back to front, so that removing a tab never moves one that is still
        // to be visited.
        for other in (0..self.tabs.len()).rev() {
            if other == kept || self.tabs[other].holds_unsaved_work(cx) {
                continue;
            }
            self.retire_tab(other, cx);
            active = active_after_close(active, other, kept);
            kept = shifted(kept, other);
        }

        self.active = active;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Closes every tab after the one at `index`, bar any holding unsaved edits.
    ///
    /// A tab with an edited file in it is left standing, for the reason
    /// [`Workspace::close_other_tabs`] gives.
    ///
    /// A tab in front of the clicked one keeps the focus if it had it — nothing
    /// it was showing has gone anywhere. Only an active tab that was itself
    /// closed hands the focus over, and it hands it to the clicked tab, which is
    /// the nearest one still standing.
    pub(super) fn close_tabs_right(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index + 1 >= self.tabs.len() {
            return;
        }

        // The clicked tab cannot move — everything closing sits behind it — so
        // it is the survivor at the same index throughout.
        let mut active = self.active;
        for other in (index + 1..self.tabs.len()).rev() {
            if self.tabs[other].holds_unsaved_work(cx) {
                continue;
            }
            self.retire_tab(other, cx);
            active = active_after_close(active, other, index);
        }

        self.active = active;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Opens a second connection to the target of the tab at `index`, in a tab
    /// of its own right after it.
    ///
    /// The tab-sized counterpart of [`Workspace::duplicate_split`], and it takes
    /// the credentials the same way — through [`Session::duplicate`], the only
    /// place that can read them. What differs is where the new session lands and
    /// therefore what can refuse it: a split has to fit beside the pane it comes
    /// from, while a new tab is given the whole body and can always be had.
    ///
    /// Which pane of a split source tab is duplicated is the one its label
    /// already names — the active one — so the tab that appears is a second
    /// connection to whatever the strip said the tab was.
    pub(super) fn duplicate_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };

        // The tab may be showing nothing but open files by now, its shell
        // having exited; there is then no target to open a second connection to.
        let Some(session) = tab.active_session(cx) else {
            return;
        };
        // The second connection is to the same host as the first, so it opens
        // looking the way the first one looks now: the profile has already had
        // its say, and the tab being duplicated may have moved on from it.
        let panel_open = tab.panel_open;
        log::info!("opening a second session to {}", session.read(cx).title());

        // `None`, not this session: a second connection to a profile whose
        // ports *this* tab is holding is precisely the case to stay off them.
        let suppressed = self.tunnels_taken_from(&session, None, window, cx);
        let session = session.update(cx, |session, cx| session.duplicate(suppressed, cx));
        let caps = Self::pane_caps_source(cx);
        let view = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
        // A duplicate of a followed file follows that same file — see
        // [`Session::duplicate`] — so it belongs in the pane a followed file
        // belongs in, strip and all, rather than in a bare grid that could not
        // say which file it was showing.
        let leaf = match session.read(cx).tail_path().map(str::to_owned) {
            Some(path) => {
                // The duplicate opens in a tab of its own, so — like every
                // other pane that is the only one in its tab — it needs no
                // name to be told apart by.
                let tail = cx.new(|cx| {
                    TailView::new(view, session.clone(), path, SharedString::default(), cx)
                });
                self.new_tail_pane(tail, session, window, cx)
            }
            None => self.new_pane(view, session, window, cx),
        };

        let at = index + 1;
        self.tabs
            .insert(at, SessionTab::single(leaf).with_panel(panel_open));
        self.active = at;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Disconnects and removes the active pane of the active tab.
    ///
    /// The pane's sibling grows into the space it leaves. On the last pane of a
    /// tab there is no sibling to grow, so the tab goes with it.
    ///
    /// An editor pane with unsaved changes asks before it goes, whichever way it
    /// was closed — the shortcut here, or the pane's own close button.
    pub(super) fn close_active_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let pane = tab.active_pane();
        let unsaved = matches!(
            tab.panes.get(pane).map(|leaf| &leaf.view),
            Some(PaneView::Editor(editor)) if editor.read(cx).is_dirty(cx)
        );
        if unsaved {
            self.ask_before_closing(pane, window, cx);
            return;
        }
        self.remove_pane(self.active, pane, window, cx);
    }

    /// Retires the pane of a session whose connection has ended.
    ///
    /// This is the automatic arm of the close policy, driven by the session
    /// observer in [`Self::new_pane`]:
    ///
    /// * `Disconnected` — the remote shell exited or the server hung up — the
    ///   pane closes by itself; its sibling grows, and the tab goes once its
    ///   last pane does. When the last tab goes, the workspace shows the start
    ///   screen again rather than quitting.
    /// * `Failed` never lands here: a session that could not connect keeps its
    ///   pane, so the error and its Reconnect button stay readable.
    ///
    /// A session that is no longer in any tab — the manual close paths remove
    /// the pane *before* disconnecting it — is a no-op, which is also what
    /// makes the observer re-entrancy safe.
    pub(super) fn close_pane_for_session(
        &mut self,
        session: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let found = self.tabs.iter().enumerate().find_map(|(index, tab)| {
            tab.panes.leaves().into_iter().find_map(|(pane, leaf)| {
                let shown = leaf.view.session(cx)?;
                (shown.entity_id() == session).then_some((index, pane))
            })
        });
        let Some((index, pane)) = found else {
            return;
        };
        self.remove_pane(index, pane, window, cx);
    }

    /// Disconnects and removes one pane of the tab at `index`.
    ///
    /// The pane's sibling grows into the space it leaves. On the last pane of a
    /// tab there is no sibling to grow, so the tab goes with it. Focus only
    /// moves when the removed pane sat in the active tab; a background tab
    /// shrinking must not steal the keyboard.
    pub(super) fn remove_pane(
        &mut self,
        index: usize,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.panes.leaf_count() < 2 {
            // Unguarded: whatever the pane going had to be asked about was asked
            // before this was called — by the close question, or by the save it
            // ended in — and the tab is that pane.
            self.close_tab_now(index, window, cx);
            return;
        }

        // Read before the removal, while the pane being closed is still in the
        // tree and the order still names it: the pane the keyboard was in
        // before this one, and layout order only if there is no such pane.
        let successor = tab
            .focus_successor(pane)
            .or_else(|| tab.panes.next_leaf(pane));

        let tab = &mut self.tabs[index];
        let Some(leaf) = tab.panes.remove(pane) else {
            return;
        };
        tab.prune_focus_order();
        // The removed pane may not have been the active one — an idle split
        // closing in the background — in which case the active pane stands.
        if !tab.panes.contains(tab.active_pane) {
            let next = successor
                .filter(|id| tab.panes.contains(*id))
                .unwrap_or_else(|| tab.panes.first_leaf().0);
            tab.focus(next);
        }

        // Dropping the leaf takes its subscriptions and its view with it, so the
        // session has to be told to hang up first. Hanging up twice — the
        // automatic path arrives here already disconnected — is a no-op. An
        // editor pane owns no session and so hangs nothing up; the session it
        // was opened out of is still being shown by the terminal pane beside it,
        // or has already gone.
        if let Some(session) = leaf.view.session(cx) {
            self.forget_panel_session(session.entity_id(), cx);
            session.update(cx, |session, cx| session.disconnect(cx));
        }

        if index == self.active {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    /// Turns the tab at `source` into a split of the active tab.
    ///
    /// The source tab leaves the strip and its panes — the whole subtree, if it
    /// was itself split — appear next to the active pane, along `axis`. Focus
    /// follows the panes that moved.
    ///
    /// Splitting is always "merge another open tab in", so it needs a target the
    /// user picks: [`Workspace::render_tab_context`] is the only way in, and
    /// there is no shortcut for it.
    pub(crate) fn merge_tab_into_active(
        &mut self,
        source: usize,
        axis: Axis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if source >= self.tabs.len() || source == self.active {
            return;
        }
        if !self.can_split_active(axis, cx) {
            // Only reachable from a stale menu: the rows offering a split are
            // left out while the pane is this small.
            log::info!("refusing to merge tab {source}: the active pane is too small to split");
            return;
        }

        let target_pane = self.tabs[self.active].active_pane();
        let incoming = self.tabs.remove(source);
        // Removing a tab in front of the active one shifts it down a slot.
        if source < self.active {
            self.active -= 1;
        }

        let follow = incoming.active_pane();
        // Taken apart rather than read field by field so the panes can be moved
        // into the merge while the focus order they came with is kept.
        let SessionTab {
            panes: arriving,
            focus_order: history,
            ..
        } = incoming;
        let tab = &mut self.tabs[self.active];
        if !tab.panes.merge_subtree(target_pane, axis, arriving) {
            // `target_pane` came from this very tab a moment ago, so this is
            // unreachable; logged rather than ignored because reaching it would
            // mean a pane has been dropped on the floor.
            log::error!("the pane to split has vanished; the merge was dropped");
            return;
        }
        // The arriving panes keep the order the keyboard visited them in, and
        // land on top of this tab's: they are what the user is looking at now,
        // so closing one of them steps back through the tab it came from before
        // reaching the tab it was merged into.
        tab.focus_order.extend(history);
        tab.focus(follow);

        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Splits the active pane along `axis` and opens a second connection to the
    /// same host in the new half.
    ///
    /// The other half of splitting, and the one that needs no target: everything
    /// it has to know — the profile and the credentials — is already in the pane
    /// the user is looking at, which is why this one *can* have a shortcut where
    /// [`Workspace::merge_tab_into_active`] cannot.
    ///
    /// The new session is independent from the moment it is created: its own
    /// transport, its own shell, its own scrollback. Nothing about the state of
    /// the original matters, so a pane whose connection failed can still be
    /// split — that is how the user retries without losing the error on screen.
    pub(crate) fn duplicate_split(
        &mut self,
        axis: Axis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if !self.can_split_active(axis, cx) {
            // Reachable from the keyboard at any size, unlike the menu rows,
            // which are left out while the pane is this small.
            log::info!("refusing to split: the active pane is too small");
            return;
        }

        let target_pane = tab.active_pane();
        // `can_split_active` already refused an editor pane above, so the active
        // pane is a terminal and this is its session.
        let Some(session) = tab.active_session(cx) else {
            return;
        };
        log::info!("opening a second session to {}", session.read(cx).title());

        // As in [`Workspace::duplicate_tab`]: the pane being split is itself a
        // sibling, so its forwardings are a reason for the new pane to stay off
        // the ports rather than an exception to it.
        let suppressed = self.tunnels_taken_from(&session, None, window, cx);
        let session = session.update(cx, |session, cx| session.duplicate(suppressed, cx));
        let caps = Self::pane_caps_source(cx);
        let view = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
        let leaf = self.new_pane(view, session, window, cx);

        let tab = &mut self.tabs[self.active];
        let Some(pane) = tab.panes.split(target_pane, axis, leaf) else {
            // `target_pane` came out of this very tab a moment ago, so this is
            // unreachable; logged rather than ignored because reaching it would
            // mean a live session has been dropped on the floor.
            log::error!("the pane to split has vanished; the new session was dropped");
            return;
        };
        tab.focus(pane);

        self.focus_active(window, cx);
        cx.notify();
    }

    /// Moves the active pane into a tab of its own, right after the current one.
    ///
    /// The session keeps running throughout: the pane, its view and its
    /// subscriptions move over unchanged. A no-op on an unsplit tab, which is
    /// already exactly this.
    pub(crate) fn break_out_active_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.panes.leaf_count() < 2 {
            return;
        }

        let pane = tab.active_pane();
        // As in [`Workspace::remove_pane`]: the pane the keyboard was in before
        // this one, read while the order still holds both of them.
        let successor = tab
            .focus_successor(pane)
            .or_else(|| tab.panes.next_leaf(pane));

        let tab = &mut self.tabs[self.active];
        let Some(leaf) = tab.panes.remove(pane) else {
            return;
        };
        tab.prune_focus_order();
        let next = successor
            .filter(|id| tab.panes.contains(*id))
            .unwrap_or_else(|| tab.panes.first_leaf().0);
        tab.focus(next);
        // Nothing about the pane changed on the way out, and neither does what
        // stands beside it.
        let panel_open = tab.panel_open;

        let index = self.active + 1;
        self.tabs
            .insert(index, SessionTab::single(leaf).with_panel(panel_open));
        self.active = index;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Moves the tab at `index` into a window of its own.
    ///
    /// The counterpart of [`Workspace::break_out_active_pane`] one size up: a
    /// pane leaves its tab there, a tab leaves its window here, and neither
    /// disturbs what is on the other end. The sessions keep running throughout —
    /// nothing is disconnected, nothing is reconnected, no scrollback is lost —
    /// because a session is an entity of the *application* and only its wiring
    /// belongs to a window. Taking the tab out and putting it back are
    /// [`Workspace::detach_tab`] and [`Workspace::adopt_tab`]; this is the pair
    /// of them with a window opened in between.
    ///
    /// A refusal costs nothing: the tab is only taken out once the move can
    /// still be undone, and a window that fails to open puts it straight back
    /// where it was rather than dropping live sessions on the floor.
    pub(super) fn move_tab_to_new_window(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Read before the window opens and directly, without the `cx.defer`
        // [`NewWindow`] needs: that one is a global handler with no window in
        // hand, and asking gpui for the front window mid-dispatch finds it
        // lifted off the map. Here the window is the argument.
        let bounds = cascaded(window.bounds());
        let Some(tab) = self.detach_tab(index, window, cx) else {
            return;
        };

        let opened = match open_workspace_window_at(bounds, cx) {
            Ok(handle) => handle,
            Err(error) => {
                log::warn!("could not open a window for the tab: {error:#}");
                // Back into the strip it just left, wired up afresh to the
                // window it never left. The tab holds live sessions and the user
                // asked for it to be *moved*, so the one thing this must not do
                // is let it fall.
                self.adopt_tab(tab, window, cx);
                return;
            }
        };

        let moved = opened.update(cx, |workspace, window, cx| {
            workspace.adopt_tab(tab, window, cx);
            // The tab is what the user is now looking at, so the window holding
            // it comes forward. gpui gives a freshly opened window the focus on
            // most platforms and not on all of them; asking is what makes the
            // two agree.
            window.activate_window();
        });
        if let Err(error) = moved {
            log::error!("the window opened for the tab went away with it: {error}");
        }
    }

    /// Takes the tab at `index` out of the strip without hanging anything up,
    /// and hands it to the caller.
    ///
    /// [`Workspace::retire_tab`] minus the disconnect and plus the tidying up
    /// that [`Workspace::close_tab_now`] does, because from this window's side a
    /// tab that has left is a tab that has gone whichever way it went: the
    /// active tab has to be corrected, the strip scrolled and the focus put
    /// somewhere that still exists.
    ///
    /// The file panel forgets the sessions that are leaving. The panel is one
    /// per window and its browsing state — the directory each session was left
    /// in, and what was expanded there — belongs to the window, not to the tab,
    /// so it cannot travel. The tab arrives in its new window browsing the
    /// session's home directory again, which is a small cost and the honest one.
    ///
    /// `None` when there is nothing to hand over: an index off the end, or
    /// [`tab_can_move_out`] refusing the only tab this window has.
    pub(super) fn detach_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<SessionTab> {
        if index >= self.tabs.len() || !tab_can_move_out(self.tabs.len()) {
            return None;
        }

        let tab = self.tabs.remove(index);
        for session in tab.sessions(cx) {
            self.forget_panel_session(session.entity_id(), cx);
        }
        // See [`Workspace::select_tab`]: a picker opened over one file must not
        // be answered against another, and the tab under it has just gone.
        self.language_menu = None;
        self.charset_menu = None;

        // Removing a tab in front of the active one shifts it down a slot.
        if index < self.active {
            self.active -= 1;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();

        Some(tab)
    }

    /// Puts a tab detached from some window — possibly this one — into this
    /// window's strip, and makes it active.
    ///
    /// Every pane is wired up again from scratch. The views are not rebuilt and
    /// the sessions are not touched: what is remade is the *wiring*, all of
    /// which named the window the tab came from. The workspace subscriptions
    /// hanging off each leaf were made in that window and pointed at that
    /// workspace, and a terminal view carries three more of its own — see
    /// [`TerminalView::rebind`]. Overwriting the leaf is what unsubscribes the
    /// old ones, a [`Subscription`] being what it is.
    ///
    /// The tab keeps its shape: the same split tree, the same active pane, the
    /// same file panel state. It lands at the end of the strip rather than
    /// beside the active tab, because it did not come from beside it — a tab
    /// arriving from elsewhere has no neighbour here to be put back next to.
    pub(super) fn adopt_tab(
        &mut self,
        mut tab: SessionTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // One source for every leaf: it captures this workspace and nothing
        // about the pane, and it is an `Rc`, so a clone per pane costs a count.
        let caps = Self::pane_caps_source(cx);
        for id in tab.panes.leaf_ids() {
            // The handle comes out before anything is built with it, so the leaf
            // it came from is no longer borrowed when its replacement goes into
            // the slot.
            let Some(view) = tab.panes.get(id).map(|leaf| leaf.view.handle()) else {
                continue;
            };
            let rewired = match view {
                PaneView::Terminal(view) => {
                    let session = view.read(cx).session().clone();
                    view.update(cx, |view, cx| view.rebind(caps.clone(), window, cx));
                    self.new_pane(view, session, window, cx)
                }
                PaneView::Editor(pane) => self.new_editor_pane(pane, window, cx),
                // The same two steps as a terminal, one entity further in: the
                // grid is what holds the window-bound subscriptions, and the
                // strip above it holds nothing that a move invalidates.
                PaneView::Tail(view) => {
                    let session = view.read(cx).session().clone();
                    view.update(cx, |view, cx| view.rebind(caps.clone(), window, cx));
                    self.new_tail_pane(view, session, window, cx)
                }
            };
            if let Some(slot) = tab.panes.get_mut(id) {
                *slot = rewired;
            }
        }

        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Shows a file the panel has read, in a tab of its own.
    ///
    /// A tab rather than a split of the pane that asked. A split gives the file
    /// half of a terminal that was already only as wide as it needed to be, and
    /// it gives it *permanently*: there is no way back to the whole width while
    /// the file is open. A tab costs the file nothing and the shell nothing, and
    /// it is what every editor the user already has does with an opened file.
    /// The strip is where tabs are switched, listed and closed, so the file gets
    /// all of that for free — the close button included, which is why
    /// [`Workspace::close_tab`] asks about unsaved changes.
    ///
    /// It lands right after the active tab, where a duplicated tab and a broken
    /// out pane also land: the new tab belongs beside the one it came from
    /// rather than at the far end of a strip the user may have to scroll.
    ///
    /// No check that the session's tab is still open, which the split needed
    /// because it had to have a pane to split. The session itself arrives with
    /// the file — [`OpenEditor::session`] holds the entity — so everything the
    /// editor needs is here whether or not the tab that asked survived the read:
    /// the bytes are in hand, the [`files::FileSource`] outlives a disconnect,
    /// and the file panel keeps browsing that session through
    /// [`SessionTab::panel_session`]. Refusing would throw a file away for a
    /// reason the user cannot see.
    pub(super) fn open_editor(
        &mut self,
        opened: &OpenEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = opened.session.entity_id();
        let path = editor_pane::file_path(&opened.dir, &opened.name);

        // Asking for a file that is already open is a request to look at it,
        // not for a second buffer over the same bytes: two panes editing one
        // file would each write the other's work away at the next save.
        if let Some((index, pane)) = self.pane_of_file(session, &path, cx) {
            self.active = index;
            self.tabs[index].focus(pane);
            self.reveal_active_tab();
            self.focus_active(window, cx);
            cx.notify();
            return;
        }

        let editor = cx.new(|cx| {
            EditorPane::new(
                opened.session.clone(),
                opened.source.clone(),
                opened.dir.clone(),
                opened.name.clone(),
                opened.file.clone(),
                opened.original_bytes.clone(),
                opened.writable,
                opened.root_access,
                cx,
            )
        });
        let leaf = self.new_editor_pane(editor, window, cx);

        // Right after the active tab, or at the head of an empty strip — which
        // is where the file lands if the shell it was read from has since been
        // closed and was the last one open.
        let at = if self.tabs.is_empty() {
            0
        } else {
            self.active + 1
        };
        // The file was opened out of the panel, so the panel was open; the tab
        // carries that over rather than shutting it, which is how the next file
        // is reached. An empty strip — the shell the file came from having been
        // closed since — has no tab to carry anything over from, and the panel
        // is what the file arrived through either way.
        let panel_open = self.tabs.get(self.active).is_none_or(|tab| tab.panel_open);
        log::info!("opening {path} in a tab of its own");
        self.tabs
            .insert(at, SessionTab::single(leaf).with_panel(panel_open));
        self.active = at;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Moves focus to the previous pane of the active tab, wrapping around.
    pub(crate) fn focus_prev_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_pane(false, window, cx);
    }

    /// Steps the active pane one place through the active tab's focus cycle.
    pub(super) fn cycle_pane(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.panes.leaf_count() < 2 {
            return;
        }

        let from = tab.active_pane();
        let next = if forward {
            tab.panes.next_leaf(from)
        } else {
            tab.panes.prev_leaf(from)
        };
        let Some(next) = next else {
            return;
        };

        tab.focus(next);
        // Focusing the pane's grid also runs `on_pane_focused`, which is
        // harmless: it finds the pane already marked active.
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Whether the active pane is big enough to be split along `axis`.
    ///
    /// The two halves inherit roughly half of the pane's current grid each, so
    /// the check is on the live column or row count rather than on pixels: a
    /// pane that would come out narrower than [`MIN_PANE_COLS`] or shorter than
    /// [`MIN_PANE_ROWS`] is not worth having.
    ///
    /// Silent, because every menu carrying a split asks this on each frame it is
    /// open, to decide which rows to grey or to leave out; the refusal is logged
    /// where it happens.
    ///
    /// Always `false` over an editor pane. Every split the workspace offers puts
    /// a *second connection to the same host* in the new half, and an editor is
    /// not a connection: there is nothing to open a second one of. Over such a
    /// pane the rows asking for it are greyed in the application and pane menus
    /// and left out of the tab menu — see [`MenuEntry::enabled`] for which menu
    /// does which — and the shortcuts do nothing.
    pub(super) fn can_split_active(&self, axis: Axis, cx: &App) -> bool {
        let Some(tab) = self.tabs.get(self.active) else {
            return false;
        };
        let PaneView::Terminal(view) = tab.active_view() else {
            return false;
        };
        let (cols, rows) = view.read(cx).session().read(cx).terminal().size();
        split_fits(axis, cols, rows)
    }

    /// [`Workspace::can_split_active`] for a pane that has handed its grid size
    /// over instead of being read for it.
    ///
    /// Same verdict, same order of questions; only the size arrives by argument.
    /// A pane asks this way while it is rendering its own menu, when reading the
    /// view back would panic — see [`PaneCapsSource`] — and the size it passes
    /// is the size that read would have returned.
    pub(super) fn can_split_sized(&self, axis: Axis, cols: u16, rows: u16) -> bool {
        let Some(tab) = self.tabs.get(self.active) else {
            return false;
        };
        if !matches!(tab.active_view(), PaneView::Terminal(_)) {
            return false;
        }
        split_fits(axis, cols, rows)
    }

    /// Whether the active pane may be broken out into a tab of its own.
    ///
    /// A tab with one pane already *is* that tab, so the command has nothing to
    /// move; [`Workspace::break_out_active_pane`] returns on exactly this
    /// condition, and the rows offering it read the same rule from here.
    pub(super) fn can_break_out_active(&self) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| tab.panes.leaf_count() > 1)
    }

    /// The three pane commands' verdicts in one answer, for a menu that needs
    /// all of them; see [`PaneCaps`].
    pub(super) fn pane_caps(&self, cx: &App) -> PaneCaps {
        PaneCaps {
            split_right: self.can_split_active(Axis::Horizontal, cx),
            split_below: self.can_split_active(Axis::Vertical, cx),
            break_out: self.can_break_out_active(),
            equalize_widths: self.can_equalize(Axis::Horizontal),
            equalize_heights: self.can_equalize(Axis::Vertical),
        }
    }

    /// [`Workspace::pane_caps`] for a pane asking about itself mid-render, which
    /// reports its grid size rather than being read for it.
    pub(super) fn pane_caps_sized(&self, cols: u16, rows: u16) -> PaneCaps {
        PaneCaps {
            split_right: self.can_split_sized(Axis::Horizontal, cols, rows),
            split_below: self.can_split_sized(Axis::Vertical, cols, rows),
            break_out: self.can_break_out_active(),
            equalize_widths: self.can_equalize(Axis::Horizontal),
            equalize_heights: self.can_equalize(Axis::Vertical),
        }
    }

    /// Builds the callback a terminal view asks the question above through.
    ///
    /// Weak on purpose, and not only to avoid a cycle through a view the
    /// workspace owns: a pane can outlive the workspace by a frame while the
    /// window is tearing down, and a menu drawn in that frame is better off
    /// offering nothing than keeping the workspace alive to answer it.
    pub(super) fn pane_caps_source(cx: &mut Context<Self>) -> PaneCapsSource {
        let workspace = cx.weak_entity();
        Rc::new(
            move |cols: u16, rows: u16, cx: &App| match workspace.upgrade() {
                Some(workspace) => workspace.read(cx).pane_caps_sized(cols, rows),
                None => PaneCaps::default(),
            },
        )
    }

    /// Scrolls the tab strip so that the active tab is on screen.
    ///
    /// The strip applies this during its next prepaint, so callers have to ask
    /// for a repaint as well.
    pub(super) fn reveal_active_tab(&self) {
        if !self.tabs.is_empty() {
            self.tab_scroll.scroll_to_item(self.active);
        }
    }

    /// Moves keyboard focus onto the active pane's terminal, or onto the
    /// workspace itself when no session is open.
    ///
    /// Without this the shortcuts stop working after the last tab is closed,
    /// because their key context only exists while something inside the
    /// workspace is focused.
    pub(super) fn focus_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.tabs.get(self.active) {
            Some(tab) => {
                let handle = tab.active_view().focus_handle(cx);
                window.focus(&handle, cx);
            }
            None => window.focus(&self.focus_handle, cx),
        }
    }

    /// Whether one of the modal dialogs is on screen.
    ///
    /// A modal takes the window over, so anything the strip would otherwise open
    /// on top of it has to stand down.
    pub(super) fn dialog_open(&self, cx: &App) -> bool {
        self.dialog.read(cx).is_open()
            || self.settings.read(cx).is_open()
            || self.about.read(cx).is_open()
            || self.update.read(cx).is_open()
            // A question rather than a dialog, but it takes the window the same
            // way and must not be drawn under a menu opened over it.
            || self.close_confirm.is_some()
            // And so is the password an elevated save asks for.
            || self.sudo_prompt.is_some()
    }

    /// Whether the active tab has a divider an `axis` pass would move.
    ///
    /// A tab with no split along that axis has nothing to even out — one pane,
    /// or panes stacked the other way — and the rows offering it are greyed or
    /// left out rather than shown doing nothing. A tab whose panes are *already*
    /// even still offers the command: the answer would flicker as a divider is
    /// dragged, and running it there costs nothing.
    pub(super) fn can_equalize(&self, axis: Axis) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| tab.panes.splits_along(axis) > 0)
    }

    /// Records where the divider of `split` has been dragged to.
    ///
    /// The share arrives from [`Splitter`] already measured against the split's
    /// own box, already clamped short of either edge and already a number, so
    /// there is nothing to sanitise here — only a tab to find. It is looked up
    /// now rather than captured when the divider was drawn, because the active
    /// tab can change between the frame that drew the handle and this event.
    pub(super) fn set_split_ratio(&mut self, split: SplitId, ratio: f32, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.panes.set_ratio(split, ratio) {
            cx.notify();
        }
    }

    /// One surface's scroll offset and the state of the bar over it.
    ///
    /// The pair is what every handler below works on, and taking it by one
    /// lookup is what lets them be written once for both surfaces rather than
    /// once each.
    pub(super) fn surface(&mut self, surface: Surface) -> (&ScrollHandle, &mut ScrollbarState) {
        match surface {
            Surface::Tabs => (&self.tab_scroll, &mut self.tab_scrollbar),
            Surface::Empty => (&self.empty_scroll, &mut self.empty_scrollbar),
        }
    }
}
