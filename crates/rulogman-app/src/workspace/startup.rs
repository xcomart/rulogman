//! Background work initiated once the workspace is constructed.
use super::*;

impl Workspace {
    pub(super) fn start_background_work(cx: &mut Context<Self>) {
        // Off the UI thread and off the critical path of the first frame:
        // `wsl.exe` is a process spawn, and the welcome screen has plenty to
        // show without it. The buttons appear underneath the fixed ones when
        // the answer lands, which is well before a user reaches for them.
        //
        // Run once and handed to both places that offer a local shell — the
        // welcome screen from the field, the connection dialog from its own
        // copy — rather than discovered twice for the same answer.
        #[cfg(windows)]
        cx.spawn(async move |this, cx| {
            let distros = cx
                .background_executor()
                .spawn(async { wsl::list_distros() })
                .await;
            this.update(cx, |workspace, cx| {
                workspace.dialog.update(cx, |dialog, cx| {
                    dialog.set_wsl_distros(&distros, cx);
                });
                workspace.wsl_distros = distros;
                cx.notify();
            })
            .ok();
        })
        .detach();

        // The update check, likewise off the UI thread: it is an HTTPS request
        // to GitHub, and nothing on screen waits for it. The tag the user may
        // have ignored is read here, on the UI thread, because the settings
        // global is only reachable from it.
        //
        // The answer opens a dialog, so it deliberately does *not* go through
        // `open_about`'s `close_overlays` route: this is the one dialog nobody
        // asked for, arriving at a moment nobody chose, and it must never take
        // the screen from something the user opened themselves — a half-typed
        // connection form above all. If anything is already up, the check simply
        // says nothing and tries again next launch.
        //
        // The guard is here, in this crate, rather than inside the check:
        // `cfg!(test)` compiled into a dependency is that dependency's build,
        // so `rugpui_shell::update::check` cannot tell a test build of *this*
        // crate from a release one, and every test that opens a window would
        // otherwise make a real request to GitHub.
        //
        // And once per process, not once per window: the check belongs to the
        // launch rather than to a window, so a second window opened from the
        // menu must not ask GitHub again — see [`claim_startup_check`].
        let ignored = shell_update::ignored_release(cx);
        if !cfg!(test) && claim_startup_check(cx) {
            cx.spawn(async move |this, cx| {
                let found = cx
                    .background_executor()
                    .spawn(async move { shell_update::check(ignored.as_deref()) })
                    .await;
                let Some(release) = found else {
                    return;
                };
                this.update(cx, |workspace, cx| {
                    if workspace.dialog_open(cx) {
                        log::debug!("update {} announced while a dialog is open", release.tag);
                        return;
                    }
                    workspace.update.update(cx, |dialog, cx| {
                        dialog.open(release, cx);
                    });
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }
}
