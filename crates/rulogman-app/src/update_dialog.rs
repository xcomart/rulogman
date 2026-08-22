//! The update dialog: announcing a release, installing it, and reporting how
//! that went.
//!
//! Reached two ways, and the difference between them is the reason this is a
//! state machine rather than a message box.
//!
//! * The **start-up check** opens it already knowing there is something to say,
//!   so it appears in [`State::Announce`] with three ways out — Update, Ignore
//!   this version, Cancel. "Not now" and "not ever" are different answers, and
//!   collapsing them would either nag a user who has decided to stay put or
//!   silence one who merely had no time today.
//! * **Check for updates** opens it *before* there is an answer, in
//!   [`State::Checking`], and it becomes an announcement, an "up to date", or a
//!   failure depending on what GitHub says. That path deliberately ignores the
//!   remembered "ignore this version" tag: the user has just overruled it by
//!   asking. Ignoring it again is still on offer, from the same button as ever.
//!
//! Pressing Update moves into [`State::Busy`], where the dialog owns a download
//! and a swap running on the background executor and draws a progress bar.
//! There are no buttons in that state and `Escape` does nothing — see
//! [`UpdateDialog::close`] for why interrupting is not offered — and it ends
//! either in `cx.restart()` or in [`State::Failed`], which offers the browser
//! hand-off this dialog used to do unconditionally.
//!
//! # What belongs where
//!
//! The dialog owns its own state, its own background work, and the restart. The
//! shell owns settings, so "ignore this version" is an *event*: the tag travels
//! out and the shell writes the file. Nothing else crosses the line.
//!
//! Structurally still a twin of [`crate::about_dialog`] — same `open`/`close`/
//! `is_open` shape, same `Escape` handling, same "renders nothing while closed"
//! contract — so the shell wires it up the same way.

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent,
    Render, SharedString, Window, div, prelude::*, px, relative,
};

use crate::i18n::ts;
use crate::ui::{Button, ButtonVariant, modal, theme};
use crate::update::{self, Check, Progress, Release, release_url};

/// Width of the dialog panel. Matches the about dialog: both are a short
/// paragraph and a row of buttons.
const DIALOG_WIDTH: f32 = 420.;

/// Height of the download progress bar. Matches the file panel's, which is the
/// only other bar in the application.
const PROGRESS_BAR: f32 = 4.;

/// Version of the running binary, named beside the one on offer so the user can
/// see what the upgrade actually is.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Emitted by [`UpdateDialog`].
pub enum UpdateDialogEvent {
    /// The user asked never to be told about this release again. The shell
    /// persists the tag; the dialog has already closed itself.
    Ignored {
        /// The release tag, exactly as GitHub published it.
        tag: String,
    },
    /// The dialog was dismissed — by Cancel, by Close, by `Escape`, by the
    /// backdrop, or by the browser having been sent to the release page. The
    /// shell should restore focus.
    Dismissed,
}

/// What the dialog is doing, and with it whether it is on screen at all.
///
/// One enum rather than a `bool` beside a pile of options: every combination
/// this names is drawable, and no combination it cannot name is.
enum State {
    /// Not on screen.
    Closed,
    /// A check the user asked for is in flight.
    Checking,
    /// A newer release exists and the user has not answered yet.
    Announce(Release),
    /// The release is being downloaded and installed. No way out but the end.
    Busy {
        /// What is being installed, kept so a failure can still offer its page.
        release: Release,
        /// How far along it is.
        phase: Phase,
    },
    /// A check the user asked for found nothing newer.
    UpToDate,
    /// A check or an install did not complete.
    Failed {
        /// The release involved, when there was one. `None` for a check that
        /// never got far enough to name a release, which is what decides
        /// whether the "open the release page" button appears.
        release: Option<Release>,
        /// Untranslated technical detail, shown under the translated heading.
        /// See [`crate::update::install`] for why it is not translated.
        message: String,
    },
}

/// The two halves of an install, as the progress line reports them.
enum Phase {
    /// Bytes are arriving.
    Downloading {
        /// Bytes received so far.
        done: u64,
        /// Bytes expected in total; zero when the release did not say.
        total: u64,
    },
    /// The archive is being unpacked and moved into place.
    Installing,
}

/// Modal dialog for everything to do with updating.
///
/// Create it once with [`UpdateDialog::new`], keep the handle, subscribe to
/// [`UpdateDialogEvent`], and render it as the last child of a `relative()`
/// root. It renders nothing while [`UpdateDialog::is_open`] is `false`, so it is
/// safe to render unconditionally.
pub struct UpdateDialog {
    /// What the dialog is showing.
    state: State,
    /// Focus of the dialog root; also the anchor for the `Escape` handler.
    focus_handle: FocusHandle,
    /// Whether focus should move into the dialog on the next render.
    pending_focus: bool,
}

impl UpdateDialog {
    /// Builds the dialog, closed.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            state: State::Closed,
            focus_handle: cx.focus_handle(),
            pending_focus: false,
        }
    }

    /// Shows the dialog, announcing `release`.
    ///
    /// The start-up check's entry point: it has already decided there is
    /// something worth saying.
    pub fn open(&mut self, release: Release, cx: &mut Context<Self>) {
        self.state = State::Announce(release);
        self.pending_focus = true;
        cx.notify();
    }

    /// Shows the dialog and asks GitHub, from the menu item.
    ///
    /// Opens *before* the answer so the click has a visible effect on a slow
    /// connection, and so the user can walk away from the question by
    /// dismissing it. A check the user cancelled that way stays cancelled: the
    /// answer is applied only if the dialog is still in [`State::Checking`] when
    /// it lands.
    ///
    /// Deliberately does not consult the ignored-version tag. Asking is an
    /// override of the earlier "don't mention this again", and answering "up to
    /// date" to a user looking at an ignored release would be a lie.
    pub fn start_check(&mut self, cx: &mut Context<Self>) {
        self.state = State::Checking;
        self.pending_focus = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async { update::check_now() })
                .await;
            this.update(cx, |dialog, cx| dialog.report_check(outcome, cx))
                .ok();
        })
        .detach();
    }

    /// Whether the dialog is visible.
    pub fn is_open(&self) -> bool {
        !matches!(self.state, State::Closed)
    }

    /// Whether an install is running, and the dialog therefore refuses to go
    /// away.
    pub fn is_busy(&self) -> bool {
        matches!(self.state, State::Busy { .. })
    }

    /// Hides the dialog without emitting an event.
    ///
    /// A no-op while an install is running. The swap renames the installed copy
    /// aside before moving the new one in, so between those two renames there is
    /// no program at that path; a dialog that could be dismissed mid-way would
    /// invite the user to close the window — and with it the process — exactly
    /// then. There is nothing to gain from allowing it either: closing the
    /// dialog would not stop the background task, only hide it.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        self.state = State::Closed;
        self.pending_focus = false;
        cx.notify();
    }

    /// Closes the dialog and reports it, so the shell can restore focus.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        cx.emit(UpdateDialogEvent::Dismissed);
        self.close(cx);
    }

    /// Applies the answer to a check the user asked for.
    ///
    /// Ignored unless the dialog is still waiting for it: by the time GitHub
    /// answers, the user may have dismissed the dialog, or the start-up check
    /// may have put an announcement up in the meantime.
    fn report_check(&mut self, outcome: Check, cx: &mut Context<Self>) {
        if !matches!(self.state, State::Checking) {
            return;
        }
        self.state = match outcome {
            Check::Newer(release) => State::Announce(release),
            Check::UpToDate => State::UpToDate,
            Check::Failed(message) => State::Failed {
                release: None,
                message,
            },
        };
        cx.notify();
    }

    /// Starts downloading and installing `release`.
    ///
    /// The work is blocking — an HTTPS transfer, a `tar`, two renames — so it
    /// runs on the background executor and reports itself over a channel this
    /// task drains. The two are polled *together*, for the reason the file
    /// panel's transfer loop is: awaiting the install and reading the counts
    /// afterwards would show a frozen bar for the whole download.
    ///
    /// Success ends the process. `cx.restart()` spawns a watcher that waits for
    /// this pid to exit and starts the application again from the path
    /// [`update::install`] hands back — the one that now holds the build that
    /// was just installed — and then quits. The path is set explicitly because
    /// gpui's fallback is `current_exe()`, and on Linux that follows the
    /// renamed-aside old binary rather than the name it had.
    fn install(&mut self, release: Release, cx: &mut Context<Self>) {
        let total = release.asset.as_ref().map_or(0, |asset| asset.size);
        self.state = State::Busy {
            release: release.clone(),
            phase: Phase::Downloading { done: 0, total },
        };
        cx.notify();

        cx.spawn(async move |this, cx| {
            let (sender, mut progress) = mpsc::unbounded();
            let job = release;
            let install = cx.background_executor().spawn(async move {
                update::install(&job, &mut |step| {
                    // The receiver is dropped only when this task is, which
                    // happens only when the dialog is; a send that fails means
                    // nobody is drawing the bar any more, and the install
                    // still has to finish.
                    let _ = sender.unbounded_send(step);
                })
            });

            let install = futures::FutureExt::fuse(install);
            futures::pin_mut!(install);

            let outcome = loop {
                futures::select! {
                    outcome = install => break outcome,
                    step = progress.next() => {
                        let Some(step) = step else { continue };
                        if this
                            .update(cx, |dialog, cx| dialog.advance(step, cx))
                            .is_err()
                        {
                            // The dialog is gone, but the swap is not: abandoning
                            // it here would leave the installation half-renamed
                            // with nobody to finish it.
                            break install.await;
                        }
                    }
                }
            };

            match outcome {
                Ok(installed) => {
                    cx.update(|cx| {
                        cx.set_restart_path(installed);
                        cx.restart();
                    });
                }
                Err(message) => {
                    this.update(cx, |dialog, cx| dialog.fail(message, cx)).ok();
                }
            }
        })
        .detach();
    }

    /// Records how far the running install has got.
    ///
    /// Repaints only when the whole percent moves: the sender already throttles
    /// to one report per quarter-megabyte, and on a fast connection that is
    /// still dozens of frames a second for a bar a pixel wide.
    fn advance(&mut self, step: Progress, cx: &mut Context<Self>) {
        let State::Busy { phase, .. } = &mut self.state else {
            return;
        };
        let visible = match (&*phase, step) {
            (
                Phase::Downloading {
                    done: shown,
                    total: known,
                },
                Progress::Downloading { done, total },
            ) => percent(done, total) != percent(*shown, *known),
            // Any other transition is a change of wording, which always shows.
            _ => true,
        };
        *phase = match step {
            Progress::Downloading { done, total } => Phase::Downloading { done, total },
            Progress::Installing => Phase::Installing,
        };
        if visible {
            cx.notify();
        }
    }

    /// Moves a running install into its error state.
    fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        let release = match std::mem::replace(&mut self.state, State::Closed) {
            State::Busy { release, .. } | State::Announce(release) => Some(release),
            _ => None,
        };
        self.state = State::Failed { release, message };
        cx.notify();
    }

    /// What the primary button does for a release with no build to install.
    ///
    /// The original behaviour, kept for the platforms the project publishes no
    /// asset for and for a release whose assets do not include the expected
    /// name: send the browser to the page and get out of the way.
    fn open_page(&mut self, release: &Release, cx: &mut Context<Self>) {
        cx.open_url(release_url(release));
        self.dismiss(cx);
    }

    /// Reports the release as unwanted, then closes.
    ///
    /// The event carries the tag because the dialog is about to forget it, and
    /// the shell — which owns the settings — needs it to write the file.
    fn ignore(&mut self, release: &Release, cx: &mut Context<Self>) {
        cx.emit(UpdateDialogEvent::Ignored {
            tag: release.tag.clone(),
        });
        self.close(cx);
    }

    /// `Escape` dismisses the dialog from anywhere inside it.
    ///
    /// Equivalent to Cancel, never to Ignore: a key pressed to make a surprise
    /// go away must not commit the user to anything. Inert during an install,
    /// for the reason on [`UpdateDialog::close`].
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.is_open() && event.keystroke.key == "escape" {
            cx.stop_propagation();
            self.dismiss(cx);
        }
    }

    /// Moves focus into the dialog when it opens, so `Escape` reaches it.
    fn apply_pending_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.pending_focus {
            return;
        }
        self.pending_focus = false;
        let handle = self.focus_handle.clone();
        window.focus(&handle, cx);
        cx.notify();
    }

    /// The heading the panel wears in the current state.
    ///
    /// "Update available" is only true where there is one; the states a manual
    /// check can land in borrow the menu item's own wording instead, which is
    /// the question the user asked.
    fn title(&self) -> SharedString {
        match &self.state {
            State::Announce(_) | State::Busy { .. } => ts!("update.title"),
            State::Failed {
                release: Some(_), ..
            } => ts!("update.title"),
            _ => ts!("menu.check_updates"),
        }
    }

    /// The paragraph above the buttons.
    fn message(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let column = div().flex().flex_col().gap(px(6.));
        let line = |text: SharedString| div().text_size(px(13.)).text_color(theme.text).child(text);
        let note = |text: SharedString| {
            div()
                .text_size(px(12.))
                .text_color(theme.text_muted)
                .child(text)
        };

        match &self.state {
            // Never rendered: `render` leaves early while closed.
            State::Closed => div().into_any_element(),
            State::Checking => column
                .child(line(ts!("update.checking")))
                .into_any_element(),
            State::UpToDate => column
                .child(line(ts!("update.up_to_date")))
                .child(note(ts!("update.installed", version = CURRENT_VERSION)))
                .into_any_element(),
            State::Announce(release) => column
                .child(line(ts!("update.available", version = release.version)))
                .child(note(ts!("update.installed", version = CURRENT_VERSION)))
                .into_any_element(),
            State::Busy { release, phase } => {
                let (label, fraction, share) = match phase {
                    Phase::Downloading { done, total } => {
                        let percent = percent(*done, *total);
                        (
                            ts!("update.downloading"),
                            f32::from(percent) / 100.,
                            Some(SharedString::from(format!("{percent}%"))),
                        )
                    }
                    // A swap is two renames and an unpack; there is no honest
                    // fraction to draw, so the bar is left full and the wording
                    // carries the state.
                    Phase::Installing => (ts!("update.installing"), 1., None),
                };
                column
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .gap(px(8.))
                            .child(line(label))
                            .children(share.map(note)),
                    )
                    .child(note(ts!("update.available", version = release.version)))
                    // Same recipe as the file panel's transfer bar: a hairline
                    // track in the border colour, so an idle-looking bar reads
                    // as part of the frame, with only the accent fill claiming
                    // attention.
                    .child(
                        div()
                            .flex_none()
                            .w_full()
                            .h(px(PROGRESS_BAR))
                            .rounded_sm()
                            .bg(theme.border)
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(fraction))
                                    .rounded_sm()
                                    .bg(theme.accent),
                            ),
                    )
                    .into_any_element()
            }
            State::Failed { message, .. } => column
                .child(line(ts!("update.failed")))
                .child(note(SharedString::from(message.clone())))
                .into_any_element(),
        }
    }

    /// The button row for the current state.
    fn buttons(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity();
        let row = div().flex().flex_row().justify_end().gap(px(8.));

        let close = |id: &'static str, label: SharedString, variant: ButtonVariant| {
            let this = this.clone();
            Button::new(id, label)
                .variant(variant)
                .on_click(move |_, _window, cx| {
                    this.update(cx, |dialog, cx| dialog.dismiss(cx));
                })
        };

        match &self.state {
            State::Closed => div().into_any_element(),
            // A check in flight can be walked away from; the answer is dropped
            // when it lands, rather than reopening a dialog the user closed.
            State::Checking => row
                .child(close(
                    "update-cancel",
                    ts!("common.cancel"),
                    ButtonVariant::Secondary,
                ))
                .into_any_element(),
            State::UpToDate => row
                .child(close(
                    "update-close",
                    ts!("common.close"),
                    ButtonVariant::Primary,
                ))
                .into_any_element(),
            // Cancel first, then the two commitments, with the recommended one
            // last and filled: the same left-to-right ordering the other
            // dialogs use for "back out" against "go ahead".
            State::Announce(release) => row
                .child(close(
                    "update-cancel",
                    ts!("common.cancel"),
                    ButtonVariant::Secondary,
                ))
                .child(
                    Button::new("update-ignore", ts!("update.ignore"))
                        .variant(ButtonVariant::Secondary)
                        .on_click({
                            let this = this.clone();
                            let release = release.clone();
                            move |_, _window, cx| {
                                let release = release.clone();
                                this.update(cx, |dialog, cx| dialog.ignore(&release, cx));
                            }
                        }),
                )
                .child(
                    Button::new("update-install", ts!("update.update"))
                        .variant(ButtonVariant::Primary)
                        .on_click({
                            let this = this.clone();
                            let release = release.clone();
                            move |_, _window, cx| {
                                let release = release.clone();
                                this.update(cx, |dialog, cx| {
                                    if release.asset.is_some() {
                                        dialog.install(release, cx);
                                    } else {
                                        // Nothing published for this platform:
                                        // hand off to the browser, as this
                                        // dialog always used to.
                                        dialog.open_page(&release, cx);
                                    }
                                });
                            }
                        }),
                )
                .into_any_element(),
            // No way out of an install but the end of it.
            State::Busy { .. } => div().into_any_element(),
            State::Failed { release, .. } => row
                .children(release.as_ref().map(|release| {
                    let this = this.clone();
                    let release = release.clone();
                    Button::new("update-page", ts!("update.open_release"))
                        .variant(ButtonVariant::Secondary)
                        .on_click(move |_, _window, cx| {
                            let release = release.clone();
                            this.update(cx, |dialog, cx| dialog.open_page(&release, cx));
                        })
                }))
                .child(close(
                    "update-close",
                    ts!("common.close"),
                    ButtonVariant::Primary,
                ))
                .into_any_element(),
        }
    }
}

/// How far along a download is, as a whole percent.
///
/// A release that reported no size has no fraction to show; treating it as
/// finished would draw a full bar over a download that has not started, so it
/// reads as zero until the bytes stop arriving.
fn percent(done: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    // The early return is not only the "more arrived than promised" case: it is
    // also what keeps the multiplication below from saturating, which on a
    // implausibly large byte count would answer 1% for a finished download.
    if done >= total {
        return 100;
    }
    let percent = done.saturating_mul(100) / total;
    u8::try_from(percent.min(100)).unwrap_or(100)
}

impl EventEmitter<UpdateDialogEvent> for UpdateDialog {}

impl Focusable for UpdateDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for UpdateDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.is_open() {
            return div().id("update-dialog");
        }

        self.apply_pending_focus(window, cx);

        let title = self.title();
        let body = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(self.message(cx))
            .child(self.buttons(cx));

        let on_dismiss = {
            let this = cx.entity();
            move |_window: &mut Window, cx: &mut App| {
                this.update(cx, |dialog, cx| dialog.dismiss(cx));
            }
        };

        // Absolute and full-size for the same reason as the other dialogs: an
        // absolutely positioned child is laid out against its direct parent.
        div()
            .id("update-dialog")
            .absolute()
            .inset_0()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(modal(
                "update-modal",
                title,
                px(DIALOG_WIDTH),
                body,
                on_dismiss,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percentage_covers_both_ends_and_the_unknown_size() {
        assert_eq!(percent(0, 1000), 0);
        assert_eq!(percent(500, 1000), 50);
        assert_eq!(percent(1000, 1000), 100);
        // More than promised is still finished, not 240%.
        assert_eq!(percent(2400, 1000), 100);
        // A release that named no size cannot be drawn as complete.
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(9_999, 0), 0);
        // The multiplication has to survive an implausible byte count.
        assert_eq!(percent(u64::MAX, u64::MAX), 100);
    }
}
