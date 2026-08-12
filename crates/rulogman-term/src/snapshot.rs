//! Immutable, renderer facing view of the terminal grid.
//!
//! A [`TerminalSnapshot`] is intentionally free of any `alacritty_terminal`
//! types so that the GUI layer can render it without depending on the terminal
//! engine. Snapshots are cheap to clone and can be handed to a renderer while
//! the model keeps processing incoming bytes.

use crate::theme::Rgb;

bitflags::bitflags! {
    /// Text attributes shared by every cell of a [`StyledRun`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RunFlags: u16 {
        /// Bold text (`SGR 1`).
        const BOLD = 1 << 0;
        /// Italic text (`SGR 3`).
        const ITALIC = 1 << 1;
        /// Underlined text (any of the `SGR 4` variants).
        const UNDERLINE = 1 << 2;
        /// Struck out text (`SGR 9`).
        const STRIKEOUT = 1 << 3;
        /// Reverse video (`SGR 7`).
        ///
        /// The swap has **already been applied** to [`StyledRun::fg`] and
        /// [`StyledRun::bg`]; this flag is informational only and must not be
        /// applied a second time by the renderer.
        const INVERSE = 1 << 4;
        /// Dim / faint text (`SGR 2`), already folded into [`StyledRun::fg`].
        const DIM = 1 << 5;
        /// Concealed text (`SGR 8`).
        ///
        /// [`StyledRun::fg`] has already been set to [`StyledRun::bg`].
        const HIDDEN = 1 << 6;
    }
}

/// A horizontal stretch of cells that share the exact same style.
///
/// A run is one of exactly two shapes, and the renderer tells them apart with
/// `text.is_ascii()`:
///
/// * a maximal stretch of plain ASCII cells sharing one style — every `char`
///   is one column wide, so the whole run can be shaped in one go;
/// * a single non-ASCII cluster — one base character plus its combining
///   marks. Such a cell never merges with its neighbours, even when the style
///   matches, so that [`StyledRun::start_col`] always names a real grid column
///   and the renderer can snap every cluster back onto the grid instead of
///   letting a fallback font's advance push the rest of the row sideways.
///
/// The split is exact: a cluster always carries at least one non-ASCII byte,
/// because its base character is non-ASCII or a combining mark is attached.
#[derive(Debug, Clone, PartialEq)]
pub struct StyledRun {
    /// The characters of the run.
    ///
    /// A double width character contributes a single `char` but occupies two
    /// terminal columns; combining marks are appended to the base character.
    pub text: String,
    /// Column index the run starts at.
    pub start_col: u16,
    /// Number of terminal columns the run spans.
    ///
    /// The same as the number of characters for an ASCII run; `2` for a double
    /// width cluster and `1` for every other one.
    pub cells: u16,
    /// Resolved text color.
    pub fg: Rgb,
    /// Resolved background color.
    pub bg: Rgb,
    /// Attributes shared by every cell of the run.
    pub flags: RunFlags,
}

/// A single visible row of the terminal.
///
/// Trailing cells that would render as blank default background are omitted, so
/// an empty row has no runs at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerminalLine {
    /// Style runs of the row, ordered by [`StyledRun::start_col`].
    pub runs: Vec<StyledRun>,
}

impl TerminalLine {
    /// `true` when the row renders as completely blank.
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Concatenate every run into a plain string.
    ///
    /// Gaps introduced by skipped double width spacer cells are not padded, so
    /// this is meant for tests and text extraction rather than layout.
    pub fn text(&self) -> String {
        self.runs.iter().map(|run| run.text.as_str()).collect()
    }
}

/// Position of the text cursor inside the visible area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPos {
    /// Row index, `0` is the topmost visible row.
    pub line: u16,
    /// Column index.
    pub col: u16,
}

/// Where the viewport sits in the scrollback.
///
/// The three numbers a scrollbar needs, and no more. [`TerminalSnapshot`]
/// carries the same ones, but building a snapshot means rebuilding every
/// visible row; a bar wants the position on every frame and the rows never.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrollPosition {
    /// How far the viewport is scrolled up into the scrollback, `0` at the
    /// bottom.
    pub display_offset: usize,
    /// Number of lines currently stored in the scrollback buffer.
    pub history: usize,
    /// Height of the viewport in rows.
    pub rows: usize,
}

/// Everything a renderer needs to paint one frame of a terminal.
#[derive(Debug, Clone)]
pub struct TerminalSnapshot {
    /// Width of the terminal in columns.
    pub cols: u16,
    /// Height of the terminal in rows.
    pub rows: u16,
    /// Visible rows, always exactly [`TerminalSnapshot::rows`] entries long.
    pub lines: Vec<TerminalLine>,
    /// Position of the text cursor.
    pub cursor: CursorPos,
    /// Whether the cursor should be painted at all.
    ///
    /// This is `false` when the application hid it (`DECTCEM`) or when the
    /// viewport is scrolled away from the cursor.
    pub cursor_visible: bool,
    /// How far the viewport is scrolled up into the scrollback, `0` is the
    /// bottom-most position.
    pub display_offset: usize,
    /// Number of lines currently stored in the scrollback buffer.
    pub total_scrollback: usize,
}

impl TerminalSnapshot {
    /// Concatenate the visible rows into a newline separated string.
    ///
    /// Mostly useful for tests and for copying the whole viewport.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str(&line.text());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, start_col: u16, cells: u16) -> StyledRun {
        StyledRun {
            text: text.to_owned(),
            start_col,
            cells,
            fg: Rgb::new(1, 2, 3),
            bg: Rgb::new(4, 5, 6),
            flags: RunFlags::empty(),
        }
    }

    #[test]
    fn line_text_concatenates_runs() {
        let line = TerminalLine {
            runs: vec![run("ab", 0, 2), run("cd", 2, 2)],
        };
        assert_eq!(line.text(), "abcd");
        assert!(!line.is_empty());
        assert!(TerminalLine::default().is_empty());
    }

    #[test]
    fn snapshot_text_joins_rows() {
        let snapshot = TerminalSnapshot {
            cols: 4,
            rows: 2,
            lines: vec![
                TerminalLine {
                    runs: vec![run("ab", 0, 2)],
                },
                TerminalLine::default(),
            ],
            cursor: CursorPos { line: 0, col: 0 },
            cursor_visible: true,
            display_offset: 0,
            total_scrollback: 0,
        };
        assert_eq!(snapshot.text(), "ab\n");
    }
}
