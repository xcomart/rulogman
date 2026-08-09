//! The multi-line plain-text editor: a rope, and a gpui element that draws only
//! what fits on screen.
//!
//! [`crate::ui::TextInput`] is a single line by construction — it replaces `\n`
//! with a space — so the editor is a new widget rather than an extension of it.
//! What carries over is the discipline, not the code: byte offsets everywhere,
//! UTF-16 only at the platform boundary, grapheme clusters for every caret step,
//! and an `EntityInputHandler` that the IME can drive without ever being handed
//! an offset that is not on a character boundary. [`mod@view`] documents each
//! departure and why it is one.
//!
//! # The two things that make it hold at a gigabyte
//!
//! * **The buffer is a rope.** An insert is O(log n), and so are
//!   `byte <-> line` and `byte <-> UTF-16 code unit`. [`mod@buffer`].
//! * **Only the visible lines are shaped.** The element works out the row range
//!   from the scroll offset and shapes those and no others. [`mod@element`].
//!
//! * **Highlighting costs one line.** [`mod@syntax`] lexes a line at a time
//!   from the state the line before it ended in, and [`mod@highlight`] caches
//!   that one state per line. An edit re-lexes from the edited line until the
//!   states stop moving, which for an ordinary keystroke is the edited line
//!   alone; reaching the visible window from the top of a hundred-thousand-line
//!   file is a table lookup rather than a hundred thousand lines of work.
//!
//! What the lexers do is deliberately shallow — seven hand-written scanners for
//! the formats a file panel over a server actually reaches, and no parser
//! behind any of them. [`syntax::Language::detect`] picks one from the file's
//! name, and a file it does not recognise is drawn exactly as everything was
//! drawn before there were lexers at all: one run a line, in the foreground
//! colour.
//!
//! # Using it
//!
//! ```ignore
//! editor::init(cx);                    // once, after `ui::init`
//!
//! let editor = cx.new(EditorView::new);
//! // The colours and the font are the host's to supply, from whatever surface
//! // the editor is sitting in; see `palette_for`.
//! editor.update(cx, |editor, cx| {
//!     editor.set_palette(palette_for(&scheme), cx);
//!     editor.set_font(font, px(font_size), cx);
//! });
//! cx.subscribe(&editor, |_, editor, event: &EditorEvent, cx| {
//!     if matches!(event, EditorEvent::Changed) {
//!         let text = editor.read(cx).text();
//!         // hand it to whoever is saving
//!     }
//! })
//! .detach();
//! ```
//!
//! [`crate::editor_pane`] is the host that does all of this for a file opened
//! out of the file panel.
//!
//! # Out of scope, deliberately
//!
//! Multiple cursors would change the shape of every command in [`mod@view`],
//! so they go in as a list of selections in one piece or not at all. Code
//! folding needs a row-to-line map between the buffer and the renderer, which
//! nothing else wants yet. Soft wrapping needs the same map and a shaping pass
//! that can split a line, and would cost the one property the element is built
//! around: that row *n* is at `n * line_height`. File reading and writing are
//! the host's, not the widget's — [`EditorView::set_text`] and
//! [`EditorView::text`] are the whole of that boundary.

pub mod buffer;
pub mod element;
pub mod find;
pub mod highlight;
pub mod history;
pub mod syntax;
pub mod view;

// The names a host mounting the editor writes, gathered so that it writes
// `editor::EditorView` rather than `editor::view::EditorView`. Only some of
// them have a caller — see [`crate::editor_pane`] — and inside a binary crate a
// re-export nobody has imported yet reads as an unused import.
#[allow(unused_imports)]
pub use self::{
    buffer::Buffer,
    element::EditorElement,
    find::{FindState, find_all},
    highlight::Highlighter,
    history::{Edit, EditKind, History, SelectionState, Transaction},
    syntax::{Language, LineState, Token, TokenKind},
    view::{EditorEvent, EditorView, init},
};

use gpui::Hsla;
use logman_term::{Rgb, TerminalTheme};

use crate::terminal_view::to_hsla;

/// The colours the text surface is drawn in.
///
/// Fifteen slots, all of them derived from the *terminal* colour scheme rather
/// than from the application [`Theme`](crate::ui::Theme): an editor pane sits
/// beside a terminal pane showing the same host, and the two surfaces reading
/// as one material is what stops the split looking like two applications glued
/// together. It also means a user who picked Solarized for their shell gets
/// Solarized for the file they open out of it, without having chosen twice.
///
/// The six syntax slots follow the same rule and are the reason it matters
/// most: a scheme's ANSI sixteen are what its author chose to have `ls` and
/// `git diff` and a shell prompt drawn in, so taking a string's green from
/// there is taking it from the person who already decided what green means on
/// this screen. Inventing a syntax palette instead would put two colour systems
/// side by side in one window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EditorPalette {
    /// Behind everything. Opaque, unlike most of the app's surfaces.
    pub background: Hsla,
    /// The text, and every token the lexers had no opinion about.
    pub foreground: Hsla,
    /// The caret.
    pub cursor: Hsla,
    /// Behind the selection.
    pub selection: Hsla,
    /// A wash across the line the caret is on.
    pub line_highlight: Hsla,
    /// The line numbers.
    pub gutter: Hsla,
    /// The line number of the caret's line.
    pub gutter_active: Hsla,
    /// Behind a match of the find query.
    pub find_match: Hsla,
    /// Behind the match the find bar is currently on.
    pub find_current: Hsla,
    /// A comment.
    pub comment: Hsla,
    /// Quoted text, including its quotes.
    pub string: Hsla,
    /// A number, and the literals that stand beside one — `true`, `null`.
    pub number: Hsla,
    /// A word the format reserves.
    pub keyword: Hsla,
    /// The left-hand side of a mapping, and a section header.
    pub key: Hsla,
    /// An expansion: `$HOME`, `${x}`, a YAML anchor.
    pub variable: Hsla,
}

/// How far the caret's line is lifted off the background, as a mix of the
/// foreground into it.
///
/// Small enough that a wash across the full width of the pane does not read as
/// a selection, which is the one thing it must never be confused with.
const LINE_HIGHLIGHT_MIX: f32 = 0.08;

/// How far a line number is lifted off the background.
///
/// Halfway is what makes the gutter legible without competing with the text: a
/// scheme's own `foreground` is what the *content* is drawn in, and numbers as
/// loud as the content would be read as content.
const GUTTER_MIX: f32 = 0.5;

/// How strongly a find match is tinted, and how much more strongly the one the
/// find bar is currently on.
const FIND_MATCH_MIX: f32 = 0.35;
/// See [`FIND_MATCH_MIX`].
const FIND_CURRENT_MIX: f32 = 0.7;

/// Index of normal green in the sixteen-slot ANSI palette.
const ANSI_GREEN: usize = 2;
/// Index of normal yellow in the sixteen-slot ANSI palette.
const ANSI_YELLOW: usize = 3;
/// Index of normal blue in the sixteen-slot ANSI palette.
const ANSI_BLUE: usize = 4;
/// Index of normal magenta in the sixteen-slot ANSI palette.
const ANSI_MAGENTA: usize = 5;
/// Index of normal cyan in the sixteen-slot ANSI palette.
const ANSI_CYAN: usize = 6;
/// Index of bright black in the sixteen-slot ANSI palette.
const ANSI_BRIGHT_BLACK: usize = 8;
/// Index of bright yellow in the sixteen-slot ANSI palette.
const ANSI_BRIGHT_YELLOW: usize = 11;

/// The least contrast ratio a syntax colour may stand at against the page it is
/// drawn on.
///
/// Well under the 4.5 that WCAG asks of body text, and deliberately: an ANSI
/// sixteen is chosen to be *distinguishable*, not to be legible as prose, and
/// holding a scheme's own blue to 4.5 against its own background would reject
/// half the schemes people actually use. What this number is really for is the
/// pathological case — a scheme whose normal blue is nearly its background —
/// where the token would otherwise vanish entirely.
const MIN_CONTRAST: f32 = 2.2;

/// The lower bar a comment is held to. A comment is *meant* to recede, so it is
/// only lifted when it has all but disappeared.
const MIN_COMMENT_CONTRAST: f32 = 1.6;

/// How many steps [`legible`] takes from a colour towards the foreground.
const LEGIBILITY_STEPS: u8 = 4;

/// `a` with `t` of `b` mixed into it, channel by channel.
///
/// Mixing rather than compositing with an alpha, because every slot this feeds
/// is painted as an *opaque* fill under the text. The editor background is
/// itself opaque — see [`EditorView::render`](view::EditorView) — so a
/// translucent wash would be composited against it anyway, and doing the
/// arithmetic here means the four highlight fills stack predictably instead of
/// each one darkening whatever it happens to land on top of.
fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0., 1.);
    let channel = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    Rgb::new(channel(a.r, b.r), channel(a.g, b.g), channel(a.b, b.b))
}

/// The relative luminance of `colour`, as sRGB defines it.
///
/// The gamma-corrected form rather than a plain average, because the whole
/// point of the number is to predict what the eye will do with the colour, and
/// a plain average calls a saturated blue as bright as a saturated green.
fn luminance(colour: Rgb) -> f32 {
    let channel = |value: u8| {
        let value = f32::from(value) / 255.;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(colour.r) + 0.7152 * channel(colour.g) + 0.0722 * channel(colour.b)
}

/// The WCAG contrast ratio between two colours, from 1 to 21.
fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    let (lighter, darker) = if a > b { (a, b) } else { (b, a) };
    (lighter + 0.05) / (darker + 0.05)
}

/// `colour`, lifted towards `foreground` until it stands `least` off
/// `background`.
///
/// The defence against a scheme whose ANSI colour for some token happens to sit
/// on top of its own background — which is not hypothetical: several popular
/// dark schemes put bright black within a few percent of the page. Walking
/// towards the *foreground* rather than towards white or black is what keeps
/// the result in the scheme's own family: a washed-out Solarized blue is still
/// recognisably Solarized, and the last step is the foreground itself, which
/// the scheme has already guaranteed is readable.
fn legible(colour: Rgb, background: Rgb, foreground: Rgb, least: f32) -> Rgb {
    (0..=LEGIBILITY_STEPS)
        .map(|step| {
            mix(
                colour,
                foreground,
                f32::from(step) / f32::from(LEGIBILITY_STEPS),
            )
        })
        .find(|candidate| contrast(*candidate, background) >= least)
        .unwrap_or(foreground)
}

/// The editor palette for a terminal colour `scheme`.
///
/// The four slots a scheme names outright — background, foreground, cursor and
/// selection — are taken verbatim, so a caret in the editor and a caret in the
/// terminal beside it are the same mark, and a selection in either is the same
/// block of colour. The five it does not name are mixed out of the ones it
/// does, which is what keeps a scheme's contrast intact whether it is a dark
/// one or a light one: nothing here assumes the background is the darker of the
/// two.
///
/// Find matches take the scheme's yellow, normal for a match and bright for the
/// one the bar is on, because yellow is the one hue a palette reserves for
/// *look here* rather than for an error or for success. Note that the selection
/// is painted after the matches and is opaque, so the current match disappears
/// under it — which is right, because [`EditorView::find_next`](view::EditorView)
/// selects the match it moves to, and the selection is then the mark.
///
/// # The six syntax slots
///
/// Each is an ANSI colour of the scheme, put through [`legible`] so that a
/// scheme which happens to place one on top of its own background does not lose
/// the token entirely:
///
/// | slot     | ANSI            | why                                                                     |
/// |----------|-----------------|-------------------------------------------------------------------------|
/// | comment  | 8 bright black  | the one slot in the sixteen whose job already *is* to be quiet           |
/// | string   | 2 green         | what every terminal scheme, and every editor theme after them, uses      |
/// | number   | 5 magenta       | a constant; the literals `true` and `null` are constants and share it    |
/// | keyword  | 4 blue          | the structural colour, and the one a prompt uses for a directory         |
/// | key      | 6 cyan          | beside blue in hue and clearly apart from it, which is what a `key: value` line needs |
/// | variable | 3 yellow        | *something is substituted here*, the same warning colour as a find match |
///
/// `number` and the `true`/`false`/`null` literals share one slot rather than
/// having two: no format here treats a boolean as a different kind of thing
/// from a number, and a palette that split them would be asking every future
/// reader to tell two magentas apart for nothing. `variable` shares yellow with
/// the find-match fill, which cannot be confused with it — one is a glyph
/// colour and the other a block painted behind glyphs.
pub fn palette_for(scheme: &TerminalTheme) -> EditorPalette {
    let background = scheme.background;
    let foreground = scheme.foreground;
    let syntax = |index: usize| {
        to_hsla(legible(
            scheme.ansi[index],
            background,
            foreground,
            MIN_CONTRAST,
        ))
    };
    EditorPalette {
        background: to_hsla(background),
        foreground: to_hsla(foreground),
        cursor: to_hsla(scheme.cursor),
        selection: to_hsla(scheme.selection),
        line_highlight: to_hsla(mix(background, foreground, LINE_HIGHLIGHT_MIX)),
        gutter: to_hsla(mix(background, foreground, GUTTER_MIX)),
        gutter_active: to_hsla(foreground),
        find_match: to_hsla(mix(background, scheme.ansi[ANSI_YELLOW], FIND_MATCH_MIX)),
        find_current: to_hsla(mix(
            background,
            scheme.ansi[ANSI_BRIGHT_YELLOW],
            FIND_CURRENT_MIX,
        )),
        comment: to_hsla(legible(
            scheme.ansi[ANSI_BRIGHT_BLACK],
            background,
            foreground,
            MIN_COMMENT_CONTRAST,
        )),
        string: syntax(ANSI_GREEN),
        number: syntax(ANSI_MAGENTA),
        keyword: syntax(ANSI_BLUE),
        key: syntax(ANSI_CYAN),
        variable: syntax(ANSI_YELLOW),
    }
}

#[cfg(test)]
mod tests;
