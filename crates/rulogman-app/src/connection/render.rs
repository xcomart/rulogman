//! Render.

use super::*;

impl Render for ConnectionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().id("connection-dialog");
        }

        self.apply_pending_focus(window, cx);
        self.watch_scroll(cx);
        let theme = theme(cx);
        let body_bar = self.hovering_scrollbar(SCROLLBARS[0].0, Surface::Body, cx);

        let local = self.is_local_selected();
        let title = if local {
            // Neither "New connection" nor "Connect": nothing is being
            // connected to, and nothing is being created.
            ts!("connection.local.name")
        } else if self.editing.is_some() {
            ts!("connection.title_edit")
        } else {
            ts!("connection.title_new")
        };

        // Only the form scrolls; the footer stays put. The modal caps the panel
        // at the window height, and the `min_h_0` chain from here down is what
        // turns that cap into a scrolling body instead of a clipped one.
        let body = div()
            .flex()
            .flex_col()
            .min_h_0()
            .gap(px(12.))
            .child(
                // The middle box exists only to hold the body's overlay bar,
                // for the same reason the profile column has one of its own.
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .child(
                        div()
                            .id("connection-body")
                            .track_scroll(&self.body_scroll)
                            .flex()
                            .flex_col()
                            .min_h_0()
                            .gap(px(12.))
                            .overflow_y_scroll()
                            .restrict_scroll_to_axis()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_none()
                                    .items_start()
                                    .gap(px(16.))
                                    .child(self.render_profile_list(cx))
                                    .child(self.render_target_panel(cx)),
                            )
                            // A local session is never saved, so there is
                            // nothing for a per-session override to be attached
                            // to — and nothing to forward a port over, jump
                            // through, or follow a remote file on either.
                            .children((!local).then(|| self.render_overrides(cx)))
                            .children((!local).then(|| self.render_hops(cx)))
                            .children((!local).then(|| self.render_tunnels(cx)))
                            .children((!local).then(|| self.render_tails(cx))),
                    )
                    .children(body_bar.render(&theme)),
            )
            .child(self.render_footer(cx));

        let on_dismiss = {
            let this = cx.entity();
            move |_window: &mut Window, cx: &mut App| {
                this.update(cx, |dialog, cx| dialog.dismiss(cx));
            }
        };

        // The wrapper exists only to own the focus handle and the `Escape`
        // binding. It has to span its parent, because an absolutely positioned
        // element is laid out against its direct parent: a shrink-to-fit
        // wrapper would collapse to zero height and drag the modal off-screen
        // with it.
        div()
            .id("connection-dialog")
            .key_context(KEY_CONTEXT)
            .absolute()
            .inset_0()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_prev))
            .on_key_down(cx.listener(Self::on_key_down))
            // Both overlay bars are answered from here: gpui hands a drag move
            // to every listener of that type wherever it sits, and this is the
            // one element mounted for the whole of either drag — the profile
            // column is rebuilt from scratch whenever the list changes under it,
            // and the body scrolls away under its own bar.
            .on_drag_move::<DraggedThumb>(cx.listener(
                |dialog, event: &DragMoveEvent<DraggedThumb>, _window, cx| {
                    dialog.drag_scrollbar(event, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|dialog, _: &MouseUpEvent, _window, cx| {
                    dialog.release_scrollbars(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|dialog, _: &MouseUpEvent, _window, cx| {
                    dialog.release_scrollbars(cx);
                }),
            )
            .child(modal(
                "connection-modal",
                title,
                px(DIALOG_WIDTH),
                body,
                on_dismiss,
            ))
            // Deferred inside, so it paints over the modal whatever its place
            // in this list, and positioned in window coordinates — which the
            // wrapper spans, so the two agree.
            .children(self.render_context(cx))
    }
}

/// Frames one fold-away section of the form.
///
/// The rule above it and the room under that rule, and nothing else: the two
/// sections at the foot of the dialog are the only things below the form
/// proper, and the line is what says so. Written once because a second section
/// that drew its own rule a pixel differently would be visible.
pub(super) fn section(rule: Hsla, body: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .child(div().h(px(1.)).w_full().flex_none().bg(rule))
        .child(div().pt(px(4.)).child(body))
}

/// The line of small print at the far end of a section header.
///
/// What is inside the section, counted, so that a collapsed section still says
/// whether there is anything under it. It sits in the header's trailing slot
/// rather than beside the title: a press on it is not a press on the
/// disclosure, and a count that folded the section when it was clicked would be
/// a target pretending to be a label.
pub(super) fn summary_note(summary: SharedString, color: Hsla) -> impl IntoElement {
    div()
        .min_w_0()
        .truncate()
        .text_size(px(11.))
        .text_color(color)
        .child(summary)
}

/// Lays a "what this field inherits" hint out to the right of a control.
pub(super) fn inherit_hint<E: IntoElement>(
    control: E,
    hint: SharedString,
    cx: &App,
) -> impl IntoElement + use<E> {
    let theme = theme(cx);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .w_full()
        .child(div().flex_1().min_w_0().child(control))
        .child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .text_size(px(11.))
                .text_color(theme.text_muted)
                .child(hint),
        )
}

/// Renders `value` without a trailing `.0`, so 14.0 shows as "14".
pub(super) fn format_number(value: f32) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

/// Installs an observer that keeps `input` numeric.
///
/// The text field has no input filter, so the content is rewritten after every
/// edit. Rewriting only when the text actually changes stops the observer from
/// re-triggering itself.
pub(super) fn digits_only(
    cx: &mut Context<ConnectionDialog>,
    input: &Entity<TextInput>,
    decimals: bool,
    max_len: usize,
) {
    cx.observe(input, move |_this, input, cx| {
        let content = input.read(cx).content().to_owned();
        let mut seen_dot = false;
        let filtered: String = content
            .chars()
            .filter(|c| {
                if c.is_ascii_digit() {
                    true
                } else if decimals && *c == '.' && !seen_dot {
                    seen_dot = true;
                    true
                } else {
                    false
                }
            })
            .take(max_len)
            .collect();
        if filtered != content {
            input.update(cx, |input, cx| input.set_content(filtered, cx));
        }
    })
    .detach();
}

/// A compact text button used inside the profile rows.
///
/// The mouse-down handler stops propagation so that clicking an action does not
/// also select the row it lives in.
pub(super) fn row_action(
    id: ElementId,
    label: SharedString,
    color: Hsla,
    hover: Hsla,
    on_click: impl Fn(&mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(18.))
        .px(px(6.))
        .rounded_sm()
        .whitespace_nowrap()
        .text_size(px(11.))
        .text_color(color)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _window, cx| on_click(cx))
        .child(label)
}

/// Ask the platform for a private key file and write the choice into `dialog`.
///
/// The picker is asynchronous, so the result arrives on a spawned task; a
/// cancelled dialog simply leaves the field untouched.
pub(super) fn browse_for_key(dialog: Entity<ConnectionDialog>, cx: &mut App) {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(ts!("connection.select_file")),
    });

    cx.spawn(async move |cx| {
        let selection = match paths.await {
            Ok(Ok(Some(paths))) => paths.into_iter().next(),
            Ok(Ok(None)) => None,
            Ok(Err(err)) => {
                log::warn!("the file picker could not be opened: {err:#}");
                None
            }
            Err(_) => None,
        };
        let Some(path) = selection else {
            return;
        };
        dialog.update(cx, |dialog, cx| dialog.set_key_path(path, cx));
    })
    .detach();
}

impl ConnectionDialog {
    /// The open row menu, if there is one.
    ///
    /// The four commands the row already carries between its click gestures and
    /// its hover buttons, plus the copy, which has no gesture of its own. Only
    /// the profile going away can leave the menu with nothing to speak for,
    /// which is what a delete from the menu itself does.
    pub(super) fn render_context(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let (id, position) = self.context?;
        self.store.get(id)?;
        let this = cx.entity();

        let entries = vec![
            // What a double-click on the row does.
            MenuEntry::new(ts!("connection.connect")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.select_profile(id, cx);
                        dialog.connect(cx);
                    });
                }
            }),
            // What the hover Edit button does: load the profile and put the
            // caret at the top of the form. No ellipsis — the form is already
            // on screen, so nothing further is being promised.
            MenuEntry::new(ts!("connection.edit")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.select_profile(id, cx);
                        dialog.pending_focus = Some(FocusTarget::Host);
                    });
                }
            }),
            MenuEntry::new(ts!("connection.duplicate")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |dialog, cx| dialog.duplicate_profile(id, cx));
                }
            }),
            MenuEntry::separator(),
            MenuEntry::new(ts!("connection.delete")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |dialog, cx| dialog.delete_profile(id, cx));
                }
            }),
        ];

        Some(
            ContextMenu::new("connection-profile-context")
                .position(position)
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |dialog, cx| dialog.close_context(cx));
                }),
        )
    }

    /// The saved-profile column.
    pub(super) fn render_profile_list(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let bar = self.hovering_scrollbar(SCROLLBARS[1].0, Surface::List, cx);
        let this = cx.entity();
        let selected = self.editing;

        let rows = self
            .store
            .profiles()
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                let id = profile.id;
                let is_selected = selected == Some(id);
                let group = SharedString::from(format!("rulogman-profile-{index}"));

                div()
                    .id(ElementId::from(("connection-profile", index)))
                    .group(group.clone())
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if is_selected {
                        theme.surface_active
                    } else {
                        gpui::transparent_black()
                    })
                    .hover(|style| {
                        style.bg(if is_selected {
                            theme.surface_active
                        } else {
                            theme.surface_hover
                        })
                    })
                    .on_click({
                        let this = this.clone();
                        move |event, _window, cx| {
                            let double = event.click_count() >= 2;
                            this.update(cx, |dialog, cx| {
                                dialog.select_profile(id, cx);
                                if double {
                                    dialog.connect(cx);
                                }
                            });
                        }
                    })
                    .on_mouse_down(MouseButton::Right, {
                        let this = this.clone();
                        move |event: &MouseDownEvent, _window, cx| {
                            // The press belongs to this row, not to the list
                            // that scrolls under it.
                            cx.stop_propagation();
                            this.update(cx, |dialog, cx| {
                                dialog.open_context(id, event.position, cx);
                            });
                        }
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_grow_1()
                            .min_w_0()
                            .gap(px(1.))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(13.))
                                    .text_color(theme.text)
                                    .child(SharedString::from(profile.name.clone())),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(profile.label())),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_none()
                            .gap(px(2.))
                            .invisible()
                            .group_hover(group, |style| style.visible())
                            .child(row_action(
                                ElementId::from(("connection-profile-edit", index)),
                                ts!("connection.edit"),
                                theme.text_muted,
                                theme.surface_hover,
                                {
                                    let this = this.clone();
                                    move |cx| {
                                        this.update(cx, |dialog, cx| {
                                            dialog.select_profile(id, cx);
                                            dialog.pending_focus = Some(FocusTarget::Host);
                                        });
                                    }
                                },
                            ))
                            .child(row_action(
                                ElementId::from(("connection-profile-delete", index)),
                                ts!("connection.delete"),
                                theme.danger,
                                theme.surface_hover,
                                {
                                    let this = this.clone();
                                    move |cx| {
                                        this.update(cx, |dialog, cx| {
                                            dialog.delete_profile(id, cx);
                                        });
                                    }
                                },
                            )),
                    )
            })
            .collect::<Vec<_>>();

        let empty = rows.is_empty();

        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(6.))
            .w(px(LIST_WIDTH))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(ts!("connection.saved_profiles")),
            )
            .child(
                // A box of exactly the list's size, there only to hold the
                // overlay bar: the list cannot hold it itself, because its own
                // children are what scroll away underneath. Sized by the list
                // rather than stretched, so the bar's track and the bordered
                // box the eye sees are the same rectangle.
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .child(
                        div()
                            .id("connection-profile-list")
                            .track_scroll(&self.list_scroll)
                            // A wheel turned over the list stops here whenever
                            // the list has anywhere to go: gpui otherwise
                            // scrolls every container under the pointer, and
                            // the body would drift along with every turn aimed
                            // at the list. gpui's own scroll handler runs
                            // before this one, so the list has already moved
                            // by the time the event is stopped. A list that
                            // fits lets the wheel through — there is nothing
                            // here for it to mean.
                            .on_scroll_wheel(cx.listener(|dialog, _, _window, cx| {
                                if dialog.list_scroll.max_offset().y > px(0.) {
                                    cx.stop_propagation();
                                }
                            }))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .p(px(4.))
                            .max_h(px(LIST_MAX_HEIGHT))
                            .overflow_y_scroll()
                            .restrict_scroll_to_axis()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface)
                            // Pinned above everything the store holds, and
                            // separated by a rule: they are not saved profiles,
                            // they are always there, and they scroll away with
                            // them rather than staying stuck to the top of a
                            // long list.
                            .children(self.render_local_rows(cx))
                            .when(empty, |this| {
                                this.child(
                                    div()
                                        .p(px(8.))
                                        .text_size(px(12.))
                                        .text_color(theme.text_muted)
                                        .child(ts!("connection.empty_list")),
                                )
                            })
                            .children(rows),
                    )
                    .children(bar.render(&theme)),
            )
    }

    /// The pinned local rows and the rule under them.
    ///
    /// Unix pins one, the login shell. Windows pins one per shell it can start
    /// — PowerShell, `cmd`, and one per installed WSL distribution — so the
    /// dialog offers the same choice the welcome screen does, and the WSL ones
    /// appear only once [`Self::set_wsl_distros`] has been told about them.
    ///
    /// A list rather than an `Option` so that the rule is a sibling of the rows
    /// instead of being wrapped in a container that would break the list's own
    /// spacing.
    pub(super) fn render_local_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = Vec::new();

        #[cfg(unix)]
        rows.push(Self::local_row(
            "connection-local".into(),
            ts!("connection.local.name"),
            self.local_shell.clone(),
            self.local_selected,
            |dialog, cx| dialog.select_local(cx),
            cx,
        ));

        #[cfg(windows)]
        for (index, shell) in self.local_shells.iter().enumerate() {
            rows.push(Self::local_row(
                ("connection-local", index).into(),
                shell.kind_label(),
                shell.name.clone(),
                self.local_selected == Some(index),
                move |dialog, cx| dialog.select_local(index, cx),
                cx,
            ));
        }

        // The rule belongs to the rows above it: with nothing pinned there is
        // nothing to separate the saved profiles from.
        if !rows.is_empty() {
            let rule = div()
                .h(px(1.))
                .flex_none()
                .my(px(2.))
                .mx(px(4.))
                .bg(theme(cx).border);
            rows.push(rule.into_any_element());
        }

        rows
    }

    /// One pinned local row: what kind of shell it is over the shell's own
    /// name, and the click behaviour of a saved profile row.
    ///
    /// `on_select` is what tells the rows apart — there is one of them on unix
    /// and several on Windows — and everything else about them is shared, so
    /// that a local row and a profile row cannot drift apart in looks.
    pub(super) fn local_row(
        id: ElementId,
        kind: SharedString,
        shell: SharedString,
        selected: bool,
        on_select: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx);
        let this = cx.entity();

        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .px(px(8.))
            .py(px(6.))
            .rounded_md()
            .cursor_pointer()
            .bg(if selected {
                theme.surface_active
            } else {
                gpui::transparent_black()
            })
            .hover(move |style| {
                style.bg(if selected {
                    theme.surface_active
                } else {
                    theme.surface_hover
                })
            })
            // Selected on a single click, opened on a double one, exactly
            // like a saved profile row.
            .on_click(move |event, _window, cx| {
                let double = event.click_count() >= 2;
                this.update(cx, |dialog, cx| {
                    on_select(dialog, cx);
                    if double {
                        dialog.connect(cx);
                    }
                });
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_grow_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(theme.text)
                            .child(kind),
                    )
                    // The shell's name is a value, not a word: never
                    // translated, and shown where a profile shows its
                    // `user@host`.
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(shell),
                    ),
            )
            .into_any_element()
    }

    /// The right-hand side of the dialog: the connection form, or the local
    /// panel while a pinned row is selected.
    pub(super) fn render_target_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.selected_local_name() {
            Some(shell) => Self::render_local_panel(shell, cx).into_any_element(),
            None => self.render_form(cx).into_any_element(),
        }
    }

    /// What stands in for the form once a pinned local row is selected.
    ///
    /// Deliberately has no controls: a local session takes no configuration,
    /// so the panel only says what pressing Connect will do — with `shell` the
    /// name of the shell it will start.
    pub(super) fn render_local_panel(
        shell: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = theme(cx);

        // Two sentences for one thought, because only unix can call the shell
        // the user's login shell: there it is the one they were given, here it
        // is the one they just picked from several.
        #[cfg(unix)]
        let hint = ts!("connection.local.hint", shell = shell);
        #[cfg(windows)]
        let hint = ts!("connection.local.hint_shell", shell = shell);

        div()
            .flex()
            .flex_col()
            .flex_grow_1()
            .min_w_0()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(theme.text)
                    .child(ts!("connection.local.title")),
            )
            .child(
                div()
                    .max_w(px(380.))
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(hint),
            )
    }

    /// The connection form.
    pub(super) fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();
        let auth_kind = self.auth_kind;

        let auth_control = Segmented::new("connection-auth")
            .options(auth_options())
            .selected(auth_kind.index())
            .tab_index(tab::AUTH)
            .on_select({
                let this = this.clone();
                move |index, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.set_auth_kind(AuthKind::from_index(index), cx);
                    });
                }
            });

        let key_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .w_full()
            .child(
                div()
                    .flex_grow_1()
                    .min_w_0()
                    .child(self.key_path_input.clone()),
            )
            .child(
                Button::new("connection-browse", ts!("connection.browse"))
                    .variant(ButtonVariant::Secondary)
                    .tab_index(tab::BROWSE)
                    .on_click({
                        let this = this.clone();
                        move |_, _window, cx| browse_for_key(this.clone(), cx)
                    }),
            );

        let secret_label = match auth_kind {
            AuthKind::PrivateKey => ts!("connection.remember_passphrase"),
            _ => ts!("connection.remember_password"),
        };

        let remember = Checkbox::new("connection-remember", secret_label)
            .checked(self.save_secret)
            .tab_index(tab::REMEMBER)
            .on_toggle({
                let this = this.clone();
                move |checked, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.save_secret = checked;
                        cx.notify();
                    });
                }
            });

        let show_files = Checkbox::new("connection-show-files", ts!("connection.show_files"))
            .checked(self.show_files)
            .tab_index(tab::SHOW_FILES)
            .on_toggle({
                let this = this.clone();
                move |checked, _window, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.show_files = checked;
                        cx.notify();
                    });
                }
            });

        div()
            .flex()
            .flex_col()
            .flex_grow_1()
            .min_w_0()
            .gap(px(10.))
            .child(form_row(ts!("connection.name"), self.name_input.clone()))
            .child(form_row(ts!("connection.host"), self.host_input.clone()))
            .child(form_row(ts!("connection.port"), self.port_input.clone()))
            .child(form_row(
                ts!("connection.username"),
                self.username_input.clone(),
            ))
            .child(form_row(ts!("connection.authentication"), auth_control))
            .when(auth_kind == AuthKind::Password, |this| {
                this.child(form_row(
                    ts!("connection.password"),
                    self.password_input.clone(),
                ))
            })
            .when(auth_kind == AuthKind::PrivateKey, |this| {
                this.child(form_row(ts!("connection.key_file"), key_row))
                    .child(form_row(
                        ts!("connection.passphrase"),
                        self.passphrase_input.clone(),
                    ))
            })
            .when(auth_kind == AuthKind::Agent, |this| {
                this.child(form_row(
                    "",
                    div()
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .child(ts!("connection.agent_unsupported")),
                ))
            })
            .when(auth_kind != AuthKind::Agent, |this| {
                this.child(form_row("", remember))
            })
            // Unconditional, unlike the row above it: what the panel does when
            // the session opens has nothing to do with how the session
            // authenticates, so it is the one checkbox here that every
            // authentication method still gets to answer.
            .child(form_row("", show_files))
    }

    /// The message strip and the action buttons.
    pub(super) fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = theme(cx);
        let this = cx.entity();
        let connectable = self.can_connect(cx);

        // One element per sentence: a status that reports several problems
        // stacks them instead of running them together on one line.
        let status = self.status.as_ref().map(|status| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .text_size(px(12.))
                .text_color(status.level.color(&theme))
                .children(status.lines.iter().map(|line| div().child(line.clone())))
        });

        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(10.))
            .child(div().h(px(1.)).w_full().flex_none().bg(theme.border))
            .children(status)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("connection-cancel", ts!("common.cancel"))
                            .variant(ButtonVariant::Secondary)
                            .tab_index(tab::CANCEL)
                            .on_click({
                                let this = this.clone();
                                move |_, _window, cx| {
                                    this.update(cx, |dialog, cx| dialog.dismiss(cx));
                                }
                            }),
                    )
                    .child(
                        Button::new("connection-connect", ts!("connection.connect"))
                            .variant(ButtonVariant::Primary)
                            .disabled(!connectable)
                            .tab_index(tab::CONNECT)
                            .on_click({
                                let this = this.clone();
                                move |_, _window, cx| {
                                    this.update(cx, |dialog, cx| dialog.connect(cx));
                                }
                            }),
                    ),
            )
    }
}
