//! Sessions.

use super::*;

impl Workspace {
    /// Every session the workspace holds, across all tabs and panes.
    pub(super) fn sessions(&self, cx: &App) -> Vec<Entity<Session>> {
        self.tabs.iter().flat_map(|tab| tab.sessions(cx)).collect()
    }

    /// Every open file the workspace holds, across all tabs and panes.
    pub(super) fn editors(&self) -> Vec<Entity<EditorPane>> {
        self.tabs.iter().flat_map(SessionTab::editors).collect()
    }

    /// Every followed file the workspace holds, across all tabs and panes.
    pub(super) fn tails(&self) -> Vec<Entity<TailView>> {
        self.tabs.iter().flat_map(SessionTab::tails).collect()
    }

    /// Whether any session other than `except`, opened from profile `id`, is
    /// currently holding port forwardings open.
    ///
    /// Every pane of every tab, not only the active ones: the tab holding the
    /// ports is very often a background one, which is the whole reason the tab
    /// strip marks it.
    ///
    /// And every pane of every *window*, not only this one's. A port is bound
    /// once per machine, so the question was never really about a window; it
    /// only looked that way while there could be one. A tab carried into a
    /// second window takes its forwardings with it, and a session here that
    /// asked only its own window would be told the ports are free, ask the
    /// server for them, and print a bind failure over its fresh screen. `window`
    /// is this window, which answers for itself and is left out of the sweep —
    /// see [`other_workspace_windows`] for why it has to be.
    ///
    /// No liveness test to go with it, because [`Session::open_tunnels`] is
    /// already one — a session that has disconnected, failed or been closed has
    /// dropped the listeners with its transport and reports nothing here. A
    /// non-empty answer therefore means "live, and holding these ports this
    /// instant".
    pub(super) fn tunnels_held_elsewhere(
        &self,
        id: Uuid,
        except: Option<EntityId>,
        window: &Window,
        cx: &App,
    ) -> bool {
        tunnels_held_for(
            id,
            except,
            self.sessions(cx)
                .into_iter()
                .chain(sessions_in_other_windows(window, cx))
                .map(|entity| {
                    let session = entity.read(cx);
                    (
                        session.profile_id(),
                        entity.entity_id(),
                        !session.open_tunnels().is_empty(),
                    )
                }),
        )
    }

    /// Whether a session starting on `session`'s profile must leave that
    /// profile's forwardings alone.
    ///
    /// The two shapes the question comes in differ only in `except`: a
    /// duplicate passes `None`, since the session it was copied from is exactly
    /// the sibling to stay off, while a reconnect passes its own id. A local
    /// session came from no profile and has no forwardings either way.
    pub(super) fn tunnels_taken_from(
        &self,
        session: &Entity<Session>,
        except: Option<EntityId>,
        window: &Window,
        cx: &App,
    ) -> bool {
        session
            .read(cx)
            .profile_id()
            .is_some_and(|id| self.tunnels_held_elsewhere(id, except, window, cx))
    }

    /// Opens `session` again, after deciding whether it may take its profile's
    /// forwardings back.
    ///
    /// The one route to [`Session::reconnect`], because that decision has to be
    /// made against the sessions that are live *now*: a tab whose sibling has
    /// since gone picks the forwardings up, and one reconnecting while the
    /// sibling still holds them stays off them and prints no failure notice
    /// over its fresh screen. The sibling may be in another window by now, which
    /// is what `window` is here for — see
    /// [`Workspace::tunnels_held_elsewhere`].
    pub(super) fn reconnect_session(
        &mut self,
        session: &Entity<Session>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let suppressed = self.tunnels_taken_from(session, Some(session.entity_id()), window, cx);
        session.update(cx, |session, cx| {
            session.set_tunnels_suppressed(suppressed);
            session.reconnect(cx);
        });
    }

    /// Re-applies the current settings to the window and every open session.
    ///
    /// Shared by the two things that can make the settings mean something new:
    /// saving them, and changing a theme or scheme file the settings point at.
    /// Deliberately does *not* move the focus — where the focus belongs after
    /// this depends on whether the dialog closed, which only the caller knows.
    pub(super) fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = app_settings::current(cx);
        // Before the repaint below, so the next frame is already drawn in the
        // newly chosen language.
        i18n::apply(settings.language.as_deref());
        // The native macOS menu bar is built once and owned by the platform, so
        // unlike the in-app menu it does not follow a repaint; it has to be
        // handed over again.
        cx.set_menus(app_menus());
        apply_ui_theme(&settings.ui_theme, cx);
        // Ahead of the repaint, so the toolbar's next frame already knows
        // whether it has to stand in for a title bar; and ahead of the two
        // calls below, which leave the accent policy and the caption colors on
        // the window, so a caption that comes back here comes back already
        // themed.
        //
        // The field follows the call rather than the stored setting: everything
        // that branches on it is asking what the window carries, not what was
        // last saved.
        if settings.window.titlebar != self.titlebar {
            self.titlebar = settings.window.titlebar;
            let custom = self.titlebar == TitlebarStyle::Custom;
            window.set_titlebar_transparent(custom, custom.then_some(TRAFFIC_LIGHT_ORIGIN));
            // The Linux counterpart of the call above, which only the Windows
            // and macOS backends implement: swap the compositor's frame for
            // client-side decorations (or back) on the live window.
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            window.request_decorations(if custom {
                gpui::WindowDecorations::Client
            } else {
                gpui::WindowDecorations::Server
            });
        }
        cx.refresh_windows();
        // The two halves of a translucent window, in this order: the platform
        // surface is told to permit alpha, and the fills are told how much of
        // it to use.
        window.set_background_appearance(chrome::window_appearance(
            settings.window.background_blur,
            settings.window.background_opacity,
        ));
        app_settings::set_tint(&settings, cx);
        // After the background appearance, never before: on Windows that call
        // re-arms the accent policy that would otherwise repaint the caption
        // out from under us.
        apply_caption_theme(window, &theme(cx), cx);
        // Every pane of every tab, not just the visible one: a background tab's
        // terminal has to come back in the newly chosen scheme too.
        for session in self.sessions(cx) {
            session.update(cx, |session, cx| session.apply_settings(cx));
        }
        // And every open file, for the same reason: whether long lines are
        // broken is one answer for the whole window, and a file left in a
        // background tab has to come back wrapped the way the one on screen is.
        for editor in self.editors() {
            editor.update(cx, |editor, cx| editor.apply_settings(cx));
        }
        // And every followed file, which is the third kind of pane and the only
        // one carrying highlight rules. Nothing about the scheme is baked in
        // here — a rule naming a slot is resolved against the palette of the
        // frame it is painted on — so this is only ever about the rule list
        // itself having changed.
        for tail in self.tails() {
            tail.update(cx, |tail, cx| tail.refresh_highlights(cx));
        }
    }

    /// Opens a session for `profile` and makes its tab active.
    ///
    /// A profile that also names files to follow — [`SessionProfile::tails`] —
    /// gets more than the shell: see [`Workspace::open_session_with_tails`],
    /// which this defers to so that a profile with nothing to follow keeps
    /// taking the plain, single-pane route through [`Workspace::adopt_session`]
    /// unchanged.
    pub(super) fn open_session(
        &mut self,
        profile: SessionProfile,
        auth: SshAuth,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::info!("opening a session to {}", profile.label());
        // Asked before the session exists, because connecting starts inside the
        // constructor: a tab already holding this profile's ports means the new
        // one must not ask for them.
        let suppressed = self.tunnels_held_elsewhere(profile.id, None, window, cx);
        let panel_open = Self::panel_opens_for(Some(&profile), cx);
        if profile.tails.is_empty() {
            let session = cx.new(|cx| Session::new(profile, auth, suppressed, cx));
            self.adopt_session(session, panel_open, window, cx);
            return;
        }
        self.open_session_with_tails(profile, auth, suppressed, panel_open, window, cx);
    }

    /// [`Workspace::open_session`] for a profile that also names files to
    /// follow: one tab holding the shell *and* one tail pane per rule,
    /// stacked below it in the rules' own order, rather than the tails each
    /// getting a tab of their own the way [`Workspace::open_tail`] opens one
    /// on request.
    ///
    /// Builds every pane itself rather than delegating to
    /// [`Workspace::adopt_session`], because that call is shaped for exactly
    /// one pane and is left alone for the plain sessions — remote and local —
    /// that still want it untouched. The actual arrangement of the panes is
    /// [`Workspace::compose_tailed_tab`], kept separate so it can be tested
    /// without a transport.
    ///
    /// Tunnels are suppressed unconditionally on every tail session, exactly
    /// as [`Workspace::open_tail_session`] suppresses them: a profile's local
    /// ports belong to the one session the user is typing into, not to a pane
    /// that only reads a log alongside it.
    pub(super) fn open_session_with_tails(
        &mut self,
        profile: SessionProfile,
        auth: SshAuth,
        suppressed: bool,
        panel_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let caps = Self::pane_caps_source(cx);
        let session = cx.new(|cx| Session::new(profile.clone(), auth.clone(), suppressed, cx));
        let view = cx.new(|cx| TerminalView::new(session.clone(), caps.clone(), window, cx));
        let shell_leaf = self.new_pane(view, session, window, cx);

        let mut tail_leaves = Vec::with_capacity(profile.tails.len());
        for rule in &profile.tails {
            let tail_session = cx.new(|cx| {
                Session::new_tail(profile.clone(), auth.clone(), rule.path.clone(), true, cx)
            });
            let terminal =
                cx.new(|cx| TerminalView::new(tail_session.clone(), caps.clone(), window, cx));
            let tail_view = cx.new(|cx| {
                TailView::new(
                    terminal,
                    tail_session.clone(),
                    rule.path.clone(),
                    // Every pane of this tab is on the one host the shell
                    // above them is on, so the name would be the same answer
                    // repeated: the strip carries the path alone.
                    SharedString::default(),
                    cx,
                )
            });
            tail_leaves.push(self.new_tail_pane(tail_view, tail_session, window, cx));
        }

        let tab = Self::compose_tailed_tab(shell_leaf, tail_leaves, panel_open);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Arranges a shell pane and its tail panes into one tab: the shell on
    /// top, the tails stacked below it in the order they are given, all rows
    /// the same height.
    ///
    /// Split out of [`Workspace::open_session_with_tails`] so it can be
    /// exercised on leaves built from dormant sessions — see
    /// `workspace_tests` — rather than only against a real connection: what
    /// this does is arrange leaves that already exist, not decide what they
    /// hold.
    ///
    /// Each split targets the pane the *previous* split returned — the
    /// shell's own id for the first tail — rather than always the shell, so
    /// each new tail lands below the one before it and the stack reads top to
    /// bottom in the rules' own order instead of growing upward from the
    /// shell in reverse. The shell pane is left both the active pane and the
    /// whole of [`SessionTab::focus_order`]: it is what the user asked to
    /// connect to, and a tail pane nobody has looked at yet has nothing to
    /// hand the keyboard back to if it were made a candidate.
    pub(super) fn compose_tailed_tab(
        shell_leaf: PaneLeaf,
        tail_leaves: Vec<PaneLeaf>,
        panel_open: bool,
    ) -> SessionTab {
        let mut tab = SessionTab::single(shell_leaf).with_panel(panel_open);
        let mut target = tab.panes.first_leaf().0;
        for leaf in tail_leaves {
            match tab.panes.split(target, Axis::Vertical, leaf) {
                Some(pane) => target = pane,
                None => {
                    // `target` is either the shell leaf `SessionTab::single`
                    // just built the tree around, or a pane id this very loop
                    // got back from `split` a moment ago, so this is
                    // unreachable; logged rather than ignored because
                    // reaching it would mean a live tail session has been
                    // dropped on the floor. Same stance as
                    // `duplicate_split`'s identical arm.
                    log::error!(
                        "the pane to split for a tail rule has vanished; the tail session was dropped"
                    );
                }
            }
        }
        tab.panes.equalize(Axis::Vertical);
        tab
    }

    /// Arranges `leaves` into one tab as a balanced grid, filled row by row in
    /// the order they are given.
    ///
    /// Balanced meaning as square as the count allows: `ceil(sqrt(n))` columns
    /// and as many rows as that needs, so eight panes land four-by-two rather
    /// than in a column eight high that gives each log three lines. The last
    /// row is the short one, which is what filling row-major leaves over.
    ///
    /// The tree is built rows first and cells second, and it has to be that
    /// way round: a split is aimed at *a pane*, so once a row has been divided
    /// into cells there is no longer any pane that stands for the whole row to
    /// aim the next row's split at. So the founding pane is split downward
    /// `rows - 1` times — each split aimed at the row above's leading pane, so
    /// the bands come out top to bottom rather than growing upward — and only
    /// then is each band divided rightward, each cell aimed at the one placed
    /// before it. Both passes therefore lay leaves down in exactly the order
    /// they arrived, which is what makes the grid readable as the list the
    /// dashboard was written as.
    ///
    /// Equalising along both axes afterwards is what makes it a grid rather
    /// than a nest of halves: [`PaneTree::equalize`] shares an area out by how
    /// many panes each side spans, so a chain of three stacked bands comes out
    /// in thirds instead of a half and two quarters.
    ///
    /// The first leaf keeps the active pane and the whole of
    /// [`SessionTab::focus_order`], as it does in
    /// [`Workspace::compose_tailed_tab`]: it is the top-left pane, which is
    /// where a reader starts, and no other pane has been looked at yet.
    ///
    /// Associated rather than a method, and taking leaves that already exist,
    /// so the arrangement can be exercised on dormant sessions without a
    /// transport — see `workspace_tests`.
    ///
    /// # Panics
    ///
    /// If `leaves` is empty. A tab has to have a pane, and the one caller
    /// returns before this on a dashboard that resolved to none.
    pub(super) fn compose_dashboard_tab(leaves: Vec<PaneLeaf>, panel_open: bool) -> SessionTab {
        let count = leaves.len();
        let cols = (count as f64).sqrt().ceil() as usize;

        // The rows, as leaves, before any of them is a pane: chunked by hand
        // rather than with `chunks`, which wants a slice of something `Clone`
        // and a `PaneLeaf` is neither.
        let mut bands: Vec<Vec<PaneLeaf>> = Vec::new();
        for leaf in leaves {
            match bands.last_mut() {
                Some(band) if band.len() < cols => band.push(leaf),
                _ => bands.push(vec![leaf]),
            }
        }

        let mut bands = bands.into_iter();
        let mut first_band = bands
            .next()
            .expect("a dashboard tab is composed from at least one pane");
        let mut tab = SessionTab::single(first_band.remove(0)).with_panel(panel_open);

        // Pass one: the bands. `heads` is the leading pane of each row and
        // `rests` what still has to go beside it, kept in step so that a split
        // that could not be made drops its whole row rather than silently
        // hanging its cells off the row above.
        let mut heads = vec![tab.panes.first_leaf().0];
        let mut rests = vec![first_band];
        for mut band in bands {
            let previous = heads[heads.len() - 1];
            match tab.panes.split(previous, Axis::Vertical, band.remove(0)) {
                Some(pane) => {
                    heads.push(pane);
                    rests.push(band);
                }
                // `previous` is either the pane `SessionTab::single` founded
                // the tree on or one this very loop was handed by `split`, so
                // this cannot happen; logged rather than ignored because
                // reaching it means a row of live tail sessions has been
                // dropped on the floor. Same stance as `compose_tailed_tab`.
                None => log::error!(
                    "the pane to split for a dashboard row has vanished; a row of tail sessions was dropped"
                ),
            }
        }

        // Pass two: the cells of each band, left to right.
        for (head, rest) in heads.into_iter().zip(rests) {
            let mut target = head;
            for leaf in rest {
                match tab.panes.split(target, Axis::Horizontal, leaf) {
                    Some(pane) => target = pane,
                    None => log::error!(
                        "the pane to split for a dashboard cell has vanished; the tail session was dropped"
                    ),
                }
            }
        }

        tab.panes.equalize(Axis::Vertical);
        tab.panes.equalize(Axis::Horizontal);
        tab
    }

    /// Opens a shell on this machine and makes its tab active.
    ///
    /// Takes nothing, because a local session is configured by nothing: the
    /// shell is the user's login shell and everything else comes from the
    /// global terminal settings.
    #[cfg(unix)]
    pub(super) fn open_local_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = cx.new(Session::new_local);
        log::info!(
            "opening a local session running {}",
            session.read(cx).label()
        );
        let panel_open = Self::panel_opens_for(None, cx);
        self.adopt_session(session, panel_open, window, cx);
    }

    /// Opens a shell running `command` on this machine and makes its tab
    /// active.
    ///
    /// The Windows counterpart of [`Workspace::open_local_session`], which
    /// takes nothing because unix has one local shell to start. Here there are
    /// several, so the caller — a button on the welcome screen — says which:
    /// `label` names it for the tab strip, `command` is the command line that
    /// starts it, and `filesystem` says whether the shell it starts stands on
    /// this machine's own filesystem or in a named WSL distribution's.
    #[cfg(windows)]
    pub(super) fn open_local_command(
        &mut self,
        label: SharedString,
        command: Vec<String>,
        filesystem: LocalFilesystem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::info!("opening a local session running {}", command.join(" "));
        let session = cx.new(|cx| Session::new_local_command(label, command, filesystem, cx));
        let panel_open = Self::panel_opens_for(None, cx);
        self.adopt_session(session, panel_open, window, cx);
    }

    /// Opens a tab running `command` on this machine in `cwd`, and makes it
    /// active.
    ///
    /// What `rulogman -e <command…>` asks for, which is what a desktop entry
    /// marked *Run in terminal* becomes once KDE has appended the flag — see
    /// [`launch::split_launch_args`]. Named apart from the Windows
    /// [`Workspace::open_local_command`] rather than sharing it: that one picks
    /// between the several shells this platform has and takes a `filesystem` to
    /// say which tree the shell stands in, and neither question exists here.
    ///
    /// The tab is labelled with the program's base name — `btop`, not
    /// `/usr/bin/btop --utf-force` — because a tab strip has room for a name
    /// and not for a command line, and because the program is what the user
    /// asked for. Whatever title the program sets replaces it, as for any other
    /// local session.
    #[cfg(unix)]
    pub(super) fn open_local_command_at(
        &mut self,
        command: Vec<String>,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The parser never produces one, but the type can hold one and a
        // program-less command line has nothing to start.
        let Some(program) = command.first() else {
            log::warn!("ignoring a launch command that names no program");
            return;
        };
        let label = SharedString::from(std::path::Path::new(program).file_name().map_or_else(
            || program.clone(),
            |name| name.to_string_lossy().into_owned(),
        ));

        log::info!("opening a local session running {}", command.join(" "));
        let session = cx.new(|cx| Session::new_local_command_at(label, command, cwd, cx));
        let panel_open = Self::panel_opens_for(None, cx);
        self.adopt_session(session, panel_open, window, cx);
    }

    /// Opens a shell on this machine standing in `dir`, and makes its tab
    /// active.
    ///
    /// The launch path: a directory named on the command line, or one a file
    /// manager's *Open with* handed over. It is deliberately not the same call
    /// as [`Workspace::open_local_session`] with an argument, because the two
    /// platforms disagree about what is missing. On unix nothing is: there is
    /// one login shell and the directory is all the caller had to add. On
    /// Windows there is no single local shell, and a path says nothing about
    /// which one was meant, so this picks the first of the shells this machine
    /// can start — PowerShell, standing on this machine's own filesystem, which
    /// is the only kind of filesystem a path from Explorer or the command line
    /// can be naming. A WSL distribution's shell is never opened this way: its
    /// filesystem is not the one the path was resolved against.
    pub(super) fn open_local_directory(
        &mut self,
        dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::info!("opening a local session in {}", dir.display());
        #[cfg(unix)]
        let session = cx.new(|cx| Session::new_local_at(dir, cx));
        #[cfg(windows)]
        let session = {
            // `local_shells` promises the fixed shells first and in a stable
            // order, so the first entry is PowerShell whatever else the machine
            // turns out to have.
            let shell = session::local_shells(&[]).remove(0);
            cx.new(|cx| {
                Session::new_local_command_at(shell.name, shell.command, shell.filesystem, dir, cx)
            })
        };
        let panel_open = Self::panel_opens_for(None, cx);
        self.adopt_session(session, panel_open, window, cx);
    }

    /// Whether this workspace is showing the start screen rather than a
    /// session.
    ///
    /// Asked from outside the window, which is why it exists at all: the tabs
    /// are this type's own business and every other question about them is
    /// answered in here. The one caller is [`open_start_dirs_in_new_window`],
    /// looking for a window it may fill instead of opening another beside it.
    pub(super) fn has_no_tabs(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Gives a freshly built session a view, a pane and a tab of its own, and
    /// activates that tab.
    ///
    /// Everything past the constructor is identical for a remote and a local
    /// session, which is the whole point of them being one type. `panel_open` is
    /// the one thing that is not, and it arrives already decided — by
    /// [`panel_opens_with`], which the caller asks because only the caller still
    /// has the profile in hand.
    pub(super) fn adopt_session(
        &mut self,
        session: Entity<Session>,
        panel_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let caps = Self::pane_caps_source(cx);
        let view = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
        let leaf = self.new_pane(view, session, window, cx);

        self.tabs
            .push(SessionTab::single(leaf).with_panel(panel_open));
        self.active = self.tabs.len() - 1;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Wires a freshly created terminal view up as a pane.
    pub(super) fn new_pane(
        &mut self,
        view: Entity<TerminalView>,
        session: Entity<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneLeaf {
        // Repaints on any session change; on a disconnect it also retires the
        // pane. `observe_in` rather than `observe` because closing a pane moves
        // focus, and focus needs the window.
        let observer = cx.observe_in(&session, window, |this, session, window, cx| {
            if matches!(
                session.read(cx).status(),
                SessionStatus::Disconnected { .. }
            ) {
                this.close_pane_for_session(session.entity_id(), window, cx);
            }
            cx.notify();
        });
        let handle = view.read(cx).focus_handle(cx);
        let id = view.entity_id();
        let clicked = cx.subscribe(&view, |this, view, _: &PaneFocused, cx| {
            this.on_pane_focused(view.entity_id(), cx);
        });
        // Both places that offer a reconnect raise this rather than calling the
        // session, because only the workspace can see what the *other* tabs are
        // forwarding — see [`Workspace::reconnect_session`], which is also why
        // this is `subscribe_in`: the answer now takes in the other windows too,
        // and naming them means naming the one to leave out.
        //
        // Kept rather than detached, unlike every earlier version of this line;
        // [`PaneLeaf::_reconnect`] says why.
        let reconnect = cx.subscribe_in(
            &view,
            window,
            |this, view, _: &ReconnectRequested, window, cx| {
                let session = view.read(cx).session().clone();
                this.reconnect_session(&session, window, cx);
            },
        );
        let focus = cx.on_focus(&handle, window, move |this, _window, cx| {
            this.on_pane_focused(id, cx);
        });

        PaneLeaf {
            view: PaneView::Terminal(view),
            _observer: Some(observer),
            _clicked: clicked,
            _focus: focus,
            _reconnect: Some(reconnect),
        }
    }

    /// Wires a freshly created editor pane up as a pane.
    ///
    /// Nothing here watches the *session*, unlike [`Workspace::new_pane`]: a
    /// session that hangs up takes its terminal with it, but not a file — the
    /// buffer is still open, still editable, and still saveable if the source
    /// can be reached, see [`EditorPane`]. What is watched instead is the pane
    /// itself, because the status bar prints where the caret is and what the
    /// file is being coloured as, and a caret move touches nothing else the
    /// workspace draws.
    pub(super) fn new_editor_pane(
        &mut self,
        pane: Entity<EditorPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneLeaf {
        let handle = pane.read(cx).focus_handle(cx);
        let id = pane.entity_id();
        let clicked = cx.subscribe_in(
            &pane,
            window,
            move |this, pane, event: &EditorPaneEvent, window, cx| match event {
                EditorPaneEvent::Focused => this.on_pane_focused(pane.entity_id(), cx),
                EditorPaneEvent::CloseRequested => this.close_editor_pane(pane, window, cx),
                EditorPaneEvent::SavedForClose => this.close_saved_editor_pane(pane, window, cx),
                EditorPaneEvent::PasswordRequested(purpose) => {
                    this.ask_sudo_password(pane.clone(), *purpose, window, cx);
                }
            },
        );
        let focus = cx.on_focus(&handle, window, move |this, _window, cx| {
            this.on_pane_focused(id, cx);
        });
        let observer = cx.observe(&pane, |_this, _pane, cx| cx.notify());

        PaneLeaf {
            view: PaneView::Editor(pane),
            _observer: Some(observer),
            _clicked: clicked,
            _focus: focus,
            // A file has no connection to offer, so there is no *Reconnect*
            // button on it to carry anywhere.
            _reconnect: None,
        }
    }

    /// Wires a freshly created tail pane up as a pane.
    ///
    /// [`Workspace::new_pane`] with one entity swapped, and deliberately no
    /// more than that: a followed file is a connection, so it wants every rule
    /// a terminal pane gets — the pane retires when the session hangs up, the
    /// strip repaints on every change to it, and the *Reconnect* button on its
    /// overlay reaches the workspace that can say whether the profile's
    /// forwardings are free. The events are the grid's own, re-emitted by
    /// [`TailView`] under the entity the workspace knows the pane by.
    pub(super) fn new_tail_pane(
        &mut self,
        view: Entity<TailView>,
        session: Entity<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneLeaf {
        let observer = cx.observe_in(&session, window, |this, session, window, cx| {
            if matches!(
                session.read(cx).status(),
                SessionStatus::Disconnected { .. }
            ) {
                this.close_pane_for_session(session.entity_id(), window, cx);
            }
            cx.notify();
        });
        let handle = view.read(cx).focus_handle(cx);
        let id = view.entity_id();
        let clicked = cx.subscribe(&view, |this, view, _: &PaneFocused, cx| {
            this.on_pane_focused(view.entity_id(), cx);
        });
        let reconnect = cx.subscribe_in(
            &view,
            window,
            |this, view, _: &ReconnectRequested, window, cx| {
                let session = view.read(cx).session().clone();
                this.reconnect_session(&session, window, cx);
            },
        );
        let focus = cx.on_focus(&handle, window, move |this, _window, cx| {
            this.on_pane_focused(id, cx);
        });

        PaneLeaf {
            view: PaneView::Tail(view),
            _observer: Some(observer),
            _clicked: clicked,
            _focus: focus,
            _reconnect: Some(reconnect),
        }
    }

    /// Records the pane rendering `view` as the active one of its tab.
    ///
    /// This is what makes a click inside a pane — [`TerminalView`] focuses
    /// itself on mouse down — move the active-pane marker, the status bar and
    /// the tab label onto that pane.
    pub(super) fn on_pane_focused(&mut self, view: EntityId, cx: &mut Context<Self>) {
        for tab in &mut self.tabs {
            let Some(pane) = tab.pane_of(view) else {
                continue;
            };
            if tab.active_pane != pane {
                tab.focus(pane);
                // The file-type and encoding pickers name the pane they were
                // opened over, and act on whichever pane is active when a row is
                // picked. Once those are two different panes they are asking
                // about one file and answering about another, so they go. A
                // press elsewhere in the window is caught by the menu's own
                // backdrop; this is for the keyboard, which moves the focus
                // without one.
                self.language_menu = None;
                self.charset_menu = None;
                cx.notify();
            }
            return;
        }
    }

    /// The saved profile `id`, as the store has it right now.
    ///
    /// Through the connection dialog because that is where the store lives —
    /// see [`Workspace::duplicate_profile`] for why there is exactly one — and
    /// by value because that is what the dialog hands out: the caller is
    /// usually about to put the profile in a closure that outlives the frame.
    pub(super) fn profile(&self, id: Uuid, cx: &App) -> Option<SessionProfile> {
        self.dialog
            .read(cx)
            .profiles()
            .into_iter()
            .find(|profile| profile.id == id)
    }

    /// Opens `path` on `profile` as a followed file, in a tab of its own.
    ///
    /// [`Workspace::open_profile`] for the other thing a saved profile can be
    /// asked for, and it makes the same two decisions in the same order: a
    /// profile that carries everything the transport needs follows the file on
    /// the click, and one that does not gets the pre-filled form first. The
    /// difference is on the far side of that form — the dialog can only say
    /// *connect*, so the request is put down in [`Workspace::pending_tail`] and
    /// picked up again when the credentials come back.
    pub(super) fn open_tail(
        &mut self,
        profile: &SessionProfile,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_overlays(cx);
        if let Some(auth) = connection::saved_credentials(profile) {
            self.open_tail_session(profile.clone(), auth, path, window, cx);
            return;
        }
        // After `close_overlays`, which clears this very field: the request is
        // being made now, and what it clears is whatever request was abandoned
        // before it.
        self.pending_tail = Some((profile.id, path));
        let id = profile.id;
        self.dialog
            .update(cx, |dialog, cx| dialog.open_profile(id, cx));
        cx.notify();
    }

    /// Evens the panes of the active tab out along `axis`.
    ///
    /// The counterpart of dragging every divider by hand, which is what a tab
    /// split more than twice otherwise needs: splitting the same pane twice
    /// leaves the first half twice the width of the other two, and no sequence
    /// of splits produces even thirds on its own.
    ///
    /// Only the dividers along `axis` move, so a width pass leaves a stacked
    /// pair inside a column exactly where the user dragged it; the arithmetic
    /// is [`PaneTree::equalize`]'s. Each pane hears about its new size the way
    /// it hears about a window resize — the grid is measured on the next frame
    /// and the pty told only if the cell count actually changed — so there is
    /// nothing to push from here.
    pub(crate) fn equalize_panes(&mut self, axis: Axis, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.panes.equalize(axis) {
            cx.notify();
        }
    }
}
