//! The editor entity: the state, the commands, and the platform input handler.
//!
//! # Offsets, and the one place they are not bytes
//!
//! Every offset stored in an [`EditorView`] is a **byte offset** into the
//! buffer. `ropey` indexes by byte and every range this file passes about is
//! one, so the only conversions here are at its edges.
//!
//! The edge that matters is [`EntityInputHandler`]. Every platform text input
//! protocol — NSTextInputClient on macOS, IMM/TSF on Windows, the input methods
//! on X11 and Wayland — counts in **UTF-16 code units**, because all three
//! grew up around UTF-16 string types. So the trait's ranges are UTF-16 and the
//! view's are bytes, and `offset_to_utf16` / `offset_from_utf16` sit at every
//! crossing. Getting this wrong is not a rendering glitch: a Hangul syllable is
//! three bytes and one UTF-16 unit, so an off-by-one here puts the caret inside
//! a character, and the next slice panics or the composition overwrites the
//! wrong text. The single-line field in [`crate::ui`] converts by walking the
//! string; this one cannot, and [`crate::editor::buffer`] explains what it does
//! instead.
//!
//! # No `DisplayMap`
//!
//! [`crate::ui::TextInput`] keeps a `DisplayMap` between "the bytes stored" and
//! "the bytes drawn", because a password field draws a bullet per grapheme and
//! the caret still has to land on the right character of the real content.
//! **This editor has no such map and needs none**: it renders every byte of the
//! buffer verbatim, so the two spaces are the same space. Masking is the only
//! thing that ever made them differ, and a file opened for reading is never
//! masked. What replaces the map is the line index — the renderer's coordinates
//! are `(line, byte column)` rather than `(byte offset)` — and that translation
//! lives in [`crate::editor::buffer`], where it is exact rather than a lookup
//! table.
//!
//! # Composition
//!
//! The IME contract is the one `TextInput` implements, extended to a buffer
//! with lines in it:
//!
//! * [`EditorView::replace_and_mark_text_in_range`] replaces a range and marks
//!   what it put there. The mark is the underlined run the renderer draws, and
//!   the range it replaces defaults to the existing mark — that is what makes
//!   `ㅎ`, `하`, `한` overwrite each other instead of accumulating.
//! * [`EditorView::replace_text_in_range`] commits, clearing the mark.
//! * [`EditorView::marked_text_range`] answers in UTF-16, because the platform
//!   uses it to place the candidate window.
//!
//! One deliberate departure from `TextInput`. gpui's own input example — which
//! `TextInput` follows byte for byte — maps the new selection with
//! `new_range.start + range.start .. new_range.end + range.end`, adding a
//! *different* base to each end. That is only harmless while the replaced range
//! is empty, which is the case for a field that has never been composed in
//! before; on Windows, where `WM_IME_COMPOSITION` sends a caret position inside
//! a composition that is replacing itself, it produces a selection stretching
//! across the syllable rather than a caret inside it. Here the new selection is
//! resolved against the *inserted text*, which is what the protocol says it is,
//! and `range.start` is the only base.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, DragMoveEvent, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, Font, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollWheelEvent, ShapedLine, UTF16Selection, Window, actions,
    div, point, prelude::*, px,
};

use rulogman_term::TerminalTheme;

use crate::editor::buffer::Buffer;
use crate::editor::element::EditorElement;
use crate::editor::find::FindState;
use crate::editor::highlight::Highlighter;
use crate::editor::history::{Edit, EditKind, History, SelectionState};
use crate::editor::syntax::Language;
use crate::editor::{EditorPalette, palette_for};
use crate::i18n::ts;
use crate::terminal_view::{DEFAULT_FONT_SIZE, LINE_HEIGHT_RATIO, terminal_font};
use crate::ui::scrollbar::{
    DraggedThumb, Scrollbar, ScrollbarAxis, ScrollbarState, hide_later, hide_now,
};
use crate::ui::{Checkbox, TextInput, theme};

actions!(
    rulogman_editor,
    [
        /// Delete the grapheme before the caret, or the selection.
        Backspace,
        /// Delete the grapheme after the caret, or the selection.
        Delete,
        /// Delete to the start of the word before the caret.
        DeleteWordLeft,
        /// Delete to the end of the word after the caret.
        DeleteWordRight,
        /// Move the caret one grapheme left.
        Left,
        /// Move the caret one grapheme right.
        Right,
        /// Move the caret one line up.
        Up,
        /// Move the caret one line down.
        Down,
        /// Move the caret to the start of the previous word.
        WordLeft,
        /// Move the caret to the end of the next word.
        WordRight,
        /// Extend the selection one grapheme left.
        SelectLeft,
        /// Extend the selection one grapheme right.
        SelectRight,
        /// Extend the selection one line up.
        SelectUp,
        /// Extend the selection one line down.
        SelectDown,
        /// Extend the selection to the start of the previous word.
        SelectWordLeft,
        /// Extend the selection to the end of the next word.
        SelectWordRight,
        /// Move the caret to the first non-blank of the line, then to column 0.
        LineStart,
        /// Move the caret to the end of the line.
        LineEnd,
        /// Extend the selection to the start of the line.
        SelectLineStart,
        /// Extend the selection to the end of the line.
        SelectLineEnd,
        /// Move the caret to the start of the buffer.
        DocumentStart,
        /// Move the caret to the end of the buffer.
        DocumentEnd,
        /// Extend the selection to the start of the buffer.
        SelectDocumentStart,
        /// Extend the selection to the end of the buffer.
        SelectDocumentEnd,
        /// Move the caret one screenful up.
        PageUp,
        /// Move the caret one screenful down.
        PageDown,
        /// Extend the selection one screenful up.
        SelectPageUp,
        /// Extend the selection one screenful down.
        SelectPageDown,
        /// Select the whole buffer.
        SelectAll,
        /// Insert a line break, carrying the current line's indent.
        Newline,
        /// Indent the selected lines, or insert one indent.
        Indent,
        /// Remove one indent from the selected lines.
        Outdent,
        /// Comment or uncomment the selected lines.
        ToggleComment,
        /// Copy the selection.
        Copy,
        /// Copy the selection and delete it.
        Cut,
        /// Insert the clipboard contents.
        Paste,
        /// Take back the last change.
        Undo,
        /// Put back the last change taken back.
        Redo,
        /// Open the find bar.
        Find,
        /// Open the find bar with the replace row showing.
        Replace,
        /// Go to the next match.
        FindNext,
        /// Go to the previous match.
        FindPrev,
        /// Replace the current match and go to the next.
        ReplaceNext,
        /// Replace every match.
        ReplaceAll,
        /// Close the find bar and return to the buffer.
        CloseFind,
        /// Open the macOS emoji / character palette.
        ShowCharacterPalette,
    ]
);

/// Key context the editor surface binds its keys to.
pub const KEY_CONTEXT: &str = "Editor";

/// Key context the find bar binds its keys to.
pub const FIND_KEY_CONTEXT: &str = "EditorFind";

/// One level of indentation, and what `Tab` inserts.
///
/// Spaces rather than a tab character: the files this editor is opened on are
/// read by other tools as often as by a person, and a width nobody has to agree
/// on is one less thing for those tools to disagree about.
const INDENT: &str = "    ";

/// What the editor tells its host about.
///
/// Everything here is a notification rather than a request: the editor holds no
/// file handle and no idea where its text came from, so saving, reloading and
/// closing are all the host's, driven off [`EditorEvent::Changed`] and
/// [`EditorView::is_dirty`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorEvent {
    /// The buffer changed.
    Changed,
    /// The caret or the selection moved.
    SelectionChanged,
    /// The user right clicked, and wants the editor's menu.
    ///
    /// The editor detects the press, takes the focus and says where it was; the
    /// host draws the menu, because this layer holds none of the strings such a
    /// menu needs. Every command it would offer is already an action on the
    /// editor — `Copy`, `Cut`, `Paste`, `Undo`, `Redo`, `SelectAll`,
    /// `ToggleComment`, `Find` — so the host dispatches them into
    /// [`KEY_CONTEXT`] rather than calling anything new, and greys them out
    /// with [`EditorView::has_selection`], [`EditorView::can_undo`],
    /// [`EditorView::can_redo`] and [`EditorView::is_read_only`].
    ContextMenu {
        /// Where the pointer was, in **window** coordinates, which is what the
        /// menu anchors to.
        position: Point<Pixels>,
    },
}

/// How much a drag selects at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Granularity {
    /// One grapheme, after a single click.
    Character,
    /// A whole word, after a double click.
    Word,
    /// A whole line, after a triple click.
    Line,
}

/// What the element measured last time it drew, so that the view can answer
/// questions about pixels.
///
/// Written by [`crate::editor::element::EditorElement`] at the end of every
/// paint and read by hit testing, by [`EditorView::bounds_for_range`] and by the
/// scrollbars, which is the same one-frame trail every scrolling surface in the
/// app lives with.
#[derive(Default)]
pub(crate) struct Layout {
    /// The editor's box in window coordinates.
    pub bounds: Option<Bounds<Pixels>>,
    /// Width of the line-number gutter.
    pub gutter: Pixels,
    /// Height of one line.
    pub line_height: Pixels,
    /// The lines drawn last frame, as `(line index, shaped line)`.
    pub lines: Vec<(usize, ShapedLine)>,
    /// The widest line seen so far, for the horizontal scroll extent.
    pub content_width: Pixels,
}

impl Layout {
    /// The shaped line for `line`, if it was on screen last frame.
    fn shaped(&self, line: usize) -> Option<&ShapedLine> {
        self.lines
            .iter()
            .find_map(|(at, shaped)| (*at == line).then_some(shaped))
    }

    /// How many whole lines fit in the text area.
    fn visible_lines(&self) -> usize {
        let Some(bounds) = self.bounds else {
            return 1;
        };
        if self.line_height <= px(0.) {
            return 1;
        }
        ((bounds.size.height / self.line_height) as usize).max(1)
    }
}

/// A multi-line plain-text editor, as a gpui entity.
///
/// ```ignore
/// let editor = cx.new(EditorView::new);
/// cx.subscribe(&editor, |_, _, event: &EditorEvent, _| match event {
///     EditorEvent::Changed => { /* the buffer is dirty */ }
///     _ => {}
/// })
/// .detach();
/// ```
pub struct EditorView {
    focus_handle: FocusHandle,
    buffer: Buffer,
    /// One [`crate::editor::syntax::LineState`] per line, and the language they
    /// were lexed under.
    ///
    /// Kept beside the buffer rather than inside it because a buffer is a
    /// document and this is an opinion about one: the same bytes are a shell
    /// script or a log depending on what the host called
    /// [`EditorView::set_language`] with. Every mutation of `buffer` goes
    /// through [`EditorView::splice`], which is the one place the two are kept
    /// in step.
    highlighter: Highlighter,
    history: History,
    /// The selected byte range, `start <= end`. A caret is an empty one.
    selected_range: Range<usize>,
    /// Whether the caret is at `selected_range.start`.
    selection_reversed: bool,
    /// The composing run, in bytes, while an IME has one open.
    marked_range: Option<Range<usize>>,
    /// The column a vertical move aims for, in graphemes, so that walking down
    /// past a short line and back up returns to where it started.
    goal_column: Option<usize>,
    read_only: bool,
    dirty: bool,
    is_selecting: bool,
    /// What a drag extends by, decided by the click count that started it.
    granularity: Granularity,
    /// The range the current drag started from, which a word or line drag
    /// never shrinks past.
    drag_anchor: Range<usize>,
    /// Scroll offset in pixels: `x` right, `y` down, both non-negative.
    scroll: Point<Pixels>,
    pub(crate) layout: Layout,
    find: FindState,
    find_query: Entity<TextInput>,
    find_replacement: Entity<TextInput>,
    vertical_bar: ScrollbarState,
    horizontal_bar: ScrollbarState,
    /// The colours the text surface is drawn in.
    ///
    /// Held rather than looked up, because the palette comes from the colour
    /// scheme of the *session* the file was opened out of, and this widget
    /// deliberately knows nothing about sessions. The host pushes one in with
    /// [`EditorView::set_palette`]; until it does, the editor draws in the
    /// default scheme.
    palette: EditorPalette,
    /// The font the text surface is shaped and drawn in.
    ///
    /// Held for the same reason the palette is, and pushed in by the same host
    /// with [`EditorView::set_font`]: the family and the size belong to the
    /// *session*'s terminal settings, and this widget knows nothing about
    /// sessions. Until a host says otherwise it is the monospace font the
    /// terminal resolved at start-up, so an editor mounted anywhere is at least
    /// never drawn in a proportional face.
    font: Font,
    /// The size the text surface is shaped and drawn at. See [`Self::font`].
    font_size: Pixels,
}

/// Registers the key bindings every [`EditorView`] relies on.
///
/// Call once during application start-up, after [`crate::ui::init`]. Everything
/// is scoped to the `Editor` and `EditorFind` key contexts, so none of it
/// escapes into the rest of the window.
pub fn init(cx: &mut App) {
    let (modifier, word) = if cfg!(target_os = "macos") {
        ("cmd", "alt")
    } else {
        ("ctrl", "ctrl")
    };
    let editor = Some(KEY_CONTEXT);
    let find = Some(FIND_KEY_CONTEXT);

    let mut bindings = vec![
        KeyBinding::new("backspace", Backspace, editor),
        KeyBinding::new("delete", Delete, editor),
        KeyBinding::new(&format!("{word}-backspace"), DeleteWordLeft, editor),
        KeyBinding::new(&format!("{word}-delete"), DeleteWordRight, editor),
        KeyBinding::new("left", Left, editor),
        KeyBinding::new("right", Right, editor),
        KeyBinding::new("up", Up, editor),
        KeyBinding::new("down", Down, editor),
        KeyBinding::new("shift-left", SelectLeft, editor),
        KeyBinding::new("shift-right", SelectRight, editor),
        KeyBinding::new("shift-up", SelectUp, editor),
        KeyBinding::new("shift-down", SelectDown, editor),
        KeyBinding::new(&format!("{word}-left"), WordLeft, editor),
        KeyBinding::new(&format!("{word}-right"), WordRight, editor),
        KeyBinding::new(&format!("{word}-shift-left"), SelectWordLeft, editor),
        KeyBinding::new(&format!("{word}-shift-right"), SelectWordRight, editor),
        KeyBinding::new("home", LineStart, editor),
        KeyBinding::new("end", LineEnd, editor),
        KeyBinding::new("shift-home", SelectLineStart, editor),
        KeyBinding::new("shift-end", SelectLineEnd, editor),
        KeyBinding::new(&format!("{modifier}-home"), DocumentStart, editor),
        KeyBinding::new(&format!("{modifier}-end"), DocumentEnd, editor),
        KeyBinding::new(
            &format!("{modifier}-shift-home"),
            SelectDocumentStart,
            editor,
        ),
        KeyBinding::new(&format!("{modifier}-shift-end"), SelectDocumentEnd, editor),
        KeyBinding::new("pageup", PageUp, editor),
        KeyBinding::new("pagedown", PageDown, editor),
        KeyBinding::new("shift-pageup", SelectPageUp, editor),
        KeyBinding::new("shift-pagedown", SelectPageDown, editor),
        KeyBinding::new("enter", Newline, editor),
        KeyBinding::new("tab", Indent, editor),
        KeyBinding::new("shift-tab", Outdent, editor),
        KeyBinding::new(&format!("{modifier}-/"), ToggleComment, editor),
        KeyBinding::new(&format!("{modifier}-a"), SelectAll, editor),
        KeyBinding::new(&format!("{modifier}-c"), Copy, editor),
        KeyBinding::new(&format!("{modifier}-x"), Cut, editor),
        KeyBinding::new(&format!("{modifier}-v"), Paste, editor),
        KeyBinding::new(&format!("{modifier}-z"), Undo, editor),
        KeyBinding::new(&format!("{modifier}-shift-z"), Redo, editor),
        KeyBinding::new(&format!("{modifier}-y"), Redo, editor),
        // The find bar is opened from the buffer and driven from inside
        // itself, so these two are bound in both contexts.
        KeyBinding::new(&format!("{modifier}-f"), Find, editor),
        KeyBinding::new(&format!("{modifier}-h"), Replace, editor),
        KeyBinding::new(&format!("{modifier}-f"), Find, find),
        KeyBinding::new(&format!("{modifier}-h"), Replace, find),
        KeyBinding::new("f3", FindNext, editor),
        KeyBinding::new("shift-f3", FindPrev, editor),
        KeyBinding::new("f3", FindNext, find),
        KeyBinding::new("shift-f3", FindPrev, find),
        KeyBinding::new("escape", CloseFind, find),
        KeyBinding::new("escape", CloseFind, editor),
        KeyBinding::new(&format!("{modifier}-alt-enter"), ReplaceAll, find),
    ];

    if cfg!(target_os = "macos") {
        bindings.push(KeyBinding::new(
            "ctrl-cmd-space",
            ShowCharacterPalette,
            editor,
        ));
    }

    cx.bind_keys(bindings);
}

impl EditorView {
    /// An empty editor over plain text.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let buffer = Buffer::new("");
        let highlighter = Highlighter::new(&buffer, Language::Plain);
        Self {
            focus_handle: cx.focus_handle(),
            buffer,
            highlighter,
            history: History::new(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            goal_column: None,
            read_only: false,
            dirty: false,
            is_selecting: false,
            granularity: Granularity::Character,
            drag_anchor: 0..0,
            scroll: point(px(0.), px(0.)),
            layout: Layout::default(),
            find: FindState::default(),
            find_query: cx.new(|cx| TextInput::new(cx).placeholder(ts!("editor.find"))),
            find_replacement: cx.new(|cx| TextInput::new(cx).placeholder(ts!("editor.replace"))),
            vertical_bar: ScrollbarState::new(),
            horizontal_bar: ScrollbarState::new(),
            palette: palette_for(&TerminalTheme::by_name_or_default("")),
            font: terminal_font(cx),
            font_size: DEFAULT_FONT_SIZE,
        }
    }

    /// The colours the text surface is drawn in.
    pub(crate) fn palette(&self) -> &EditorPalette {
        &self.palette
    }

    /// The font the text surface is shaped and drawn in.
    pub(crate) fn font(&self) -> &Font {
        &self.font
    }

    /// The size the text surface is shaped and drawn at.
    pub(crate) const fn font_size(&self) -> Pixels {
        self.font_size
    }

    /// The height of one row.
    ///
    /// Derived from the font size rather than from the window's text style,
    /// because the element is built around "row *n* is at `n * line_height`" and
    /// both halves of that have to move together when the size changes. The
    /// ratio is the terminal's [`LINE_HEIGHT_RATIO`], so a file opened beside
    /// the shell it came from has rows of exactly the same pitch.
    pub(crate) fn line_height(&self) -> Pixels {
        self.font_size * LINE_HEIGHT_RATIO
    }

    /// Shapes and draws the text surface in `font` at `font_size` from the next
    /// frame on.
    ///
    /// Cheap to call every frame, on the same terms as
    /// [`EditorView::set_palette`]: the font settings can change under an open
    /// editor, and an unchanged pair repaints nothing.
    pub fn set_font(&mut self, font: Font, font_size: Pixels, cx: &mut Context<Self>) {
        if self.font == font && self.font_size == font_size {
            return;
        }
        self.font = font;
        self.font_size = font_size;
        cx.notify();
    }

    /// Colours the text surface as `language` from the next frame on.
    ///
    /// A no-op when the language has not moved, on the same terms as
    /// [`EditorView::set_palette`] and [`EditorView::set_font`]: the host is
    /// free to push the answer in on every frame, and an unchanged one costs
    /// nothing. A changed one re-lexes the whole buffer, which is right —
    /// nothing about the old cache survives a change to what a `#` means — and
    /// affordable, because it happens when a file is opened and not while
    /// anyone is typing.
    pub fn set_language(&mut self, language: Language, cx: &mut Context<Self>) {
        if self.highlighter.language() == language {
            return;
        }
        self.highlighter.set_language(language, &self.buffer);
        cx.notify();
    }

    /// The language the buffer is being coloured as.
    pub fn language(&self) -> Language {
        self.highlighter.language()
    }

    /// Draws the text surface in `palette` from the next frame on.
    ///
    /// Cheap to call every frame, which is how the host keeps up with a colour
    /// scheme that can change under it: an unchanged palette repaints nothing.
    pub fn set_palette(&mut self, palette: EditorPalette, cx: &mut Context<Self>) {
        if self.palette == palette {
            return;
        }
        self.palette = palette;
        cx.notify();
    }

    /// Makes the editor refuse every change.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Makes the editor refuse every change, or stop refusing them.
    pub fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        self.read_only = read_only;
        cx.notify();
    }

    /// Whether the editor is refusing changes.
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The whole buffer, as a `String`.
    pub fn text(&self) -> String {
        self.buffer.text()
    }

    /// Replaces the whole buffer, clearing the history and the dirty flag.
    ///
    /// This is "a file was opened", not "something was pasted": undo does not
    /// cross it, and the editor is clean afterwards.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.buffer = Buffer::new(text);
        self.highlighter.reset(&self.buffer);
        self.history.clear();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.goal_column = None;
        self.dirty = false;
        self.scroll = point(px(0.), px(0.));
        self.find.matches.clear();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Whether the buffer has changed since it was set or last marked clean.
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Marks the buffer clean, for a host that has just saved it.
    pub fn mark_clean(&mut self, cx: &mut Context<Self>) {
        self.dirty = false;
        cx.notify();
    }

    /// The selected byte range. Empty when there is only a caret.
    pub fn selection(&self) -> Range<usize> {
        self.selected_range.clone()
    }

    /// Whether anything is selected, as opposed to there being only a caret.
    ///
    /// What tells a host menu whether "copy" and "cut" are worth offering.
    pub fn has_selection(&self) -> bool {
        !self.selected_range.is_empty()
    }

    /// Whether there is a change to take back.
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    /// Whether there is a change to put back.
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// The caret's byte offset.
    pub fn caret(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    /// The caret's place in the file the way a person counts it: the line and
    /// the column, both from one.
    ///
    /// The column is counted in *graphemes*, not bytes, which is the only
    /// count that answers "how far along the line is the caret" — the same
    /// measure a vertical move aims for, so the number in a status bar agrees
    /// with where <kbd>↑</kbd> puts the caret. A byte column would say 7 in the
    /// middle of a Korean word and 3 for the same place in an English one.
    ///
    /// One-based here rather than at the caller, because there is only one
    /// reason to ask — to show it — and every caller would add the same one.
    pub fn caret_position(&self) -> (usize, usize) {
        let caret = self.caret();
        (
            self.buffer.line_of(caret) + 1,
            self.buffer.grapheme_column(caret) + 1,
        )
    }

    /// How many lines the buffer holds.
    ///
    /// A buffer ending in a newline counts the empty line after it, which is the
    /// line the caret can be put on and so the line a reader counts.
    pub fn line_count(&self) -> usize {
        self.buffer.line_count()
    }

    /// Moves the caret to `offset`, collapsing the selection.
    pub fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clamp(offset);
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.after_move(cx);
    }

    /// Selects `range`, leaving the caret at its end.
    pub fn select_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let start = self.clamp(range.start);
        let end = self.clamp(range.end);
        self.selected_range = start.min(end)..start.max(end);
        self.selection_reversed = false;
        self.after_move(cx);
    }

    // --- internals -----------------------------------------------------------

    /// `offset`, brought inside the buffer and onto a character boundary.
    fn clamp(&self, offset: usize) -> usize {
        let offset = offset.min(self.buffer.len());
        let rope = self.buffer.rope();
        rope.char_to_byte(rope.byte_to_char(offset))
    }

    /// The current selection, in the form the history keeps.
    fn selection_state(&self) -> SelectionState {
        SelectionState {
            range: self.selected_range.clone(),
            reversed: self.selection_reversed,
        }
    }

    /// Restores a selection the history handed back.
    fn set_selection_state(&mut self, state: &SelectionState) {
        self.selected_range = self.clamp(state.range.start)..self.clamp(state.range.end);
        self.selection_reversed = state.reversed;
    }

    /// What every caret movement ends with: the undo group closes, the goal
    /// column is forgotten and the caret is brought on screen.
    fn after_move(&mut self, cx: &mut Context<Self>) {
        self.history.break_group();
        self.goal_column = None;
        self.scroll_to_caret();
        cx.emit(EditorEvent::SelectionChanged);
        cx.notify();
    }

    /// Extends the selection to `offset`, keeping the anchor where it is.
    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clamp(offset);
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.history.break_group();
        self.scroll_to_caret();
        cx.emit(EditorEvent::SelectionChanged);
        cx.notify();
    }

    /// Applies a replacement to the buffer and the syntax cache, and nothing
    /// else: no history, no selection, no notification.
    ///
    /// The one place the buffer is mutated, which is why it is also the one
    /// place the cache is brought back into step. Everything above it is
    /// arranged so that this is called with a range that is already clamped and
    /// already on character boundaries.
    fn splice(&mut self, range: Range<usize>, text: &str) {
        let first = self.buffer.line_of(range.start);
        let removed = self.buffer.line_of(range.end) - first;
        let added = text.bytes().filter(|byte| *byte == b'\n').count();
        self.buffer.replace(range, text);
        self.highlighter.edited(&self.buffer, first, removed, added);
        self.dirty = true;
    }

    /// Replaces `range` with `text`, records it, and leaves the caret after it.
    fn edit(&mut self, range: Range<usize>, text: &str, kind: EditKind, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let range = self.clamp(range.start)..self.clamp(range.end);
        let before = self.selection_state();
        let removed = self.buffer.slice(range.clone());
        self.splice(range.clone(), text);

        let caret = range.start + text.len();
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.marked_range = None;
        self.goal_column = None;

        self.history.push(
            Edit {
                start: range.start,
                removed,
                inserted: text.to_owned(),
            },
            kind,
            before,
            self.selection_state(),
        );
        self.changed(cx);
    }

    /// What every buffer change ends with.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.scroll_to_caret();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Applies several edits as one undo step.
    ///
    /// `edits` are applied in the order given and each one's offsets have to be
    /// valid at the moment it is applied; building them from the bottom of the
    /// buffer upwards is what makes that true without any bookkeeping.
    fn transact(&mut self, edits: Vec<Edit>, after: Range<usize>, cx: &mut Context<Self>) {
        if self.read_only || edits.is_empty() {
            return;
        }
        let before = self.selection_state();
        for edit in &edits {
            self.splice(edit.old_range(), &edit.inserted);
        }
        self.selected_range = self.clamp(after.start)..self.clamp(after.end);
        self.selection_reversed = false;
        self.marked_range = None;
        self.goal_column = None;
        self.history
            .push_transaction(edits, before, self.selection_state());
        self.changed(cx);
    }

    /// Deletes the selection, or `fallback` when there is none.
    fn delete_with(&mut self, fallback: Range<usize>, kind: EditKind, cx: &mut Context<Self>) {
        let range = if self.selected_range.is_empty() {
            fallback
        } else {
            self.selected_range.clone()
        };
        if range.start == range.end {
            return;
        }
        self.edit(range, "", kind, cx);
    }

    // --- scrolling -----------------------------------------------------------

    /// The scroll offset, in pixels.
    pub(crate) const fn scroll_offset(&self) -> Point<Pixels> {
        self.scroll
    }

    /// How far the content extends past the viewport, in pixels, per axis.
    fn scrollable(&self) -> Point<Pixels> {
        let Some(bounds) = self.layout.bounds else {
            return point(px(0.), px(0.));
        };
        let height = self.layout.line_height * (self.buffer.line_count() as f32);
        let width = self.layout.content_width + self.layout.gutter + px(32.);
        point(
            (width - bounds.size.width).max(px(0.)),
            (height - bounds.size.height).max(px(0.)),
        )
    }

    /// Sets the scroll offset, clamped to the content.
    fn scroll_by(&mut self, delta: Point<Pixels>) {
        let limit = self.scrollable();
        self.scroll = point(
            (self.scroll.x + delta.x).clamp(px(0.), limit.x),
            (self.scroll.y + delta.y).clamp(px(0.), limit.y),
        );
    }

    /// Brings the caret into view, moving as little as possible.
    fn scroll_to_caret(&mut self) {
        let Some(bounds) = self.layout.bounds else {
            return;
        };
        let line_height = self.layout.line_height;
        if line_height <= px(0.) {
            return;
        }
        let line = self.buffer.line_of(self.caret()) as f32;
        let top = line_height * line;
        let bottom = top + line_height;
        if top < self.scroll.y {
            self.scroll.y = top;
        } else if bottom > self.scroll.y + bounds.size.height {
            self.scroll.y = bottom - bounds.size.height;
        }
        self.scroll.y = self.scroll.y.clamp(px(0.), self.scrollable().y);

        // Horizontally, only when the caret's column is already shaped: at
        // startup nothing is, and guessing would jump the view.
        if let Some(shaped) = self.layout.shaped(self.buffer.line_of(self.caret())) {
            let (_, column) = self.buffer.point_of(self.caret());
            let x = shaped.x_for_index(column.min(shaped.len()));
            let width = bounds.size.width - self.layout.gutter;
            if x < self.scroll.x {
                self.scroll.x = x;
            } else if x > self.scroll.x + width - px(8.) {
                self.scroll.x = x - width + px(8.);
            }
            self.scroll.x = self.scroll.x.clamp(px(0.), self.scrollable().x);
        }
    }

    // --- hit testing ---------------------------------------------------------

    /// The byte offset under `position`, in window coordinates.
    pub(crate) fn offset_for_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.layout.bounds else {
            return 0;
        };
        let line_height = self.layout.line_height;
        if line_height <= px(0.) {
            return 0;
        }
        let relative_y = position.y - bounds.top() + self.scroll.y;
        let line = if relative_y < px(0.) {
            0
        } else {
            ((relative_y / line_height) as usize).min(self.buffer.line_count() - 1)
        };
        let x = position.x - bounds.left() - self.layout.gutter + self.scroll.x;
        let start = self.buffer.line_start(line);
        match self.layout.shaped(line) {
            // Off the left edge is the head of the line, never a negative
            // index into it.
            Some(shaped) if x > px(0.) => start + shaped.closest_index_for_x(x),
            Some(_) | None => start,
        }
    }

    // --- commands ------------------------------------------------------------

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let to = self.buffer.prev_grapheme(self.caret());
            self.move_to(to, cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let to = self.buffer.next_grapheme(self.caret());
            self.move_to(to, cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.buffer.prev_grapheme(self.caret());
        self.select_to(to, cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.buffer.next_grapheme(self.caret());
        self.select_to(to, cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.buffer.prev_word(self.caret());
        self.move_to(to, cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.buffer.next_word(self.caret());
        self.move_to(to, cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.buffer.prev_word(self.caret());
        self.select_to(to, cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.buffer.next_word(self.caret());
        self.select_to(to, cx);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1, false, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1, false, cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1, true, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1, true, cx);
    }

    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        let page = self.layout.visible_lines() as isize;
        self.move_vertically(-page, false, cx);
    }

    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        let page = self.layout.visible_lines() as isize;
        self.move_vertically(page, false, cx);
    }

    fn select_page_up(&mut self, _: &SelectPageUp, _: &mut Window, cx: &mut Context<Self>) {
        let page = self.layout.visible_lines() as isize;
        self.move_vertically(-page, true, cx);
    }

    fn select_page_down(&mut self, _: &SelectPageDown, _: &mut Window, cx: &mut Context<Self>) {
        let page = self.layout.visible_lines() as isize;
        self.move_vertically(page, true, cx);
    }

    /// Moves the caret `lines` rows, keeping the goal column.
    ///
    /// The column is counted in graphemes rather than pixels. In a proportional
    /// font that is not where the caret looked to be; in the monospaced font a
    /// log is read in, it is exactly where it looked to be, and it is the only
    /// definition that survives being asked in a headless test.
    fn move_vertically(&mut self, lines: isize, extend: bool, cx: &mut Context<Self>) {
        let caret = self.caret();
        let column = self
            .goal_column
            .unwrap_or_else(|| self.buffer.grapheme_column(caret));
        let line = self.buffer.line_of(caret) as isize;
        let target = (line + lines).clamp(0, self.buffer.line_count() as isize - 1) as usize;
        let offset = self.buffer.offset_at_column(target, column);

        if extend {
            self.select_to(offset, cx);
        } else {
            let offset = self.clamp(offset);
            self.selected_range = offset..offset;
            self.selection_reversed = false;
            self.history.break_group();
            self.scroll_to_caret();
            cx.emit(EditorEvent::SelectionChanged);
            cx.notify();
        }
        // Set after the move, because both branches above clear it.
        self.goal_column = Some(column);
    }

    fn line_start(&mut self, _: &LineStart, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.smart_line_start();
        self.move_to(to, cx);
    }

    fn line_end(&mut self, _: &LineEnd, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.buffer.line_of(self.caret());
        let to = self.buffer.line_end(line);
        self.move_to(to, cx);
    }

    fn select_line_start(&mut self, _: &SelectLineStart, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.smart_line_start();
        self.select_to(to, cx);
    }

    fn select_line_end(&mut self, _: &SelectLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.buffer.line_of(self.caret());
        let to = self.buffer.line_end(line);
        self.select_to(to, cx);
    }

    /// The first non-blank of the caret's line, or its head when the caret is
    /// already there.
    fn smart_line_start(&self) -> usize {
        let line = self.buffer.line_of(self.caret());
        let start = self.buffer.line_start(line);
        let text = self.buffer.line_text(line);
        let indent = text.len() - text.trim_start_matches([' ', '\t']).len();
        if self.caret() == start + indent {
            start
        } else {
            start + indent
        }
    }

    fn document_start(&mut self, _: &DocumentStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn document_end(&mut self, _: &DocumentEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.buffer.len(), cx);
    }

    fn select_document_start(
        &mut self,
        _: &SelectDocumentStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(0, cx);
    }

    fn select_document_end(
        &mut self,
        _: &SelectDocumentEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.buffer.len(), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.buffer.len();
        self.selection_reversed = false;
        self.after_move(cx);
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let caret = self.caret();
        let from = self.buffer.prev_grapheme(caret);
        self.delete_with(from..caret, EditKind::DeleteBack, cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        let caret = self.caret();
        let to = self.buffer.next_grapheme(caret);
        self.delete_with(caret..to, EditKind::DeleteForward, cx);
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let caret = self.caret();
        let from = self.buffer.prev_word(caret);
        self.delete_with(from..caret, EditKind::Other, cx);
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let caret = self.caret();
        let to = self.buffer.next_word(caret);
        self.delete_with(caret..to, EditKind::Other, cx);
    }

    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        // Auto-indent: the new line starts with whatever the current one starts
        // with, so a nested block in a YAML file stays lined up without anyone
        // pressing space.
        let line = self.buffer.line_of(self.selected_range.start);
        let text = self.buffer.line_text(line);
        let indent: String = text
            .chars()
            .take_while(|ch| *ch == ' ' || *ch == '\t')
            .collect();
        let mut inserted = String::with_capacity(indent.len() + 1);
        inserted.push('\n');
        inserted.push_str(&indent);
        let range = self.selected_range.clone();
        self.edit(range, &inserted, EditKind::Typing, cx);
    }

    fn indent(&mut self, _: &Indent, _: &mut Window, cx: &mut Context<Self>) {
        let (first, last) = line_span(&self.buffer, &self.selected_range);
        if first == last && self.selected_range.is_empty() {
            // A caret on one line: `Tab` is a character, not a command.
            let range = self.selected_range.clone();
            self.edit(range, INDENT, EditKind::Other, cx);
            return;
        }

        // Bottom upwards, so that every edit's offsets are the ones the buffer
        // has when it is applied.
        let mut edits = Vec::new();
        for line in (first..=last).rev() {
            let at = self.buffer.line_start(line);
            edits.push(Edit {
                start: at,
                removed: String::new(),
                inserted: INDENT.to_owned(),
            });
        }
        let grown = INDENT.len() * (last - first + 1);
        let after = self.selected_range.start + INDENT.len()..self.selected_range.end + grown;
        self.transact(edits, after, cx);
    }

    fn outdent(&mut self, _: &Outdent, _: &mut Window, cx: &mut Context<Self>) {
        let (first, last) = line_span(&self.buffer, &self.selected_range);
        let mut edits = Vec::new();
        let mut removed_before_start = 0;
        let mut removed_total = 0;
        for line in (first..=last).rev() {
            let start = self.buffer.line_start(line);
            let text = self.buffer.line_text(line);
            let width = outdent_width(&text);
            if width == 0 {
                continue;
            }
            edits.push(Edit {
                start,
                removed: text[..width].to_owned(),
                inserted: String::new(),
            });
            removed_total += width;
            if start < self.selected_range.start {
                removed_before_start = width;
            }
        }
        let start = self
            .selected_range
            .start
            .saturating_sub(removed_before_start);
        let after = start
            ..self
                .selected_range
                .end
                .saturating_sub(removed_total)
                .max(start);
        self.transact(edits, after, cx);
    }

    fn toggle_comment(&mut self, _: &ToggleComment, _: &mut Window, cx: &mut Context<Self>) {
        // A format with no comment syntax has no toggle. JSON is the only one,
        // and writing a `#` into a `.json` would produce a file its own reader
        // rejects; the context menu greys the row for the same reason.
        let Some(prefix) = self.highlighter.language().line_comment() else {
            return;
        };
        let (first, last) = line_span(&self.buffer, &self.selected_range);

        let lines: Vec<(usize, String)> = (first..=last)
            .map(|line| (line, self.buffer.line_text(line).into_owned()))
            .filter(|(_, text)| !text.trim().is_empty())
            .collect();
        if lines.is_empty() {
            return;
        }

        // Uncomment only when every line is already commented; otherwise the
        // press comments the block, which is what a mixed selection means.
        let all_commented = lines
            .iter()
            .all(|(_, text)| text.trim_start().starts_with(prefix));
        let column = lines
            .iter()
            .map(|(_, text)| text.len() - text.trim_start().len())
            .min()
            .unwrap_or(0);

        let mut edits = Vec::new();
        for (line, text) in lines.iter().rev() {
            let start = self.buffer.line_start(*line);
            if all_commented {
                let indent = text.len() - text.trim_start().len();
                let mut width = prefix.len();
                // Take the space back too, if this is a comment we wrote.
                if text[indent + width..].starts_with(' ') {
                    width += 1;
                }
                edits.push(Edit {
                    start: start + indent,
                    removed: text[indent..indent + width].to_owned(),
                    inserted: String::new(),
                });
            } else {
                edits.push(Edit {
                    start: start + column,
                    removed: String::new(),
                    inserted: format!("{prefix} "),
                });
            }
        }
        let after = self.selected_range.clone();
        self.transact(edits, after, cx);
        // The selection offsets moved with the text; recompute rather than
        // guess, by putting the caret back on the same line and column it was.
        let caret = self.clamp(self.caret());
        self.selected_range = caret..caret;
        cx.notify();
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            return;
        }
        let text = self.buffer.slice(self.selected_range.clone());
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            return;
        }
        let text = self.buffer.slice(self.selected_range.clone());
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        let range = self.selected_range.clone();
        self.edit(range, "", EditKind::Other, cx);
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        // Line breaks are kept, unlike in the single-line field: a pasted block
        // of configuration is the whole point of a multi-line editor.
        let range = self.selected_range.clone();
        self.edit(range, &text, EditKind::Other, cx);
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(transaction) = self.history.pop_undo() else {
            return;
        };
        for edit in transaction.edits.iter().rev() {
            self.splice(edit.new_range(), &edit.removed);
        }
        self.set_selection_state(&transaction.before);
        self.marked_range = None;
        self.history.finish_undo(transaction);
        self.changed(cx);
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(transaction) = self.history.pop_redo() else {
            return;
        };
        for edit in &transaction.edits {
            self.splice(edit.old_range(), &edit.inserted);
        }
        self.set_selection_state(&transaction.after);
        self.marked_range = None;
        self.history.finish_redo(transaction);
        self.changed(cx);
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    // --- find ----------------------------------------------------------------

    fn open_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        self.show_find(false, window, cx);
    }

    fn open_replace(&mut self, _: &Replace, window: &mut Window, cx: &mut Context<Self>) {
        self.show_find(true, window, cx);
    }

    /// Opens the bar, seeding it with the selection when there is one.
    fn show_find(&mut self, replacing: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.find.open = true;
        self.find.replacing = replacing;
        if !self.selected_range.is_empty() {
            let seed = self.buffer.slice(self.selected_range.clone());
            if !seed.contains('\n') {
                self.find_query
                    .update(cx, |input, cx| input.set_content(seed, cx));
            }
        }
        self.refresh_matches(cx);
        let handle = self.find_query.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        cx.notify();
    }

    fn close_find(&mut self, _: &CloseFind, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find.open {
            // With the find bar already shut there is nothing here for Escape
            // to do; let it climb to whoever is listening above the editor.
            cx.propagate();
            return;
        }
        self.find.open = false;
        self.find.matches.clear();
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(range) = self.find.advance() {
            self.select_range(range, cx);
        }
    }

    fn find_prev(&mut self, _: &FindPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(range) = self.find.retreat() {
            self.select_range(range, cx);
        }
    }

    fn replace_next(&mut self, _: &ReplaceNext, _: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = self.find.current() else {
            return;
        };
        let replacement = self.find_replacement.read(cx).content().to_owned();
        self.edit(range.clone(), &replacement, EditKind::Other, cx);
        self.find.shift_after_replace(&range, replacement.len());
        if let Some(next) = self.find.current() {
            self.select_range(next, cx);
        }
    }

    fn replace_all(&mut self, _: &ReplaceAll, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only || self.find.matches.is_empty() {
            return;
        }
        let replacement = self.find_replacement.read(cx).content().to_owned();
        // Bottom upwards, so that no edit disturbs the offsets of the next.
        let edits: Vec<Edit> = self
            .find
            .matches
            .iter()
            .rev()
            .map(|range| Edit {
                start: range.start,
                removed: self.buffer.slice(range.clone()),
                inserted: replacement.clone(),
            })
            .collect();
        let caret = self.caret();
        self.transact(edits, caret..caret, cx);
        self.refresh_matches(cx);
    }

    /// Puts `query` into the find bar and re-runs the search.
    ///
    /// For a host that starts a search of its own — "find this host name in the
    /// log", say — and for tests.
    pub fn set_find_query(&mut self, query: &str, cx: &mut Context<Self>) {
        self.find_query
            .update(cx, |input, cx| input.set_content(query.to_owned(), cx));
        self.refresh_matches(cx);
        cx.notify();
    }

    /// Puts `text` into the replace field.
    pub fn set_find_replacement(&mut self, text: &str, cx: &mut Context<Self>) {
        self.find_replacement
            .update(cx, |input, cx| input.set_content(text.to_owned(), cx));
        cx.notify();
    }

    /// Sets whether the search distinguishes case, and re-runs it.
    pub fn set_find_case_sensitive(&mut self, case_sensitive: bool, cx: &mut Context<Self>) {
        self.find.case_sensitive = case_sensitive;
        self.refresh_matches(cx);
        cx.notify();
    }

    /// Every match of the current query, in order.
    pub fn matches(&self) -> &[Range<usize>] {
        &self.find.matches
    }

    /// Re-runs the search over the buffer, from whatever the query field says.
    fn refresh_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.find_query.read(cx).content().to_owned();
        let text = self.buffer.text();
        self.find.search(&text, &query, self.caret());
    }

    /// Keeps the matches in step with the query field, which has no change
    /// callback of its own.
    ///
    /// Called from `render`, where a stale highlight would be visible one frame
    /// later; comparing against the query the matches were found with is what
    /// keeps it from re-scanning on every frame.
    fn sync_matches(&mut self, cx: &mut Context<Self>) {
        if !self.find.open {
            return;
        }
        let query = self.find_query.read(cx).content().to_owned();
        if query == self.find.query {
            return;
        }
        let text = self.buffer.text();
        self.find.search(&text, &query, self.caret());
    }

    /// What the renderer paints a highlight behind.
    pub(crate) fn find_matches(&self) -> &[Range<usize>] {
        &self.find.matches
    }

    /// Which match is the current one.
    pub(crate) fn current_match(&self) -> Option<Range<usize>> {
        self.find.current()
    }

    // --- mouse ---------------------------------------------------------------

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let offset = self.offset_for_position(event.position);
        self.is_selecting = true;
        self.granularity = match event.click_count {
            0 | 1 => Granularity::Character,
            2 => Granularity::Word,
            _ => Granularity::Line,
        };

        match self.granularity {
            Granularity::Character => {
                if event.modifiers.shift {
                    self.select_to(offset, cx);
                } else {
                    self.drag_anchor = offset..offset;
                    self.move_to(offset, cx);
                }
            }
            Granularity::Word => {
                self.drag_anchor = self.buffer.word_at(offset);
                let anchor = self.drag_anchor.clone();
                self.select_range(anchor, cx);
            }
            Granularity::Line => {
                let line = self.buffer.line_of(offset);
                let end = if line + 1 < self.buffer.line_count() {
                    self.buffer.line_start(line + 1)
                } else {
                    self.buffer.len()
                };
                self.drag_anchor = self.buffer.line_start(line)..end;
                let anchor = self.drag_anchor.clone();
                self.select_range(anchor, cx);
            }
        }
    }

    /// A right click: take the focus, say where it was, and touch nothing else.
    ///
    /// The caret and the selection stay exactly where they are, which is what
    /// every editor does, and the reason is the main use of the gesture: the
    /// menu is nearly always raised *over* a selection in order to copy or cut
    /// it, and a press that collapsed the selection first would leave every one
    /// of those items either greyed out or acting on nothing. A right click
    /// moving the selection is a rule about lists — a tree row, a tab — where
    /// the press names one thing; here it would destroy the argument the menu is
    /// about.
    fn on_right_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.focus_handle.focus(window, cx);
        cx.emit(EditorEvent::ContextMenu {
            position: event.position,
        });
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_selecting {
            return;
        }
        let offset = self.offset_for_position(event.position);
        match self.granularity {
            Granularity::Character => self.select_to(offset, cx),
            Granularity::Word => {
                let word = self.buffer.word_at(offset);
                let range =
                    self.drag_anchor.start.min(word.start)..self.drag_anchor.end.max(word.end);
                self.select_range(range, cx);
            }
            Granularity::Line => {
                let line = self.buffer.line_of(offset);
                let start = self.buffer.line_start(line);
                let end = if line + 1 < self.buffer.line_count() {
                    self.buffer.line_start(line + 1)
                } else {
                    self.buffer.len()
                };
                let range = self.drag_anchor.start.min(start)..self.drag_anchor.end.max(end);
                self.select_range(range, cx);
            }
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(self.layout.line_height);
        self.scroll_by(point(-delta.x, -delta.y));
        self.wake_bars(cx);
        cx.notify();
    }

    // --- scrollbars ----------------------------------------------------------

    /// Notices the surface has moved and arms the fade-out, exactly as every
    /// other scrolling surface in the app does it.
    fn wake_bars(&mut self, cx: &mut Context<Self>) {
        let limit = self.scrollable();
        let progress = |scrolled: Pixels, limit: Pixels| {
            if limit <= px(0.) {
                0.
            } else {
                f32::from(scrolled) / f32::from(limit)
            }
        };
        if let Some(epoch) = self.vertical_bar.moved(progress(self.scroll.y, limit.y)) {
            hide_later(epoch, cx, |editor: &mut Self| {
                Some(&mut editor.vertical_bar)
            });
        }
        if let Some(epoch) = self.horizontal_bar.moved(progress(self.scroll.x, limit.x)) {
            hide_later(epoch, cx, |editor: &mut Self| {
                Some(&mut editor.horizontal_bar)
            });
        }
    }

    /// One of the two overlay bars as it stands this frame.
    ///
    /// `pub(crate)` for the regression test that holds the thumb to the scroll
    /// position; the app itself never reads a bar from outside.
    pub(crate) fn scrollbar(&self, axis: ScrollbarAxis) -> Option<Scrollbar> {
        let bounds = self.layout.bounds?;
        let limit = self.scrollable();
        let (visible, scrollable, scrolled, state) = match axis {
            ScrollbarAxis::Vertical => (
                bounds.size.height,
                limit.y,
                self.scroll.y,
                &self.vertical_bar,
            ),
            ScrollbarAxis::Horizontal => (
                bounds.size.width,
                limit.x,
                self.scroll.x,
                &self.horizontal_bar,
            ),
        };
        if scrollable <= px(0.) {
            return None;
        }
        Some(
            Scrollbar::new(
                match axis {
                    ScrollbarAxis::Vertical => "editor-v-bar",
                    ScrollbarAxis::Horizontal => "editor-h-bar",
                },
                axis,
                bounds,
                f32::from(visible),
                f32::from(scrollable),
                // The raw distance, not the fraction of the range: the bar
                // divides by `scrollable` itself, and handing it a value that
                // was already divided once pinned the thumb to the top however
                // far the surface had scrolled.
                f32::from(scrolled),
            )
            .fade(state.fade()),
        )
    }

    /// The state of whichever bar rides `axis`.
    fn bar_mut(&mut self, axis: ScrollbarAxis) -> &mut ScrollbarState {
        match axis {
            ScrollbarAxis::Vertical => &mut self.vertical_bar,
            ScrollbarAxis::Horizontal => &mut self.horizontal_bar,
        }
    }

    /// Puts a bar up while the pointer rests on the edge it rides, and starts
    /// it going the moment the pointer leaves.
    fn hover_bar(&mut self, axis: ScrollbarAxis, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            if self.bar_mut(axis).hover_enter() {
                cx.notify();
            }
            return;
        }

        if let Some(epoch) = self.bar_mut(axis).hover_leave() {
            hide_now(self, epoch, cx, move |editor: &mut Self| {
                Some(editor.bar_mut(axis))
            });
        }
    }

    /// Lets go of a thumb and starts the clock that takes the bar down.
    fn release_thumb(&mut self, cx: &mut Context<Self>) {
        for (axis, released) in [
            (ScrollbarAxis::Vertical, self.vertical_bar.release()),
            (ScrollbarAxis::Horizontal, self.horizontal_bar.release()),
        ] {
            let Some(epoch) = released else {
                continue;
            };
            hide_later(epoch, cx, move |editor: &mut Self| {
                Some(editor.bar_mut(axis))
            });
        }
        cx.notify();
    }

    /// Moves the surface to where a dragged thumb says it should be.
    fn on_thumb_drag(&mut self, event: &DragMoveEvent<DraggedThumb>, cx: &mut Context<Self>) {
        for axis in [ScrollbarAxis::Vertical, ScrollbarAxis::Horizontal] {
            let Some(bar) = self.scrollbar(axis) else {
                continue;
            };
            let Some(progress) = bar.dragged(event, cx) else {
                continue;
            };
            let limit = self.scrollable();
            match axis {
                ScrollbarAxis::Vertical => {
                    self.vertical_bar.hold();
                    self.scroll.y = limit.y * progress.clamp(0., 1.);
                }
                ScrollbarAxis::Horizontal => {
                    self.horizontal_bar.hold();
                    self.scroll.x = limit.x * progress.clamp(0., 1.);
                }
            }
            cx.notify();
        }
    }

    // --- what the element reads ---------------------------------------------

    /// The buffer, for the renderer.
    pub(crate) const fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// The syntax cache, for the renderer.
    pub(crate) const fn highlighter(&self) -> &Highlighter {
        &self.highlighter
    }

    /// The composing run, in bytes, for the underline the renderer draws.
    pub(crate) fn marked(&self) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    /// How many lines the element shaped last frame.
    ///
    /// The virtualisation, as a number: it is the height of the viewport in
    /// rows and never the length of the buffer, which is what the tests read.
    pub(crate) fn shaped_lines(&self) -> usize {
        self.layout.lines.len()
    }

    /// Whether the editor has the keyboard.
    pub(crate) fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    /// The focus handle the element hands to `window.handle_input`.
    pub(crate) fn input_focus(&self) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// The first and last line `range` touches, the terminator of the last one
/// excluded.
///
/// What `Tab`, `shift-Tab` and the comment toggle all act on: they are commands
/// about lines, and a selection is the only thing that says which.
fn line_span(buffer: &Buffer, range: &Range<usize>) -> (usize, usize) {
    let first = buffer.line_of(range.start);
    // A selection that ends exactly at the head of a line has not touched it.
    let last_offset =
        if range.end > range.start && buffer.line_start(buffer.line_of(range.end)) == range.end {
            range.end - 1
        } else {
            range.end
        };
    (first, buffer.line_of(last_offset))
}

/// How many leading bytes one press of `shift-tab` takes off a line.
fn outdent_width(text: &str) -> usize {
    if text.starts_with('\t') {
        return 1;
    }
    text.bytes()
        .take(INDENT.len())
        .take_while(|byte| *byte == b' ')
        .count()
}

impl EventEmitter<EditorEvent> for EditorView {}

impl Focusable for EditorView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for EditorView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.buffer.range_from_utf16(&range_utf16);
        actual_range.replace(self.buffer.range_to_utf16(&range));
        Some(self.buffer.slice(range))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.buffer.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.buffer.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
        self.history.cancel_composition();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        // The same precedence the platform protocol specifies and `TextInput`
        // implements: an explicit range wins, then the composing run, then the
        // selection.
        let range = range_utf16
            .as_ref()
            .map(|range| self.buffer.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let range = self.clamp(range.start)..self.clamp(range.end);

        if self.history.in_composition() {
            // Committing a composition: the buffer already holds the last
            // preview, and the history records the whole syllable as one edit.
            self.splice(range.clone(), new_text);
            let caret = range.start + new_text.len();
            self.selected_range = caret..caret;
            self.selection_reversed = false;
            self.marked_range = None;
            self.goal_column = None;
            self.history
                .end_composition(new_text.to_owned(), self.selection_state());
            self.changed(cx);
            return;
        }

        // An ordinary keystroke. One grapheme at a time is what the platform
        // sends, so this is where a run of typing gets grouped.
        let kind = if new_text.chars().count() == 1 && range.is_empty() {
            EditKind::Typing
        } else {
            EditKind::Other
        };
        self.edit(range, new_text, kind, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range| self.buffer.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let range = self.clamp(range.start)..self.clamp(range.end);

        if !self.history.in_composition() {
            let displaced = self.buffer.slice(range.clone());
            let before = self.selection_state();
            self.history
                .begin_composition(range.start, displaced, before);
        }

        self.splice(range.clone(), new_text);

        self.marked_range = if new_text.is_empty() {
            None
        } else {
            Some(range.start..range.start + new_text.len())
        };

        // The new selection is relative to the text just inserted -- see the
        // module documentation for why this is not what gpui's own example
        // does, and what that costs on Windows.
        self.selected_range = match new_selected_range_utf16 {
            Some(relative) => {
                let start = range.start + utf16_to_byte(new_text, relative.start);
                let end = range.start + utf16_to_byte(new_text, relative.end);
                start.min(end)..start.max(end)
            }
            None => {
                let caret = range.start + new_text.len();
                caret..caret
            }
        };
        self.selection_reversed = false;
        self.goal_column = None;

        if new_text.is_empty() {
            // An empty preview cancels the composition rather than committing
            // an empty one.
            self.history.cancel_composition();
        }
        self.changed(cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.buffer.range_from_utf16(&range_utf16);
        let (line, column) = self.buffer.point_of(range.start);
        let shaped = self.layout.shaped(line)?;
        let (end_line, end_column) = self.buffer.point_of(range.end);

        let left = element_bounds.left() + self.layout.gutter - self.scroll.x
            + shaped.x_for_index(column.min(shaped.len()));
        let right = if end_line == line {
            element_bounds.left() + self.layout.gutter - self.scroll.x
                + shaped.x_for_index(end_column.min(shaped.len()))
        } else {
            element_bounds.right()
        };
        let top = element_bounds.top() + self.layout.line_height * (line as f32) - self.scroll.y;
        Some(Bounds::from_corners(
            point(left, top),
            point(right, top + self.layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.buffer.offset_to_utf16(self.offset_for_position(point)))
    }
}

/// The byte offset `offset_utf16` code units into `text`.
///
/// A local walk over the inserted text, which is one syllable long, rather than
/// a buffer index: the offsets an IME sends with a preview are relative to the
/// preview.
fn utf16_to_byte(text: &str, offset_utf16: usize) -> usize {
    let mut utf16 = 0;
    for (at, ch) in text.char_indices() {
        if utf16 >= offset_utf16 {
            return at;
        }
        utf16 += ch.len_utf16();
    }
    text.len()
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_matches(cx);
        let theme = theme(cx);
        let palette = self.palette;
        let read_only = self.read_only;

        let surface = div()
            .id("editor-surface")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .relative()
            .flex_grow_1()
            .size_full()
            .overflow_hidden()
            // Opaque even over a translucent window, unlike the terminal
            // surface: a file is read a character at a time and a desktop
            // showing through behind it is exactly the wrong place for contrast
            // to go. Safe to paint unconditionally because it is *opaque* — the
            // alpha saturation that stops two tinted fills from stacking is not
            // a hazard for a fill that means to hide what is under it.
            .bg(palette.background)
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::line_start))
            .on_action(cx.listener(Self::line_end))
            .on_action(cx.listener(Self::select_line_start))
            .on_action(cx.listener(Self::select_line_end))
            .on_action(cx.listener(Self::document_start))
            .on_action(cx.listener(Self::document_end))
            .on_action(cx.listener(Self::select_document_start))
            .on_action(cx.listener(Self::select_document_end))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::select_page_up))
            .on_action(cx.listener(Self::select_page_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::open_find))
            .on_action(cx.listener(Self::open_replace))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_prev))
            .on_action(cx.listener(Self::close_find))
            .when(!read_only, |this| {
                this.on_action(cx.listener(Self::backspace))
                    .on_action(cx.listener(Self::delete))
                    .on_action(cx.listener(Self::delete_word_left))
                    .on_action(cx.listener(Self::delete_word_right))
                    .on_action(cx.listener(Self::newline))
                    .on_action(cx.listener(Self::indent))
                    .on_action(cx.listener(Self::outdent))
                    .on_action(cx.listener(Self::toggle_comment))
                    .on_action(cx.listener(Self::cut))
                    .on_action(cx.listener(Self::paste))
                    .on_action(cx.listener(Self::undo))
                    .on_action(cx.listener(Self::redo))
                    // Bound on the surface as well as on the bar, so that a
                    // host driving the search itself does not have to open the
                    // bar to use them.
                    .on_action(cx.listener(Self::replace_next))
                    .on_action(cx.listener(Self::replace_all))
            })
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_drag_move::<DraggedThumb>(cx.listener(
                |editor, event: &DragMoveEvent<DraggedThumb>, _window, cx| {
                    editor.on_thumb_drag(event, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|editor, _: &MouseUpEvent, _window, cx| editor.release_thumb(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|editor, _: &MouseUpEvent, _window, cx| editor.release_thumb(cx)),
            )
            .child(EditorElement::new(cx.entity()))
            .children(self.scrollbar(ScrollbarAxis::Vertical).and_then(|bar| {
                bar.on_hover(cx.listener(|editor, hovered: &bool, _window, cx| {
                    editor.hover_bar(ScrollbarAxis::Vertical, *hovered, cx);
                }))
                .render(&theme)
            }))
            .children(self.scrollbar(ScrollbarAxis::Horizontal).and_then(|bar| {
                bar.on_hover(cx.listener(|editor, hovered: &bool, _window, cx| {
                    editor.hover_bar(ScrollbarAxis::Horizontal, *hovered, cx);
                }))
                .render(&theme)
            }));

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(surface)
            .when(self.find.open, |this| this.child(self.render_find_bar(cx)))
    }
}

impl EditorView {
    /// The find bar, which is an ordinary row of widgets and not part of the
    /// text surface at all.
    fn render_find_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let total = self.find.matches.len();
        // Not a translated string: both numbers are values, and every language
        // writes "3/17" the same way.
        let position = if total == 0 {
            "0/0".to_owned()
        } else {
            format!("{}/{total}", self.find.current + 1)
        };
        let case_sensitive = self.find.case_sensitive;
        let replacing = self.find.replacing;
        let query = self.find_query.clone();
        let replacement = self.find_replacement.clone();

        div()
            .key_context(FIND_KEY_CONTEXT)
            .flex()
            .flex_col()
            .gap(px(4.))
            .w_full()
            .p(px(6.))
            .bg(theme.surface)
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(13.))
            .text_color(theme.text)
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_prev))
            .on_action(cx.listener(Self::close_find))
            .on_action(cx.listener(Self::replace_next))
            .on_action(cx.listener(Self::replace_all))
            .on_action(cx.listener(Self::open_find))
            .on_action(cx.listener(Self::open_replace))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(div().flex_grow_1().child(query))
                    .child(div().flex_none().min_w(px(56.)).child(position))
                    .child(
                        // `Aa` rather than a word: it is the mark every editor
                        // puts on this toggle, and it is not English.
                        Checkbox::new("editor-find-case", "Aa")
                            .checked(case_sensitive)
                            .on_toggle({
                                let editor = cx.entity();
                                move |checked, _window, cx| {
                                    editor.update(cx, |editor, cx| {
                                        editor.find.case_sensitive = checked;
                                        editor.refresh_matches(cx);
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
            )
            .when(replacing, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(div().flex_grow_1().child(replacement)),
                )
            })
    }
}
