//! Keeping a followed file's lines, so the screen can be redrawn from them.
//!
//! A terminal forgets the bytes it was fed the moment it has drawn them, which
//! is right for a shell and wrong for a log that is being filtered: hiding the
//! lines that do not match a pattern — and showing them again when the pattern
//! changes or goes — needs the lines themselves, not the cells they became.
//! [`LineKeeper`] sits between the charset decoder and the emulator, holds on
//! to every complete line up to a limit, and decides which of them the emulator
//! sees. A pattern change throws the screen away and replays what is kept.
//!
//! The decision is taken on the line as it *reads*: escape sequences and
//! control characters are left out of the text a predicate is shown, so a
//! log written in color matches the words on screen, while the emulator still
//! receives every byte of the lines that pass.

use std::collections::VecDeque;
use std::rc::Rc;

/// Says whether a line — its visible text, with no line ending — is shown.
pub type LinePredicate = Rc<dyn Fn(&str) -> bool>;

/// Longest line held back while waiting for its end, in bytes.
///
/// A followed file that is not text — or one that never writes a newline —
/// would otherwise grow the pending line without limit; past this it is taken
/// as complete.
const MAX_PENDING: usize = 1 << 20;

/// The sequence that puts colors and attributes back to their defaults,
/// written before a shown line when lines were hidden since the last one.
///
/// A shown line may set a color that a hidden line after it would have reset;
/// without this, every following match would be painted in it.
const SGR_RESET: &[u8] = b"\x1b[0m";

/// Where the screen stands, for deciding whether a shown line needs
/// [`SGR_RESET`] in front of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Since {
    /// Nothing shown since the screen was emptied.
    Fresh,
    /// The last line was shown.
    Shown,
    /// A line was shown, and at least one hidden after it.
    Hidden,
}

/// The complete lines of a stream, up to a limit, and the predicate that
/// decides which reach the emulator.
pub(crate) struct LineKeeper {
    /// How many complete lines are kept.
    capacity: usize,
    /// Complete lines, oldest first, each with its own `\n`.
    lines: VecDeque<Vec<u8>>,
    /// The line still being written: everything after the last `\n`.
    pending: Vec<u8>,
    /// How much of [`Self::pending`] has already gone to the emulator — only
    /// ever non-zero with no predicate, which is when a line is shown as it
    /// arrives rather than once it is whole.
    pending_shown: usize,
    /// The filter, or `None` to show every line.
    predicate: Option<LinePredicate>,
    /// What happened since the last line shown.
    since: Since,
}

impl LineKeeper {
    /// A keeper holding up to `capacity` lines, showing all of them.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            lines: VecDeque::new(),
            pending: Vec::new(),
            pending_shown: 0,
            predicate: None,
            since: Since::Fresh,
        }
    }

    /// Whether a predicate is in force.
    pub(crate) fn is_filtering(&self) -> bool {
        self.predicate.is_some()
    }

    /// Takes in `bytes` of the stream and appends to `out` what the emulator
    /// should see of them now.
    ///
    /// With no predicate that is every byte, as it arrives. With one, a line is
    /// only judged once it is whole, so the line still being written is held
    /// back until its `\n` comes.
    pub(crate) fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        let mut rest = bytes;
        while let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
            self.pending.extend_from_slice(&rest[..=end]);
            rest = &rest[end + 1..];
            self.complete(out);
        }
        self.pending.extend_from_slice(rest);
        if self.pending.len() >= MAX_PENDING {
            self.complete(out);
        }
        if self.predicate.is_none() {
            out.extend_from_slice(&self.pending[self.pending_shown..]);
            self.pending_shown = self.pending.len();
        }
    }

    /// The pending line is whole: keep it, and show it if it passes.
    fn complete(&mut self, out: &mut Vec<u8>) {
        let line = std::mem::take(&mut self.pending);
        let shown = std::mem::take(&mut self.pending_shown);
        self.emit(&line, shown, out);
        self.lines.push_back(line);
        while self.lines.len() > self.capacity {
            self.lines.pop_front();
        }
    }

    /// Appends `line` to `out` if it passes, skipping the first `shown` bytes
    /// that already went out.
    fn emit(&mut self, line: &[u8], shown: usize, out: &mut Vec<u8>) {
        match &self.predicate {
            None => out.extend_from_slice(&line[shown.min(line.len())..]),
            Some(predicate) => {
                if predicate(&visible_text(line)) {
                    if self.since == Since::Hidden {
                        out.extend_from_slice(SGR_RESET);
                    }
                    self.since = Since::Shown;
                    out.extend_from_slice(line);
                } else if self.since == Since::Shown {
                    self.since = Since::Hidden;
                }
            }
        }
    }

    /// Changes the predicate and appends to `out` the whole stream as kept,
    /// as it reads under the new one — for a screen that has just been
    /// emptied.
    pub(crate) fn replay_with(&mut self, predicate: Option<LinePredicate>, out: &mut Vec<u8>) {
        self.predicate = predicate;
        self.since = Since::Fresh;
        let lines = std::mem::take(&mut self.lines);
        for line in &lines {
            self.emit(line, 0, out);
        }
        self.lines = lines;
        self.pending_shown = 0;
        if self.predicate.is_none() {
            out.extend_from_slice(&self.pending);
            self.pending_shown = self.pending.len();
        }
    }

    /// Forgets every line, keeping the predicate — the stream starts over.
    pub(crate) fn clear(&mut self) {
        self.lines.clear();
        self.pending.clear();
        self.pending_shown = 0;
        self.since = Since::Fresh;
    }

    /// Keeps only the newest `count` complete lines.
    pub(crate) fn keep_last(&mut self, count: usize) {
        while self.lines.len() > count {
            self.lines.pop_front();
        }
    }
}

/// The text of `line` as it reads on screen: escape sequences, control
/// characters and the line ending left out, invalid UTF-8 replaced.
pub(crate) fn visible_text(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_escape(&mut chars),
            // A tab reads as space; every other control character reads as
            // nothing.
            '\t' => out.push('\t'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Skips the rest of an escape sequence whose `ESC` has just been read.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        // CSI: parameters and intermediates up to a final byte in `@`..`~`.
        Some('[') => {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
        // OSC, DCS, SOS, PM, APC: up to BEL or ST (`ESC \`).
        Some(']' | 'P' | 'X' | '^' | '_') => {
            while let Some(c) = chars.next() {
                if c == '\x07' {
                    break;
                }
                if c == '\x1b' {
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    break;
                }
            }
        }
        // Intermediates, then one final character: charset designations and
        // the like.
        Some(c) if (' '..='/').contains(&c) => {
            for c in chars.by_ref() {
                if !(' '..='/').contains(&c) {
                    break;
                }
            }
        }
        // A two-character sequence, or a lone `ESC` at the end.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keeper_with(pattern: &'static str) -> LineKeeper {
        let mut keeper = LineKeeper::new(100);
        keeper.replay_with(
            Some(Rc::new(move |line: &str| line.contains(pattern))),
            &mut Vec::new(),
        );
        keeper
    }

    fn fed(keeper: &mut LineKeeper, bytes: &[u8]) -> String {
        let mut out = Vec::new();
        keeper.feed(bytes, &mut out);
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn without_a_predicate_every_byte_goes_through_as_it_arrives() {
        let mut keeper = LineKeeper::new(100);
        assert_eq!(fed(&mut keeper, b"one\r\ntw"), "one\r\ntw");
        assert_eq!(fed(&mut keeper, b"o\r\n"), "o\r\n");
        assert_eq!(keeper.lines.len(), 2);
    }

    #[test]
    fn with_a_predicate_only_whole_matching_lines_go_through() {
        let mut keeper = keeper_with("ERROR");
        assert_eq!(fed(&mut keeper, b"INFO a\r\nERROR b\r\nERR"), "ERROR b\r\n");
        // The held-back line is judged once it ends, as a whole.
        assert_eq!(fed(&mut keeper, b"OR c\r\n"), "ERROR c\r\n");
    }

    #[test]
    fn a_replay_redraws_what_is_kept_under_the_new_predicate() {
        let mut keeper = LineKeeper::new(100);
        fed(&mut keeper, b"INFO a\r\nERROR b\r\nWARN c\r\npart");
        let mut out = Vec::new();
        keeper.replay_with(
            Some(Rc::new(|line: &str| !line.starts_with("INFO"))),
            &mut out,
        );
        assert_eq!(String::from_utf8(out).unwrap(), "ERROR b\r\nWARN c\r\n");

        let mut out = Vec::new();
        keeper.replay_with(None, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "INFO a\r\nERROR b\r\nWARN c\r\npart",
            "the pending line is shown again as soon as nothing filters"
        );
        // And the rest of it is not shown twice when it ends.
        assert_eq!(fed(&mut keeper, b"ial\r\n"), "ial\r\n");
    }

    #[test]
    fn only_the_newest_lines_are_kept() {
        let mut keeper = LineKeeper::new(2);
        fed(&mut keeper, b"1\n2\n3\n");
        let mut out = Vec::new();
        keeper.replay_with(None, &mut out);
        assert_eq!(out, b"2\n3\n");
    }

    #[test]
    fn colors_are_matched_by_their_words_and_passed_on_intact() {
        let mut keeper = keeper_with("ERROR disk");
        let colored = b"\x1b[31mERROR\x1b[0m disk full\r\n";
        assert_eq!(fed(&mut keeper, colored), String::from_utf8_lossy(colored));
    }

    #[test]
    fn a_color_a_hidden_line_would_have_ended_is_ended_anyway() {
        let mut keeper = keeper_with("shown");
        assert_eq!(
            fed(
                &mut keeper,
                b"\x1b[31mshown start\nhidden end\x1b[0m\nshown next\n"
            ),
            "\x1b[31mshown start\n\x1b[0mshown next\n"
        );
        // Lines hidden before anything was shown left nothing to undo.
        let mut keeper = keeper_with("shown");
        assert_eq!(fed(&mut keeper, b"hidden\nshown\n"), "shown\n");
    }

    #[test]
    fn a_line_that_never_ends_is_taken_as_whole_past_the_limit() {
        let mut keeper = keeper_with("x");
        let long = vec![b'x'; MAX_PENDING];
        let mut out = Vec::new();
        keeper.feed(&long, &mut out);
        assert_eq!(out.len(), MAX_PENDING);
        assert!(keeper.pending.is_empty());
    }

    #[test]
    fn visible_text_drops_sequences_and_controls() {
        assert_eq!(visible_text(b"\x1b[1;31mred\x1b[m\r\n"), "red");
        assert_eq!(visible_text(b"\x1b]0;title\x07after"), "after");
        assert_eq!(
            visible_text(b"\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
        assert_eq!(visible_text(b"\x1b(Bplain\tcol"), "plain\tcol");
        assert_eq!(visible_text("한글 로그\r\n".as_bytes()), "한글 로그");
    }
}
