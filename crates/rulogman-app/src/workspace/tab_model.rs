//! Tab contents, pane handles and focus history.

use super::*;

/// What one pane is showing.
///
/// A tab is still a tab *of sessions* — the strip, the status bar and every
/// shortcut speak for a session — but a pane no longer has to be one. An editor
/// pane belongs to the session it was opened out of without *being* it, which is
/// the whole of the difference the two arms below encode: only a terminal
/// answers [`PaneView::session`], so only a terminal is closed when its session
/// hangs up, counted when the workspace disconnects everything, or offered to
/// the file panel.
pub(super) enum PaneView {
    /// A shell, over SSH or on this machine. Owns its [`Session`] entity.
    Terminal(Entity<TerminalView>),
    /// A file opened out of the file panel.
    Editor(Entity<EditorPane>),
    /// A remote file being followed, `tail -f` style, over a session of its own.
    ///
    /// A session like any other, which is the whole reason it is not an editor:
    /// it connects, it can drop, it can be reconnected, and it wears a status
    /// dot in the strip — so it answers [`PaneView::session`] exactly as a
    /// terminal does, and every rule written against that answer applies to it
    /// unchanged. What makes it its own arm rather than a terminal is the strip
    /// above the grid; see [`TailView`].
    Tail(Entity<TailView>),
}

impl PaneView {
    /// A second handle on the same surface.
    ///
    /// Not a copy of anything: an [`Entity`] is a handle into the application's
    /// entity map, so what this clones is the reference and not the terminal or
    /// the buffer behind it. That is what lets a pane be taken out of one
    /// window's wiring and put into another's without the surface it draws being
    /// rebuilt — see [`Workspace::adopt_tab`], the only caller.
    ///
    /// Spelled out rather than derived so that the paragraph above has somewhere
    /// to live: a bare `Clone` on this type would read as "duplicate the pane",
    /// which is a different command this application also has.
    pub(super) fn handle(&self) -> Self {
        match self {
            Self::Terminal(view) => Self::Terminal(view.clone()),
            Self::Editor(pane) => Self::Editor(pane.clone()),
            Self::Tail(view) => Self::Tail(view.clone()),
        }
    }

    /// The entity behind the pane, which is what a focus event names.
    pub(super) fn entity_id(&self) -> EntityId {
        match self {
            Self::Terminal(view) => view.entity_id(),
            Self::Editor(pane) => pane.entity_id(),
            Self::Tail(view) => view.entity_id(),
        }
    }

    /// Where the keyboard goes when this pane is made active.
    pub(super) fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::Terminal(view) => view.read(cx).focus_handle(cx),
            Self::Editor(pane) => pane.read(cx).focus_handle(cx),
            // The grid's own, handed on by the strip above it: a followed file
            // is read, selected and copied out of exactly as a shell is.
            Self::Tail(view) => view.read(cx).focus_handle(cx),
        }
    }

    /// The session this pane *is*, if it is one.
    ///
    /// `None` for an editor, which merely came from one. That is what keeps an
    /// open file on screen after the shell it was read from exits: the
    /// disconnect closes the panes showing that session, and this pane is not
    /// one of them.
    pub(super) fn session(&self, cx: &App) -> Option<Entity<Session>> {
        match self {
            Self::Terminal(view) => Some(view.read(cx).session().clone()),
            Self::Editor(_) => None,
            // A followed file *is* its session, unlike an editor: the tab is
            // named by its title, dotted by its status, closed when it hangs up
            // and offered a reconnect when it fails, all through this answer.
            Self::Tail(view) => Some(view.read(cx).session().clone()),
        }
    }

    /// The session an editor pane was opened out of, if this pane is one.
    ///
    /// The counterpart of [`PaneView::session`] and pointedly not a widening of
    /// it: that one answers "which session *is* this pane", which is what the
    /// tab label, the status bar and the disconnect path all ask, and an editor
    /// has to keep answering `None` there or a tab of open files would report
    /// itself as a connection. This one answers "which filesystem is this file
    /// on", which only the file panel asks.
    pub(super) fn editor_session(&self, cx: &App) -> Option<Entity<Session>> {
        match self {
            Self::Terminal(_) => None,
            Self::Editor(pane) => Some(pane.read(cx).session().clone()),
            // Nothing to add: this question is asked by the file panel, and a
            // pane that answers [`PaneView::session`] has already answered it.
            Self::Tail(_) => None,
        }
    }

    /// What the tab strip calls this pane when there is no session to name it
    /// after.
    pub(super) fn label(&self, cx: &App) -> SharedString {
        match self {
            Self::Terminal(view) => view.read(cx).session().read(cx).title(),
            Self::Editor(pane) => {
                let pane = pane.read(cx);
                editor_tab_label(pane.name(), &pane.session().read(cx).title())
            }
            // Its session's title, which is already `file - connection`; see
            // [`Session::title`], which is where a followed file is named.
            Self::Tail(view) => view.read(cx).session().read(cx).title(),
        }
    }

    /// The pane's surface, as an element.
    pub(super) fn element(&self) -> AnyElement {
        match self {
            Self::Terminal(view) => view.clone().into_any_element(),
            Self::Editor(pane) => pane.clone().into_any_element(),
            Self::Tail(view) => view.clone().into_any_element(),
        }
    }
}

/// One pane: the view showing a session, plus the wiring that keeps the
/// workspace in step with it.
pub(super) struct PaneLeaf {
    /// The surface this pane draws.
    pub(super) view: PaneView,
    /// Repaints the workspace when what it draws *about* this pane changes.
    ///
    /// Two different subscriptions behind one field, because the two kinds of
    /// pane have different things worth watching. A terminal's watches its
    /// *session*: the tab strip prints its title and its status dot. An
    /// editor's watches the *pane*, because the status bar prints the caret's
    /// line and the file's language, and both are read off the pane — a caret
    /// move changes nothing the workspace would otherwise be asked to redraw.
    ///
    /// `Option` because it was once terminals only; it is now always `Some`,
    /// and stays an `Option` so that a pane kind with nothing to watch can be
    /// added without threading a dummy subscription through.
    pub(super) _observer: Option<Subscription>,
    /// Records this pane as the active one when a click focuses its view.
    ///
    /// Driven by [`PaneFocused`] rather than `cx.on_focus`: gpui fires focus
    /// listeners after the frame that carried the click was already drawn, so
    /// a frame-swap driven that way would not show up until the next input
    /// event — the active-pane frame would visibly trail the click.
    pub(super) _clicked: Subscription,
    /// Backstop for focus arriving by any route other than a click, e.g. a
    /// future programmatic `window.focus`. One frame late by gpui's dispatch
    /// order, which does not matter for paths that repaint anyway.
    pub(super) _focus: Subscription,
    /// Carries the pane's *Reconnect* button to the workspace that owns the
    /// pane right now.
    ///
    /// Kept beside the three above rather than detached, which is what it used
    /// to be. A detached subscription outlives the leaf and goes on speaking for
    /// the workspace that made it, so a tab moved into another window would
    /// still be reconnecting through the workspace it left — and its button
    /// would go dead the moment that window closed. Held here, it is dropped and
    /// remade with the leaf; see [`Workspace::adopt_tab`].
    ///
    /// `Option` for the reason [`Self::_observer`] is one: only a terminal has a
    /// connection to offer, and an editor pane has nothing to listen for.
    pub(super) _reconnect: Option<Subscription>,
}

/// One tab: a tree of panes, one of which is active.
pub(super) struct SessionTab {
    /// The panes of this tab. Never empty — the last pane closes the tab.
    pub(super) panes: PaneTree<PaneLeaf>,
    /// The pane the tab label, the status bar and the shortcuts act on.
    pub(super) active_pane: PaneId,
    /// Every pane of this tab in the order the keyboard last visited them, the
    /// most recent last.
    ///
    /// What [`SessionTab::focus_successor`] reads, and the only reason it is
    /// kept: closing the pane you are working in should hand the keyboard back
    /// to the one you came from, not to whichever pane happens to sit next in
    /// layout order. On a tab split three ways those are routinely different
    /// panes, and the layout answer sends the user somewhere they have not
    /// looked at since the split was made.
    ///
    /// Holds ids rather than an index, so a pane closing elsewhere in the tree
    /// cannot silently rename an entry; ids are never reused, so a stale one
    /// reads as gone. Entries are pruned as panes go — see
    /// [`SessionTab::prune_focus_order`] — and reads tolerate a stale one
    /// anyway, because a pane can leave by a path that never came through here.
    pub(super) focus_order: Vec<PaneId>,
    /// Whether the file panel is showing beside this tab's panes.
    ///
    /// One flag per tab rather than one for the window, because what the panel
    /// browses is per tab already: it follows the active tab's session, so a
    /// window-wide switch meant that opening the panel for the host being
    /// configured also opened it, at the same width, over the tab that was only
    /// tailing a log. Where the flag starts is [`panel_opens_with`]; from then
    /// on it is the tab's own, and the toggle only ever moves the active one's.
    ///
    /// Session state, not persisted: the profile — or the setting, for a local
    /// shell — is what the next session is opened from, and a tab that outlived
    /// the choice is not worth a second place to write it down.
    pub(super) panel_open: bool,
    /// A name for the tab that outranks whatever its active pane is showing.
    ///
    /// `None` on every tab that was opened as a connection or grown by hand,
    /// and those are right to be named after their active pane: such a tab *is*
    /// whichever pane the user is looking at, and a split whose halves went to
    /// two different hosts would otherwise go on claiming to be the one it
    /// started as.
    ///
    /// A dashboard tab is the other kind of thing. It is a named arrangement
    /// the user made, opened as a whole and closed as a whole, and naming it
    /// after whichever of its panes last held focus would leave the strip
    /// saying `error.log - db-01` for a tab called *Deploy watch* — a label
    /// that changes as the keyboard moves, for a tab that did not.
    pub(super) label: Option<SharedString>,
    /// The dashboard this tab was opened from, if it is a dashboard tab.
    ///
    /// The write-target for *Save layout to dashboard*: a tab that carries an
    /// id is one whose current arrangement can be captured back onto the stored
    /// [`Dashboard`], and one that does not — a connection or a hand-grown tab —
    /// has no dashboard to save to. `None` on every tab but the ones
    /// [`Workspace::open_dashboard`] opens, which is why it rides alongside
    /// [`Self::label`] and is set the same way.
    pub(super) dashboard: Option<Uuid>,
}

impl SessionTab {
    /// A tab of a single pane showing `leaf`, with the file panel showing.
    ///
    /// The panel is what every tab used to open with, so it is what a tab whose
    /// caller has nothing better to say still opens with; the callers that do
    /// have something to say follow this with [`SessionTab::with_panel`].
    pub(super) fn single(leaf: PaneLeaf) -> Self {
        let panes = PaneTree::single(leaf);
        let active_pane = panes.first_leaf().0;
        Self {
            panes,
            active_pane,
            focus_order: vec![active_pane],
            panel_open: true,
            label: None,
            dashboard: None,
        }
    }

    /// The same tab, opening with the file panel showing or not.
    pub(super) fn with_panel(mut self, open: bool) -> Self {
        self.panel_open = open;
        self
    }

    /// The same tab, carrying a name of its own. See [`SessionTab::label`].
    pub(super) fn with_label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The same tab, remembering the dashboard it was opened from. See
    /// [`SessionTab::dashboard`].
    pub(super) fn with_dashboard(mut self, id: Uuid) -> Self {
        self.dashboard = Some(id);
        self
    }

    /// The active pane, falling back to the first one.
    ///
    /// The fallback only matters if [`SessionTab::active_pane`] ever went stale;
    /// a tab always has a pane to speak for it, so this never fails.
    pub(super) fn active_pane(&self) -> PaneId {
        if self.panes.contains(self.active_pane) {
            self.active_pane
        } else {
            self.panes.first_leaf().0
        }
    }

    /// Marks `pane` as the active one and as the most recently focused.
    ///
    /// Every path that moves the active-pane marker goes through here, so that
    /// the order below is a record of where the keyboard has actually been
    /// rather than of the subset of moves someone remembered to log. The pane
    /// is lifted out of the order before being pushed, so an id appears once
    /// and revisiting a pane moves it to the front rather than stacking up.
    pub(super) fn focus(&mut self, pane: PaneId) {
        self.active_pane = pane;
        self.focus_order.retain(|id| *id != pane);
        self.focus_order.push(pane);
    }

    /// The pane to hand the keyboard to once `closing` has gone: the most
    /// recently focused pane that is still standing.
    ///
    /// `None` on a tab whose order has nothing else live in it — a pane that
    /// was never focused closing beside one that never was either — and the
    /// caller falls back to layout order, which is what this replaced and is
    /// still the right answer when there is no history to go on.
    ///
    /// `closing` is skipped explicitly rather than relied on to be gone: this is
    /// asked *before* the removal, while the pane is still in the tree, because
    /// the order it is read from is about to be pruned.
    pub(super) fn focus_successor(&self, closing: PaneId) -> Option<PaneId> {
        self.focus_order
            .iter()
            .rev()
            .copied()
            .find(|id| *id != closing && self.panes.contains(*id))
    }

    /// Drops from the focus order every pane the tree no longer holds.
    ///
    /// Called after a removal. Without it the order grows for the life of the
    /// tab and a long-dead pane could be picked as a successor — `contains`
    /// guards the read as well, so this is about the list not growing without
    /// bound rather than about correctness.
    pub(super) fn prune_focus_order(&mut self) {
        let Self {
            panes, focus_order, ..
        } = self;
        focus_order.retain(|id| panes.contains(*id));
    }

    /// The view of the active pane.
    pub(super) fn active_view(&self) -> &PaneView {
        let pane = self.active_pane();
        match self.panes.get(pane) {
            Some(leaf) => &leaf.view,
            None => &self.panes.first_leaf().1.view,
        }
    }

    /// The session this tab speaks for: the active pane's, or the first one it
    /// has if the active pane is an editor.
    ///
    /// The fallback is what keeps the tab label and the status bar describing a
    /// *session* while the keyboard happens to be in a file of a split tab. A
    /// tab with no terminal in it at all — a file opened into a tab of its own,
    /// or one outliving the shell it came from — has no session, and every
    /// caller says so in its own words rather than inventing one. The file panel
    /// is the exception, and asks [`SessionTab::panel_session`] instead.
    pub(super) fn active_session(&self, cx: &App) -> Option<Entity<Session>> {
        self.active_view().session(cx).or_else(|| {
            self.panes
                .leaves()
                .into_iter()
                .find_map(|(_, leaf)| leaf.view.session(cx))
        })
    }

    /// The session the file panel browses while this tab is active.
    ///
    /// [`SessionTab::active_session`] first, so a tab that has a terminal in it
    /// browses exactly what it always did. Only a tab that has none — an open
    /// file in a tab of its own, which is what "Edit" now makes — falls through
    /// to the session that file was *read from*, which is the one filesystem the
    /// panel could usefully be showing beside it.
    ///
    /// Kept apart from `active_session` rather than folded into it because the
    /// two are asked by different callers for different reasons: the tab label,
    /// the status dot and the tab menu's connection rows all read that one, and
    /// answering them with an editor's origin would dress a tab of files up as a
    /// live connection — offering to reconnect a session the tab is not showing.
    pub(super) fn panel_session(&self, cx: &App) -> Option<Entity<Session>> {
        self.active_session(cx).or_else(|| {
            self.active_view().editor_session(cx).or_else(|| {
                self.panes
                    .leaves()
                    .into_iter()
                    .find_map(|(_, leaf)| leaf.view.editor_session(cx))
            })
        })
    }

    /// The pane a close aimed at this whole tab has to ask about first.
    ///
    /// Only ever the tab's *only* pane; see [`tab_close_asks`] for why a split
    /// tab is not covered.
    pub(super) fn unsaved_lone_editor(&self, cx: &App) -> Option<PaneId> {
        let (id, leaf) = self.panes.first_leaf();
        let unsaved = matches!(
            &leaf.view,
            PaneView::Editor(editor) if editor.read(cx).is_dirty(cx)
        );
        tab_close_asks(self.panes.leaf_count(), unsaved).then_some(id)
    }

    /// Whether closing this tab outright would lose edits nobody was asked
    /// about.
    pub(super) fn holds_unsaved_work(&self, cx: &App) -> bool {
        self.panes.leaves().into_iter().any(|(_, leaf)| {
            matches!(
                &leaf.view,
                PaneView::Editor(editor) if editor.read(cx).is_dirty(cx)
            )
        })
    }

    /// Every session in this tab, one per terminal pane.
    pub(super) fn sessions(&self, cx: &App) -> Vec<Entity<Session>> {
        self.panes
            .leaves()
            .into_iter()
            .filter_map(|(_, leaf)| leaf.view.session(cx))
            .collect()
    }

    /// Every open file in this tab, one per editor pane.
    ///
    /// The counterpart of [`Self::sessions`] for the other kind of leaf, and
    /// here for the same reason: a setting that changes has to reach the panes
    /// of the background tabs too, and a leaf is the only place a pane is
    /// reachable from.
    pub(super) fn editors(&self) -> Vec<Entity<EditorPane>> {
        self.panes
            .leaves()
            .into_iter()
            .filter_map(|(_, leaf)| match &leaf.view {
                PaneView::Editor(pane) => Some(pane.clone()),
                PaneView::Terminal(_) | PaneView::Tail(_) => None,
            })
            .collect()
    }

    /// Every followed file in this tab, one per tail pane.
    ///
    /// The third of the same family, and here for the same reason as
    /// [`Self::editors`]: a highlight rule that changes has to reach the tails
    /// of the background tabs too, and only a leaf knows where a pane is.
    /// Separate from [`Self::sessions`] because the rules are held by the
    /// *pane* — a tail session answers which rules apply, and the pane is what
    /// compiles them and hands them to its grid.
    pub(super) fn tails(&self) -> Vec<Entity<TailView>> {
        self.panes
            .leaves()
            .into_iter()
            .filter_map(|(_, leaf)| match &leaf.view {
                PaneView::Tail(pane) => Some(pane.clone()),
                PaneView::Terminal(_) | PaneView::Editor(_) => None,
            })
            .collect()
    }

    /// The pane rendering `view`, if any.
    ///
    /// Panes are found by view rather than by id because a focus event only
    /// says which surface was focused, and a pane keeps its view across merges
    /// and break-outs.
    pub(super) fn pane_of(&self, view: EntityId) -> Option<PaneId> {
        self.panes
            .leaves()
            .into_iter()
            .find(|(_, leaf)| leaf.view.entity_id() == view)
            .map(|(id, _)| id)
    }
}
