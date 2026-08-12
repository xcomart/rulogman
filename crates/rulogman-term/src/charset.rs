//! Character set of a session's byte stream, and the transcoding either way.
//!
//! A terminal moves bytes, not text. UTF-8 is the assumption everywhere else in
//! this crate — `alacritty_terminal`'s parser decodes UTF-8 itself and has no
//! opinion to offer about anything else — but a host whose locale is `ko_KR.euc-kr`
//! or `ja_JP.SJIS` sends bytes that mean something entirely different, and a
//! filename typed back at such a shell has to leave in the same encoding it
//! arrived in. So the conversion sits at the edges of the byte path: inbound in
//! [`crate::TerminalModel::feed`], outbound in [`crate::encode_key`] and
//! [`crate::encode_paste`].
//!
//! Every encoding offered here is ASCII-transparent — the bytes `0x00..=0x7f`
//! decode to themselves and no multi-byte sequence contains one — which is what
//! makes a single conversion point sufficient: escape sequences, the `CSI`
//! introducer and the OSC terminators all survive the decoder untouched, so the
//! emulator behind it needs to know nothing about any of this.
//!
//! ```
//! use rulogman_term::Charset;
//!
//! let euc_kr = Charset::from_label_or_utf8("EUC-KR");
//! assert_eq!(euc_kr.name(), "EUC-KR");
//! assert_eq!(euc_kr.encode("안녕"), vec![0xbe, 0xc8, 0xb3, 0xe7]);
//! ```

use std::fmt;

use encoding_rs::{CoderResult, EncoderResult, Encoding};

/// Scratch buffer size for one turn of the outbound encoding loop.
///
/// Sized to swallow a whole paste line in a single pass while still being a
/// stack allocation; the loop is correct at any size, this only sets how often
/// it goes around.
const ENCODE_CHUNK: usize = 512;

/// Byte the outbound encoder writes for a character the charset cannot express.
///
/// `encoding_rs`' own `Encoding::encode` substitutes an HTML numeric character
/// reference (`&#9834;`) instead, which is right for a web form and disastrous
/// for a shell: the user would find literal `&#9834;` on their command line.
/// A `?` is what `iconv //TRANSLIT` and the terminals do, and it is at least
/// obviously a placeholder.
const UNMAPPABLE: u8 = b'?';

/// Reserve for one turn of the inbound decoding loop when the exact bound is
/// unavailable.
const DECODE_CHUNK: usize = 4096;

/// Floor on the per-turn decode reserve, so the loop always makes progress.
///
/// `decode_to_string` writes into the destination's *spare* capacity and never
/// grows it, so a reserve of zero would return `OutputFull` forever. Four bytes
/// is one `char` at its widest.
const MIN_DECODE_RESERVE: usize = 4;

/// A character set a session's byte stream is encoded in.
///
/// Wraps a WHATWG encoding, and is `Copy` because that is all it is: a
/// `&'static` reference into `encoding_rs`' table of static encodings.
///
/// The WHATWG labels are the reason to go through that table rather than name
/// encodings ourselves — `euc-kr` there resolves to Windows-949/UHC, the
/// superset that Korean hosts and files actually contain, rather than the
/// narrow 1987 EUC-KR a standards-literal decoder would apply and then reject
/// half the text with.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Charset(&'static Encoding);

impl Charset {
    /// UTF-8: the default, and the only sane one for a host that says nothing.
    pub const UTF8: Charset = Charset(encoding_rs::UTF_8);

    /// The character sets offered in the UI, in the order they are offered.
    ///
    /// A curated list rather than the whole WHATWG registry, which holds several
    /// dozen encodings almost none of which any living host speaks: these are
    /// UTF-8 first, then the CJK encodings a legacy host in the region plausibly
    /// runs, then Cyrillic and Western single-byte code pages. A user who needs
    /// something outside it can still write the label into `profiles.json` by
    /// hand — [`Charset::from_label_or_utf8`] accepts anything the registry
    /// knows.
    pub const SUPPORTED: [Charset; 9] = [
        Charset(encoding_rs::UTF_8),
        Charset(encoding_rs::EUC_KR),
        Charset(encoding_rs::SHIFT_JIS),
        Charset(encoding_rs::EUC_JP),
        Charset(encoding_rs::GBK),
        Charset(encoding_rs::GB18030),
        Charset(encoding_rs::BIG5),
        Charset(encoding_rs::WINDOWS_1251),
        Charset(encoding_rs::WINDOWS_1252),
    ];

    /// Resolve a WHATWG encoding label, or `None` when nothing answers to it.
    ///
    /// Matching is the registry's own: case-insensitive, surrounding whitespace
    /// ignored, and every historical alias of an encoding accepted — `euc-kr`,
    /// `EUC-KR`, `csksc56011987` and `windows-949` all name the same thing.
    pub fn for_label(label: &str) -> Option<Charset> {
        Encoding::for_label(label.as_bytes()).map(Charset)
    }

    /// Resolve a label read from `profiles.json`, falling back to UTF-8.
    ///
    /// The tolerant loader, for the same reason the rest of the settings layer
    /// clamps rather than rejects: a hand-edited profile naming an encoding that
    /// does not exist must still open a session, and the session it opens should
    /// be the one it would have been before anybody typed the label.
    pub fn from_label_or_utf8(label: &str) -> Charset {
        Self::for_label(label).unwrap_or(Self::UTF8)
    }

    /// Canonical name of the encoding: `"UTF-8"`, `"EUC-KR"`, `"Shift_JIS"`,
    /// `"gb18030"`, `"windows-1251"`.
    ///
    /// Both the stored label and the string the UI shows, deliberately: the
    /// canonical form round-trips through [`Charset::for_label`], so a value
    /// written by this build is read back by it exactly, and there is no second
    /// spelling to keep in step with the first.
    pub fn name(&self) -> &'static str {
        self.0.name()
    }

    /// Whether this is UTF-8, and so whether any transcoding is needed at all.
    ///
    /// The callers use it as a fast path, which matters because it is the case
    /// almost every session is in.
    pub fn is_utf8(&self) -> bool {
        self.0 == encoding_rs::UTF_8
    }

    /// Encode outbound text — a keystroke, a paste, a file being saved.
    ///
    /// Characters the charset cannot express become `?`. Use
    /// [`Charset::encode_lossy`] where the caller wants to know that happened.
    pub fn encode(&self, text: &str) -> Vec<u8> {
        if self.is_utf8() {
            return text.as_bytes().to_vec();
        }
        self.encode_lossy(text).0
    }

    /// [`Charset::encode`], also reporting whether anything was substituted.
    ///
    /// The flag is for a caller that owes the user a warning — saving a file in
    /// a charset that cannot hold all of its text loses information silently
    /// otherwise, and the user is the only one who can decide that is fine.
    pub fn encode_lossy(&self, text: &str) -> (Vec<u8>, bool) {
        if self.is_utf8() {
            return (text.as_bytes().to_vec(), false);
        }

        let mut encoder = self.0.new_encoder();
        let mut out = Vec::with_capacity(
            encoder
                .max_buffer_length_from_utf8_without_replacement(text.len())
                // Only unreachable for input near the size of the address
                // space; a guess still terminates, the `Vec` just regrows.
                .unwrap_or(text.len()),
        );
        let mut substituted = false;
        let mut buf = [0u8; ENCODE_CHUNK];
        let mut rest = text;

        loop {
            // `last` is true because a `Charset` encodes one complete string per
            // call and keeps no encoder between them: a stateful encoding — none
            // in `SUPPORTED`, but `for_label` can name one — must get its
            // return-to-ASCII trailer emitted here or the bytes are truncated
            // mid-state.
            let (result, read, written) =
                encoder.encode_from_utf8_without_replacement(rest, &mut buf, true);
            out.extend_from_slice(&buf[..written]);
            rest = &rest[read..];
            match result {
                EncoderResult::InputEmpty => break,
                // More room wanted, for the next characters or for the trailer.
                EncoderResult::OutputFull => {}
                // The offending character has already been consumed, so the
                // loop resumes after it rather than on it.
                EncoderResult::Unmappable(_) => {
                    out.push(UNMAPPABLE);
                    substituted = true;
                }
            }
        }

        (out, substituted)
    }

    /// Decode a complete buffer — a whole file, read in one piece.
    ///
    /// The counterpart of [`Charset::encode_lossy`], and the one to reach for
    /// when the bytes are all present at once: [`CharsetDecoder`] exists to
    /// carry a partial character between two reads of a socket, which a buffer
    /// that is already whole has none of.
    ///
    /// Malformed bytes become U+FFFD rather than failing, and the flag says
    /// whether any did — so a caller that decoded a file in the charset the
    /// user picked can tell them the guess did not fit.
    ///
    /// Deliberately *without* byte order mark handling. `Encoding::decode`
    /// sniffs a leading BOM and silently decodes in whatever encoding it names
    /// instead of the one it was asked for, which would make a picker showing
    /// this charset's name a lie about the text underneath it.
    pub fn decode_lossy(&self, bytes: &[u8]) -> (String, bool) {
        let (text, had_malformed) = self.0.decode_without_bom_handling(bytes);
        (text.into_owned(), had_malformed)
    }
}

impl Default for Charset {
    fn default() -> Self {
        Self::UTF8
    }
}

impl fmt::Debug for Charset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A derived `Debug` would try to print `Encoding`, which is an opaque
        // table entry and formats as one; the name is the whole of what a
        // `Charset` is.
        f.debug_tuple("Charset").field(&self.name()).finish()
    }
}

/// Streaming decoder for one session's inbound bytes.
///
/// A multi-byte character routinely arrives split across two reads of a socket
/// or a pty — nothing on either side aligns a chunk boundary to a character —
/// so the partial sequence has to survive between calls. That is exactly the
/// state `encoding_rs::Decoder` exists to carry, and the reason a `Charset`
/// alone is not enough to decode a stream.
///
/// Decoding is lossy: malformed bytes become U+FFFD rather than failing the
/// session. There is nothing better to do with them — a terminal has no way to
/// ask for a retransmission, and a stray byte from a binary file dumped to the
/// screen must not take the shell down with it.
pub struct CharsetDecoder {
    inner: encoding_rs::Decoder,
}

impl CharsetDecoder {
    /// Start decoding a stream in `charset`, with no partial sequence pending.
    pub fn new(charset: Charset) -> Self {
        Self {
            inner: charset.0.new_decoder(),
        }
    }

    /// Decode one chunk, appending the text to `out`.
    ///
    /// The end of the stream is never signalled: a session does not end tidily,
    /// it stops, and a trailing partial sequence has no bytes coming that would
    /// complete it. So `last` is always false, and an incomplete character at
    /// the tail of a chunk is simply held until the next one.
    pub fn decode(&mut self, bytes: &[u8], out: &mut String) {
        let mut rest = bytes;
        loop {
            // `decode_to_string` writes into the string's spare capacity and
            // never grows it itself, so reserving is what drives the loop.
            let reserve = self
                .inner
                .max_utf8_buffer_length(rest.len())
                .unwrap_or(DECODE_CHUNK)
                .max(MIN_DECODE_RESERVE);
            out.reserve(reserve);

            let (result, read, _had_errors) = self.inner.decode_to_string(rest, out, false);
            rest = &rest[read..];
            match result {
                CoderResult::InputEmpty => break,
                CoderResult::OutputFull => {}
            }
        }
    }
}

impl fmt::Debug for CharsetDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CharsetDecoder")
            .field("encoding", &self.inner.encoding().name())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `안녕` in EUC-KR: two KS X 1001 characters, two bytes each.
    const HELLO_EUC_KR: [u8; 4] = [0xbe, 0xc8, 0xb3, 0xe7];

    fn euc_kr() -> Charset {
        Charset::for_label("euc-kr").expect("euc-kr is a registry label")
    }

    fn decode(charset: Charset, chunks: &[&[u8]]) -> String {
        let mut decoder = CharsetDecoder::new(charset);
        let mut out = String::new();
        for chunk in chunks {
            decoder.decode(chunk, &mut out);
        }
        out
    }

    #[test]
    fn euc_kr_round_trips_hangul() {
        let charset = euc_kr();
        assert_eq!(charset.encode("안녕"), HELLO_EUC_KR.to_vec());
        assert_eq!(decode(charset, &[&HELLO_EUC_KR]), "안녕");
    }

    #[test]
    fn an_unmappable_character_becomes_a_question_mark() {
        let charset = euc_kr();
        // U+1D11E MUSICAL SYMBOL G CLEF: outside the BMP, so no legacy CJK
        // encoding has a code point for it.
        let (bytes, substituted) = charset.encode_lossy("a\u{1d11e}b");
        assert_eq!(bytes, b"a?b");
        assert!(substituted);
        // Never an HTML numeric character reference, which is what
        // `Encoding::encode` would have produced.
        assert!(!bytes.windows(2).any(|pair| pair == b"&#"));

        let (bytes, substituted) = charset.encode_lossy("안녕");
        assert_eq!(bytes, HELLO_EUC_KR.to_vec());
        assert!(!substituted);
    }

    #[test]
    fn a_character_split_across_chunks_is_resumed() {
        // The common case on a slow link, and the whole reason the decoder is
        // long-lived: neither half is a character on its own.
        assert_eq!(
            decode(euc_kr(), &[&HELLO_EUC_KR[..1], &HELLO_EUC_KR[1..]]),
            "안녕"
        );
        assert_eq!(
            decode(euc_kr(), &[&HELLO_EUC_KR[..3], &HELLO_EUC_KR[3..]]),
            "안녕"
        );
        assert!(
            !decode(euc_kr(), &[&HELLO_EUC_KR[..1], &HELLO_EUC_KR[1..]]).contains('\u{fffd}'),
            "a resumed character must not produce a replacement character"
        );
    }

    #[test]
    fn ascii_passes_through_a_legacy_charset_unchanged() {
        let charset = euc_kr();
        // What lets the escape sequences reach the emulator intact.
        let ansi = b"\x1b[1;31mls -l\x07\r\n";
        assert_eq!(decode(charset, &[ansi]), "\x1b[1;31mls -l\x07\r\n");
        assert_eq!(charset.encode("\x1b[A"), b"\x1b[A".to_vec());
    }

    #[test]
    fn a_whole_buffer_decodes_in_one_call() {
        let (text, had_malformed) = euc_kr().decode_lossy(&HELLO_EUC_KR);
        assert_eq!(text, "안녕");
        assert!(!had_malformed);
    }

    #[test]
    fn a_malformed_byte_is_replaced_and_reported() {
        // 0xbe opens a two-byte sequence and 0x20 cannot close one, so the
        // pair is not a character in this encoding at all.
        let (text, had_malformed) = euc_kr().decode_lossy(&[b'a', 0xbe, 0x20, b'b']);
        assert!(text.contains('\u{fffd}'), "{text:?} kept no replacement");
        assert!(had_malformed, "the caller has to be able to see this");
    }

    #[test]
    fn a_byte_order_mark_is_not_sniffed_away() {
        // The UTF-8 BOM, decoded as EUC-KR because that is what was asked for.
        // `Encoding::decode` would have switched to UTF-8 behind the caller's
        // back, and a picker naming EUC-KR would then be describing text that
        // was never decoded as any such thing.
        let (text, _) = euc_kr().decode_lossy(&[0xef, 0xbb, 0xbf, b'a']);
        assert!(!text.starts_with('a'), "{text:?} was decoded as UTF-8");
        // UTF-8 keeps the mark as U+FEFF rather than eating it, for the same
        // reason: what the bytes say, not what they hint at.
        assert_eq!(
            Charset::UTF8.decode_lossy(&[0xef, 0xbb, 0xbf, b'a']).0,
            "\u{feff}a"
        );
    }

    #[test]
    fn labels_resolve_case_insensitively() {
        assert_eq!(Charset::for_label("euc-kr"), Charset::for_label("EUC-KR"));
        assert_eq!(euc_kr().name(), "EUC-KR");
        assert_eq!(
            Charset::for_label("shift_jis").map(|c| c.name()),
            Some("Shift_JIS")
        );
        assert_eq!(Charset::for_label("nonsense"), None);
    }

    #[test]
    fn an_unknown_label_falls_back_to_utf8() {
        assert_eq!(Charset::from_label_or_utf8("nonsense"), Charset::UTF8);
        assert_eq!(Charset::from_label_or_utf8(""), Charset::UTF8);
        assert_eq!(Charset::from_label_or_utf8("utf-8"), Charset::default());
        assert!(Charset::default().is_utf8());
        assert!(!euc_kr().is_utf8());
    }

    #[test]
    fn utf8_encodes_and_decodes_verbatim() {
        let charset = Charset::UTF8;
        assert_eq!(charset.encode("ls 안녕\r"), "ls 안녕\r".as_bytes());
        let (bytes, substituted) = charset.encode_lossy("안녕 \u{1d11e}");
        assert_eq!(bytes, "안녕 \u{1d11e}".as_bytes());
        assert!(!substituted, "UTF-8 can express every character");
        assert_eq!(decode(charset, &["안녕".as_bytes()]), "안녕");
    }

    #[test]
    fn the_supported_list_starts_with_utf8_and_has_no_duplicates() {
        assert_eq!(Charset::SUPPORTED[0], Charset::UTF8);
        let names: Vec<&str> = Charset::SUPPORTED.iter().map(Charset::name).collect();
        for (index, name) in names.iter().enumerate() {
            assert!(
                !names[..index].contains(name),
                "{name} appears twice in the offered list"
            );
            // Every name must survive a round trip through the label lookup, or
            // a saved profile would not reload as what the user picked.
            assert_eq!(Charset::for_label(name).map(|c| c.name()), Some(*name));
        }
    }

    #[test]
    fn a_long_run_crosses_the_scratch_buffer_boundary() {
        // Longer than `ENCODE_CHUNK`, so the encode loop goes around at least
        // twice and has to resume where it left off.
        let charset = euc_kr();
        let text = "안녕".repeat(ENCODE_CHUNK);
        let bytes = charset.encode(&text);
        assert_eq!(bytes.len(), HELLO_EUC_KR.len() * ENCODE_CHUNK);
        assert_eq!(decode(charset, &[&bytes]), text);
    }

    #[test]
    fn debug_prints_the_encoding_name() {
        assert_eq!(format!("{:?}", euc_kr()), "Charset(\"EUC-KR\")");
    }
}
