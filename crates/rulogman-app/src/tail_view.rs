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
//! The strip is also the pane's handle. It can be painted a color of the
//! user's choosing — one of a fixed palette, or any `#rrggbb` — which is what
//! makes one pane of a crowded dashboard findable at a glance; and it
//! can be dragged onto another pane of the same tab, where the two trade
//! places. Neither is decided here: a right-click asks the workspace for the
//! menu ([`HeaderMenuRequested`]), and a drop is the workspace's to answer,
//! because only the workspace knows which tab the two panes are in.
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

use std::rc::Rc;

use gpui::{
    App, Bounds, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Font, Hsla,
    MouseButton, MouseDownEvent, Pixels, Point, SharedString, Subscription, TextRun, Window,
    canvas, div, prelude::*, px, rgb,
};
use rugpui::{Theme, parse_hex, theme, tooltip_label};

use crate::highlight::Highlighter;
use crate::session::Session;
use crate::terminal_view::{
    EditConnectionRequested, PaneCapsSource, PaneFocused, ReconnectRequested, TerminalView,
};

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

/// The palette a header can be painted from: the name each color is stored
/// under, the color itself, and the translation key it is listed by.
///
/// Fixed colors rather than the slots of the terminal's color scheme, so a
/// header looks the same whatever scheme the session behind it runs: it is
/// chrome, like the strip it replaces, and a scheme's idea of "blue" is not
/// the application's. Mid-tone, so that either black or white text reads on
/// every one of them.
pub const HEADER_COLORS: [(&str, u32, &str); 9] = [
    ("red", 0xdc2626, "tab.color.red"),
    ("orange", 0xea580c, "tab.color.orange"),
    ("yellow", 0xca8a04, "tab.color.yellow"),
    ("green", 0x16a34a, "tab.color.green"),
    ("teal", 0x0d9488, "tab.color.teal"),
    ("blue", 0x2563eb, "tab.color.blue"),
    ("purple", 0x7c3aed, "tab.color.purple"),
    ("pink", 0xdb2777, "tab.color.pink"),
    ("gray", 0x6b7280, "tab.color.gray"),
];

/// What the stored header color `color` paints: a palette name, or a
/// `#rrggbb` of the user's own. `None` for anything else, which the header
/// draws as no color at all.
///
/// The one resolution, shared by the header itself and by every list that
/// shows the colors on offer, so a swatch is exactly the color the header
/// comes out in.
pub fn header_fill(color: &str) -> Option<Hsla> {
    if let Some(index) = header_preset(color) {
        return Some(rgb(HEADER_COLORS[index].1).into());
    }
    let color = color.trim();
    color.starts_with('#').then(|| parse_hex(color)).flatten()
}

/// The palette entry `color` names, if it is one.
pub fn header_preset(color: &str) -> Option<usize> {
    let color = color.trim();
    HEADER_COLORS
        .iter()
        .position(|(name, ..)| color.eq_ignore_ascii_case(name))
}

/// The `#rrggbb` the stored `color` paints, for a field that edits it as a
/// color of the user's own — a palette name included, so that switching to a
/// custom color starts from the one already on the header.
pub fn header_hex(color: &str) -> Option<String> {
    let fill = header_fill(color)?.to_rgb();
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        channel(fill.r),
        channel(fill.g),
        channel(fill.b)
    ))
}

/// The palette as swatches, for a color picker's preset row.
pub fn header_swatches() -> Vec<Hsla> {
    HEADER_COLORS
        .iter()
        .map(|(_, value, _)| rgb(*value).into())
        .collect()
}

/// The text color that reads on a header filled `fill`.
fn header_text(fill: Hsla) -> Hsla {
    let fill = fill.to_rgb();
    // Rec. 601 luma, which is all a choice between two text colors needs.
    let luma = 0.299 * fill.r + 0.587 * fill.g + 0.114 * fill.b;
    if luma > 0.55 {
        gpui::black()
    } else {
        gpui::white()
    }
}

/// A right-click on the header, asking for its menu at `position`, in window
/// coordinates.
pub struct HeaderMenuRequested {
    /// Where the pointer was when the button went down.
    pub position: Point<Pixels>,
}

/// What a header carries while it is dragged: the pane it belongs to, by the
/// entity the workspace knows it by.
///
/// Dropped onto another pane of the same tab, it asks the two to trade places;
/// see `Workspace::swap_panes`. Anywhere else it is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DraggedTail {
    /// The [`TailView`] whose header was picked up.
    pub view: EntityId,
}

/// The chip that follows the pointer while a header is dragged.
///
/// Drawn the way the tab strip draws a dragged tab, since it is the same
/// gesture on a different handle.
struct TailGhost {
    /// The file name the dragged pane is following.
    name: SharedString,
    /// Palette the ghost is drawn with.
    theme: Theme,
}

impl Render for TailGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.))
            .py(px(4.))
            .bg(self.theme.surface_active)
            .border_1()
            .border_color(self.theme.accent)
            .rounded_md()
            .text_size(px(HEADER_TEXT_SIZE))
            .text_color(self.theme.text)
            .child(self.name.clone())
    }
}

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
    /// The color the header is painted in, as stored, or `None` for the
    /// theme's own strip. See [`header_fill`] for the spellings.
    color: Option<String>,
    /// How wide the path had to fit when it was last drawn, or `None` until the
    /// probe has reported once. See the module header for why this is a frame
    /// behind.
    path_width: Option<Pixels>,
    /// Passes the grid's *this pane was clicked* on to the workspace.
    _clicked: Subscription,
    /// Passes the grid's *Reconnect* on to the workspace.
    _reconnect: Subscription,
    /// Re-emits the grid's *Edit connection…*, for the same reason.
    _edit: Subscription,
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
    /// no leaf answers to. Re-emitting keeps [`crate::workspace::Workspace::new_tail_pane`]
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
        let edit = cx.subscribe(
            &terminal,
            |_this, _view, _: &EditConnectionRequested, cx| {
                cx.emit(EditConnectionRequested);
            },
        );

        // Before the first frame, so the log the session has already begun
        // pouring in is colored from its first line rather than from the
        // moment the settings are next applied.
        let highlighter = compile_highlights(&session, cx);
        terminal.update(cx, |view, cx| view.set_highlights(highlighter, cx));

        Self {
            terminal,
            session,
            path: SharedString::from(path),
            connection,
            color: None,
            path_width: None,
            _clicked: clicked,
            _reconnect: reconnect,
            _edit: edit,
        }
    }

    /// The same pane, with its header painted `color`; see [`Self::set_color`].
    pub fn with_color(mut self, color: Option<String>) -> Self {
        self.color = color;
        self
    }

    /// The session this pane is following a file over.
    pub fn session(&self) -> &Entity<Session> {
        &self.session
    }

    /// The color the header is painted in, as stored.
    pub fn color(&self) -> Option<&str> {
        self.color.as_deref()
    }

    /// Paints the header `color` — a spelling [`header_fill`] reads — or gives
    /// it back the theme's strip with `None`.
    pub fn set_color(&mut self, color: Option<String>, cx: &mut Context<Self>) {
        if self.color == color {
            return;
        }
        self.color = color;
        cx.notify();
    }

    /// The header's fill and the text color that reads on it, or `None` when
    /// the header has no color of its own — or one this build cannot read,
    /// which is drawn the same way.
    fn header_colors(&self) -> Option<(Hsla, Hsla)> {
        let fill = header_fill(self.color.as_deref()?)?;
        Some((fill, header_text(fill)))
    }

    /// Re-reads the highlight rules and hands the grid the compiled result.
    ///
    /// Called when the settings are applied, which is the one thing that can
    /// change a live pane's rules: the global list is read fresh every time
    /// [`Session::highlight_rules`] is asked, so a rule added in the dialog
    /// reaches every pane on screen at once. The per-file list is read off the
    /// profile copy the session was opened with and so does *not* change under
    /// a running pane; see that method for why.
    pub fn refresh_highlights(&mut self, cx: &mut Context<Self>) {
        let highlighter = compile_highlights(&self.session, cx);
        self.terminal
            .update(cx, |view, cx| view.set_highlights(highlighter, cx));
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

impl EventEmitter<EditConnectionRequested> for TailView {}

impl EventEmitter<HeaderMenuRequested> for TailView {}

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

        let (fill, text, muted) = match self.header_colors() {
            Some((fill, text)) => (fill, text, text.opacity(0.75)),
            None => (theme.surface, theme.text, theme.text_muted),
        };
        let dragged = DraggedTail {
            view: cx.entity_id(),
        };
        let name = SharedString::from(crate::session::remote_file_name(&full).to_owned());

        let header = div()
            .id("tail-header")
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h(px(HEADER_HEIGHT))
            .px(px(8.))
            .bg(fill)
            .border_b_1()
            .border_color(theme.border)
            .text_size(font_size)
            .text_color(text)
            // The handle a pane is moved by: dropped on another pane of the
            // tab, the two trade places. The workspace answers the drop.
            .on_drag(dragged, move |_, _, _, cx| {
                // The function, not the palette this frame was drawn with: the
                // local `theme` above shadows it.
                let theme = rugpui::theme(cx);
                let name = name.clone();
                cx.new(|_| TailGhost { name, theme })
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|_view, event: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                    cx.emit(HeaderMenuRequested {
                        position: event.position,
                    });
                }),
            )
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
                    .text_color(muted)
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

/// The rules `session` is to be colored by, compiled, or `None` when nothing
/// would ever be recolored.
///
/// Compiled per pane rather than shared between the panes running the same
/// list: a tab of tails is a handful of panes and a rule list a handful of
/// regexes, so a cache keyed by rule list would cost more to keep honest — a
/// pane's rules change when the settings do — than the compiles it saves.
///
/// `None` for all three of the ways there can be nothing to do: a session that
/// is not following a file, an empty rule list (which is how a user switches
/// highlighting off), and a list whose every rule was disabled or unparseable.
/// The grid checks for exactly that `None` and skips the whole pass, so the
/// three arrive at the same place by design.
fn compile_highlights(session: &Entity<Session>, cx: &App) -> Option<Rc<Highlighter>> {
    let rules = session.read(cx).highlight_rules(cx)?;
    let highlighter = Highlighter::compile(&rules);
    (!highlighter.is_empty()).then(|| Rc::new(highlighter))
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
/// still recognizable, `/var/log/nginx/a` is not. The last component is never
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
    fn a_header_color_is_a_palette_name_or_a_hex_of_the_users_own() {
        assert_eq!(header_fill("blue"), Some(rgb(0x2563eb).into()));
        assert_eq!(header_fill(" Blue "), header_fill("blue"));
        assert_eq!(header_fill("#102030"), parse_hex("#102030"));
        // Neither a palette name nor a `#` color: drawn as no color.
        assert_eq!(header_fill("102030"), None);
        assert_eq!(header_fill("bright_red"), None);
        assert_eq!(header_preset("PINK"), Some(7));
        assert_eq!(header_preset("#db2777"), None);
    }

    #[test]
    fn a_header_color_is_spelled_as_hex_for_the_picker() {
        assert_eq!(header_hex("blue").as_deref(), Some("#2563eb"));
        assert_eq!(header_hex("#A1B2C3").as_deref(), Some("#a1b2c3"));
        assert_eq!(header_hex("nonsense"), None);
    }

    #[test]
    fn header_text_is_dark_on_light_fills_and_light_on_dark_ones() {
        assert_eq!(header_text(rgb(0xfde68a).into()), gpui::black());
        assert_eq!(header_text(rgb(0x1e3a8a).into()), gpui::white());
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
