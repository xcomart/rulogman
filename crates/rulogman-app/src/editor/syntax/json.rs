//! JSON, which is the one format here that fits on a line.
//!
//! A JSON string may not contain a raw newline and JSON has no comments, so
//! there is nothing for a line to leave open and nothing to carry —
//! [`Language::carries_state`](super::Language::carries_state) says so, and the
//! cache above skips this language entirely.
//!
//! The one distinction worth making is the one JSON itself does not: a string
//! followed by a `:` is a member name, and a string anywhere else is a value.
//! That is what turns a wall of quotes into a document with a shape.
//!
//! # What is given up
//!
//! An unterminated string is coloured to the end of its line rather than being
//! reported. There is nowhere here to report it to, and a half-typed string
//! reading as a string is exactly right while it is being typed.

use super::{
    Runs, Token, TokenKind, char_step, number, quote_body, skip_spaces, word_boundary, word_end,
};

/// The three bare words JSON allows.
const LITERALS: &[&str] = &["false", "null", "true"];

/// The tokens of one line of JSON.
pub fn lex_line(line: &str) -> Vec<Token> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut runs = Runs::new();
    let mut at = 0;

    while at < len {
        let byte = bytes[at];
        match byte {
            b'"' => match quote_body(line, at + 1, b'"', true) {
                Some(end) => {
                    // A member name is a string with a colon after it, give or
                    // take the whitespace a formatter left in between.
                    let kind = if bytes.get(skip_spaces(bytes, end)) == Some(&b':') {
                        TokenKind::Key
                    } else {
                        TokenKind::String
                    };
                    runs.push(kind, at, end);
                    at = end;
                }
                None => {
                    runs.push(TokenKind::String, at, len);
                    at = len;
                }
            },
            // A leading `-` belongs to the number only when a digit follows it,
            // so a stray one stays punctuation.
            _ if word_boundary(bytes, at)
                && (byte.is_ascii_digit()
                    || (byte == b'-'
                        && matches!(bytes.get(at + 1), Some(next) if next.is_ascii_digit()))) =>
            {
                let end = number(line, at);
                runs.push(TokenKind::Number, at, end);
                at = end.max(at + 1);
            }
            _ if byte.is_ascii_alphabetic() && word_boundary(bytes, at) => {
                let end = word_end(bytes, at);
                if LITERALS.binary_search(&&line[at..end]).is_ok() {
                    runs.push(TokenKind::Literal, at, end);
                }
                at = end;
            }
            _ => at += char_step(line, at),
        }
    }

    runs.finish(len)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{kinds, tiles};
    use super::*;

    /// The tokens of `line`.
    fn lex(line: &str) -> Vec<Token> {
        let tokens = lex_line(line);
        tiles(line, &tokens);
        tokens
    }

    #[test]
    fn the_literal_table_is_sorted() {
        assert!(LITERALS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_member_name_is_told_from_a_value() {
        assert_eq!(
            kinds(r#"{"host": "web"}"#, &lex(r#"{"host": "web"}"#)),
            [
                (TokenKind::Plain, "{"),
                (TokenKind::Key, r#""host""#),
                (TokenKind::Plain, ": "),
                (TokenKind::String, r#""web""#),
                (TokenKind::Plain, "}"),
            ]
        );
    }

    #[test]
    fn a_name_split_from_its_colon_is_still_a_name() {
        // What a formatter that aligns colons produces.
        assert_eq!(lex(r#""host"   : 1"#)[0].kind, TokenKind::Key);
    }

    #[test]
    fn escapes_do_not_end_a_string() {
        let line = r#"{"path": "c:\\x\"y", "n": 1}"#;
        let strings: Vec<_> = lex(line)
            .into_iter()
            .filter(|token| token.kind == TokenKind::String)
            .map(|token| line[token.start..token.end].to_owned())
            .collect();
        assert_eq!(strings, [r#""c:\\x\"y""#]);
    }

    #[test]
    fn numbers_and_the_three_bare_words() {
        assert_eq!(
            kinds("[-1.5e3, true, null]", &lex("[-1.5e3, true, null]")),
            [
                (TokenKind::Plain, "["),
                (TokenKind::Number, "-1.5e3"),
                (TokenKind::Plain, ", "),
                (TokenKind::Literal, "true"),
                (TokenKind::Plain, ", "),
                (TokenKind::Literal, "null"),
                (TokenKind::Plain, "]"),
            ]
        );
    }

    #[test]
    fn an_unterminated_string_stops_at_the_line() {
        let line = r#"{"half: "#;
        let tokens = lex(line);
        let last = tokens.last().expect("a line with text has tokens");
        assert_eq!((last.kind, last.end), (TokenKind::String, line.len()));
    }

    #[test]
    fn nothing_here_panics_on_anything() {
        for line in ["", "\"", "-", "{}", "[,]", r#"{"한글": "값"}"#, "🙂"] {
            tiles(line, &lex_line(line));
        }
    }
}
