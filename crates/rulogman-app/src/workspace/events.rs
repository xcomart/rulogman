//! Routes child events to workspace operations.
use super::*;

impl Workspace {
    pub(super) fn on_dialog_event(
        &mut self,
        dialog: &Entity<ConnectionDialog>,
        event: &ConnectionDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ConnectionDialogEvent::Connect { profile, auth } => {
                dialog.update(cx, |dialog, cx| dialog.close(cx));
                // The dialog says "connect" and nothing more, so what
                // the connection is *for* has to be remembered on this
                // side: a form opened by [`Workspace::open_tail`]
                // finishes that request rather than opening a shell the
                // user never asked for. The id has to match — the form
                // can be pointed at another connection while it is up,
                // and that is a different request, which discards this
                // one rather than following the wrong host's log.
                // A dashboard waiting on this connection is the same kind of
                // request, and carries on to the next connection or opens.
                if self.answer_pending_dashboard(profile.id, auth, window, cx) {
                    return;
                }
                match self.pending_tail.take().filter(|(id, _)| *id == profile.id) {
                    Some((_, path)) => {
                        self.open_tail_session(profile.clone(), auth.clone(), path, window, cx)
                    }
                    None => self.open_session(profile.clone(), auth.clone(), window, cx),
                }
            }
            #[cfg(unix)]
            ConnectionDialogEvent::ConnectLocal => {
                dialog.update(cx, |dialog, cx| dialog.close(cx));
                self.open_local_session(window, cx);
            }
            #[cfg(windows)]
            ConnectionDialogEvent::ConnectLocalShell(shell) => {
                dialog.update(cx, |dialog, cx| dialog.close(cx));
                self.open_local_command(
                    shell.name.clone(),
                    shell.command.clone(),
                    shell.filesystem.clone(),
                    window,
                    cx,
                );
            }
            ConnectionDialogEvent::Dismissed => {
                dialog.update(cx, |dialog, cx| dialog.close(cx));
                // A followed file the form was opened for is dropped
                // with the form: the user answered the question by
                // walking away from it.
                self.pending_tail = None;
                self.pending_dashboard = None;
                self.focus_active(window, cx);
            }
        }
    }

    pub(super) fn on_settings_event(
        &mut self,
        dialog: &Entity<SettingsDialog>,
        event: &SettingsDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // The dialog has already replaced and persisted the settings
            // global by the time it emits this; the shell re-applies the
            // parts that touch live windows and sessions.
            SettingsDialogEvent::Applied => {
                // The dashboards are edited in that dialog and written by
                // it, so this is the moment the window's copy of them stops
                // describing the file. Before the refocus and the redraw,
                // so the welcome screen's next frame is the new list — and
                // before the settings are applied, which hands the menu bar
                // the list again.
                self.reload_dashboards(cx);
                self.apply_settings(window, cx);
                // The settings are one answer for the application, not for
                // the window they were saved in: every other window has to
                // come back in the new theme and the new language too.
                apply_settings_elsewhere(window, cx);
                // The dialog closes itself after applying; without a refocus
                // the window focus dangles on its unrendered controls and
                // macOS disables every menu item validated through it.
                self.focus_active(window, cx);
            }
            // The same work, minus the refocus: the dialog is still open and
            // the user is still typing in it, so taking the focus back to
            // the terminal here would pull it out from under them.
            SettingsDialogEvent::ThemesChanged => {
                self.apply_settings(window, cx);
                apply_settings_elsewhere(window, cx);
            }
            SettingsDialogEvent::Dismissed => {
                dialog.update(cx, |dialog, cx| dialog.close(cx));
                self.focus_active(window, cx);
            }
        }
    }

    pub(super) fn on_about_event(
        &mut self,
        dialog: &Entity<AboutDialog>,
        event: &AboutDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            AboutDialogEvent::Dismissed => {
                dialog.update(cx, |dialog, cx| dialog.close(cx));
                self.focus_active(window, cx);
            }
        }
    }

    pub(super) fn on_update_event(
        &mut self,
        dialog: &Entity<UpdateDialog>,
        event: &UpdateDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            UpdateDialogEvent::Ignored { tag } => {
                // The dialog has already closed itself; writing the file
                // goes through the policy installed in `main`, because the
                // application is what owns the settings.
                shell_update::remember_ignored(tag, cx);
                self.focus_active(window, cx);
            }
            UpdateDialogEvent::Installed(_) => {
                // The new build is on disk and the restart is the
                // application's to perform. The path is named explicitly:
                // the swap renames the running image aside, so on Linux
                // gpui's own fallback — `current_exe()` — would follow it
                // and come back on the *old* build. The shell recorded the
                // right answer when `rugpui_shell::init` installed the
                // identity, which is before anything could move it. The
                // dialog stays on screen: the process is about to go, and
                // closing it first would flash the window back into view
                // for a fraction of a second.
                if let Some(path) = rugpui_shell::restart_path() {
                    cx.set_restart_path(path);
                }
                cx.restart();
            }
            UpdateDialogEvent::Dismissed => {
                dialog.update(cx, |dialog, cx| dialog.close(cx));
                self.focus_active(window, cx);
            }
        }
    }

    pub(super) fn on_panel_event(
        &mut self,
        _panel: &Entity<FilePanel>,
        event: &FilePanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let FilePanelEvent::OpenEditor(opened) = event;
        self.open_editor(opened, window, cx);
    }
}
