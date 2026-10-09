//! Dialogs.

use super::*;

impl Workspace {
    /// Reads what has been typed and answers the question with it.
    ///
    /// The OK button's path; <kbd>Enter</kbd> in the field takes the text
    /// straight to [`Workspace::submit_sudo_password`] instead, because it
    /// already has it in hand and the field it would be read back out of is
    /// leased at that moment.
    pub(super) fn confirm_sudo_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = &self.sudo_prompt else {
            return;
        };
        let password = prompt.input.read(cx).content().to_owned();
        self.submit_sudo_password(password, window, cx);
    }

    /// Hands `password` to whatever the question was asked for.
    ///
    /// Three routes out of here, and which one is taken says everything about
    /// what the pane's mode becomes:
    ///
    /// * **Unlock** — the source validates the password and, where the box is
    ///   ticked, keeps it. The pane unlocks on success, in the mode that says
    ///   where the next save's password will come from. A refusal leaves the
    ///   dialog up with the reason under the field, which is the whole reason
    ///   for validating before a buffer is unlocked at all.
    /// * **Save, remembered** — the same validation first, which is what makes
    ///   the tick box a promise rather than a hope: a password that is going to
    ///   be kept is proved before it is. The pane's mode is upgraded and the
    ///   save goes out needing nothing further.
    /// * **Save, not remembered** — no validation at all, and the dialog goes
    ///   down on the press. The password travels with the write, and a wrong
    ///   one comes back as an ordinary failed save in the pane's own strip; a
    ///   round trip to learn that a moment earlier would buy nothing.
    ///
    /// An empty field is sent like anything else. An empty password is still a
    /// password as far as `sudo` is concerned, and refusing it here would mean
    /// writing our own version of the sentence the host is about to give.
    pub(super) fn submit_sudo_password(
        &mut self,
        password: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(prompt) = &self.sudo_prompt else {
            return;
        };
        if prompt.busy {
            return;
        }
        let (pane, purpose, remember) = (prompt.pane.clone(), prompt.purpose, prompt.remember);

        // The one route that asks the source nothing: the password travels with
        // the bytes, and the write is where it is judged.
        if purpose == RootPurpose::Save && !remember {
            self.close_sudo_prompt(window, cx);
            pane.update(cx, |pane, cx| pane.save_with_password(password, cx));
            return;
        }

        if let Some(prompt) = &mut self.sudo_prompt {
            prompt.busy = true;
            prompt.error = None;
        }
        cx.notify();

        let source = pane.read(cx).source().clone();
        cx.spawn_in(window, async move |workspace, cx| {
            let result = source.unlock_root(Some(&password), remember).await;
            workspace
                .update_in(cx, |workspace, window, cx| match result {
                    Ok(()) => {
                        // `Remembered` whenever the box was ticked, and the
                        // `Save` route only ever arrives here with it ticked —
                        // the other one never asked the source anything.
                        let mode = if remember {
                            RootMode::Remembered
                        } else {
                            RootMode::EveryTime
                        };
                        workspace.close_sudo_prompt(window, cx);
                        pane.update(cx, |pane, cx| {
                            pane.unlock_as_root(mode, cx);
                            // The source keeps the password from here on, so
                            // the save that was waiting needs none of its own.
                            if purpose == RootPurpose::Save {
                                pane.resume_save(cx);
                            }
                        });
                    }
                    // Left up, with the reason: the whole point of validating
                    // before anything is written is that there is still a field
                    // on screen to try again in.
                    Err(error) => {
                        if let Some(prompt) = &mut workspace.sudo_prompt {
                            prompt.busy = false;
                            prompt.error = Some(SharedString::from(error.to_string()));
                        }
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Puts the password question away without answering it.
    ///
    /// Cancel, `Escape`, and a click on the backdrop all land here, and none of
    /// them changes anything about the pane: a locked buffer stays locked, and
    /// an unsaved one stays unsaved — see
    /// [`EditorPane::abandon_root_save`] for the one intent that has to be let
    /// go of with it.
    pub(super) fn cancel_sudo_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = &self.sudo_prompt else {
            return;
        };
        let (pane, purpose) = (prompt.pane.clone(), prompt.purpose);
        self.close_sudo_prompt(window, cx);
        if purpose == RootPurpose::Save {
            pane.update(cx, |pane, cx| pane.abandon_root_save(cx));
        }
    }

    /// Drops the question and hands the keyboard back to the file.
    ///
    /// The password field goes with it, which is the only place a typed
    /// password ever was.
    pub(super) fn close_sudo_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sudo_prompt.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Moves focus to the next pane of the active tab, wrapping around.
    pub(crate) fn focus_next_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_pane(true, window, cx);
    }

    /// Closes every dialog and the dropdown menu.
    ///
    /// Every `open_*` method starts here, which is what keeps the modals
    /// mutually exclusive: only one of them can ever be on screen, and opening
    /// one always puts the menu away. The update dialog is closed here like the
    /// rest, so a user who reaches for a command instead of one of its buttons
    /// is not left with a stale announcement floating over the window — except
    /// while it is installing, when its own `close` refuses and the swap is
    /// allowed to finish; see [`UpdateDialog::close`].
    pub(super) fn close_overlays(&mut self, cx: &mut Context<Self>) {
        self.menu_open = false;
        self.tab_menu_open = false;
        self.tab_context = None;
        self.language_menu = None;
        self.charset_menu = None;
        self.empty_context = None;
        // Anything that opens an overlay is a fresh intention, and a followed
        // file nobody got round to is stale by the time the next one arrives —
        // see [`Workspace::pending_tail`]. Set again, by `open_tail`, *after*
        // this call.
        self.pending_tail = None;
        // Cancelled rather than parked. The safe answer to "close it and lose
        // the changes?" is no, and a user who has just reached for a different
        // command has plainly stopped answering this one; leaving it up would
        // put two modals on the screen at once.
        self.close_confirm = None;
        // The password question goes the same way, and taking it rather than
        // clearing it is what drops the field the password was typed into. Its
        // pane keeps whatever it had: still locked, or still holding an unsaved
        // buffer — with the one intent that has to be let go of let go of here
        // too. Nothing focuses anything: `ask_sudo_password` calls this on its
        // way *in*, and the focus it wants is the field it is about to build.
        if let Some(prompt) = self.sudo_prompt.take()
            && prompt.purpose == RootPurpose::Save
        {
            prompt
                .pane
                .update(cx, |pane, cx| pane.abandon_root_save(cx));
        }
        if self.dialog.read(cx).is_open() {
            self.dialog.update(cx, |dialog, cx| dialog.close(cx));
        }
        if self.settings.read(cx).is_open() {
            self.settings.update(cx, |dialog, cx| dialog.close(cx));
        }
        if self.about.read(cx).is_open() {
            self.about.update(cx, |dialog, cx| dialog.close(cx));
        }
        if self.update.read(cx).is_open() {
            self.update.update(cx, |dialog, cx| dialog.close(cx));
        }
    }

    /// Shows the connection dialog with an empty form.
    pub(super) fn open_dialog(&mut self, cx: &mut Context<Self>) {
        self.close_overlays(cx);
        self.dialog.update(cx, |dialog, cx| dialog.open_new(cx));
        cx.notify();
    }

    /// Opens a saved profile, showing the connection dialog only if it has to.
    ///
    /// A profile the user already finished configuring — a remembered password,
    /// or a key that needs no passphrase — carries everything the transport
    /// needs, so presenting the dialog again would be one form to dismiss
    /// before a session the user has already asked for. Those profiles connect
    /// on the click.
    ///
    /// The dialog still opens, pre-filled, whenever anything would have to be
    /// typed or corrected: a password that was never remembered, an encrypted
    /// key with no stored passphrase, or a key file that has gone missing.
    /// Agent profiles connect directly without looking up a stored secret.
    ///
    /// Deciding that reads the OS keychain, and possibly the key file,
    /// synchronously on the UI thread — the same work the dialog's Connect
    /// button does, one click earlier.
    pub(super) fn open_profile(
        &mut self,
        profile: &SessionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_overlays(cx);
        if let Some(auth) = connection::saved_credentials(profile) {
            self.open_session(profile.clone(), auth, window, cx);
            return;
        }
        let id = profile.id;
        self.dialog
            .update(cx, |dialog, cx| dialog.open_profile(id, cx));
        cx.notify();
    }

    /// Copies the saved profile `id` and shows the copy in the list.
    ///
    /// Routed through the dialog rather than through a store of the workspace's
    /// own, because there is only one store: the dialog holds it, and the empty
    /// state lists what the dialog holds.
    ///
    /// The same goes for [`Workspace::delete_profile`] below — one deletion, one
    /// code path — and with it goes the dialog's message strip, which is where
    /// either of them says that the list could not be written. From here that
    /// message has nowhere to appear; the log line the storage layer writes is
    /// what is left of it.
    pub(super) fn duplicate_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.dialog
            .update(cx, |dialog, cx| dialog.duplicate_profile(id, cx));
        cx.notify();
    }

    /// Forgets the saved profile `id`, keychain entry and all.
    pub(super) fn delete_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.dialog
            .update(cx, |dialog, cx| dialog.delete_profile(id, cx));
        cx.notify();
    }

    /// Shows the settings dialog.
    pub(super) fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.close_overlays(cx);
        self.settings.update(cx, |dialog, cx| dialog.open(cx));
        cx.notify();
    }

    /// Shows the about dialog.
    pub(super) fn open_about(&mut self, cx: &mut Context<Self>) {
        self.close_overlays(cx);
        self.about.update(cx, |dialog, cx| dialog.open(cx));
        cx.notify();
    }

    /// Asks GitHub for the latest release and shows the answer.
    ///
    /// Goes through `close_overlays` where the start-up check pointedly does
    /// not: this dialog was asked for, so it is entitled to the screen the way
    /// every other menu command is.
    ///
    /// Refuses while an install is already running, which is the one case where
    /// the update dialog cannot be closed and so must not be reopened into a
    /// different state. An install in *any* window counts — see
    /// [`Workspace::update_installing`].
    pub(super) fn check_updates(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.update_installing(window, cx) {
            return;
        }
        self.close_overlays(cx);
        self.update.update(cx, |dialog, cx| dialog.start_check(cx));
        cx.notify();
    }

    /// Whether an update install is running, in this window or in any other.
    ///
    /// Asked of every window because the answer is about the process: the
    /// install rewrites the running image, so a second window must not start a
    /// download over one already being written.
    ///
    /// This window answers for itself and is left out of the sweep — see
    /// [`other_workspace_windows`] for why it has to be.
    pub(super) fn update_installing(&self, window: &Window, cx: &App) -> bool {
        self.update.read(cx).is_busy() || installing_elsewhere(window, cx)
    }

    /// Shows or hides the file panel of the active tab.
    ///
    /// One command whichever session is active: a remote one browses the server
    /// over SFTP and a local one browses this computer, so every open session
    /// has a filesystem behind the panel and none of them is a reason to refuse.
    ///
    /// The active tab's flag and no other, which is the whole of what the
    /// command means now that the panel is opened per connection: a tab told to
    /// show the panel goes on showing it while the user works in the tab beside
    /// it, and the profile — or the setting a local shell follows — decides only
    /// where a tab starts, never where it stays.
    ///
    /// No tab is a reason to refuse. The welcome screen takes the place of the
    /// body the panel is drawn beside, so there is nothing to browse, nowhere to
    /// draw it, and no tab to write the answer on. The menu row greys out for
    /// the same reason, and this guard is what makes the shortcut and the macOS
    /// menu item agree with it.
    pub(super) fn toggle_file_panel(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        tab.panel_open = !tab.panel_open;
        cx.notify();
    }

    /// Renders the question asked before an edited file is thrown away.
    ///
    /// Three answers, and "Save" is the one that does not finish here. A save is
    /// a transfer that can fail — over a session that may well be the reason the
    /// pane is being closed — so rather than hold the question up over a write
    /// crossing the network, the dialog goes down on the press and the pane
    /// carries on from there: it says "saving…" in its header as it does for
    /// every other save, and closes itself only once the bytes are on the far
    /// end. A failure keeps the pane, with the reason in the strip beneath the
    /// file, which is the one place a reason belongs; the question is not asked
    /// again, because nothing about the file has changed since it was answered.
    pub(super) fn render_close_confirm(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pane = self.close_confirm?;
        // The pane can have gone since the question was asked; so has the
        // question, then.
        let name =
            self.tabs
                .iter()
                .find_map(|tab| match tab.panes.get(pane).map(|leaf| &leaf.view) {
                    Some(PaneView::Editor(editor)) => Some(editor.read(cx).name().clone()),
                    _ => None,
                })?;

        let theme = theme(cx);
        let this = cx.entity();
        let body = div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.text)
                    .child(ts!("editor.close_unsaved", name = name.to_string())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("editor-close-cancel", ts!("editor.close_cancel"))
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                workspace.cancel_close_editor(cx);
                                // Straight back into the file the question was
                                // about, which is where the caret already was.
                                workspace.focus_active(window, cx);
                            })),
                    )
                    .child(
                        Button::new("editor-close-discard", ts!("editor.close_discard"))
                            .variant(ButtonVariant::Danger)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                workspace.confirm_close_editor(window, cx);
                            })),
                    )
                    // Last, where every dialog in the application puts the
                    // answer it expects — and the one place the destructive
                    // button must not be, since that is where a hurried hand
                    // goes.
                    .child(
                        Button::new("editor-close-save", ts!("common.save"))
                            .variant(ButtonVariant::Primary)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                workspace.save_and_close_editor(window, cx);
                            })),
                    ),
            );

        Some(
            div()
                .absolute()
                .inset_0()
                .child(modal(
                    "editor-close-confirm",
                    ts!("editor.close_title"),
                    px(400.),
                    body,
                    move |window, cx| {
                        this.update(cx, |workspace, cx| {
                            workspace.cancel_close_editor(cx);
                            workspace.focus_active(window, cx);
                        });
                    },
                ))
                .into_any_element(),
        )
    }

    /// Renders the password an elevated save is waiting on.
    ///
    /// Built like the close question above — the same [`modal`], the same
    /// button row with the expected answer last — and different in the one way
    /// that matters: this dialog can be answered *wrongly*, so it does not
    /// always go down on the press. A refusal comes back into it, under the
    /// field, and the field keeps what was typed so a mistyped character is
    /// corrected rather than retyped. See
    /// [`Workspace::submit_sudo_password`] for which answers can fail.
    ///
    /// The prompt names the file, because a window can hold several open ones
    /// and the dialog covers whichever pane it belongs to.
    pub(super) fn render_sudo_prompt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self.sudo_prompt.as_ref()?;
        let theme = theme(cx);
        let this = cx.entity();
        let name = prompt.pane.read(cx).name().to_string();

        let body = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.text)
                    .child(ts!("editor.sudo_prompt", name = name)),
            )
            .child(prompt.input.clone())
            .child(
                Checkbox::new("editor-sudo-remember", ts!("editor.sudo_remember"))
                    .checked(prompt.remember)
                    .tab_index(1)
                    .on_toggle({
                        let this = this.clone();
                        move |checked, _window, cx| {
                            this.update(cx, |workspace, cx| {
                                if let Some(prompt) = &mut workspace.sudo_prompt {
                                    prompt.remember = checked;
                                }
                                cx.notify();
                            });
                        }
                    }),
            )
            // The host's own words, in the host's own language, and drawn in
            // the colour the pane draws a failed save in — because that is what
            // this is, caught early enough to try again.
            .children(prompt.error.clone().map(|error| {
                div()
                    .text_size(px(12.))
                    .text_color(theme.danger)
                    .child(error)
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("editor-sudo-cancel", ts!("common.cancel"))
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                workspace.cancel_sudo_password(window, cx);
                            })),
                    )
                    .child(
                        Button::new("editor-sudo-confirm", ts!("common.ok"))
                            .variant(ButtonVariant::Primary)
                            // While an attempt is in flight there is nothing to
                            // press: the answer is on its way and a second one
                            // would only queue behind it.
                            .disabled(prompt.busy)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                workspace.confirm_sudo_password(window, cx);
                            })),
                    ),
            );

        Some(
            div()
                .absolute()
                .inset_0()
                .child(modal(
                    "editor-sudo-prompt",
                    ts!("editor.sudo_title"),
                    px(400.),
                    body,
                    move |window, cx| {
                        this.update(cx, |workspace, cx| {
                            workspace.cancel_sudo_password(window, cx);
                        });
                    },
                ))
                .into_any_element(),
        )
    }
}
