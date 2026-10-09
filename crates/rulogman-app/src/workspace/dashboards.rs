//! Dashboards.

use super::*;

impl Workspace {
    /// Arranges `leaves` into a tab following the saved geometry `layout`,
    /// divider positions and all, rather than the fresh grid
    /// [`Workspace::compose_dashboard_tab`] lays down.
    ///
    /// `leaves` are in [`Dashboard::panes`] order and a [`LayoutNode::Leaf`]
    /// names its pane by index into that same order, so leaf `pane` is
    /// `leaves[pane]`. The caller only reaches here with a `layout` that
    /// [`Dashboard::valid_layout`] has already confirmed is a permutation of
    /// `0..leaves.len()`, which is what lets every leaf be placed exactly once
    /// and read back below without a missing or repeated index.
    ///
    /// A geometry that turns out not to match after all — which should be
    /// impossible past `valid_layout` — is not worth a broken tab: it is logged
    /// and the grid takes over, so a bug here degrades to the arrangement the
    /// user would have got before layouts existed.
    ///
    /// Windowless-testable like the other composers: it builds the tree and
    /// nothing that needs a window.
    pub(super) fn compose_dashboard_layout(
        leaves: Vec<PaneLeaf>,
        layout: &LayoutNode,
        panel_open: bool,
    ) -> SessionTab {
        /// The index of the leftmost pane of `node` — the one that ends up
        /// top-left of the space `node` fills, and the leaf every enclosing
        /// split shares as its own first child's head.
        fn head(node: &LayoutNode) -> usize {
            let mut node = node;
            loop {
                match node {
                    LayoutNode::Leaf { pane } => return *pane,
                    LayoutNode::Split { first, .. } => node = first,
                }
            }
        }

        /// Whether the leaves of `node` are exactly `0..count`, each once. The
        /// same permutation [`Dashboard::valid_layout`] enforces, re-checked
        /// here against the leaves actually handed over so a build never indexes
        /// out of range or drops a pane on a caller that skipped the check.
        fn covers(node: &LayoutNode, count: usize) -> bool {
            fn walk(node: &LayoutNode, seen: &mut [bool], placed: &mut usize) -> bool {
                match node {
                    LayoutNode::Leaf { pane } => match seen.get_mut(*pane) {
                        Some(slot) if !*slot => {
                            *slot = true;
                            *placed += 1;
                            true
                        }
                        _ => false,
                    },
                    LayoutNode::Split { first, second, .. } => {
                        walk(first, seen, placed) && walk(second, seen, placed)
                    }
                }
            }
            let mut seen = vec![false; count];
            let mut placed = 0;
            walk(node, &mut seen, &mut placed) && placed == count
        }

        /// Grows the placeholder leaf `anchor` into the arrangement `node`.
        ///
        /// [`PaneTree`] can only ever attach an incoming subtree as the *second*
        /// child of a split whose first child is a single existing leaf, so an
        /// arbitrary tree is built by expanding in place: the split is made
        /// while its first child is still the lone `anchor`, then each child is
        /// grown into the leaf it now sits on. The invariant that makes this
        /// consume every pane exactly once is that `anchor` already holds the
        /// leaf `head(node)` on entry — seeded once at the root, and re-seeded
        /// for each split's second child from the pane its right subtree leads
        /// with.
        fn expand(
            panes: &mut PaneTree<PaneLeaf>,
            anchor: PaneId,
            node: &LayoutNode,
            slots: &mut [Option<PaneLeaf>],
        ) -> bool {
            let LayoutNode::Split {
                axis,
                first,
                second,
                ..
            } = node
            else {
                // A leaf: `anchor` was seeded with this pane already, so the
                // arrangement here is complete.
                return true;
            };
            let Some(second_leaf) = slots.get_mut(head(second)).and_then(Option::take) else {
                log::error!("a dashboard layout named a pane out of range or twice");
                return false;
            };
            let axis = layout_axis(*axis);
            let Some(new_id) = panes.split(anchor, axis, second_leaf) else {
                log::error!("the pane to grow a dashboard layout onto has vanished");
                return false;
            };
            // The first child stays on `anchor`, which still holds `head(first)`
            // — the same pane as `head(node)`; the second grows onto the leaf
            // just seeded with `head(second)`.
            expand(panes, anchor, first, slots) && expand(panes, new_id, second, slots)
        }

        /// Pairs each split of `spec` with the live split that was built from it
        /// and records the ratio to restore. The two trees have the same shape
        /// by construction, so the walk stays in lockstep; a divergence that
        /// should be impossible is logged and abandons the ratios rather than
        /// guessing.
        fn ratios(
            spec: &LayoutNode,
            live: &PaneNode<PaneLeaf>,
            out: &mut Vec<(SplitId, f32)>,
        ) -> bool {
            match (spec, live) {
                (LayoutNode::Leaf { .. }, PaneNode::Leaf { .. }) => true,
                (
                    LayoutNode::Split {
                        ratio,
                        first,
                        second,
                        ..
                    },
                    PaneNode::Split {
                        id,
                        first: live_first,
                        second: live_second,
                        ..
                    },
                ) => {
                    out.push((*id, *ratio));
                    ratios(first, live_first, out) && ratios(second, live_second, out)
                }
                _ => {
                    log::error!("a dashboard layout diverged from the tree it built");
                    false
                }
            }
        }

        let count = leaves.len();
        if !covers(layout, count) {
            log::error!(
                "a dashboard layout does not match its panes; falling back to a grid of {count}"
            );
            return Self::compose_dashboard_tab(leaves, panel_open);
        }

        // Consumed by index, since a leaf names its pane by position and the
        // order is not the tree's own.
        let mut slots: Vec<Option<PaneLeaf>> = leaves.into_iter().map(Some).collect();
        // Seed the root with its leftmost pane; `expand` re-seeds each split's
        // second child in turn, so every pane is placed exactly once.
        let Some(root_leaf) = slots.get_mut(head(layout)).and_then(Option::take) else {
            // `covers` just proved the index is in range, so this cannot happen.
            log::error!("a dashboard layout lost its first pane between checks");
            // Nothing left to fall back with — the leaves are half-taken — so
            // rebuild the survivors into a grid rather than panic.
            let survivors: Vec<PaneLeaf> = slots.into_iter().flatten().collect();
            return Self::compose_dashboard_tab(survivors, panel_open);
        };
        let mut tab = SessionTab::single(root_leaf).with_panel(panel_open);
        let anchor = tab.panes.first_leaf().0;
        if !expand(&mut tab.panes, anchor, layout, &mut slots) {
            // Half the leaves are already in the tree, so the grid is no longer
            // an option; the tab keeps the shape built so far, which is still a
            // usable arrangement of the panes that made it in.
            log::error!("a dashboard layout could not be fully built; showing what was arranged");
            return tab;
        }

        // A second pass, because `split` mints its dividers at an even ratio and
        // the ids to move them are only knowable once the tree exists.
        let mut wanted = Vec::new();
        if ratios(layout, tab.panes.root(), &mut wanted) {
            for (id, ratio) in wanted {
                tab.panes.set_ratio(id, ratio);
            }
        }
        tab
    }

    /// [`panel_opens_with`] asked against the settings this run is on.
    ///
    /// The one place the global is read for this, so that the three local
    /// openers and the remote one all reach the same setting the same way.
    pub(super) fn panel_opens_for(profile: Option<&SessionProfile>, cx: &App) -> bool {
        panel_opens_with(profile, &app_settings::current(cx).files)
    }

    /// Follows `path` on `profile` with `auth`, in a tab right after the active
    /// one.
    ///
    /// The tab lands beside the tab it was asked for from, exactly as an opened
    /// file does and for the same reason — see [`Workspace::open_editor`],
    /// whose insertion this mirrors — and it opens with the file panel shut
    /// whatever the profile says about panels: there is no shell on the other
    /// end to browse a filesystem beside, and [`Session::files`] answers
    /// nothing for such a session anyway.
    ///
    /// The forwardings are suppressed unconditionally. A profile's local ports
    /// belong to one session at a time, and the one that should hold them is
    /// the shell the user works in — not a pane that opened to read a log and
    /// would take them from it, or fail to bind them and say so in yellow over
    /// the first screen of the file.
    pub(super) fn open_tail_session(
        &mut self,
        profile: SessionProfile,
        auth: SshAuth,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::info!("following {path} on {}", profile.label());
        let caps = Self::pane_caps_source(cx);
        let session = cx.new(|cx| Session::new_tail(profile, auth, path.clone(), true, cx));
        let terminal = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
        // Alone in its tab, with nothing to be told apart from.
        let view = cx
            .new(|cx| TailView::new(terminal, session.clone(), path, SharedString::default(), cx));
        let leaf = self.new_tail_pane(view, session, window, cx);

        let at = if self.tabs.is_empty() {
            0
        } else {
            self.active + 1
        };
        self.tabs
            .insert(at, SessionTab::single(leaf).with_panel(false));
        self.active = at;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Follows `path` on the connection `profile_id` in a new pane *below* the
    /// active pane of the tab at `tab_index`.
    ///
    /// [`Workspace::open_tail`] opens a followed file in a tab of its own; this
    /// opens one in a tab that already exists, which is the difference between
    /// looking at a log and building an arrangement of them. It is what makes a
    /// dashboard tab something the user can compose by hand: add a pane, drag
    /// the divider, add another — and then *Save layout to dashboard* writes
    /// exactly what is on screen back to the store. Nothing else in the
    /// application can grow a dashboard tab a pane, so without this the only way
    /// to change what a dashboard holds is the settings dialog's list.
    ///
    /// Below rather than beside, because a log is a wide thing: two half-width
    /// panes each wrap their lines twice, while two half-height ones each show
    /// half as many whole lines. Vertical is the axis the pane count can grow
    /// along without the content becoming unreadable, which is also why the
    /// default grid [`Workspace::compose_dashboard_tab`] lays down stacks rows.
    ///
    /// The focus is left exactly where it was. The user is adding a pane to
    /// something they are reading, not switching to it, and a followed file has
    /// no input to take anyway.
    ///
    /// A connection with nothing saved gets its form put up and nothing else,
    /// the same one-more-click answer [`Workspace::open_dashboard`] gives and
    /// for the same reason: the dialog can only say *connect*, so it cannot
    /// come back to a pane it was never told about. Unlike
    /// [`Workspace::open_tail`], no request is parked in
    /// [`Workspace::pending_tail`] — that field opens a *tab*, and resuming
    /// through it would put the file somewhere other than the tab that was
    /// asked about, which is worse than not resuming at all.
    pub(super) fn add_tail_to_tab(
        &mut self,
        tab_index: usize,
        profile_id: Uuid,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Also clears any parked `pending_tail`, which this call deliberately
        // does not set again; see the doc comment.
        self.close_overlays(cx);

        let Some(profile) = self.profile(profile_id, cx) else {
            log::warn!("the connection {path} would be followed over no longer exists");
            return;
        };
        let Some(auth) = connection::saved_credentials(&profile) else {
            log::info!(
                "{} is waiting on saved credentials before {path} can be added",
                profile.name
            );
            self.dialog
                .update(cx, |dialog, cx| dialog.open_profile(profile_id, cx));
            cx.notify();
            return;
        };
        let Some(target) = self.tabs.get(tab_index).map(SessionTab::active_pane) else {
            // The menu and the tab it speaks for are a frame apart, so the tab
            // can have closed since the row was drawn.
            return;
        };
        log::info!("adding {path} on {} to a tab", profile.label());

        // Built in the order [`Workspace::open_dashboard`] builds a pane in,
        // and with the same two decisions: the connection's name rides along,
        // because a tab grown this way may well mix hosts and two `access.log`s
        // have to be tellable apart; and the profile's forwardings are
        // suppressed, because a pane that only reads a log must not take a
        // profile's local ports from the shell the user works in.
        let connection = SharedString::from(profile.name.clone());
        let caps = Self::pane_caps_source(cx);
        let session = cx.new(|cx| Session::new_tail(profile, auth, path.clone(), true, cx));
        let terminal = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
        let view = cx.new(|cx| TailView::new(terminal, session.clone(), path, connection, cx));
        let leaf = self.new_tail_pane(view, session, window, cx);

        let tab = &mut self.tabs[tab_index];
        if tab.panes.split(target, Axis::Vertical, leaf).is_none() {
            // `target` came out of this very tab a moment ago, so this is
            // unreachable; logged rather than ignored because reaching it would
            // mean a live session has been dropped on the floor.
            log::error!("the pane to split has vanished; the followed file was dropped");
            return;
        }
        cx.notify();
    }

    /// Re-reads the saved dashboards from disk.
    ///
    /// A failure keeps the list the window already has, exactly as
    /// `ConnectionDialog::reload_store` keeps the profiles it already has: the
    /// alternative is emptying the welcome screen because a write was
    /// interrupted, which loses the user a click and tells them nothing.
    pub(super) fn reload_dashboards(&mut self) {
        match DashboardStore::load() {
            Ok(store) => self.dashboards = store,
            Err(err) => log::warn!("keeping the dashboards already loaded: {err:#}"),
        }
    }

    /// Opens the dashboard `id`: every file it names, each followed over the
    /// connection that reaches it, in one tab arranged as a grid.
    ///
    /// This is [`Workspace::open_tail`] several times over, and it makes the
    /// same two decisions that call makes — resolve, then check the
    /// credentials — but it has to make them for the whole set before it opens
    /// anything, because what it opens is a single tab.
    ///
    /// A pane whose profile has been deleted is skipped rather than fatal: a
    /// dangling reference is a state the store keeps on purpose (see
    /// [`rulogman_core::dashboard`]), and the four logs that *can* be opened
    /// are worth more than a refusal naming the fifth. A dashboard with nothing
    /// left to open is the one case that opens no tab.
    ///
    /// # Why the credentials are all or nothing
    ///
    /// A profile with nothing saved needs the connection form, and the form can
    /// only answer for one connection: a dashboard spanning three such hosts
    /// would be three dialogs in a row, each one having to be remembered
    /// against a tab that does not exist yet. So this version does not open a
    /// partial tab and does not queue anything. It says which connections are
    /// unsaved, opens the form pre-filled on the first of them — the fix is one
    /// *Save* away — and leaves the dashboard to be clicked again. A second
    /// click is a smaller price than a queue of dialogs, and unlike the queue
    /// it is a thing the user can see the shape of.
    pub(super) fn open_dashboard(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.close_overlays(cx);

        let Some(dashboard) = self.dashboards.get(id).cloned() else {
            log::warn!("the dashboard that was asked for is no longer in the store");
            return;
        };
        if dashboard.panes.is_empty() {
            log::info!("dashboard {} names no files to follow", dashboard.name);
            return;
        }
        log::info!(
            "opening dashboard {} over {} file(s)",
            dashboard.name,
            dashboard.panes.len()
        );

        let mut resolved: Vec<(SessionProfile, String)> = Vec::with_capacity(dashboard.panes.len());
        for pane in &dashboard.panes {
            match self.profile(pane.profile, cx) {
                Some(profile) => resolved.push((profile, pane.path.clone())),
                None => log::warn!(
                    "dashboard {} follows {} over a connection that no longer exists; the pane is skipped",
                    dashboard.name,
                    pane.path
                ),
            }
        }
        if resolved.is_empty() {
            log::warn!(
                "dashboard {} has no file left whose connection still exists",
                dashboard.name
            );
            return;
        }

        // Once per distinct connection rather than once per pane: reading the
        // keychain, and possibly a key file, is what this asks, and two panes
        // on one host are asking it the same question.
        let mut credentials: Vec<(Uuid, SshAuth)> = Vec::new();
        let mut missing: Vec<(Uuid, String)> = Vec::new();
        for (profile, _) in &resolved {
            if credentials.iter().any(|(id, _)| *id == profile.id)
                || missing.iter().any(|(id, _)| *id == profile.id)
            {
                continue;
            }
            match connection::saved_credentials(profile) {
                Some(auth) => credentials.push((profile.id, auth)),
                None => missing.push((profile.id, profile.name.clone())),
            }
        }
        if let Some(first) = missing.first().map(|(id, _)| *id) {
            let names: Vec<&str> = missing.iter().map(|(_, name)| name.as_str()).collect();
            log::info!(
                "dashboard {} is waiting on saved credentials for {}",
                dashboard.name,
                names.join(", ")
            );
            self.dialog
                .update(cx, |dialog, cx| dialog.open_profile(first, cx));
            cx.notify();
            return;
        }

        let caps = Self::pane_caps_source(cx);
        let mut leaves = Vec::with_capacity(resolved.len());
        for (profile, path) in resolved {
            let Some(auth) = credentials
                .iter()
                .find(|(id, _)| *id == profile.id)
                .map(|(_, auth)| auth.clone())
            else {
                // Unreachable: the sweep above filed every distinct profile
                // under one list or the other, and a non-empty `missing` has
                // already returned.
                log::error!("a dashboard pane lost the credentials it was just checked for");
                continue;
            };
            // The connection's name, for the pane's own header: it is what
            // tells two hosts' `access.log`s apart, and this is the only place
            // that still has the profile to read it from.
            let connection = SharedString::from(profile.name.clone());
            // Tunnels suppressed on every one of them, for the reason
            // [`Workspace::open_tail_session`] suppresses them: a pane that
            // only reads a log must not take a profile's local ports from the
            // shell the user works in.
            let session = cx.new(|cx| Session::new_tail(profile, auth, path.clone(), true, cx));
            let terminal =
                cx.new(|cx| TerminalView::new(session.clone(), caps.clone(), window, cx));
            let view = cx.new(|cx| TailView::new(terminal, session.clone(), path, connection, cx));
            leaves.push(self.new_tail_pane(view, session, window, cx));
        }

        if leaves.is_empty() {
            // Only reachable through the `else` arm above, which is itself
            // unreachable; the guard is here because the alternative is
            // composing a tab out of no panes, which panics.
            log::error!("dashboard {} built no panes to open", dashboard.name);
            return;
        }

        // No panel, for the reason a single followed file opens without one:
        // there is no shell on the other end of any of these panes to browse a
        // filesystem beside.
        //
        // The saved geometry is honoured only when every pane made it in: a
        // leaf names its pane by position in `dashboard.panes`, and skipping a
        // pane whose profile is gone would shift those positions out from under
        // the layout. A short set falls back to the grid, which needs no such
        // correspondence; `valid_layout` guards the rest.
        let tab = match dashboard.valid_layout() {
            Some(layout) if leaves.len() == dashboard.panes.len() => {
                Self::compose_dashboard_layout(leaves, layout, false)
            }
            _ => Self::compose_dashboard_tab(leaves, false),
        }
        .with_label(dashboard.name)
        .with_dashboard(id);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Reads the arrangement of `tab` back into the pair a dashboard is stored
    /// as: the panes it shows, in depth-first layout order, and the geometry
    /// tree laid over them.
    ///
    /// The running pane index and the panes vector are grown together in one
    /// depth-first walk, so a [`LayoutNode::Leaf`] and its [`DashboardPane`]
    /// always agree on which pane they mean without a second lookup. Every leaf
    /// must be a followed file — a session that answers both a profile and a
    /// tail path — because a dashboard is nothing but followed files; a pane
    /// that is anything else (a shell the user split in, an opened editor)
    /// aborts the whole capture with `None`, since saving a partial set would
    /// silently drop it.
    ///
    /// Free of any window, so it is testable the way the composers are.
    pub(super) fn capture_tab_layout(
        tab: &SessionTab,
        cx: &App,
    ) -> Option<(Vec<DashboardPane>, LayoutNode)> {
        fn walk(
            node: &PaneNode<PaneLeaf>,
            panes: &mut Vec<DashboardPane>,
            cx: &App,
        ) -> Option<LayoutNode> {
            match node {
                PaneNode::Leaf { payload, .. } => {
                    let session = payload.view.session(cx)?;
                    let session = session.read(cx);
                    // Both or neither: a followed file answers a profile and a
                    // path, and anything missing one is not a pane a dashboard
                    // can name.
                    let profile = session.profile_id()?;
                    let path = session.tail_path()?.to_owned();
                    let pane = panes.len();
                    panes.push(DashboardPane { profile, path });
                    Some(LayoutNode::Leaf { pane })
                }
                PaneNode::Split {
                    axis,
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    // First then second, the same order the panes vector is
                    // grown in, so leaf indices stay in step with it.
                    let first = walk(first, panes, cx)?;
                    let second = walk(second, panes, cx)?;
                    Some(LayoutNode::Split {
                        axis: layout_axis_of(*axis),
                        ratio: *ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    })
                }
            }
        }

        let mut panes = Vec::new();
        let layout = walk(tab.panes.root(), &mut panes, cx)?;
        Some((panes, layout))
    }

    /// Captures the arrangement of the tab at `index` onto the dashboard it was
    /// opened from, replacing that dashboard's panes and geometry with what is
    /// on screen now.
    ///
    /// A no-op on a tab that is not a dashboard: there is nowhere to write the
    /// arrangement, so the command simply says so and stops.
    ///
    /// This deliberately captures the *current* pane set, not only the dividers:
    /// a pane the user closed since opening the dashboard is gone from the save,
    /// and one they split in that is not a followed file makes the capture
    /// refuse rather than drop it. Saving the layout is thus also how the user
    /// prunes or reshuffles a dashboard from the tab itself.
    ///
    /// There is no toast surface to report through, so success and every
    /// failure are logged; the menu entry that invokes this is only offered on a
    /// dashboard tab, which is the one confirmation the user does see.
    pub(super) fn save_tab_layout(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(id) = tab.dashboard else {
            log::info!("the tab whose layout was asked for is not a dashboard; nothing to save");
            return;
        };
        let Some((panes, layout)) = Self::capture_tab_layout(tab, cx) else {
            log::warn!(
                "dashboard {id} has a pane that is not a followed file; its layout was not saved"
            );
            return;
        };
        let name = tab
            .label
            .as_ref()
            .map(|label| label.to_string())
            .unwrap_or_default();

        // The stored entry is the one to update; it must still be there, but a
        // fresh one keeps the id if it somehow is not, rather than losing the
        // capture.
        let mut dashboard = self.dashboards.get(id).cloned().unwrap_or_else(|| {
            log::error!("dashboard {id} vanished from the store before its layout could be saved");
            let mut fresh = Dashboard::new(name);
            fresh.id = id;
            fresh
        });
        dashboard.panes = panes;
        dashboard.layout = Some(layout);
        self.dashboards.upsert(dashboard);
        if let Err(err) = self.dashboards.save() {
            log::error!("could not write dashboards.json after capturing a layout: {err:#}");
            return;
        }
        log::info!("saved the current arrangement to dashboard {id}");
    }

    /// Shows the connection dialog with the saved profile `id` loaded into the
    /// form, ready to be changed.
    ///
    /// The sibling of [`Workspace::open_profile`], for the other thing a saved
    /// profile can be asked for: that one is on its way to a session and only
    /// shows the form when something is missing, while this one is the form.
    pub(super) fn edit_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.close_overlays(cx);
        self.dialog
            .update(cx, |dialog, cx| dialog.edit_profile(id, cx));
        cx.notify();
    }
}
