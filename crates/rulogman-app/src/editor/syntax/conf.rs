//! The flat configuration formats: `.ini`, `.conf`, `.cfg`, `.properties`,
//! `.env`, and the `sshd_config` family.
//!
//! One lexer for all of them because they are one format with several
//! punctuations. A line is a comment, a `[section]`, or a mapping — and the
//! mapping is spelled `key = value`, `key: value` or `key value` depending on
//! whose parser reads it. Nothing crosses a line, which is why
//! [`Language::carries_state`](super::Language::carries_state) says this
//! language has nothing to remember.
//!
//! # What is given up
//!
//! * `key value` is only read as a mapping when the key is a bare word, so a
//!   line of an `/etc/hosts`-shaped file is not turned into a key by accident.
//! * A trailing `#` is only a comment when whitespace precedes it, because a
//!   `#` is a legal character in a password and half of these files hold one.
//! * `.properties` continuation lines — a value ending in `\` — are not
//!   followed. The next line reads as another mapping, which is what it looks
//!   like.

use super::{
    Runs, Token, TokenKind, char_step, number, quote_body, skip_spaces, word_boundary, word_end,
};

/// The words a value can be instead of a string or a number.
///
/// The union of what these formats spell a boolean with, since no single one of
/// them agrees with the others and a file is never ambiguous about which it
/// meant.
const LITERALS: &[&str] = &[
    "FALSE", "False", "NO", "OFF", "ON", "TRUE", "True", "YES", "false", "no", "none", "null",
    "off", "on", "true", "yes",
];

/// The one word that can stand in front of a key, in a `.env` file meant to be
/// sourced as well as read.
const EXPORT: &str = "export";

/// The tokens of one line of a flat configuration file.
pub fn lex_line(line: &str) -> Vec<Token> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut runs = Runs::new();
    let head = skip_spaces(bytes, 0);

    // `;` is the ini spelling, `!` the `.properties` one, and `#` everyone's.
    if matches!(bytes.get(head), Some(b'#' | b';' | b'!')) {
        runs.push(TokenKind::Comment, head, len);
        return runs.finish(len);
    }

    if bytes.get(head) == Some(&b'[') {
        // To the last `]` on the line, so `[a.b]` and a stray one both come out
        // as the header they are being typed towards.
        let end = line.rfind(']').map_or(len, |at| at + 1);
        runs.push(TokenKind::Key, head, end);
        return value(runs, line, end);
    }

    let mut at = head;
    if line[at..].starts_with(EXPORT) && matches!(bytes.get(at + EXPORT.len()), Some(b' ' | b'\t'))
    {
        runs.push(TokenKind::Keyword, at, at + EXPORT.len());
        at = skip_spaces(bytes, at + EXPORT.len());
    }

    let key = word_end(bytes, at);
    if key > at {
        // The separator decides whether this was a mapping at all. An `=` or a
        // `:` says so outright; whitespace says so only when something follows
        // it, which is how `sshd_config` writes one and how a bare word alone
        // on a line stays a bare word.
        match bytes.get(key) {
            Some(b'=' | b':') => {
                runs.push(TokenKind::Key, at, key);
                at = key + 1;
            }
            Some(b' ' | b'\t') if skip_spaces(bytes, key) < len => {
                runs.push(TokenKind::Key, at, key);
                at = key;
            }
            _ => {}
        }
    }

    value(runs, line, at)
}

/// Scans the value side of a line — everything a mapping's key does not cover.
fn value(mut runs: Runs, line: &str, from: usize) -> Vec<Token> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut at = from;

    while at < len {
        let byte = bytes[at];
        match byte {
            b'#' | b';' if at > 0 && matches!(bytes.get(at - 1), Some(b' ' | b'\t')) => {
                runs.push(TokenKind::Comment, at, len);
                at = len;
            }
            b'"' | b'\'' => {
                let end = quote_body(line, at + 1, byte, byte == b'"').unwrap_or(len);
                runs.push(TokenKind::String, at, end);
                at = end;
            }
            // A `$VAR` in a `.env` file, and in every `.conf` that is read by a
            // shell before it is read by anything else.
            b'$' if matches!(bytes.get(at + 1), Some(b'{')) => {
                let mut end = at + 2;
                while end < len && bytes[end] != b'}' {
                    end += char_step(line, end);
                }
                let end = if end < len { end + 1 } else { len };
                runs.push(TokenKind::Variable, at, end);
                at = end;
            }
            b'$' => {
                let end = word_end(bytes, at + 1);
                if end > at + 1 {
                    runs.push(TokenKind::Variable, at, end);
                    at = end;
                } else {
                    at += 1;
                }
            }
            b'0'..=b'9' if word_boundary(bytes, at) => {
                let end = number(line, at);
                runs.push(TokenKind::Number, at, end);
                at = end.max(at + 1);
            }
            _ if (byte.is_ascii_alphabetic() || byte == b'_') && word_boundary(bytes, at) => {
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
    fn all_three_comment_marks_work() {
        for line in ["# a", "; a", "! a", "   # a"] {
            assert!(
                lex(line)
                    .iter()
                    .any(|token| token.kind == TokenKind::Comment),
                "{line:?} was not a comment"
            );
        }
    }

    #[test]
    fn a_section_header_is_one_key() {
        assert_eq!(
            kinds("[server]", &lex("[server]")),
            [(TokenKind::Key, "[server]")]
        );
    }

    #[test]
    fn the_three_spellings_of_a_mapping() {
        for line in ["Port = 22", "Port: 22", "Port 22"] {
            assert_eq!(
                kinds(line, &lex(line))[0],
                (TokenKind::Key, "Port"),
                "{line:?}"
            );
            assert!(
                lex(line)
                    .iter()
                    .any(|token| token.kind == TokenKind::Number),
                "{line:?}"
            );
        }
    }

    #[test]
    fn a_bare_word_on_its_own_is_not_a_key() {
        // Nothing follows it, so nothing was mapped to anything.
        assert!(!lex("standalone").iter().any(|t| t.kind == TokenKind::Key));
    }

    #[test]
    fn an_exported_variable_keeps_its_key() {
        assert_eq!(
            kinds("export TOKEN=abc", &lex("export TOKEN=abc")),
            [
                (TokenKind::Keyword, "export"),
                (TokenKind::Plain, " "),
                (TokenKind::Key, "TOKEN"),
                (TokenKind::Plain, "=abc"),
            ]
        );
    }

    #[test]
    fn a_hash_inside_a_value_is_part_of_it() {
        // The password case, which is why the rule asks for whitespace.
        let line = "pass = a#b";
        assert!(!lex(line).iter().any(|t| t.kind == TokenKind::Comment));
        assert!(
            lex("pass = a # b")
                .iter()
                .any(|t| t.kind == TokenKind::Comment)
        );
    }

    #[test]
    fn a_value_can_be_quoted_expanded_or_a_literal() {
        let line = r#"url = "http://$HOST/${path}" # see below"#;
        let spans = kinds(line, &lex(line));
        assert_eq!(spans[0], (TokenKind::Key, "url"));
        assert!(spans.iter().any(|(kind, _)| *kind == TokenKind::String));
        assert!(spans.iter().any(|(kind, _)| *kind == TokenKind::Comment));
        assert!(
            lex("debug = true")
                .iter()
                .any(|t| t.kind == TokenKind::Literal)
        );
        assert!(
            lex("home = $HOME")
                .iter()
                .any(|t| t.kind == TokenKind::Variable)
        );
    }

    #[test]
    fn nothing_here_panics_on_anything() {
        for line in [
            "",
            "[",
            "]",
            "=",
            "$",
            "${",
            "\"",
            "키 = 값 # 주석",
            "🙂=🙂",
        ] {
            tiles(line, &lex_line(line));
        }
    }
}
