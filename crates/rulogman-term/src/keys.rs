//! Translation of key presses into the byte sequences a PTY expects.
//!
//! The types here are deliberately GUI agnostic: the windowing layer is
//! expected to lower its own key events into a [`KeyInput`] before calling
//! [`encode_key`].
//!
//! Every entry point takes the session's [`Charset`], because what reaches the
//! remote has to be what that host reads: a filename typed at an EUC-KR shell
//! must arrive in EUC-KR, not in the UTF-8 the keyboard layout produced. Only
//! the text goes through it — the control bytes and escape sequences below are
//! written as the literal bytes they are, which is also what they would come out
//! of any of the supported charsets as.

use crate::charset::Charset;

/// A logical key, independent of any keyboard layout or windowing toolkit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    /// A character producing key, already resolved through the keyboard layout.
    Char(char),
    /// Return / Enter.
    Enter,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Escape.
    Escape,
    /// Cursor up.
    Up,
    /// Cursor down.
    Down,
    /// Cursor left.
    Left,
    /// Cursor right.
    Right,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Insert.
    Insert,
    /// Delete.
    Delete,
    /// Function key, `1..=12`.
    F(u8),
}

/// A key press together with its modifier state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput {
    /// The key that was pressed.
    pub code: KeyCode,
    /// Control modifier.
    pub ctrl: bool,
    /// Alt / Option modifier.
    pub alt: bool,
    /// Shift modifier.
    ///
    /// For [`KeyCode::Char`] the shift state is expected to already be baked
    /// into the character, so it only matters for the non-character keys.
    pub shift: bool,
}

impl KeyInput {
    /// A key press without any modifier.
    pub fn new(code: KeyCode) -> Self {
        Self {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// Builder style setter for the control modifier.
    pub fn with_ctrl(mut self, ctrl: bool) -> Self {
        self.ctrl = ctrl;
        self
    }

    /// Builder style setter for the alt modifier.
    pub fn with_alt(mut self, alt: bool) -> Self {
        self.alt = alt;
        self
    }

    /// Builder style setter for the shift modifier.
    pub fn with_shift(mut self, shift: bool) -> Self {
        self.shift = shift;
        self
    }

    /// `true` when at least one modifier is held.
    fn has_modifiers(&self) -> bool {
        self.ctrl || self.alt || self.shift
    }

    /// The xterm modifier parameter: `1 + shift + 2 * alt + 4 * ctrl`.
    fn modifier_param(&self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }
}

/// Terminal modes that influence how keys are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TermModes {
    /// `DECCKM`: cursor keys emit `SS3` instead of `CSI` sequences.
    pub app_cursor: bool,
    /// `DECKPAM`: the numeric keypad is in application mode.
    pub app_keypad: bool,
    /// Pasted text has to be wrapped in bracketed paste markers.
    pub bracketed_paste: bool,
}

/// Start marker of a bracketed paste.
const PASTE_START: &[u8] = b"\x1b[200~";
/// End marker of a bracketed paste.
const PASTE_END: &[u8] = b"\x1b[201~";

/// Encode a key press into the bytes that should be written to the PTY.
///
/// Returns `None` when the key produces no output, for example an unsupported
/// function key. `charset` only reaches the character producing keys; every
/// other branch emits ASCII.
pub fn encode_key(input: KeyInput, modes: TermModes, charset: Charset) -> Option<Vec<u8>> {
    match input.code {
        KeyCode::Char(c) => Some(encode_char(c, input, charset)),
        KeyCode::Enter => Some(with_alt(input, b"\r")),
        KeyCode::Tab => {
            if input.shift {
                Some(b"\x1b[Z".to_vec())
            } else {
                Some(with_alt(input, b"\t"))
            }
        }
        KeyCode::Backspace => {
            let byte = if input.ctrl { 0x08 } else { 0x7f };
            Some(with_alt(input, &[byte]))
        }
        KeyCode::Escape => Some(with_alt(input, &[0x1b])),
        KeyCode::Up => Some(cursor_key(b'A', input, modes)),
        KeyCode::Down => Some(cursor_key(b'B', input, modes)),
        KeyCode::Right => Some(cursor_key(b'C', input, modes)),
        KeyCode::Left => Some(cursor_key(b'D', input, modes)),
        KeyCode::Home => Some(cursor_key(b'H', input, modes)),
        KeyCode::End => Some(cursor_key(b'F', input, modes)),
        KeyCode::Insert => Some(tilde_key(2, input)),
        KeyCode::Delete => Some(tilde_key(3, input)),
        KeyCode::PageUp => Some(tilde_key(5, input)),
        KeyCode::PageDown => Some(tilde_key(6, input)),
        KeyCode::F(1) => Some(ss3_key(b'P', input)),
        KeyCode::F(2) => Some(ss3_key(b'Q', input)),
        KeyCode::F(3) => Some(ss3_key(b'R', input)),
        KeyCode::F(4) => Some(ss3_key(b'S', input)),
        KeyCode::F(5) => Some(tilde_key(15, input)),
        KeyCode::F(6) => Some(tilde_key(17, input)),
        KeyCode::F(7) => Some(tilde_key(18, input)),
        KeyCode::F(8) => Some(tilde_key(19, input)),
        KeyCode::F(9) => Some(tilde_key(20, input)),
        KeyCode::F(10) => Some(tilde_key(21, input)),
        KeyCode::F(11) => Some(tilde_key(23, input)),
        KeyCode::F(12) => Some(tilde_key(24, input)),
        KeyCode::F(_) => None,
    }
}

/// Wrap `text` for a paste operation.
///
/// In bracketed paste mode the text is surrounded by the `CSI 200 ~` /
/// `CSI 201 ~` markers and stripped of escape characters so that it can not
/// terminate the paste early. Otherwise line endings are normalised to `\r`,
/// which is what the shell would have seen had the user pressed Enter.
///
/// Both the sanitising and the newline normalisation happen on the text, before
/// `charset` is applied: they are decisions about characters, and only the
/// payload is transcoded — the markers are ours to write and stay ASCII.
pub fn encode_paste(text: &str, modes: TermModes, charset: Charset) -> Vec<u8> {
    if modes.bracketed_paste {
        let sanitized = charset.encode(&text.replace('\x1b', ""));
        let mut out = Vec::with_capacity(sanitized.len() + PASTE_START.len() + PASTE_END.len());
        out.extend_from_slice(PASTE_START);
        out.extend_from_slice(&sanitized);
        out.extend_from_slice(PASTE_END);
        out
    } else {
        charset.encode(&text.replace("\r\n", "\r").replace('\n', "\r"))
    }
}

/// Prefix `bytes` with `ESC` when the alt modifier is held.
fn with_alt(input: KeyInput, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if input.alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

/// Encode a character producing key.
///
/// The control branch is charset-free by construction: a C0 code is a byte, not
/// a character, and every supported charset leaves that range alone anyway.
fn encode_char(c: char, input: KeyInput, charset: Charset) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    if input.alt {
        out.push(0x1b);
    }

    let control = if input.ctrl { control_byte(c) } else { None };
    match control {
        Some(byte) => out.push(byte),
        None => {
            let mut buf = [0u8; 4];
            let text = c.encode_utf8(&mut buf);
            if charset.is_utf8() {
                out.extend_from_slice(text.as_bytes());
            } else {
                out.extend_from_slice(&charset.encode(text));
            }
        }
    }

    out
}

/// Map a character to its C0 control code, if one exists.
fn control_byte(c: char) -> Option<u8> {
    let byte = match c {
        ' ' | '@' | '2' => 0x00,
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    };
    Some(byte)
}

/// Encode a cursor style key (arrows, Home, End).
fn cursor_key(final_byte: u8, input: KeyInput, modes: TermModes) -> Vec<u8> {
    if input.has_modifiers() {
        // Modified cursor keys always use the CSI form.
        let mut out = format!("\x1b[1;{}", input.modifier_param()).into_bytes();
        out.push(final_byte);
        out
    } else if modes.app_cursor {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

/// Encode an `SS3` style function key (F1 - F4).
fn ss3_key(final_byte: u8, input: KeyInput) -> Vec<u8> {
    if input.has_modifiers() {
        let mut out = format!("\x1b[1;{}", input.modifier_param()).into_bytes();
        out.push(final_byte);
        out
    } else {
        vec![0x1b, b'O', final_byte]
    }
}

/// Encode a `CSI <number> ~` style key.
fn tilde_key(number: u8, input: KeyInput) -> Vec<u8> {
    if input.has_modifiers() {
        format!("\x1b[{};{}~", number, input.modifier_param()).into_bytes()
    } else {
        format!("\x1b[{number}~").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyInput {
        KeyInput::new(code)
    }

    /// The charset every test but the two legacy ones runs with.
    fn utf8() -> Charset {
        Charset::default()
    }

    fn euc_kr() -> Charset {
        Charset::from_label_or_utf8("EUC-KR")
    }

    fn encode(code: KeyCode, modes: TermModes) -> Vec<u8> {
        encode_key(key(code), modes, utf8()).expect("key should produce bytes")
    }

    #[test]
    fn plain_characters_are_utf8() {
        assert_eq!(encode(KeyCode::Char('a'), TermModes::default()), b"a");
        assert_eq!(
            encode(KeyCode::Char('가'), TermModes::default()),
            "가".as_bytes()
        );
    }

    #[test]
    fn a_legacy_charset_encodes_the_character_and_not_the_escape() {
        let modes = TermModes::default();
        // `안` is 0xBEC8 in EUC-KR, and ASCII stays ASCII.
        assert_eq!(
            encode_key(key(KeyCode::Char('안')), modes, euc_kr()).unwrap(),
            vec![0xbe, 0xc8]
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('a')), modes, euc_kr()).unwrap(),
            b"a"
        );

        // Alt keeps its `ESC` prefix, and the charset applies to what follows.
        let alt_han = KeyInput::new(KeyCode::Char('안')).with_alt(true);
        assert_eq!(
            encode_key(alt_han, modes, euc_kr()).unwrap(),
            vec![0x1b, 0xbe, 0xc8]
        );

        // A control code is a byte, so the charset never sees it.
        let ctrl_c = KeyInput::new(KeyCode::Char('c')).with_ctrl(true);
        assert_eq!(encode_key(ctrl_c, modes, euc_kr()).unwrap(), vec![0x03]);
        assert_eq!(
            encode_key(key(KeyCode::Up), modes, euc_kr()).unwrap(),
            b"\x1b[A"
        );
    }

    #[test]
    fn ctrl_letters_map_to_control_codes() {
        let modes = TermModes::default();
        let ctrl_c = KeyInput::new(KeyCode::Char('c')).with_ctrl(true);
        assert_eq!(encode_key(ctrl_c, modes, utf8()).unwrap(), vec![0x03]);

        let ctrl_upper_c = KeyInput::new(KeyCode::Char('C')).with_ctrl(true);
        assert_eq!(encode_key(ctrl_upper_c, modes, utf8()).unwrap(), vec![0x03]);

        let ctrl_a = KeyInput::new(KeyCode::Char('a')).with_ctrl(true);
        assert_eq!(encode_key(ctrl_a, modes, utf8()).unwrap(), vec![0x01]);

        let ctrl_z = KeyInput::new(KeyCode::Char('z')).with_ctrl(true);
        assert_eq!(encode_key(ctrl_z, modes, utf8()).unwrap(), vec![0x1a]);
    }

    #[test]
    fn ctrl_punctuation_and_space() {
        let modes = TermModes::default();
        let cases = [(' ', 0x00u8), ('[', 0x1b), ('\\', 0x1c), (']', 0x1d)];
        for (c, expected) in cases {
            let input = KeyInput::new(KeyCode::Char(c)).with_ctrl(true);
            assert_eq!(
                encode_key(input, modes, utf8()).unwrap(),
                vec![expected],
                "ctrl+{c:?}"
            );
        }
    }

    #[test]
    fn ctrl_without_mapping_falls_back_to_the_character() {
        let input = KeyInput::new(KeyCode::Char('1')).with_ctrl(true);
        assert_eq!(
            encode_key(input, TermModes::default(), utf8()).unwrap(),
            b"1"
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        let input = KeyInput::new(KeyCode::Char('a')).with_alt(true);
        assert_eq!(
            encode_key(input, TermModes::default(), utf8()).unwrap(),
            vec![0x1b, b'a']
        );

        let ctrl_alt = KeyInput::new(KeyCode::Char('c'))
            .with_alt(true)
            .with_ctrl(true);
        assert_eq!(
            encode_key(ctrl_alt, TermModes::default(), utf8()).unwrap(),
            vec![0x1b, 0x03]
        );
    }

    #[test]
    fn simple_control_keys() {
        let modes = TermModes::default();
        assert_eq!(encode(KeyCode::Enter, modes), b"\r");
        assert_eq!(encode(KeyCode::Tab, modes), b"\t");
        assert_eq!(encode(KeyCode::Escape, modes), vec![0x1b]);
        assert_eq!(encode(KeyCode::Backspace, modes), vec![0x7f]);

        let ctrl_backspace = KeyInput::new(KeyCode::Backspace).with_ctrl(true);
        assert_eq!(
            encode_key(ctrl_backspace, modes, utf8()).unwrap(),
            vec![0x08]
        );

        let shift_tab = KeyInput::new(KeyCode::Tab).with_shift(true);
        assert_eq!(encode_key(shift_tab, modes, utf8()).unwrap(), b"\x1b[Z");
    }

    #[test]
    fn arrow_keys_respect_app_cursor_mode() {
        let normal = TermModes::default();
        let app = TermModes {
            app_cursor: true,
            ..TermModes::default()
        };

        assert_eq!(encode(KeyCode::Up, normal), b"\x1b[A");
        assert_eq!(encode(KeyCode::Down, normal), b"\x1b[B");
        assert_eq!(encode(KeyCode::Right, normal), b"\x1b[C");
        assert_eq!(encode(KeyCode::Left, normal), b"\x1b[D");

        assert_eq!(encode(KeyCode::Up, app), b"\x1bOA");
        assert_eq!(encode(KeyCode::Down, app), b"\x1bOB");
        assert_eq!(encode(KeyCode::Right, app), b"\x1bOC");
        assert_eq!(encode(KeyCode::Left, app), b"\x1bOD");
    }

    #[test]
    fn modified_arrows_use_csi_with_modifier_parameter() {
        let app = TermModes {
            app_cursor: true,
            ..TermModes::default()
        };
        let ctrl_up = KeyInput::new(KeyCode::Up).with_ctrl(true);
        assert_eq!(encode_key(ctrl_up, app, utf8()).unwrap(), b"\x1b[1;5A");

        let shift_left = KeyInput::new(KeyCode::Left).with_shift(true);
        assert_eq!(
            encode_key(shift_left, TermModes::default(), utf8()).unwrap(),
            b"\x1b[1;2D"
        );

        let alt_right = KeyInput::new(KeyCode::Right).with_alt(true);
        assert_eq!(
            encode_key(alt_right, TermModes::default(), utf8()).unwrap(),
            b"\x1b[1;3C"
        );

        let all = KeyInput::new(KeyCode::Down)
            .with_ctrl(true)
            .with_alt(true)
            .with_shift(true);
        assert_eq!(
            encode_key(all, TermModes::default(), utf8()).unwrap(),
            b"\x1b[1;8B"
        );
    }

    #[test]
    fn home_and_end() {
        let normal = TermModes::default();
        let app = TermModes {
            app_cursor: true,
            ..TermModes::default()
        };
        assert_eq!(encode(KeyCode::Home, normal), b"\x1b[H");
        assert_eq!(encode(KeyCode::End, normal), b"\x1b[F");
        assert_eq!(encode(KeyCode::Home, app), b"\x1bOH");
        assert_eq!(encode(KeyCode::End, app), b"\x1bOF");
    }

    #[test]
    fn editing_and_paging_keys() {
        let modes = TermModes::default();
        assert_eq!(encode(KeyCode::Insert, modes), b"\x1b[2~");
        assert_eq!(encode(KeyCode::Delete, modes), b"\x1b[3~");
        assert_eq!(encode(KeyCode::PageUp, modes), b"\x1b[5~");
        assert_eq!(encode(KeyCode::PageDown, modes), b"\x1b[6~");

        let ctrl_delete = KeyInput::new(KeyCode::Delete).with_ctrl(true);
        assert_eq!(
            encode_key(ctrl_delete, modes, utf8()).unwrap(),
            b"\x1b[3;5~"
        );
    }

    #[test]
    fn function_keys() {
        let modes = TermModes::default();
        assert_eq!(encode(KeyCode::F(1), modes), b"\x1bOP");
        assert_eq!(encode(KeyCode::F(2), modes), b"\x1bOQ");
        assert_eq!(encode(KeyCode::F(3), modes), b"\x1bOR");
        assert_eq!(encode(KeyCode::F(4), modes), b"\x1bOS");
        assert_eq!(encode(KeyCode::F(5), modes), b"\x1b[15~");
        assert_eq!(encode(KeyCode::F(6), modes), b"\x1b[17~");
        assert_eq!(encode(KeyCode::F(12), modes), b"\x1b[24~");
        assert_eq!(encode_key(key(KeyCode::F(13)), modes, utf8()), None);
        assert_eq!(encode_key(key(KeyCode::F(0)), modes, utf8()), None);
    }

    #[test]
    fn paste_without_bracketed_mode_normalises_newlines() {
        let modes = TermModes::default();
        assert_eq!(encode_paste("a\r\nb\nc", modes, utf8()), b"a\rb\rc");
    }

    #[test]
    fn paste_with_bracketed_mode_is_wrapped_and_sanitized() {
        let modes = TermModes {
            bracketed_paste: true,
            ..TermModes::default()
        };
        assert_eq!(encode_paste("hi", modes, utf8()), b"\x1b[200~hi\x1b[201~");
        // An embedded terminator must not be able to end the paste early.
        assert_eq!(
            encode_paste("a\x1b[201~b", modes, utf8()),
            b"\x1b[200~a[201~b\x1b[201~"
        );
    }

    #[test]
    fn a_legacy_charset_transcodes_the_payload_and_not_the_markers() {
        let bracketed = TermModes {
            bracketed_paste: true,
            ..TermModes::default()
        };
        let mut expected = PASTE_START.to_vec();
        expected.extend_from_slice(&[0xbe, 0xc8, b'\n']);
        expected.extend_from_slice(PASTE_END);
        assert_eq!(encode_paste("안\n", bracketed, euc_kr()), expected);

        // Without bracketing the newline is still normalised first, on the text.
        assert_eq!(
            encode_paste("안\r\n녕", TermModes::default(), euc_kr()),
            vec![0xbe, 0xc8, b'\r', 0xb3, 0xe7]
        );
    }
}
