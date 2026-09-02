//! One followed file, in a pane of its own.
//!
//! A tail pane is a [`TerminalView`] with a strip above it. That is the whole
//! of the type: the session on the other end runs a `tail -F` rather than a
//! shell (see [`Session::new_tail`]), and everything a terminal already does —
//! decoding the output, keeping the scrollback, selecting and copying out of
//! it, drawing the connection overlay and its *Reconnect* button — is exactly
//! what a reader of a log wants and is not reimplemented here.
//!
//! What the strip is for is the one thing the grid cannot say: *which* file.
//! The tab strip says it too, but only as a file name — `access.log` tells two
//! panes apart until the day the user follows `/var/log/nginx/access.log` and
//! `/srv/app/log/access.log` at once. So the pane carries the path, and carries
//! as much of it as will fit: [`abbreviate_path`] shortens directory names to
//! their first letter, from the left, until the line fits the pane, which keeps
//! the end of the path — the part that identifies the file — on screen at every
//! width. The whole path is always one hover away, in the tooltip.
//!
//! And the strip says *whose* file, at its right end and subdued: a dashboard
//! puts panes from several connections in one tab, at which point two panes
//! both reading `/var/log/nginx/access.log` are told apart by nothing but the
//! name of the connection each one reads it over. Subdued, and second in the
//! reading order, because the path is what a reader is looking for and the
//! host is the tiebreak — and absent altogether when the caller has no name to
//! give, so a pane opened on its own looks exactly as it always did.
//!
//! # Why the width is a frame old
//!
//! Whether a candidate fits can only be answered by the text shaper, and the
//! shaper needs a [`Window`]; how much room there is to fit into can only be
//! answered by the layout, which has not run when [`Render::render`] is called.
//! The two never meet in one pass, so the pane measures its own header with a
//! [`canvas`] probe and abbreviates the *next* frame against what it found —
//! and asks for that frame itself, but only when the width actually changed, so
//! a resize settles in one extra frame and a steady pane repaints no more often
//! than the log gives it reason to.

use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, Font, MouseButton,
    MouseDownEvent, Pixels, SharedString, Subscription, TextRun, Window, canvas, div, prelude::*,
    px,
};
use rugpui::{theme, tooltip_label};

use crate::session::Session;
use crate::terminal_view::{PaneCapsSource, PaneFocused, ReconnectRequested, TerminalView};

/// Height of the strip above the grid.
///
/// The same height an open file's header has, and deliberately: the two are the
/// same piece of furniture — a line of chrome naming what the pane below is
/// showing — and a tab strip whose panes disagreed by two pixels about where
/// their content starts would look like a mistake rather than like a choice.
const HEADER_HEIGHT: f32 = 26.;

/// Size of the path text in the strip.
///
/// Chrome-sized rather than terminal-sized: the strip belongs to the
/// application, and the font it is measured in has to be the font it is drawn
/// in, which is why this is a constant both [`TailView::render`] and its probe
/// read rather than something the style sets on its own.
const HEADER_TEXT_SIZE: f32 = 11.;

/// A pane following one file on a remote host.
pub struct TailView {
    /// The grid the file scrolls through, and everything that goes with it.
    terminal: Entity<TerminalView>,
    /// The session behind that grid.
    ///
    /// Held here as well as inside the terminal view, so that the workspace can
    /// ask a leaf which session it is without reading two entities to find out;
    /// it is a handle into the entity map, so the second copy costs a count.
    session: Entity<Session>,
    /// The file being followed, as the profile spells it.
    ///
    /// The path is fixed for the life of the pane — a followed file is what the
    /// session *is* — so it is kept rather than re-read from the session on
    /// every frame.
    path: SharedString,
    /// Name of the connection the file is being read over, or empty when the
    /// caller had none to give.
    ///
    /// Kept here rather than read off the session for the reason [`Self::path`]
    /// is: it cannot change while the pane lives, and the header is drawn on
    /// every frame the log gives it.
    connection: SharedString,
    /// How wide the path had to fit when it was last drawn, or `None` until the
    /// probe has reported once. See the module header for why this is a frame
    /// behind.
    path_width: Option<Pixels>,
    /// Passes the grid's *this pane was clicked* on to the workspace.
    _clicked: Subscription,
    /// Passes the grid's *Reconnect* on to the workspace.
    _reconnect: Subscription,
}

impl TailView {
    /// Wraps `terminal` — which must be showing a session built by
    /// [`Session::new_tail`] — in a header naming `path`, and `connection`
    /// after it.
    ///
    /// `connection` is the profile's name, and may be empty: a pane that is the
    /// only one in its tab has nothing to be told apart from, and the strip
    /// then carries the path alone.
    ///
    /// The two events the workspace wires a pane up with are re-emitted rather
    /// than listened for on the inner view, because the workspace knows this
    /// pane by *this* entity: a focus reported by the grid would name a surface
    /// no leaf answers to. Re-emitting keeps [`crate::Workspace::new_tail_pane`]
    /// a copy of its terminal counterpart, event for event.
    pub fn new(
        terminal: Entity<TerminalView>,
        session: Entity<Session>,
        path: String,
        connection: SharedString,
        cx: &mut Context<Self>,
    ) -> Self {
        let clicked = cx.subscribe(&terminal, |_this, _view, _: &PaneFocused, cx| {
            cx.emit(PaneFocused);
        });
        let reconnect = cx.subscribe(&terminal, |_this, _view, _: &ReconnectRequested, cx| {
            cx.emit(ReconnectRequested);
        });

        Self {
            terminal,
            session,
            path: SharedString::from(path),
            connection,
            path_width: None,
            _clicked: clicked,
            _reconnect: reconnect,
        }
    }

    /// The session this pane is following a file over.
    pub fn session(&self) -> &Entity<Session> {
        &self.session
    }

    /// Rebinds the grid to the window this pane has been moved into.
    ///
    /// Pure delegation: everything window-bound in a tail pane belongs to the
    /// terminal underneath it — see [`TerminalView::rebind`] for what and why —
    /// and the header holds nothing but a path and a width.
    pub fn rebind(&mut self, caps: PaneCapsSource, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal
            .update(cx, |view, cx| view.rebind(caps, window, cx));
    }

    /// Records the width the header's path slot was given, and asks for a
    /// repaint if it has changed.
    ///
    /// Only on a change, which is what keeps this from being a loop: the frame
    /// this notification asks for measures the same width and stops here.
    fn measured(&mut self, width: Pixels, cx: &mut Context<Self>) {
        if self.path_width == Some(width) {
            return;
        }
        self.path_width = Some(width);
        cx.notify();
    }
}

impl EventEmitter<PaneFocused> for TailView {}

impl EventEmitter<ReconnectRequested> for TailView {}

impl Focusable for TailView {
    /// The grid's own handle, so that the keyboard lands where the selection,
    /// the scrollback and the copy shortcut already are. The header is chrome
    /// and takes no focus of its own.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.terminal.read(cx).focus_handle(cx)
    }
}

impl Render for TailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let full = self.path.clone();
        // The font the strip is drawn in, taken off the style stack rather than
        // named here: the application's UI font is the settings' business, and
        // a measurement made in another one would fit text that then does not.
        let font = window.text_style().font();
        let font_size = px(HEADER_TEXT_SIZE);
        let shown = match self.path_width {
            Some(width) => SharedString::from(abbreviate_path(&full, |candidate| {
                measure(candidate, &font, font_size, window) <= width
            })),
            // The opening frame, with nothing measured yet: the whole path,
            // truncated by the style if it is too long. The probe reports
            // during that very frame's prepaint, so the abbreviated form is on
            // screen by the next one.
            None => full.clone(),
        };

        let this = cx.entity();
        // Sized and positioned to be exactly the slot it measures, and painting
        // nothing: it is here for its bounds alone.
        let probe = canvas(
            move |bounds: Bounds<Pixels>, _window, cx| {
                this.update(cx, |view, cx| view.measured(bounds.size.width, cx));
            },
            |_bounds, (), _window, _cx| {},
        )
        .absolute()
        .size_full();

        let header = div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h(px(HEADER_HEIGHT))
            .px(px(8.))
            .bg(theme.surface)
            .border_b_1()
            .border_color(theme.border)
            .text_size(font_size)
            .text_color(theme.text)
            .child(
                div()
                    .id("tail-path")
                    .relative()
                    .flex_1()
                    .min_w_0()
                    // The one place the path is never shortened, whatever the
                    // pane's width: a tooltip is one line by construction and a
                    // path is one line by nature, so the two fit each other.
                    .tooltip(tooltip_label(full))
                    .child(div().min_w_0().truncate().child(shown))
                    .child(probe),
            )
            // Never abbreviated and never given up: it is short by nature — a
            // profile name, not a path — and it is the whole of what the right
            // end of the strip is for. `flex_none` is what makes the path the
            // side that yields when the pane is narrow.
            .children((!self.connection.is_empty()).then(|| {
                div()
                    .flex_none()
                    .pl(px(8.))
                    .text_color(theme.text_muted)
                    .child(self.connection.clone())
            }));

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .min_h_0()
            // A press anywhere on the pane — the header included — belongs to
            // the grid, which is where every command this pane answers to
            // lives. Propagation is left alone, so a press on the grid itself
            // still starts a selection there.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _: &MouseDownEvent, window, cx| {
                    let handle = view.terminal.read(cx).focus_handle(cx);
                    window.focus(&handle, cx);
                    cx.emit(PaneFocused);
                }),
            )
            .child(header)
            .child(div().flex_1().min_h_0().child(self.terminal.clone()))
    }
}

/// Width `text` occupies in `font` at `font_size`.
///
/// The shaper rather than a character count, because the strip is drawn in a
/// proportional font and a path is full of the narrowest glyphs there are —
/// `/`, `.`, `l`, `i` — which a count would charge the same as an `m`.
fn measure(text: &str, font: &Font, font_size: Pixels, window: &mut Window) -> Pixels {
    if text.is_empty() {
        return px(0.);
    }
    let run = TextRun {
        len: text.len(),
        font: font.clone(),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(SharedString::from(text.to_owned()), font_size, &[run], None)
        .width
}

/// The most of `path` that `fits` will take, shortening directory names from
/// the left.
///
/// `/var/log/nginx/access.log` becomes `/v/log/nginx/access.log`, then
/// `/v/l/nginx/access.log`, then `/v/l/n/access.log` — one component per step,
/// and the first candidate that fits is the answer. Leftmost first because the
/// end of a path is the part that identifies the file: `/v/l/n/access.log` is
/// still recognisable, `/var/log/nginx/a` is not. The last component is never
/// touched at all, for the same reason.
///
/// A path that already fits comes back untouched, and one that does not fit
/// even fully abbreviated comes back fully abbreviated — the caller draws it
/// with an ellipsis, which is the honest answer when there is genuinely no room.
///
/// `fits` rather than a width, so that the rule can be asserted against a
/// counted-characters stand-in without a window, a font or a shaper; see the
/// tests below. It is called at most once per directory component, which
/// matters because each call shapes a line.
fn abbreviate_path<F>(path: &str, mut fits: F) -> String
where
    F: FnMut(&str) -> bool,
{
    if fits(path) {
        return path.to_owned();
    }

    let mut parts: Vec<&str> = path.split('/').collect();
    // Everything but the last component, which is the file name. A path with no
    // separator in it is one component, so this is zero and nothing is done to
    // it — there is no directory to give up.
    let directories = parts.len().saturating_sub(1);
    for index in 0..directories {
        let Some(initial) = first_char(parts[index]) else {
            // An empty component: the leading one of an absolute path, or a
            // doubled separator. Neither has a letter to keep, and neither is
            // any shorter for being visited.
            continue;
        };
        if initial.len() == parts[index].len() {
            // Already one character. Shortening it to itself would cost a
            // measurement and change nothing.
            continue;
        }
        parts[index] = initial;
        let candidate = parts.join("/");
        if fits(&candidate) {
            return candidate;
        }
    }

    parts.join("/")
}

/// The first character of `component`, as a slice of it, or `None` when there
/// is none.
///
/// A `char` and not a byte, so a path whose directories are spelled in Korean
/// or Greek is abbreviated to a letter rather than to the first third of one.
fn first_char(component: &str) -> Option<&str> {
    let first = component.chars().next()?;
    Some(&component[..first.len_utf8()])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path with room to spare on every side.
    const LOG: &str = "/var/log/nginx/access.log";

    /// A stand-in for the shaper: every character one unit wide.
    ///
    /// The rule under test is about which components are given up and in what
    /// order, which is the same rule at any font; counting characters is what
    /// lets the expectations below be read as the strings they are.
    fn narrower_than(limit: usize) -> impl FnMut(&str) -> bool {
        move |candidate: &str| candidate.chars().count() <= limit
    }

    #[test]
    fn a_path_that_fits_is_left_exactly_as_it_is() {
        assert_eq!(abbreviate_path(LOG, |_| true), LOG);
    }

    #[test]
    fn components_are_given_up_one_at_a_time_from_the_left() {
        // Just too narrow for the whole path (25), wide enough for the first
        // component's initial (23).
        assert_eq!(
            abbreviate_path(LOG, narrower_than(23)),
            "/v/log/nginx/access.log"
        );
        // One more step, and no further: `nginx` is still whole.
        assert_eq!(
            abbreviate_path(LOG, narrower_than(21)),
            "/v/l/nginx/access.log"
        );
    }

    #[test]
    fn a_pane_with_no_room_gets_every_directory_abbreviated() {
        assert_eq!(
            abbreviate_path(LOG, narrower_than(20)),
            "/v/l/n/access.log",
            "the last candidate that fits was not the shortest one"
        );
        // And when even that does not fit, the shortest form is what comes
        // back: the caller draws it with an ellipsis rather than being handed
        // a path it has already been told is too wide.
        assert_eq!(abbreviate_path(LOG, |_| false), "/v/l/n/access.log");
    }

    #[test]
    fn the_file_name_is_never_abbreviated() {
        // Not even when it is the only component there is, and not even when
        // nothing fits: a pane naming `a` instead of `access.log` names
        // nothing.
        assert_eq!(abbreviate_path("access.log", |_| false), "access.log");
        assert_eq!(abbreviate_path("/access.log", |_| false), "/access.log");
        assert!(
            abbreviate_path(LOG, |_| false).ends_with("/access.log"),
            "the file name was shortened away"
        );
    }

    #[test]
    fn a_component_spelled_in_another_script_keeps_its_first_letter() {
        // The bytes of `데` are three; the character is one. Slicing by byte
        // would panic here, and slicing by the wrong count would produce a
        // component that is not text.
        assert_eq!(
            abbreviate_path("/데이터/로그/서버.log", |_| false),
            "/데/로/서버.log"
        );
    }

    #[test]
    fn nothing_is_spent_on_a_component_that_cannot_get_shorter() {
        // A doubled separator and a one-letter directory: neither has anything
        // to give, and the run of candidates must not stall on them.
        assert_eq!(abbreviate_path("//a/opt/x.log", |_| false), "//a/o/x.log");
    }
}
