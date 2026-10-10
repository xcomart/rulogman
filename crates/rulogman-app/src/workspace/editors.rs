//! Editors.

use super::*;

impl Workspace {
    /// The pane already editing `path` on `session`, if there is one.
    pub(super) fn pane_of_file(
        &self,
        session: EntityId,
        path: &str,
        cx: &App,
    ) -> Option<(usize, PaneId)> {
        self.tabs.iter().enumerate().find_map(|(index, tab)| {
            tab.panes.leaves().into_iter().find_map(|(pane, leaf)| {
                let PaneView::Editor(editor) = &leaf.view else {
                    return None;
                };
                let editor = editor.read(cx);
                // Both halves matter: the same path on two hosts is two files,
                // and one host's file opened from two tabs is still one file.
                (editor.session().entity_id() == session && editor.path() == path)
                    .then_some((index, pane))
            })
        })
    }

    /// The tab and pane a view is rendered in, wherever it is.
    pub(super) fn locate_pane(&self, view: EntityId) -> Option<(usize, PaneId)> {
        self.tabs
            .iter()
            .enumerate()
            .find_map(|(index, tab)| tab.pane_of(view).map(|pane| (index, pane)))
    }

    /// Closes an editor pane, asking first if it holds unsaved changes.
    pub(super) fn close_editor_pane(
        &mut self,
        editor: &Entity<EditorPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((index, pane)) = self.locate_pane(editor.entity_id()) else {
            return;
        };
        if editor.read(cx).is_dirty(cx) {
            self.ask_before_closing(pane, window, cx);
            return;
        }
        self.remove_pane(index, pane, window, cx);
    }

    /// Closes an editor pane whose save-and-close write has landed.
    ///
    /// No unsaved-changes check on the way through, unlike
    /// [`Self::close_editor_pane`]: the pane only reports this once a write that
    /// covered every edit in it succeeded, so asking the question again would be
    /// asking about something the save already answered. A pane that has gone
    /// while the bytes were in flight — its tab closed from the strip — is
    /// simply not there to close, and there is nothing to say about it.
    pub(super) fn close_saved_editor_pane(
        &mut self,
        editor: &Entity<EditorPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((index, pane)) = self.locate_pane(editor.entity_id()) else {
            return;
        };
        self.remove_pane(index, pane, window, cx);
    }

    /// Puts the unsaved-changes question up over `pane`.
    ///
    /// The keyboard comes to the workspace itself while it stands, for the same
    /// reason every dialog takes it: <kbd>Esc</kbd> has to reach the handler
    /// that cancels the question rather than the editor underneath, which binds
    /// the key for its own find bar.
    pub(super) fn ask_before_closing(
        &mut self,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_confirm = Some(pane);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Closes the pane whose close was confirmed, unsaved changes and all.
    pub(super) fn confirm_close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.close_confirm.take() else {
            return;
        };
        cx.notify();
        // The pane can have gone while the question stood — its tab closed from
        // the strip — in which case there is nothing left to close.
        let Some(index) = self.tabs.iter().position(|tab| tab.panes.contains(pane)) else {
            return;
        };
        self.remove_pane(index, pane, window, cx);
    }

    /// Hands the pane the close question was about the save it just offered, and
    /// takes the question down.
    ///
    /// The question goes on the press rather than staying up over the transfer:
    /// the pane's own header already says a save is running, and holding a modal
    /// over a write that may be crossing an SSH session would block the whole
    /// window on the slowest thing in it. Everything after this is the pane's —
    /// see [`EditorPane::save_and_close`] — and the only way back here is
    /// [`EditorPaneEvent::SavedForClose`], which arrives only if the write
    /// landed and covered every edit. A failure never returns: it stays on the
    /// pane, under the file it is about.
    pub(super) fn save_and_close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.close_confirm.take() else {
            return;
        };
        cx.notify();
        // The pane can have gone while the question stood — its tab closed from
        // the strip — in which case there is nothing left to save.
        let editor =
            self.tabs
                .iter()
                .find_map(|tab| match tab.panes.get(pane).map(|leaf| &leaf.view) {
                    Some(PaneView::Editor(editor)) => Some(editor.clone()),
                    _ => None,
                });
        let Some(editor) = editor else {
            return;
        };
        editor.update(cx, |editor, cx| editor.save_and_close(cx));
        // Back into the file, which stays open and editable while the bytes are
        // in flight — and which stays for good if they never arrive.
        self.focus_active(window, cx);
    }

    /// Puts the close question away, leaving the pane open.
    ///
    /// Deliberately does *not* restore the focus: the two callers want different
    /// things done with it, and only they know which — the button hands it back
    /// to the pane the question was about, while `Escape` goes through the
    /// dismissal path every other overlay uses.
    pub(super) fn cancel_close_editor(&mut self, cx: &mut Context<Self>) {
        if self.close_confirm.take().is_some() {
            cx.notify();
        }
    }

    /// Puts up the password question an editor pane asked for.
    ///
    /// The pane is held rather than looked up again later, and the field is
    /// built here rather than kept on the workspace, so that the whole question
    /// — what it is for, what has been typed into it, what the last attempt was
    /// told — lives and dies together. See [`SudoPrompt`].
    ///
    /// The field takes the keyboard at once: there is one thing to do with this
    /// dialog and it is to type, and a modal that has to be clicked into first
    /// is a modal that has interrupted the user twice.
    pub(super) fn ask_sudo_password(
        &mut self,
        pane: Entity<EditorPane>,
        purpose: RootPurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Every other modal opens through here, and this one has to as well:
        // two questions on one screen would leave the user answering whichever
        // was drawn last.
        self.close_overlays(cx);

        let workspace = cx.weak_entity();
        let input = cx.new(|cx| {
            TextInput::new(cx)
                .context_menu(input_menu_labels)
                .masked(true)
                .tab_index(0)
                .on_submit({
                    move |password, window, cx| {
                        let (workspace, password) = (workspace.clone(), password.to_owned());
                        // Deferred for the reason the connection dialog defers its
                        // own submit: this fires from inside the field's `update`,
                        // so the field is leased out of the entity map, and the
                        // answer below may well be the thing that drops it.
                        window.defer(cx, move |window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    workspace.submit_sudo_password(password, window, cx);
                                })
                                .ok();
                        });
                    }
                })
        });
        let handle = input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);

        self.sudo_prompt = Some(SudoPrompt {
            pane,
            purpose,
            input,
            remember: false,
            error: None,
            busy: false,
        });
        cx.notify();
    }
}
