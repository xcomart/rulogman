//! The terminal model: an ANSI parser plus a scrollback aware screen buffer.
//!
//! [`TerminalModel`] wraps `alacritty_terminal`'s [`Term`] and drives it with
//! bytes that arrive from an SSH channel. It deliberately does **not** spawn a
//! local PTY - `alacritty_terminal::tty` is unused - and it is not `Send`,
//! because it is only ever touched from the UI thread.

use std::cell::RefCell;
use std::rc::Rc;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, GridCell, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, Processor};

use crate::charset::{Charset, CharsetDecoder};
use crate::cwd::CwdTracker;
use crate::keys::TermModes;
use crate::snapshot::{
    CursorPos, RunFlags, ScrollPosition, StyledRun, TerminalLine, TerminalSnapshot,
};
use crate::theme::{Rgb, TerminalTheme};

/// Grid geometry handed to [`Term::new`] and [`Term::resize`].
#[derive(Debug, Clone, Copy)]
struct TermDimensions {
    columns: usize,
    screen_lines: usize,
    total_lines: usize,
}

impl TermDimensions {
    fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let columns = cols as usize;
        let screen_lines = rows as usize;
        Self {
            columns,
            screen_lines,
            total_lines: screen_lines.saturating_add(scrollback),
        }
    }
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.total_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// State the [`EventListener`] shares with the model.
#[derive(Debug, Default)]
struct SharedState {
    /// Title change requested since the last drain.
    ///
    /// The outer `Option` means "a change happened", the inner one carries the
    /// new title (`None` after `OSC 2` with an empty argument / `ResetTitle`).
    pending_title: Option<Option<String>>,
    /// Replies the terminal wants to send back over the channel, for example
    /// the answer to a Device Status Report.
    pty_output: Vec<u8>,
}

/// Event sink that funnels the few events we care about into [`SharedState`].
#[derive(Debug, Clone)]
struct EventProxy {
    state: Rc<RefCell<SharedState>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => self.state.borrow_mut().pending_title = Some(Some(title)),
            Event::ResetTitle => self.state.borrow_mut().pending_title = Some(None),
            Event::PtyWrite(text) => {
                self.state
                    .borrow_mut()
                    .pty_output
                    .extend_from_slice(text.as_bytes());
            }
            _ => {}
        }
    }
}

/// A terminal screen fed by a byte stream.
pub struct TerminalModel {
    term: Term<EventProxy>,
    parser: Processor,
    state: Rc<RefCell<SharedState>>,
    theme: TerminalTheme,
    scrollback: usize,
    title: Option<String>,
    /// Watches the same bytes for working directory announcements, which the
    /// `alacritty` parser discards.
    cwd: CwdTracker,
    /// What the incoming bytes mean, and what outgoing ones have to be.
    ///
    /// Kept here rather than at the transport because it is the emulator's
    /// question: the model is the only thing that sees the whole byte stream,
    /// and the input encoder needs the same answer the decoder is using.
    charset: Charset,
    /// Inbound decoder, `None` exactly when the charset is UTF-8.
    ///
    /// The absence is the fast path and not merely an optimisation: `vte` decodes
    /// UTF-8 itself, including partial sequences across chunks, so a UTF-8
    /// session must reach it as the untouched bytes it always did.
    decoder: Option<CharsetDecoder>,
    /// Reused destination for the decoder, so a decoded session does not
    /// allocate a `String` for every chunk that arrives.
    decode_buf: String,
}

impl std::fmt::Debug for TerminalModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (cols, rows) = self.size();
        f.debug_struct("TerminalModel")
            .field("cols", &cols)
            .field("rows", &rows)
            .field("scrollback", &self.scrollback)
            .field("title", &self.title)
            .field("cwd", &self.cwd.cwd())
            .field("charset", &self.charset)
            .finish_non_exhaustive()
    }
}

impl TerminalModel {
    /// Create a terminal of `cols` x `rows` cells with `scrollback` lines of
    /// history.
    ///
    /// Both dimensions are clamped to at least one cell.
    pub fn new(cols: u16, rows: u16, scrollback: usize, theme: TerminalTheme) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);

        let state = Rc::new(RefCell::new(SharedState::default()));
        let term = Self::build_term(cols, rows, scrollback, Rc::clone(&state));

        Self {
            term,
            parser: Processor::new(),
            state,
            theme,
            scrollback,
            title: None,
            cwd: CwdTracker::new(),
            charset: Charset::default(),
            decoder: None,
            decode_buf: String::new(),
        }
    }

    fn build_term(
        cols: u16,
        rows: u16,
        scrollback: usize,
        state: Rc<RefCell<SharedState>>,
    ) -> Term<EventProxy> {
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        let dimensions = TermDimensions::new(cols, rows, scrollback);
        Term::new(config, &dimensions, EventProxy { state })
    }

    /// Feed raw bytes coming from the remote shell into the parser.
    ///
    /// Returns `true` when the bytes announced a new working directory, so a
    /// caller can react to a directory change without polling
    /// [`TerminalModel::cwd`] on every chunk.
    ///
    /// On a session whose charset is not UTF-8 the chunk is transcoded first,
    /// and the emulator sees the UTF-8 it is the only thing able to read. That
    /// costs the escape sequences nothing: every supported charset is
    /// ASCII-transparent, so `ESC`, `CSI` and the OSC terminators come out of
    /// the decoder as the same bytes that went in.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        if self.decoder.is_some() {
            // Taken apart and put back to satisfy the borrow checker: the
            // decoder and the buffer are both fields of `self`, and `advance`
            // wants `self` too.
            let mut buf = std::mem::take(&mut self.decode_buf);
            buf.clear();
            if let Some(decoder) = &mut self.decoder {
                decoder.decode(bytes, &mut buf);
            }
            let cwd_changed = self.advance(buf.as_bytes());
            self.decode_buf = buf;
            return cwd_changed;
        }

        self.advance(bytes)
    }

    /// Feed text rulogman generated itself, bypassing the charset decoder.
    ///
    /// The distinction matters: a notice such as a failed port forwarding is
    /// written *here*, in UTF-8, and is not part of the remote host's byte
    /// stream. Running it through an EUC-KR decoder would mangle every non-ASCII
    /// character in it — and worse, hand the decoder bytes that split its
    /// pending state, corrupting the next real chunk from the host.
    pub fn feed_str(&mut self, text: &str) -> bool {
        self.advance(text.as_bytes())
    }

    /// Drive the emulator and the directory watcher with UTF-8 bytes.
    fn advance(&mut self, bytes: &[u8]) -> bool {
        // Runs before the emulator because the sequences it looks for are the
        // ones `alacritty` drops; it only observes and never rewrites `bytes`.
        // It runs after the charset decoder for a different reason: the `OSC 7`
        // payload is text too, and a path from a legacy-charset shell would
        // otherwise arrive as bytes the tracker rejects as non-UTF-8.
        let cwd_changed = self.cwd.feed(bytes).is_some();

        self.parser.advance(&mut self.term, bytes);

        if let Some(title) = self.state.borrow_mut().pending_title.take() {
            self.title = title;
        }

        cwd_changed
    }

    /// Resize the terminal, clamping both dimensions to at least one cell.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if self.size() == (cols, rows) {
            return;
        }
        self.term
            .resize(TermDimensions::new(cols, rows, self.scrollback));
    }

    /// Current size as `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        let grid = self.term.grid();
        (grid.columns() as u16, grid.screen_lines() as u16)
    }

    /// Where the viewport currently sits in the scrollback.
    ///
    /// The cheap half of [`TerminalModel::snapshot`], for callers that want the
    /// scroll position every frame and the screen contents not at all.
    pub fn scroll_position(&self) -> ScrollPosition {
        let grid = self.term.grid();
        ScrollPosition {
            display_offset: grid.display_offset(),
            history: grid.history_size(),
            rows: grid.screen_lines(),
        }
    }

    /// Build an immutable view of the visible screen for the renderer.
    pub fn snapshot(&self) -> TerminalSnapshot {
        let grid = self.term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        let display_offset = grid.display_offset();

        let mut lines = Vec::with_capacity(rows);
        for row in 0..rows {
            let line = Line(row as i32 - display_offset as i32);
            lines.push(self.build_line(&grid[line], cols));
        }

        let content = self.term.renderable_content();
        let cursor_point = content.cursor.point;
        let cursor_row = cursor_point.line.0 as isize + display_offset as isize;
        let cursor_in_view = cursor_row >= 0 && (cursor_row as usize) < rows;
        let cursor = CursorPos {
            line: cursor_row.clamp(0, rows.saturating_sub(1) as isize) as u16,
            col: cursor_point.column.0.min(cols.saturating_sub(1)) as u16,
        };
        let cursor_visible = cursor_in_view && content.cursor.shape != CursorShape::Hidden;

        TerminalSnapshot {
            cols: cols as u16,
            rows: rows as u16,
            lines,
            cursor,
            cursor_visible,
            display_offset,
            total_scrollback: grid.history_size(),
        }
    }

    /// Turn a single grid row into style runs.
    fn build_line(&self, row: &alacritty_terminal::grid::Row<Cell>, cols: usize) -> TerminalLine {
        // Drop trailing cells that render as blank default background so that
        // an untouched row produces no runs at all.
        let mut len = cols;
        while len > 0 && row[Column(len - 1)].is_empty() {
            len -= 1;
        }

        let mut runs: Vec<StyledRun> = Vec::new();
        let mut current: Option<StyledRun> = None;

        for col in 0..len {
            let cell = &row[Column(col)];

            // The trailing half of a double width character carries no glyph of
            // its own; skipping it keeps the column alignment intact because the
            // wide character already occupies two cells when rendered.
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }

            let (fg, bg, flags) = self.cell_style(cell);
            // Only plain ASCII cells may share a run: their glyphs advance by
            // exactly one cell in a monospace font, so shaping them together
            // still lands every character on its own column. Everything else
            // becomes a run of its own, because a fallback font's advance would
            // otherwise drag the rest of the run off the grid.
            let simple = cell.c.is_ascii() && cell.zerowidth().is_none();
            let extends = simple
                && current.as_ref().is_some_and(|run| {
                    run.text.is_ascii() && run.fg == fg && run.bg == bg && run.flags == flags
                });

            if extends {
                let run = current.as_mut().expect("checked above");
                push_cell(&mut run.text, cell);
                run.cells += 1;
            } else {
                if let Some(run) = current.take() {
                    runs.push(run);
                }
                let mut text = String::new();
                push_cell(&mut text, cell);
                current = Some(StyledRun {
                    text,
                    start_col: col as u16,
                    cells: if cell.flags.contains(Flags::WIDE_CHAR) {
                        2
                    } else {
                        1
                    },
                    fg,
                    bg,
                    flags,
                });
            }
        }

        if let Some(run) = current {
            runs.push(run);
        }

        TerminalLine { runs }
    }

    /// Resolve the final colors and attributes of a single cell.
    fn cell_style(&self, cell: &Cell) -> (Rgb, Rgb, RunFlags) {
        let flags = run_flags(cell.flags);
        let mut fg = self.theme.resolve(cell.fg, true, flags);
        let mut bg = self.theme.resolve(cell.bg, false, flags);

        if flags.contains(RunFlags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.contains(RunFlags::HIDDEN) {
            fg = bg;
        }

        (fg, bg, flags)
    }

    /// Scroll the viewport; positive values move up into the scrollback.
    pub fn scroll_lines(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    /// Jump back to the bottom of the scrollback.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Forget the scrollback, keeping the live screen.
    ///
    /// The rows that have already scrolled off the screen go; the ones the user
    /// is looking at — prompt, cursor and all — stay exactly where they are.
    /// That is the difference from [`TerminalModel::reset`], which throws the
    /// screen away too: this is a housekeeping command aimed at the history, not
    /// a fresh start, so the title and the working directory are left alone as
    /// well. Neither describes the output, and the remote shell has no way of
    /// knowing it should announce them again.
    ///
    /// The viewport lands back at the bottom, because whatever offset it held
    /// counted from a history that no longer exists.
    pub fn clear_scrollback(&mut self) {
        self.term.grid_mut().clear_history();
    }

    /// Replace the color palette. Existing content is re-colored on the next
    /// [`TerminalModel::snapshot`].
    pub fn set_theme(&mut self, theme: TerminalTheme) {
        self.theme = theme;
    }

    /// The palette currently in use.
    pub fn theme(&self) -> &TerminalTheme {
        &self.theme
    }

    /// Set the charset the byte stream is in, in both directions.
    ///
    /// Called once per connection, before any byte arrives — and again on a
    /// reconnect, where dropping the old decoder is exactly right: the partial
    /// multi-byte sequence it was holding belonged to a stream that no longer
    /// exists, and completing it with the first bytes of the new one would
    /// produce a character neither host sent.
    ///
    /// Only the transcoding changes. The emulator is never told, because every
    /// supported charset is ASCII-transparent and therefore leaves the control
    /// bytes it parses exactly where they were.
    pub fn set_charset(&mut self, charset: Charset) {
        self.charset = charset;
        self.decoder = (!charset.is_utf8()).then(|| CharsetDecoder::new(charset));
    }

    /// The charset in use, which input encoders need as well.
    pub fn charset(&self) -> Charset {
        self.charset
    }

    /// Window title set through `OSC 0` / `OSC 2`, if any.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Working directory of the remote shell, as announced through `OSC 7` or
    /// `OSC 1337`.
    ///
    /// `None` until a shell reports one; shells that never emit either sequence
    /// leave this empty for the whole session.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.cwd()
    }

    /// Terminal modes relevant for key encoding.
    pub fn modes(&self) -> TermModes {
        let mode = self.term.mode();
        TermModes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        }
    }

    /// Take the bytes the terminal wants to send back to the remote side.
    ///
    /// Escape sequences such as a Device Status Report (`CSI 6 n`) expect an
    /// answer on the same channel; the caller is responsible for writing the
    /// returned bytes to the SSH channel. Returns an empty vector when there is
    /// nothing to send.
    pub fn take_pty_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.state.borrow_mut().pty_output)
    }

    /// Reset the terminal to its initial state, dropping screen, scrollback,
    /// title, working directory and any half-parsed escape sequence.
    ///
    /// The charset itself survives — it describes the host, not the screen — but
    /// the decoder's pending bytes go the same way the parser's do, and for the
    /// same reason.
    pub fn reset(&mut self) {
        let (cols, rows) = self.size();
        self.term = Self::build_term(cols, rows, self.scrollback, Rc::clone(&self.state));
        self.parser = Processor::new();
        self.title = None;
        self.cwd.reset();
        self.set_charset(self.charset);
        *self.state.borrow_mut() = SharedState::default();
    }
}

/// Append a cell's glyph plus any combining marks to `text`.
fn push_cell(text: &mut String, cell: &Cell) {
    text.push(cell.c);
    if let Some(zerowidth) = cell.zerowidth() {
        text.extend(zerowidth);
    }
}

/// Translate `alacritty_terminal`'s cell flags into renderer facing ones.
fn run_flags(flags: Flags) -> RunFlags {
    let mut out = RunFlags::empty();
    out.set(RunFlags::BOLD, flags.contains(Flags::BOLD));
    out.set(RunFlags::ITALIC, flags.contains(Flags::ITALIC));
    out.set(RunFlags::UNDERLINE, flags.intersects(Flags::ALL_UNDERLINES));
    out.set(RunFlags::STRIKEOUT, flags.contains(Flags::STRIKEOUT));
    out.set(RunFlags::INVERSE, flags.contains(Flags::INVERSE));
    out.set(RunFlags::DIM, flags.contains(Flags::DIM));
    out.set(RunFlags::HIDDEN, flags.contains(Flags::HIDDEN));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(cols: u16, rows: u16) -> TerminalModel {
        TerminalModel::new(cols, rows, 100, TerminalTheme::dark())
    }

    #[test]
    fn dimensions_are_clamped() {
        let term = TerminalModel::new(0, 0, 0, TerminalTheme::dark());
        assert_eq!(term.size(), (1, 1));
        assert_eq!(term.snapshot().lines.len(), 1);
    }

    #[test]
    fn plain_text_lands_on_the_first_line() {
        let mut term = model(20, 5);
        term.feed(b"hello");

        let snapshot = term.snapshot();
        assert_eq!(snapshot.lines.len(), 5);
        assert_eq!(snapshot.lines[0].text(), "hello");
        assert_eq!(snapshot.cursor, CursorPos { line: 0, col: 5 });
        assert!(snapshot.cursor_visible);
    }

    #[test]
    fn identical_styles_are_merged_into_one_run() {
        let mut term = model(20, 3);
        term.feed(b"hello");

        let line = &term.snapshot().lines[0];
        assert_eq!(line.runs.len(), 1);
        assert_eq!(line.runs[0].start_col, 0);
        assert_eq!(line.runs[0].text, "hello");
    }

    #[test]
    fn sgr_bold_red_is_reflected_in_the_run() {
        let mut term = model(20, 3);
        term.feed(b"\x1b[1;31mX");

        let line = &term.snapshot().lines[0];
        assert_eq!(line.runs.len(), 1);
        let run = &line.runs[0];
        assert_eq!(run.text, "X");
        assert!(run.flags.contains(RunFlags::BOLD));
        // Bold promotes red to bright red.
        assert_eq!(run.fg, term.theme().ansi[9]);
        assert!(run.fg.r > 150 && run.fg.r > run.fg.g && run.fg.r > run.fg.b);
    }

    #[test]
    fn style_changes_split_runs() {
        let mut term = model(20, 3);
        term.feed(b"ab\x1b[31mcd");

        let line = &term.snapshot().lines[0];
        assert_eq!(line.runs.len(), 2);
        assert_eq!(line.runs[0].text, "ab");
        assert_eq!(line.runs[0].start_col, 0);
        assert_eq!(line.runs[1].text, "cd");
        assert_eq!(line.runs[1].start_col, 2);
    }

    #[test]
    fn inverse_swaps_foreground_and_background() {
        let mut term = model(20, 3);
        term.feed(b"a\x1b[7mb");

        let line = &term.snapshot().lines[0];
        assert_eq!(line.runs.len(), 2);

        let normal = &line.runs[0];
        let inverted = &line.runs[1];
        assert_eq!(normal.fg, term.theme().foreground);
        assert_eq!(normal.bg, term.theme().background);
        assert_eq!(inverted.fg, normal.bg);
        assert_eq!(inverted.bg, normal.fg);
        assert!(inverted.flags.contains(RunFlags::INVERSE));
    }

    #[test]
    fn hidden_paints_text_in_the_background_color() {
        let mut term = model(20, 3);
        term.feed(b"\x1b[8mX");

        let run = &term.snapshot().lines[0].runs[0];
        assert!(run.flags.contains(RunFlags::HIDDEN));
        assert_eq!(run.fg, run.bg);
    }

    #[test]
    fn erase_display_leaves_empty_lines() {
        let mut term = model(20, 4);
        term.feed(b"hello\r\nworld\r\n");
        assert!(!term.snapshot().lines[0].is_empty());

        term.feed(b"\x1b[2J");
        let snapshot = term.snapshot();
        assert_eq!(snapshot.lines.len(), 4);
        for line in &snapshot.lines {
            assert!(line.is_empty(), "expected blank line, got {:?}", line);
        }
    }

    #[test]
    fn resize_after_newlines_keeps_the_snapshot_consistent() {
        let mut term = model(20, 5);
        for i in 0..40 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }

        for (cols, rows) in [(40u16, 10u16), (10, 3), (1, 1), (80, 24), (20, 5)] {
            term.resize(cols, rows);
            let snapshot = term.snapshot();
            assert_eq!(snapshot.cols, cols);
            assert_eq!(snapshot.rows, rows);
            assert_eq!(snapshot.lines.len(), rows as usize);
            assert_eq!(term.size(), (cols, rows));
        }
    }

    #[test]
    fn scrollback_offset_moves_and_returns_to_the_bottom() {
        let mut term = model(20, 5);
        for i in 0..30 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }

        let snapshot = term.snapshot();
        assert_eq!(snapshot.display_offset, 0);
        assert!(
            snapshot.total_scrollback >= 5,
            "scrollback: {}",
            snapshot.total_scrollback
        );

        term.scroll_lines(5);
        assert_eq!(term.snapshot().display_offset, 5);

        term.scroll_lines(-2);
        assert_eq!(term.snapshot().display_offset, 3);

        term.scroll_to_bottom();
        assert_eq!(term.snapshot().display_offset, 0);
    }

    #[test]
    fn scrolled_viewport_shows_older_content() {
        let mut term = model(20, 5);
        for i in 0..30 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }

        let bottom = term.snapshot();
        term.scroll_lines(2);
        let scrolled = term.snapshot();

        // Scrolling up by two moves every visible row two positions down.
        assert_ne!(bottom.lines[0].text(), scrolled.lines[0].text());
        assert_eq!(bottom.lines[0].text(), scrolled.lines[2].text());
        assert_eq!(bottom.lines[1].text(), scrolled.lines[3].text());
    }

    #[test]
    fn clearing_the_scrollback_keeps_the_screen_and_snaps_to_the_bottom() {
        let mut term = model(20, 5);
        for i in 0..30 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }
        term.feed(b"\x1b]0;rulogman\x07");
        let screen = term.snapshot().text();
        assert!(term.snapshot().total_scrollback > 0);

        // Scrolled away from the bottom first, so the snap back is observable.
        term.scroll_lines(4);
        assert_eq!(term.snapshot().display_offset, 4);

        term.clear_scrollback();

        let after = term.snapshot();
        assert_eq!(after.total_scrollback, 0);
        assert_eq!(after.display_offset, 0);
        assert_eq!(after.text(), screen);
        // Neither of these describes the output that was dropped.
        assert_eq!(term.title(), Some("rulogman"));
        assert_eq!(term.size(), (20, 5));
    }

    #[test]
    fn scrolling_beyond_the_history_is_clamped() {
        let mut term = model(20, 5);
        term.feed(b"just one line");

        term.scroll_lines(1000);
        assert_eq!(term.snapshot().display_offset, 0);

        term.scroll_lines(-1000);
        assert_eq!(term.snapshot().display_offset, 0);
    }

    #[test]
    fn osc_sets_and_resets_the_window_title() {
        let mut term = model(20, 5);
        assert_eq!(term.title(), None);

        term.feed(b"\x1b]0;rulogman\x07");
        assert_eq!(term.title(), Some("rulogman"));

        term.feed(b"\x1b]2;other\x1b\\");
        assert_eq!(term.title(), Some("other"));

        term.reset();
        assert_eq!(term.title(), None);
    }

    #[test]
    fn osc_7_and_1337_track_the_remote_directory() {
        let mut term = model(20, 5);
        assert_eq!(term.cwd(), None);

        assert!(term.feed(b"\x1b]7;file://remote/home/dennis\x07"));
        assert_eq!(term.cwd(), Some("/home/dennis"));

        // The same directory again is not a change.
        assert!(!term.feed(b"\x1b]7;file://remote/home/dennis\x07"));
        assert!(!term.feed(b"plain output\r\n"));
        assert_eq!(term.cwd(), Some("/home/dennis"));

        assert!(term.feed(b"\x1b]1337;CurrentDir=/var/log\x1b\\"));
        assert_eq!(term.cwd(), Some("/var/log"));

        term.reset();
        assert_eq!(term.cwd(), None);
    }

    #[test]
    fn a_directory_sequence_leaves_no_text_on_the_screen() {
        let mut term = model(40, 3);
        term.feed(b"a\x1b]7;file://h/tmp\x07b");

        assert_eq!(term.snapshot().lines[0].text(), "ab");
        assert_eq!(term.cwd(), Some("/tmp"));
    }

    #[test]
    fn a_directory_sequence_split_across_feeds_is_resumed() {
        let mut term = model(40, 3);
        assert!(!term.feed(b"\x1b]7;file://h/ho"));
        assert!(term.feed(b"me/x\x07done"));

        assert_eq!(term.cwd(), Some("/home/x"));
        assert_eq!(term.snapshot().lines[0].text(), "done");
    }

    #[test]
    fn modes_track_the_terminal_state() {
        let mut term = model(20, 5);
        assert_eq!(term.modes(), TermModes::default());

        term.feed(b"\x1b[?1h\x1b[?2004h");
        let modes = term.modes();
        assert!(modes.app_cursor);
        assert!(modes.bracketed_paste);

        term.feed(b"\x1b[?1l\x1b[?2004l");
        let modes = term.modes();
        assert!(!modes.app_cursor);
        assert!(!modes.bracketed_paste);
    }

    #[test]
    fn cursor_visibility_follows_dectcem_and_the_viewport() {
        let mut term = model(20, 5);
        assert!(term.snapshot().cursor_visible);

        term.feed(b"\x1b[?25l");
        assert!(!term.snapshot().cursor_visible);

        term.feed(b"\x1b[?25h");
        assert!(term.snapshot().cursor_visible);

        for i in 0..30 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }
        term.scroll_lines(10);
        assert!(!term.snapshot().cursor_visible);
        term.scroll_to_bottom();
        assert!(term.snapshot().cursor_visible);
    }

    #[test]
    fn reset_clears_screen_and_scrollback() {
        let mut term = model(20, 5);
        for i in 0..30 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }
        assert!(term.snapshot().total_scrollback > 0);

        term.reset();
        let snapshot = term.snapshot();
        assert_eq!(snapshot.total_scrollback, 0);
        assert_eq!(snapshot.display_offset, 0);
        assert_eq!(term.size(), (20, 5));
        for line in &snapshot.lines {
            assert!(line.is_empty());
        }
    }

    #[test]
    fn wide_characters_keep_the_column_layout() {
        let mut term = model(20, 3);
        term.feed("한글x".as_bytes());

        let line = &term.snapshot().lines[0];
        // The spacer cells are dropped, so the text holds three characters ...
        assert_eq!(line.text(), "한글x");
        // ... and each wide character is a run of its own spanning two columns,
        // which is what puts `x` at column four.
        let spans: Vec<_> = line
            .runs
            .iter()
            .map(|run| (run.text.as_str(), run.start_col, run.cells))
            .collect();
        assert_eq!(spans, vec![("한", 0, 2), ("글", 2, 2), ("x", 4, 1)]);
    }

    #[test]
    fn non_ascii_cells_are_split_out_of_the_surrounding_ascii_run() {
        let mut term = model(20, 3);
        // A braille glyph between two stretches of ASCII: the ASCII cells merge
        // with each other but never with the cluster, whose fallback font may
        // advance by something other than one cell.
        term.feed("ab\u{2840}cd".as_bytes());

        let line = &term.snapshot().lines[0];
        let spans: Vec<_> = line
            .runs
            .iter()
            .map(|run| (run.text.as_str(), run.start_col, run.cells))
            .collect();
        assert_eq!(spans, vec![("ab", 0, 2), ("\u{2840}", 2, 1), ("cd", 3, 2)]);
        assert_eq!(line.text(), "ab\u{2840}cd");
    }

    #[test]
    fn narrow_private_use_glyphs_occupy_a_single_column_each() {
        let mut term = model(20, 3);
        // The powerline separators vim's airline draws sit in the private use
        // area and are one column wide, so each becomes its own run.
        term.feed("\u{e0b0}\u{e0b2}".as_bytes());

        let line = &term.snapshot().lines[0];
        let spans: Vec<_> = line
            .runs
            .iter()
            .map(|run| (run.text.as_str(), run.start_col, run.cells))
            .collect();
        assert_eq!(spans, vec![("\u{e0b0}", 0, 1), ("\u{e0b2}", 1, 1)]);
    }

    #[test]
    fn trigram_symbols_stay_narrow_like_the_deployed_wcwidth() {
        let mut term = model(20, 3);
        // Unicode 16 widened the trigram block to two columns, but glibc's
        // wcwidth and vim's own table still call it one. vim-airline draws
        // U+2630 in its default powerline status line, so the grid has to
        // advance by a single column or the line overflows onto the next row.
        term.feed("\u{2630}x".as_bytes());

        let line = &term.snapshot().lines[0];
        let spans: Vec<_> = line
            .runs
            .iter()
            .map(|run| (run.text.as_str(), run.start_col, run.cells))
            .collect();
        assert_eq!(spans, vec![("\u{2630}", 0, 1), ("x", 1, 1)]);
    }

    #[test]
    fn combining_marks_are_attached_to_the_base_character() {
        let mut term = model(20, 3);
        // `e` followed by a combining acute accent.
        term.feed("e\u{0301}".as_bytes());

        let line = &term.snapshot().lines[0];
        assert_eq!(line.text(), "e\u{0301}");
        // The mark makes the cell a cluster, so it holds one column on its own.
        assert_eq!(line.runs.len(), 1);
        assert_eq!(line.runs[0].cells, 1);
    }

    #[test]
    fn changing_the_theme_recolors_the_snapshot() {
        let mut term = model(20, 3);
        term.feed(b"x");
        assert_eq!(
            term.snapshot().lines[0].runs[0].fg,
            TerminalTheme::dark().foreground
        );

        term.set_theme(TerminalTheme::light());
        assert_eq!(
            term.snapshot().lines[0].runs[0].fg,
            TerminalTheme::light().foreground
        );
        assert_eq!(term.theme().background, TerminalTheme::light().background);
    }

    #[test]
    fn device_status_reports_are_queued_for_the_channel() {
        let mut term = model(20, 5);
        assert!(term.take_pty_output().is_empty());

        term.feed(b"\x1b[6n");
        assert_eq!(term.take_pty_output(), b"\x1b[1;1R");
        assert!(term.take_pty_output().is_empty());
    }

    /// `안녕하세요` in EUC-KR.
    const GREETING_EUC_KR: [u8; 10] = [0xbe, 0xc8, 0xb3, 0xe7, 0xc7, 0xcf, 0xbc, 0xbc, 0xbf, 0xe4];

    fn euc_kr_model(cols: u16, rows: u16) -> TerminalModel {
        let mut term = model(cols, rows);
        term.set_charset(Charset::from_label_or_utf8("EUC-KR"));
        assert_eq!(term.charset().name(), "EUC-KR");
        term
    }

    #[test]
    fn a_legacy_charset_is_decoded_before_the_emulator_sees_it() {
        let mut term = euc_kr_model(20, 3);
        term.feed(&GREETING_EUC_KR);

        assert_eq!(term.snapshot().lines[0].text(), "안녕하세요");
    }

    #[test]
    fn a_legacy_character_split_across_feeds_is_resumed() {
        let mut term = euc_kr_model(20, 3);
        // The split lands between the two bytes of `하`, which is what a chunk
        // boundary on a real link does sooner or later.
        term.feed(&GREETING_EUC_KR[..5]);
        term.feed(&GREETING_EUC_KR[5..]);

        let text = term.snapshot().lines[0].text();
        assert_eq!(text, "안녕하세요");
        assert!(!text.contains('\u{fffd}'), "got {text:?}");
    }

    #[test]
    fn escape_sequences_survive_a_legacy_charset_decoder() {
        let mut term = euc_kr_model(20, 3);
        term.feed(b"\x1b[31m");
        term.feed(&GREETING_EUC_KR[..4]);
        term.feed(b"\x1b[0mx");

        let line = &term.snapshot().lines[0];
        assert_eq!(line.text(), "안녕x");
        // The colour was applied to the decoded text, and reset after it.
        assert_eq!(line.runs[0].fg, term.theme().ansi[1]);
        assert_eq!(line.runs[2].text, "x");
        assert_eq!(line.runs[2].fg, term.theme().foreground);
    }

    #[test]
    fn a_legacy_charset_still_tracks_the_remote_directory() {
        // The decoder runs first precisely so that this works: the tracker only
        // accepts a UTF-8 payload.
        let mut term = euc_kr_model(40, 3);
        let mut bytes = b"\x1b]7;file://h/tmp/".to_vec();
        bytes.extend_from_slice(&GREETING_EUC_KR[..4]);
        bytes.push(0x07);
        assert!(term.feed(&bytes));

        assert_eq!(term.cwd(), Some("/tmp/안녕"));
    }

    #[test]
    fn locally_generated_text_bypasses_the_charset_decoder() {
        let mut term = euc_kr_model(40, 3);
        // rulogman's own notices are UTF-8 whatever the host speaks.
        term.feed_str("rulogman: 실패\r\n");
        term.feed(&GREETING_EUC_KR[..4]);

        let snapshot = term.snapshot();
        assert_eq!(snapshot.lines[0].text(), "rulogman: 실패");
        // And the host's stream picks up where it was, undisturbed.
        assert_eq!(snapshot.lines[1].text(), "안녕");
    }

    #[test]
    fn a_reset_drops_a_pending_partial_character() {
        let mut term = euc_kr_model(20, 3);
        term.feed(&GREETING_EUC_KR[..1]);
        term.reset();
        // The lead byte is gone, so these four bytes are two whole characters
        // rather than one broken one followed by them.
        term.feed(&GREETING_EUC_KR[..4]);

        assert_eq!(term.charset().name(), "EUC-KR");
        assert_eq!(term.snapshot().lines[0].text(), "안녕");
    }

    #[test]
    fn the_default_charset_leaves_the_byte_path_alone() {
        let mut term = model(20, 3);
        assert_eq!(term.charset(), Charset::UTF8);
        term.feed("안녕".as_bytes());
        assert_eq!(term.snapshot().lines[0].text(), "안녕");

        // Even a UTF-8 character split across feeds, which `vte` resumes itself.
        let mut term = model(20, 3);
        let bytes = "안".as_bytes();
        term.feed(&bytes[..2]);
        term.feed(&bytes[2..]);
        assert_eq!(term.snapshot().lines[0].text(), "안");
    }

    #[test]
    fn split_escape_sequences_are_resumed_across_feeds() {
        let mut term = model(20, 3);
        term.feed(b"\x1b[1");
        term.feed(b";31m");
        term.feed(b"Z");

        let run = &term.snapshot().lines[0].runs[0];
        assert_eq!(run.text, "Z");
        assert!(run.flags.contains(RunFlags::BOLD));
    }
}
