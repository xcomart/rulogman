//! Working directory tracking from OSC escape sequences.
//!
//! Remote shells announce the directory they are in with one of two escape
//! sequences:
//!
//! * `OSC 7` - `ESC ] 7 ; file://host/percent%20encoded/path ST`, emitted by
//!   fish out of the box and by bash / zsh once the prompt is configured for it.
//! * `OSC 1337` - `ESC ] 1337 ; CurrentDir=/plain/path ST`, the iTerm2 variant.
//!
//! In both cases `ST` is either a `BEL` (`0x07`) or `ESC \`.
//!
//! The parser inside `alacritty_terminal` drops both sequences on the floor, so
//! [`CwdTracker`] watches the byte stream on its way to the emulator instead. It
//! is a pure observer: it never consumes or rewrites a byte, and the same buffer
//! is handed to the emulator unchanged.

/// Escape, `0x1b`.
const ESC: u8 = 0x1b;
/// Bell, one of the two OSC terminators.
const BEL: u8 = 0x07;
/// Cancel, aborts an in-flight escape sequence.
const CAN: u8 = 0x18;
/// Substitute, aborts an in-flight escape sequence.
const SUB: u8 = 0x1a;

/// Upper bound for a buffered OSC payload.
///
/// Anything longer cannot be a plausible path and is far more likely to be an
/// image transfer or a stray binary blob, so the payload is dropped instead of
/// being buffered without end. Parsing resumes at the terminator.
const MAX_OSC_LEN: usize = 4096;

/// Where the scanner currently is inside an escape sequence.
///
/// The state survives a call to [`CwdTracker::feed`], so a sequence split across
/// chunk boundaries - which is the common case for a slow remote prompt - is
/// resumed rather than lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Ordinary text; the scanner is looking for the next `ESC`.
    Ground,
    /// An `ESC` was seen and the sequence's introducer is expected next.
    Escape,
    /// Inside `ESC ]`, collecting the payload up to the terminator.
    Osc,
    /// Inside an OSC payload, right after an `ESC`; a `\` closes the sequence.
    OscEscape,
}

/// Incremental scanner that extracts the remote working directory from a
/// terminal byte stream.
///
/// Feed every byte that goes to the emulator through [`CwdTracker::feed`]; the
/// method reports a directory only when it actually changed.
///
/// ```
/// use rulogman_term::CwdTracker;
///
/// let mut tracker = CwdTracker::new();
/// assert_eq!(tracker.feed(b"\x1b]7;file://host/tmp/work\x07"), Some("/tmp/work".to_owned()));
/// // The same directory again is not a change.
/// assert_eq!(tracker.feed(b"\x1b]7;file://host/tmp/work\x07"), None);
/// assert_eq!(tracker.cwd(), Some("/tmp/work"));
/// ```
#[derive(Debug)]
pub struct CwdTracker {
    /// Parser state, carried across chunk boundaries.
    state: State,
    /// Payload of the OSC currently being collected.
    buffer: Vec<u8>,
    /// Set when the payload passed [`MAX_OSC_LEN`]; the sequence is discarded.
    overflow: bool,
    /// Last directory reported by the remote shell.
    cwd: Option<String>,
}

impl Default for CwdTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CwdTracker {
    /// A tracker that has not seen a directory yet.
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buffer: Vec::new(),
            overflow: false,
            cwd: None,
        }
    }

    /// The directory last announced by the remote shell, if any.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Forget the current directory and any half-parsed sequence.
    pub fn reset(&mut self) {
        self.state = State::Ground;
        self.buffer.clear();
        self.overflow = false;
        self.cwd = None;
    }

    /// Observe one chunk of output.
    ///
    /// Returns the new directory when this chunk changed it, and `None`
    /// otherwise - including when the chunk repeats the directory that is
    /// already known, so that callers can use the return value to decide
    /// whether anything needs redrawing. When a chunk carries several
    /// announcements only the last one survives.
    ///
    /// Plain text costs one scan for `ESC` per chunk and touches nothing else,
    /// which matters because every byte of remote output passes through here.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        // Cloned lazily: only a chunk that actually carries an announcement
        // pays for it, which is the rare case on a hot path.
        let mut before: Option<Option<String>> = None;

        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => match bytes[i..].iter().position(|&b| b == ESC) {
                    Some(offset) => {
                        i += offset + 1;
                        self.state = State::Escape;
                    }
                    None => break,
                },
                State::Escape => {
                    let byte = bytes[i];
                    i += 1;
                    match byte {
                        b']' => {
                            self.buffer.clear();
                            self.overflow = false;
                            self.state = State::Osc;
                        }
                        // `ESC ESC` restarts the sequence rather than ending it.
                        ESC => {}
                        _ => self.state = State::Ground,
                    }
                }
                State::Osc => {
                    let start = i;
                    while i < bytes.len() && !matches!(bytes[i], BEL | ESC | CAN | SUB) {
                        i += 1;
                    }
                    self.push(&bytes[start..i]);

                    if i == bytes.len() {
                        // Terminator not in this chunk; resume on the next one.
                        break;
                    }
                    let byte = bytes[i];
                    i += 1;
                    match byte {
                        BEL => {
                            if let Some(cwd) = self.finish() {
                                self.commit(cwd, &mut before);
                            }
                        }
                        ESC => self.state = State::OscEscape,
                        // CAN / SUB abandon the sequence.
                        _ => self.discard(),
                    }
                }
                State::OscEscape => {
                    if bytes[i] == b'\\' {
                        i += 1;
                        if let Some(cwd) = self.finish() {
                            self.commit(cwd, &mut before);
                        }
                    } else {
                        // A bare `ESC` inside the payload ends the string and
                        // starts a new escape sequence; the byte is re-read in
                        // `Escape` rather than consumed here.
                        self.discard();
                        self.state = State::Escape;
                    }
                }
            }
        }

        match before {
            Some(before) if before.as_deref() != self.cwd.as_deref() => self.cwd.clone(),
            _ => None,
        }
    }

    /// Append payload bytes, giving up on the sequence once it grows past
    /// [`MAX_OSC_LEN`].
    fn push(&mut self, bytes: &[u8]) {
        if self.overflow {
            return;
        }
        if self.buffer.len() + bytes.len() > MAX_OSC_LEN {
            self.overflow = true;
            self.buffer.clear();
            return;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// Drop the collected payload and go back to scanning ordinary text.
    fn discard(&mut self) {
        self.buffer.clear();
        self.overflow = false;
        self.state = State::Ground;
    }

    /// Interpret a terminated payload and return the directory it announces.
    fn finish(&mut self) -> Option<String> {
        let payload = std::mem::take(&mut self.buffer);
        let overflow = self.overflow;
        self.overflow = false;
        self.state = State::Ground;

        if overflow {
            return None;
        }
        parse_payload(&payload)
    }

    /// Store a directory, remembering what the value was before this chunk.
    fn commit(&mut self, cwd: String, before: &mut Option<Option<String>>) {
        if before.is_none() {
            *before = Some(self.cwd.clone());
        }
        self.cwd = Some(cwd);
    }
}

/// Extract a directory from a complete OSC payload (everything between
/// `ESC ]` and the terminator).
fn parse_payload(payload: &[u8]) -> Option<String> {
    if let Some(rest) = payload.strip_prefix(b"7;") {
        parse_file_url(rest)
    } else if let Some(rest) = payload.strip_prefix(b"1337;") {
        let path = rest.strip_prefix(b"CurrentDir=")?;
        non_empty_utf8(path.to_vec())
    } else {
        None
    }
}

/// Parse the `file://host/path` URL of an `OSC 7`.
///
/// The host is ignored - it names the machine the path lives on, which is
/// already the machine this session is connected to - and an empty host
/// (`file:///path`) is just as acceptable.
fn parse_file_url(url: &[u8]) -> Option<String> {
    let rest = strip_prefix_ignore_ascii_case(url, b"file://")?;
    // Everything up to the first slash is the host; the slash itself belongs to
    // the path. A URL without one carries no path at all.
    let slash = rest.iter().position(|&b| b == b'/')?;
    non_empty_utf8(percent_decode(&rest[slash..]))
}

/// `slice::strip_prefix` with ASCII case folding, for the URL scheme.
fn strip_prefix_ignore_ascii_case<'a>(bytes: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&bytes[prefix.len()..])
    } else {
        None
    }
}

/// Decode `%XX` escapes; anything that is not a well formed escape is passed
/// through untouched, which is what a shell that forgot to encode a `%` needs.
fn percent_decode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = if bytes[i] == b'%' && i + 2 < bytes.len() {
            match (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                (Some(hi), Some(lo)) => Some((hi << 4) | lo),
                _ => None,
            }
        } else {
            None
        };

        match decoded {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    out
}

/// Value of a single hexadecimal digit.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Turn decoded bytes into a path, rejecting empty and non UTF-8 results.
fn non_empty_utf8(bytes: Vec<u8>) -> Option<String> {
    let text = String::from_utf8(bytes).ok()?;
    if text.is_empty() { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a whole sequence in one go.
    fn track(bytes: &[u8]) -> Option<String> {
        CwdTracker::new().feed(bytes)
    }

    #[test]
    fn osc_7_with_bel_terminator() {
        assert_eq!(
            track(b"\x1b]7;file://myhost/home/dennis\x07"),
            Some("/home/dennis".to_owned())
        );
    }

    #[test]
    fn osc_7_with_st_terminator() {
        assert_eq!(
            track(b"\x1b]7;file://myhost/var/log\x1b\\"),
            Some("/var/log".to_owned())
        );
    }

    #[test]
    fn osc_7_accepts_an_empty_host() {
        assert_eq!(track(b"\x1b]7;file:///srv\x07"), Some("/srv".to_owned()));
    }

    #[test]
    fn osc_7_scheme_is_case_insensitive() {
        assert_eq!(track(b"\x1b]7;FILE://h/opt\x07"), Some("/opt".to_owned()));
    }

    #[test]
    fn osc_7_percent_decodes_spaces_and_utf8() {
        assert_eq!(
            track(b"\x1b]7;file://h/home/my%20docs\x07"),
            Some("/home/my docs".to_owned())
        );
        // "/작업/폴더" in percent encoded UTF-8.
        assert_eq!(
            track(b"\x1b]7;file://h/%EC%9E%91%EC%97%85/%ED%8F%B4%EB%8D%94\x07"),
            Some("/작업/폴더".to_owned())
        );
    }

    #[test]
    fn a_lone_percent_survives_decoding() {
        assert_eq!(
            track(b"\x1b]7;file://h/tmp/100%/x%2Fy%zz\x07"),
            Some("/tmp/100%/x/y%zz".to_owned())
        );
    }

    #[test]
    fn the_root_directory_is_a_valid_path() {
        assert_eq!(track(b"\x1b]7;file://h/\x07"), Some("/".to_owned()));
    }

    #[test]
    fn osc_1337_current_dir() {
        assert_eq!(
            track(b"\x1b]1337;CurrentDir=/home/dennis/work\x07"),
            Some("/home/dennis/work".to_owned())
        );
        assert_eq!(
            track("\x1b]1337;CurrentDir=/한글/경로\x1b\\".as_bytes()),
            Some("/한글/경로".to_owned())
        );
    }

    #[test]
    fn osc_1337_percent_signs_are_literal() {
        assert_eq!(
            track(b"\x1b]1337;CurrentDir=/tmp/100%20\x07"),
            Some("/tmp/100%20".to_owned())
        );
    }

    #[test]
    fn other_oscs_and_sequences_are_ignored() {
        let mut tracker = CwdTracker::new();
        assert_eq!(tracker.feed(b"\x1b]0;a title\x07"), None);
        assert_eq!(tracker.feed(b"\x1b]2;another\x1b\\"), None);
        assert_eq!(
            tracker.feed(b"\x1b]8;;https://example.com\x07link\x07"),
            None
        );
        assert_eq!(tracker.feed(b"\x1b]4;1;rgb:ff/00/00\x07"), None);
        assert_eq!(tracker.feed(b"\x1b[1;31mred\x1b[0m plain text\r\n"), None);
        assert_eq!(tracker.feed(b"\x1b]1337;SetMark\x07"), None);
        assert_eq!(tracker.feed(b"\x1b]7;http://h/not-a-file\x07"), None);
        assert_eq!(tracker.feed(b"\x1b]7;file://hostonly\x07"), None);
        assert_eq!(tracker.feed(b"\x1b]7;\x07"), None);
        assert_eq!(tracker.feed(b"\x1b]77;file://h/nope\x07"), None);
        assert_eq!(tracker.cwd(), None);
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        assert_eq!(track(b"\x1b]7;file://h/%ff%fe\x07"), None);
    }

    #[test]
    fn a_repeated_directory_is_not_a_change() {
        let mut tracker = CwdTracker::new();
        assert_eq!(
            tracker.feed(b"\x1b]7;file://h/a\x07"),
            Some("/a".to_owned())
        );
        assert_eq!(tracker.feed(b"\x1b]7;file://h/a\x07"), None);
        assert_eq!(tracker.feed(b"plain output\r\n"), None);
        assert_eq!(tracker.cwd(), Some("/a"));
    }

    #[test]
    fn the_last_announcement_in_a_chunk_wins() {
        let mut tracker = CwdTracker::new();
        assert_eq!(
            tracker
                .feed(b"\x1b]7;file://h/a\x07x\x1b]1337;CurrentDir=/b\x07y\x1b]7;file://h/c\x07"),
            Some("/c".to_owned())
        );
        assert_eq!(tracker.cwd(), Some("/c"));
    }

    #[test]
    fn a_round_trip_within_one_chunk_reports_no_change() {
        let mut tracker = CwdTracker::new();
        tracker.feed(b"\x1b]7;file://h/a\x07");
        assert_eq!(
            tracker.feed(b"\x1b]7;file://h/b\x07\x1b]7;file://h/a\x07"),
            None
        );
        assert_eq!(tracker.cwd(), Some("/a"));
    }

    /// Split `input` after `at` bytes and feed both halves.
    fn feed_split(input: &[u8], at: usize) -> Option<String> {
        let mut tracker = CwdTracker::new();
        let first = tracker.feed(&input[..at]);
        let second = tracker.feed(&input[at..]);
        assert_eq!(first, None, "the split half reported a directory too early");
        second
    }

    #[test]
    fn a_sequence_split_at_any_offset_is_resumed() {
        let input = b"\x1b]7;file://myhost/home/my%20docs\x07";
        for at in 1..input.len() {
            assert_eq!(
                feed_split(input, at),
                Some("/home/my docs".to_owned()),
                "split at {at}"
            );
        }
    }

    #[test]
    fn a_sequence_split_at_any_offset_is_resumed_with_st() {
        let input = b"\x1b]1337;CurrentDir=/opt/data\x1b\\";
        for at in 1..input.len() {
            assert_eq!(
                feed_split(input, at),
                Some("/opt/data".to_owned()),
                "split at {at}"
            );
        }
    }

    #[test]
    fn a_sequence_split_into_single_bytes_is_resumed() {
        let mut tracker = CwdTracker::new();
        let input = b"noise\x1b]7;file://h/a%20b\x1b\\more";
        let mut found = None;
        for byte in input {
            if let Some(cwd) = tracker.feed(&[*byte]) {
                found = Some(cwd);
            }
        }
        assert_eq!(found, Some("/a b".to_owned()));
    }

    #[test]
    fn an_escape_inside_the_payload_starts_a_new_sequence() {
        let mut tracker = CwdTracker::new();
        // The first OSC is abandoned by the bare `ESC`, the second one lands.
        assert_eq!(
            tracker.feed(b"\x1b]7;file://h/aborted\x1b]7;file://h/kept\x07"),
            Some("/kept".to_owned())
        );
    }

    #[test]
    fn a_doubled_escape_restarts_the_sequence() {
        assert_eq!(
            track(b"\x1b\x1b]7;file://h/tmp\x07"),
            Some("/tmp".to_owned())
        );
    }

    #[test]
    fn can_and_sub_abort_the_sequence() {
        let mut tracker = CwdTracker::new();
        assert_eq!(tracker.feed(b"\x1b]7;file://h/gone\x18"), None);
        assert_eq!(tracker.feed(b"\x1b]7;file://h/gone\x1a"), None);
        // The parser is back in ground state and picks up the next one.
        assert_eq!(
            tracker.feed(b"\x1b]7;file://h/here\x07"),
            Some("/here".to_owned())
        );
    }

    #[test]
    fn an_oversized_payload_is_dropped_and_parsing_resumes() {
        let mut tracker = CwdTracker::new();
        let mut junk = Vec::from(&b"\x1b]7;file://h/"[..]);
        junk.extend(std::iter::repeat_n(b'x', MAX_OSC_LEN * 2));
        junk.push(BEL);
        assert_eq!(tracker.feed(&junk), None);
        assert!(tracker.buffer.is_empty());
        assert_eq!(
            tracker.feed(b"\x1b]7;file://h/after\x07"),
            Some("/after".to_owned())
        );
    }

    #[test]
    fn an_unterminated_payload_does_not_grow_without_bound() {
        let mut tracker = CwdTracker::new();
        tracker.feed(b"\x1b]1337;CurrentDir=");
        for _ in 0..64 {
            tracker.feed(&[b'y'; 1024]);
        }
        assert!(tracker.buffer.len() <= MAX_OSC_LEN);
        assert_eq!(tracker.feed(b"\x07"), None);
        assert_eq!(
            tracker.feed(b"\x1b]7;file://h/fresh\x07"),
            Some("/fresh".to_owned())
        );
    }

    #[test]
    fn binary_junk_never_produces_a_directory() {
        let mut tracker = CwdTracker::new();
        let junk: Vec<u8> = (0..=255u8).cycle().take(8192).collect();
        assert_eq!(tracker.feed(&junk), None);
        assert_eq!(tracker.cwd(), None);
        assert_eq!(
            tracker.feed(b"\x1b]7;file://h/still-works\x07"),
            Some("/still-works".to_owned())
        );
    }

    #[test]
    fn reset_forgets_the_directory_and_the_parser_state() {
        let mut tracker = CwdTracker::new();
        tracker.feed(b"\x1b]7;file://h/a\x07");
        tracker.feed(b"\x1b]7;file://h/half");
        tracker.reset();

        assert_eq!(tracker.cwd(), None);
        // The dangling half sequence is gone, so its tail is plain text now.
        assert_eq!(tracker.feed(b"-written\x07"), None);
        assert_eq!(
            tracker.feed(b"\x1b]7;file://h/a\x07"),
            Some("/a".to_owned())
        );
    }
}
