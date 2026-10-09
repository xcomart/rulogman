//! View.

use super::*;

/// A box that keeps `content` in the middle while it fits, and lets it be
/// scrolled from the top once it does not.
///
/// `justify_center` does the first half and ruins the second. With more content
/// than room, a centred column hangs off both ends of its box, and scrolling
/// only ever reaches what lies past the *end* of one — so the head of the column
/// goes off the top edge and stays there, unreachable. Automatic margins share
/// out whatever room is spare, which centres the column exactly as `justify_center`
/// would, and collapse to nothing when there is none, which leaves the column at
/// the top with all of it below the fold and so all of it reachable.
///
/// Three boxes. The outermost is what the overlay bar hangs off, because the
/// scrolling box cannot hold it — its children are what scroll away underneath
/// it — and it is what the caller styles, the fill included. Inside it is the
/// box that scrolls, and inside that the one carrying the margins and the
/// breathing room that keeps either end of the scroll off the edge.
pub(super) fn centered_scroll(
    id: &'static str,
    scroll: &ScrollHandle,
    bar: Scrollbar,
    theme: &Theme,
    content: impl IntoElement,
) -> Div {
    div()
        .relative()
        .flex()
        .flex_col()
        .flex_grow_1()
        .min_h_0()
        .child(
            div()
                .id(id)
                .track_scroll(scroll)
                .flex()
                .flex_col()
                .flex_grow_1()
                .min_h_0()
                .items_center()
                .overflow_y_scroll()
                .restrict_scroll_to_axis()
                .child(
                    // `flex_none` so that a column taller than the box overflows
                    // it — and is scrolled to — rather than being squeezed into
                    // it, which is what a flex item does by default.
                    div()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .items_center()
                        .my_auto()
                        .py(px(SCROLL_MARGIN))
                        .child(content),
                ),
        )
        .children(bar.render(theme))
}

/// Renders one node of a pane tree.
///
/// A split becomes a flex box in the direction of its axis, with each child
/// sized by `flex_basis`; the `min_w_0` / `min_h_0` on the box *and* on both
/// children is what lets those bases actually divide the space, instead of the
/// terminals inside insisting on their measured width. The pty follows on its
/// own: [`TerminalView`]'s element recomputes the grid from whatever bounds it
/// is given and only pushes a resize when the cell count changed.
///
/// A leaf renders the terminal view itself. When `frame` is set — a split tab,
/// or a single pane with the file panel beside it — every leaf is framed with a
/// hairline, accent coloured on the active one. The frames double as the
/// divider between neighbours, which is why there is no separate divider
/// element — a third hairline squeezed between two of them would only thicken
/// the seam. Every pane is framed, not just the active one, so that moving
/// focus recolours the frame without shifting the layout by a pixel. It is a
/// border rather than a fill because a translucent window allows only one
/// tinted fill per pixel and the terminal surface already owns it.
///
/// `panel_focused` demotes the active leaf back to the plain border colour: the
/// file panel wears the accent frame while it holds the keyboard, and two
/// accent frames at once would say the keystroke is going to both places.
///
/// A split is a [`Splitter`], which lays its own grab band over the divider and
/// hands back the ratio the pointer asks for. It is asked to draw no seam of
/// its own: the pane frames on either side already meet there, and a third
/// hairline between two of them would only thicken the line.
pub(super) fn render_pane(
    node: &PaneNode<PaneLeaf>,
    active: PaneId,
    frame: bool,
    panel_focused: bool,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    match node {
        PaneNode::Leaf { id, payload } => {
            let border = if *id == active && !panel_focused {
                theme.accent
            } else {
                theme.border
            };
            div()
                .id(("pane", id.as_u64()))
                .flex()
                .size_full()
                .min_w_0()
                .min_h_0()
                .when(frame, |pane| pane.border_1().border_color(border))
                .child(payload.view.element())
                .into_any_element()
        }
        PaneNode::Split {
            id,
            axis,
            ratio,
            first,
            second,
        } => {
            let id = *id;
            let workspace = cx.entity();
            // Both children are rendered up front because each one needs `cx`
            // for the splitters further down the tree, and a closure holding it
            // could not then be called twice.
            let first = render_pane(first, active, frame, panel_focused, theme, cx);
            let second = render_pane(second, active, frame, panel_focused, theme, cx);

            // The tree's own axis and gpui's are two enums of the same two
            // words: the pane crate names a direction without depending on a
            // framework, and the widget takes the framework's.
            let axis = match axis {
                Axis::Horizontal => gpui::Axis::Horizontal,
                Axis::Vertical => gpui::Axis::Vertical,
            };

            Splitter::new(("split", id.as_u64()), axis)
                .ratio(*ratio)
                .seamless()
                .first(first)
                .second(second)
                .on_change(move |ratio, _window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.set_split_ratio(id, ratio, cx);
                    });
                })
                .into_any_element()
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        // Before anything is built, so the panel is already pointed at the
        // active pane's session by the time it renders itself as a child.
        self.sync_file_panel(cx);
        self.watch_scroll(cx);
        let toolbar = self.render_toolbar(window, cx);
        let body = self.render_body(window, cx);
        let status_bar = self.render_status_bar(cx);
        let tab_context = self.render_tab_context(cx);
        // Only ever open over the empty state, which is what the body draws
        // while there is no tab; a session opened from the menu itself takes
        // the state — and with `close_overlays`, the menu — off the screen.
        let empty_context = self.render_empty_context(cx);
        let language_menu = self.render_language_menu(cx);
        let charset_menu = self.render_charset_menu(cx);
        let close_confirm = self.render_close_confirm(cx);
        let sudo_prompt = self.render_sudo_prompt(cx);
        let dialog = self
            .dialog
            .read(cx)
            .is_open()
            .then(|| div().absolute().inset_0().child(self.dialog.clone()));
        let settings = self
            .settings
            .read(cx)
            .is_open()
            .then(|| div().absolute().inset_0().child(self.settings.clone()));
        let about = self
            .about
            .read(cx)
            .is_open()
            .then(|| div().absolute().inset_0().child(self.about.clone()));
        let update = self
            .update
            .read(cx)
            .is_open()
            .then(|| div().absolute().inset_0().child(self.update.clone()));

        // With client-side decorations the compositor stops drawing the drop
        // shadow along with the frame, so the window has to bring its own:
        // the surface grows a transparent band all round, the content is
        // inset by it, and the shadow is painted into it. The inset call
        // keeps `_GTK_FRAME_EXTENTS` in step so the compositor treats the
        // content edge, not the surface edge, as the window.
        let tiling = chrome::client_tiling(window);
        if tiling.is_some() {
            window.set_client_inset(px(chrome::SHADOW_BAND));
        } else {
            // Clears the extents a client-side frame may have left behind
            // when the setting switches back to the system title bar on a
            // live window; a no-op under decorations that never set any.
            window.set_client_inset(px(0.));
        }

        // No background fill here on purpose. The three bands below — toolbar,
        // body and status bar — cover the window between them, and each paints
        // its own. A fill at this level would sit *under* the translucent
        // terminal and empty-state fills and compose back to opaque, which is
        // exactly what made `window.background_opacity` and `background_blur`
        // look like they did nothing.
        let content = div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .text_color(theme.text)
            .text_size(px(13.))
            // The overlay bars are answered from here rather than from the
            // surfaces they ride: gpui hands a drag move to every listener of
            // that type wherever it sits, and the root is the one element that
            // is always mounted while a drag of one is in flight.
            .on_drag_move::<DraggedThumb>(cx.listener(
                move |workspace, event: &DragMoveEvent<DraggedThumb>, _window, cx| {
                    workspace.drag_scrollbar(event, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|workspace, _: &MouseUpEvent, _window, cx| {
                    workspace.release_scrollbars(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|workspace, _: &MouseUpEvent, _window, cx| {
                    workspace.release_scrollbars(cx);
                }),
            )
            .on_action(cx.listener(Self::new_session_action))
            .on_action(cx.listener(Self::close_session_action))
            .on_action(cx.listener(Self::focus_next_pane_action))
            .on_action(cx.listener(Self::focus_prev_pane_action))
            .on_action(cx.listener(Self::break_out_pane_action))
            .on_action(cx.listener(Self::equalize_widths_action))
            .on_action(cx.listener(Self::equalize_heights_action))
            .on_action(cx.listener(Self::move_tab_to_new_window_action))
            .on_action(cx.listener(Self::duplicate_split_right_action))
            .on_action(cx.listener(Self::duplicate_split_below_action))
            .on_action(cx.listener(Self::toggle_file_panel_action))
            .on_action(cx.listener(Self::save_dashboard_layout_action))
            .on_action(cx.listener(Self::open_settings_action))
            .on_action(cx.listener(Self::show_about_action))
            .on_action(cx.listener(Self::check_updates_action))
            .on_action(cx.listener(Self::select_tab_action))
            .on_action(cx.listener(Self::open_dashboard_action))
            .on_action(cx.listener(Self::dismiss_dialog_action))
            .child(toolbar)
            .child(body)
            .child(status_bar)
            // Deferred inside, so it paints above the three bands whatever its
            // place in this list.
            .children(tab_context)
            .children(empty_context)
            .children(language_menu)
            .children(charset_menu)
            .children(dialog)
            .children(settings)
            .children(about)
            .children(update)
            .children(close_confirm)
            .children(sudo_prompt);

        let Some(tiling) = tiling else {
            // A server-decorated window: the compositor frames and shadows
            // it, and the content is the whole surface.
            return content.into_any_element();
        };
        chrome::render_client_frame(content, tiling, theme.border, window.is_window_active())
            .into_any_element()
    }
}

impl Workspace {
    /// Renders the panes of the active tab, or the empty state.
    pub(super) fn render_body(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.tabs.get(self.active) else {
            return self.render_empty_state(cx);
        };

        let theme = theme(cx);
        let panel_open = self.panel_showing();
        // A lone terminal with nothing beside it is drawn exactly as it was
        // before panes existed: no frame, no divider, the terminal filling the
        // body. Once it is split, or once the file panel is open next to it,
        // there is a second thing that can hold the keyboard and the frame has
        // to be there to say which one does.
        let frame = tab.panes.leaf_count() > 1 || panel_open;
        // Asked of the focus tree at render time for the same reason the panel
        // asks it — see `FilePanel::render`. Only one of the two frames wears
        // the accent, so the active pane gives its own up while the panel has
        // the keyboard.
        let panel_focused = panel_open && self.panel.focus_handle(cx).contains_focused(window, cx);
        let active = tab.active_pane();
        let root = tab.panes.root();
        let panel = panel_open.then(|| self.panel.clone());

        div()
            .flex()
            .flex_row()
            .flex_grow_1()
            .min_w_0()
            .min_h_0()
            .children(panel)
            .child(div().flex().flex_1().min_w_0().min_h_0().child(render_pane(
                root,
                active,
                frame,
                panel_focused,
                &theme,
                cx,
            )))
            .into_any_element()
    }

    /// The same pair, for the render paths that only read it.
    pub(super) fn surface_ref(&self, surface: Surface) -> (&ScrollHandle, &ScrollbarState) {
        match surface {
            Surface::Tabs => (&self.tab_scroll, &self.tab_scrollbar),
            Surface::Empty => (&self.empty_scroll, &self.empty_scrollbar),
        }
    }

    /// One surface's overlay scroll indicator, as it stands.
    ///
    /// Rebuilt on demand rather than kept, because everything it is made of —
    /// the surface's box, how far it overflows, where it sits — is measured
    /// afresh by gpui on every layout pass.
    pub(super) fn scrollbar(&self, id: &'static str, surface: Surface) -> Scrollbar {
        let (handle, state) = self.surface_ref(surface);
        Scrollbar::for_handle(id, surface.axis(), handle).fade(state.fade())
    }

    /// The same bar, listening for the pointer reaching the edge it rides.
    ///
    /// Only the bars that are drawn need it: the ones the drag path builds are
    /// there to be measured, and never reach an element tree.
    pub(super) fn hovering_scrollbar(
        &self,
        id: &'static str,
        surface: Surface,
        cx: &mut Context<Self>,
    ) -> Scrollbar {
        self.scrollbar(id, surface).on_hover(cx.listener(
            move |workspace, hovered: &bool, _window, cx| {
                workspace.hover_scrollbar(surface, *hovered, cx);
            },
        ))
    }

    /// Puts each surface's bar up whenever that surface has moved, and starts
    /// the clock that takes it down again.
    ///
    /// Called from `render` because that is where every way of scrolling them
    /// meets: a wheel over the tabs or the empty state, and the jump that brings
    /// a newly activated tab back into view.
    pub(super) fn watch_scroll(&mut self, cx: &mut Context<Self>) {
        for (_, surface) in SCROLLBARS {
            let (handle, state) = self.surface(surface);
            let scrolled = scrolled(handle, surface.axis());
            if let Some(epoch) = state.moved(scrolled) {
                hide_later(epoch, cx, move |workspace| {
                    Some(workspace.surface(surface).1)
                });
            }
        }
    }

    /// Scrolls whichever surface's thumb has been dragged.
    ///
    /// Every element listening for this drag type hears every such drag, so each
    /// bar checks that the one being dragged is its own before answering.
    pub(super) fn drag_scrollbar(
        &mut self,
        event: &DragMoveEvent<DraggedThumb>,
        cx: &mut Context<Self>,
    ) {
        for (id, surface) in SCROLLBARS {
            let Some(progress) = self.scrollbar(id, surface).dragged(event, cx) else {
                continue;
            };

            // Held even when the pointer moved along the other axis and the
            // surface has not budged: the bar has to stay up for as long as it
            // is being held, and a still pointer moves nothing to notice.
            let (handle, state) = self.surface(surface);
            state.hold();
            scroll_to(handle, surface.axis(), progress);
            cx.notify();
            return;
        }
    }

    /// Lets go of whichever thumb was being held, and starts its clock again.
    ///
    /// Every mouse release in the window arrives here; all but the one ending a
    /// drag of a bar find nothing to let go of.
    pub(super) fn release_scrollbars(&mut self, cx: &mut Context<Self>) {
        for (_, surface) in SCROLLBARS {
            if let Some(epoch) = self.surface(surface).1.release() {
                hide_later(epoch, cx, move |workspace| {
                    Some(workspace.surface(surface).1)
                });
                cx.notify();
            }
        }
    }

    /// Puts one surface's bar up while the pointer rests on the edge it rides,
    /// and starts it going the moment the pointer leaves.
    ///
    /// Told which surface rather than asked to work it out: each strip carries
    /// this listener already and knows only its own.
    pub(super) fn hover_scrollbar(
        &mut self,
        surface: Surface,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let state = self.surface(surface).1;
        if hovered {
            if state.hover_enter() {
                cx.notify();
            }
            return;
        }

        let Some(epoch) = state.hover_leave() else {
            return;
        };
        hide_now(self, epoch, cx, move |workspace| {
            Some(workspace.surface(surface).1)
        });
    }

    /// Renders the placeholder shown while no session is open.
    pub(super) fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let this = cx.entity();
        let profiles = self.dialog.read(cx).profiles();

        // Above the saved profiles, and deliberately: a dashboard is one click
        // to every log a deploy is watched through, while a profile below it is
        // one click to one shell. The aggregate is the bigger thing to be
        // offered, so it is offered first.
        let dashboards = (!self.dashboards.is_empty()).then(|| {
            let rows = self
                .dashboards
                .dashboards()
                .iter()
                .enumerate()
                .map(|(index, dashboard)| {
                    let id = dashboard.id;
                    Button::new(
                        ElementId::from(("dashboard", index)),
                        dashboard.name.clone(),
                    )
                    .variant(ButtonVariant::Ghost)
                    .full_width(true)
                    .on_click({
                        let this = this.clone();
                        move |_, window, cx| {
                            this.update(cx, |workspace, cx| {
                                workspace.open_dashboard(id, window, cx)
                            });
                        }
                    })
                })
                .collect::<Vec<_>>();

            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .w(px(320.))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.text_muted)
                        .child(ts!("empty.dashboards")),
                )
                .children(rows)
        });

        let saved = (!profiles.is_empty()).then(|| {
            let rows = profiles.into_iter().enumerate().map(|(index, profile)| {
                let id = ElementId::from(("saved-profile", index));
                let label = format!("{}  ·  {}", profile.name, profile.label());
                let profile_id = profile.id;
                let button = Button::new(id, label)
                    .variant(ButtonVariant::Ghost)
                    .full_width(true)
                    .on_click({
                        let this = this.clone();
                        move |_, window, cx| {
                            this.update(cx, |workspace, cx| {
                                workspace.open_profile(&profile, window, cx)
                            });
                        }
                    });

                // The right-click is answered by a wrapper rather than by the
                // button, which takes clicks and nothing else: a `Button` is
                // the application's one push control and has no business
                // growing a menu hook for the single place that wants one.
                div()
                    .id(ElementId::from(("saved-profile-row", index)))
                    .w_full()
                    .on_mouse_down(MouseButton::Right, {
                        let this = this.clone();
                        move |event: &MouseDownEvent, _window, cx| {
                            cx.stop_propagation();
                            this.update(cx, |workspace, cx| {
                                workspace.open_empty_context(profile_id, event.position, cx);
                            });
                        }
                    })
                    .child(button)
            });

            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .w(px(320.))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.text_muted)
                        .child(ts!("empty.saved_profiles")),
                )
                .children(rows)
        });

        let local = self.render_empty_local(cx);
        let shortcut = ts!("empty.hint", shortcut = format!("{SHORTCUT_MODIFIER}+T"));
        let bar = self.hovering_scrollbar(SCROLLBARS[1].0, Surface::Empty, cx);

        let content = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(14.))
            .child(
                div()
                    .text_size(px(30.))
                    .text_color(theme.text)
                    .child("rulogman"),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.text_muted)
                    .child(shortcut),
            )
            .child(
                div().w(px(320.)).child(
                    Button::new("empty-new-session", ts!("menu.new_session"))
                        .full_width(true)
                        .on_click({
                            let this = this.clone();
                            move |_, _window, cx| {
                                this.update(cx, |workspace, cx| workspace.open_dialog(cx));
                            }
                        }),
                ),
            )
            .children(local)
            .children(dashboards)
            .children(saved);

        // The fill goes on the box the helper hands back, which is the whole of
        // the body: the tint has to cover it however little of it the column
        // reaches, this being the only fill over the body while no session is
        // open and so where the window opacity lands on the empty state.
        centered_scroll(EMPTY_STATE, &self.empty_scroll, bar, &theme, content)
            .bg(app_settings::window_tint(theme.background, cx))
            .into_any_element()
    }

    /// The empty state's local terminal buttons.
    ///
    /// Sits between the button that opens the connection dialog and the saved
    /// profiles, and unlike either of them it opens a session outright rather
    /// than a dialog: a local shell has no host, no credentials and nothing to
    /// save, so there is nothing for a dialog to ask. The shell's name rides
    /// along after a separator, exactly as a profile row carries its
    /// `user@host`, so each button says which shell the press will start.
    ///
    /// Unix has one of them, the login shell, and so needs no choosing.
    /// Windows has as many as it has shells to start — PowerShell, `cmd`, and
    /// one per installed WSL distribution — and the WSL ones appear only once
    /// the discovery started in [`Workspace::new`] has answered, so this can
    /// return one button on one frame and four on the next.
    pub(super) fn render_empty_local(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        #[cfg(windows)]
        {
            let this = cx.entity();

            // The same list the connection dialog pins above its saved
            // profiles, built from the one place that knows how each of these
            // shells is started — a button that opened a shell the dialog does
            // not offer, or the other way round, would be a difference between
            // two ways of asking for the same thing.
            //
            // A WSL entry labels itself `WSL` rather than as another local
            // terminal: the shell it opens is a Linux one on a filesystem of
            // its own, which is a different place to be than the two above it
            // — and that difference travels into the session, so that its file
            // panel browses the distribution the shell is standing in rather
            // than this machine's disk.
            let rows = session::local_shells(&self.wsl_distros)
                .into_iter()
                .enumerate()
                .map(|(index, shell)| {
                    let this = this.clone();
                    let text = format!("{}  ·  {}", shell.kind_label(), shell.name);
                    Button::new(("empty-local", index), text)
                        .variant(ButtonVariant::Secondary)
                        .full_width(true)
                        .on_click(move |_, window, cx| {
                            // Cloned per press rather than moved: the handler is
                            // kept for the life of the button and may be pressed
                            // again, opening a second tab on the same shell.
                            let (label, command, filesystem) = (
                                shell.name.clone(),
                                shell.command.clone(),
                                shell.filesystem.clone(),
                            );
                            this.update(cx, |workspace, cx| {
                                workspace.open_local_command(label, command, filesystem, window, cx)
                            });
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>();

            Some(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .w(px(320.))
                    .children(rows)
                    .into_any_element(),
            )
        }

        #[cfg(unix)]
        {
            let this = cx.entity();
            let label = format!(
                "{}  ·  {}",
                ts!("connection.local.name"),
                rulogman_pty::login_shell_name()
            );
            Some(
                div()
                    .w(px(320.))
                    .child(
                        Button::new("empty-local-session", label)
                            .variant(ButtonVariant::Secondary)
                            .full_width(true)
                            .on_click(move |_, window, cx| {
                                this.update(cx, |workspace, cx| {
                                    workspace.open_local_session(window, cx)
                                });
                            }),
                    )
                    .into_any_element(),
            )
        }
    }
}
