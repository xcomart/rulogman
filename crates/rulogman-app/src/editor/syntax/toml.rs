//! TOML: tables, keys, and the one construct that crosses a line.
//!
//! TOML is regular enough that a line scanner gets most of it right. The three
//! things worth being careful about are the ones a reader looks for: the
//! `[table]` header that says where you are, the key on the left of an `=`, and
//! the `"""` string that runs on until it is closed.
//!
//! # What is given up
//!
//! * A key is a word with an `=` after it, whatever the nesting. That gets
//!   `a = 1`, `a.b = 1` and `{ x = 1 }` right and would call the `x` in a
//!   comparison a key too, except that TOML has no comparisons.
//! * A multi-line string's closing delimiter is found by searching for three
//!   quotes, without regard for a backslash before them. `\"""` inside a
//!   `"""` string ends it early, in colour only.

use super::{
    Carry, LineState, Runs, Token, TokenKind, char_step, number, quote_body, skip_spaces,
    word_boundary,
};

/// The two bare words TOML allows as values.
const LITERALS: &[&str] = &["false", "true"];

/// The tokens of one line of TOML, and the state it leaves behind.
pub fn lex_line(line: &str, state: LineState) -> (Vec<Token>, LineState) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut runs = Runs::new();
    let mut at = 0;

    if let Carry::Multiline(quote) = state.0 {
        match triple_end(line, 0, quote) {
            Some(end) => {
                runs.push(TokenKind::String, 0, end);
                at = end;
            }
            None => {
                runs.push(TokenKind::String, 0, len);
                return (runs.finish(len), state);
            }
        }
    }

    // A `[table]` or `[[array]]` header, which is only a header at the head of
    // a line: a `[` anywhere else opens an array.
    let head = skip_spaces(bytes, at);
    if at == 0 && bytes.get(head) == Some(&b'[') {
        let end = header_end(line, head);
        runs.push(TokenKind::Key, head, end);
        at = end;
    }

    while at < len {
        let byte = bytes[at];
        match byte {
            b'#' => {
                runs.push(TokenKind::Comment, at, len);
                at = len;
            }
            b'"' | b'\''
                if bytes.get(at + 1) == Some(&byte) && bytes.get(at + 2) == Some(&byte) =>
            {
                match triple_end(line, at + 3, byte) {
                    Some(end) => {
                        runs.push(TokenKind::String, at, end);
                        at = end;
                    }
                    None => {
                        runs.push(TokenKind::String, at, len);
                        return (runs.finish(len), LineState(Carry::Multiline(byte)));
                    }
                }
            }
            b'"' | b'\'' => {
                let end = quote_body(line, at + 1, byte, byte == b'"').unwrap_or(len);
                // A quoted key is still a key.
                let kind = if bytes.get(skip_spaces(bytes, end)) == Some(&b'=') {
                    TokenKind::Key
                } else {
                    TokenKind::String
                };
                runs.push(kind, at, end);
                at = end;
            }
            _ if word_boundary(bytes, at)
                && (byte.is_ascii_digit()
                    || (matches!(byte, b'-' | b'+')
                        && matches!(bytes.get(at + 1), Some(next) if next.is_ascii_digit()))) =>
            {
                let end = number(line, at);
                runs.push(TokenKind::Number, at, end);
                at = end.max(at + 1);
            }
            _ if (byte.is_ascii_alphabetic() || byte == b'_') && word_boundary(bytes, at) => {
                let end = bare_key_end(bytes, at);
                let word = &line[at..end];
                if LITERALS.binary_search(&word).is_ok() {
                    runs.push(TokenKind::Literal, at, end);
                } else if matches!(bytes.get(skip_spaces(bytes, end)), Some(b'=' | b'.')) {
                    // The `.` is what makes both halves of a dotted key read as
                    // one thing rather than as a key with a word in front of it.
                    runs.push(TokenKind::Key, at, end);
                }
                at = end;
            }
            _ => at += char_step(line, at),
        }
    }

    (runs.finish(len), LineState::START)
}

/// The end of a table header whose `[` is at `at`.
///
/// The last `]` before a comment, so that `[[a.b]]` comes out whole; a header
/// that never closes takes the rest of the line, which is what it looks like
/// while it is being typed.
fn header_end(line: &str, at: usize) -> usize {
    let bytes = line.as_bytes();
    let mut end = at;
    let mut close = None;
    while end < bytes.len() {
        match bytes[end] {
            b'#' => break,
            b']' => {
                close = Some(end + 1);
                end += 1;
            }
            b'"' | b'\'' => {
                end =
                    quote_body(line, end + 1, bytes[end], bytes[end] == b'"').unwrap_or(bytes.len())
            }
            _ => end += char_step(line, end),
        }
    }
    close.unwrap_or(bytes.len())
}

/// The end of a multi-line string body starting at `at`, delimited by three
/// `quote` bytes.
fn triple_end(line: &str, at: usize, quote: u8) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut end = at;
    while end + 3 <= bytes.len() {
        if bytes[end] == quote && bytes[end + 1] == quote && bytes[end + 2] == quote {
            return Some(end + 3);
        }
        end += char_step(line, end);
    }
    None
}

/// The end of the bare key starting at `at`: TOML allows `-` in one, unlike
/// most of the formats here.
fn bare_key_end(bytes: &[u8], at: usize) -> usize {
    let mut end = at;
    while matches!(bytes.get(end), Some(byte) if byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
    {
        end += 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::super::tests::{kinds, tiles};
    use super::*;

    /// The tokens of `line` from a clean state.
    fn lex(line: &str) -> Vec<Token> {
        let (tokens, _) = lex_line(line, LineState::START);
        tiles(line, &tokens);
        tokens
    }

    #[test]
    fn a_table_header_is_one_key() {
        assert_eq!(
            kinds("[server.http]", &lex("[server.http]")),
            [(TokenKind::Key, "[server.http]")]
        );
        assert_eq!(
            kinds("[[hosts]] # many", &lex("[[hosts]] # many")),
            [
                (TokenKind::Key, "[[hosts]]"),
                (TokenKind::Plain, " "),
                (TokenKind::Comment, "# many"),
            ]
        );
    }

    #[test]
    fn a_key_is_the_word_before_the_equals() {
        assert_eq!(
            kinds("keep-alive = 30", &lex("keep-alive = 30")),
            [
                (TokenKind::Key, "keep-alive"),
                (TokenKind::Plain, " = "),
                (TokenKind::Number, "30"),
            ]
        );
    }

    #[test]
    fn both_halves_of_a_dotted_key_are_keys() {
        let line = "a.b = 1";
        let keys: Vec<_> = lex(line)
            .into_iter()
            .filter(|token| token.kind == TokenKind::Key)
            .map(|token| line[token.start..token.end].to_owned())
            .collect();
        assert_eq!(keys, ["a", "b"]);
    }

    #[test]
    fn an_inline_table_keeps_its_keys() {
        let line = "point = { x = 1, y = 2 }";
        assert_eq!(
            lex(line)
                .iter()
                .filter(|token| token.kind == TokenKind::Key)
                .count(),
            3
        );
    }

    #[test]
    fn a_quoted_key_is_a_key_and_a_quoted_value_is_a_string() {
        assert_eq!(
            kinds(r#""a b" = "c""#, &lex(r#""a b" = "c""#)),
            [
                (TokenKind::Key, r#""a b""#),
                (TokenKind::Plain, " = "),
                (TokenKind::String, r#""c""#),
            ]
        );
    }

    #[test]
    fn a_multiline_string_carries_to_where_it_closes() {
        let (tokens, after) = lex_line(r#"text = """first"#, LineState::START);
        assert_eq!(tokens[0].kind, TokenKind::Key);
        assert_eq!(after, LineState(Carry::Multiline(b'"')));

        let (middle, still) = lex_line("second # not a comment", after);
        assert_eq!(middle[0].kind, TokenKind::String);
        assert_eq!(still, after);

        let (last, closed) = lex_line(r#"third""" # a comment"#, after);
        assert!(closed.is_start());
        assert_eq!(last[0].kind, TokenKind::String);
        assert_eq!(last.last().expect("tokens").kind, TokenKind::Comment);
    }

    #[test]
    fn a_triple_quote_that_opens_and_closes_on_one_line_carries_nothing() {
        assert!(lex_line(r#"a = """one""""#, LineState::START).1.is_start());
    }

    #[test]
    fn a_date_reads_as_one_number() {
        let line = "when = 2026-08-08";
        assert_eq!(
            kinds(line, &lex(line)),
            [
                (TokenKind::Key, "when"),
                (TokenKind::Plain, " = "),
                (TokenKind::Number, "2026-08-08"),
            ]
        );
    }

    #[test]
    fn nothing_here_panics_on_anything() {
        for line in [
            "",
            "[",
            "[[",
            "\"\"\"",
            "'''",
            "=",
            "a =",
            "키 = \"값\"",
            "🙂 = 🙂",
        ] {
            let (tokens, _) = lex_line(line, LineState::START);
            tiles(line, &tokens);
        }
    }
}
