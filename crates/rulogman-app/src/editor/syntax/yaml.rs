//! YAML, as far as one line at a time can see it.
//!
//! YAML is context-sensitive in ways a line scanner cannot follow, so this
//! follows the two things that carry the meaning on screen: the key of a
//! mapping, and where a block scalar's body begins and ends. Everything else —
//! anchors, tags, flow collections — is scanned the way the shell lexer scans a
//! command line, one interesting thing at a time.
//!
//! # What is given up
//!
//! * A block scalar's body is "every line indented further than the line that
//!   opened it, plus the blank lines between them". The specification says the
//!   indentation is fixed by the scalar's *first* line and may be given
//!   explicitly by a digit; the simplification differs only for a body whose
//!   first line is indented *less* than its introducer, which cannot happen in
//!   a valid document.
//! * A quoted scalar that spans lines is not carried over. Multi-line flow
//!   scalars are rare, and carrying them would mean guessing at the folding
//!   rules to know where they end.
//! * `- ` and `:` are structure, not tokens: colouring them is what makes a
//!   YAML file look like a punctuation exercise.

use super::{
    Carry, LineState, Runs, Token, TokenKind, char_step, indent_of, number, quote_body,
    skip_spaces, word_boundary, word_end,
};

/// The scalars that are values rather than words.
///
/// The YAML 1.1 set, which is what most readers still implement: `yes`, `no`,
/// `on` and `off` are booleans there, and a file that relies on it reads
/// better when they are coloured as the booleans they will become.
const LITERALS: &[&str] = &[
    "FALSE", "False", "NO", "NULL", "Null", "OFF", "ON", "TRUE", "True", "YES", "false", "no",
    "null", "off", "on", "true", "yes",
];

/// The tokens of one line of YAML, and the state it leaves behind.
pub fn lex_line(line: &str, state: LineState) -> (Vec<Token>, LineState) {
    let bytes = line.as_bytes();
    let len = bytes.len();

    if let Carry::BlockScalar(introduced_at) = state.0 {
        // A blank line inside a block scalar belongs to it however far it is
        // indented, which is to say not at all.
        if line.trim().is_empty() || indent_of(line) > usize::from(introduced_at) {
            let mut runs = Runs::new();
            runs.push(TokenKind::String, 0, len);
            return (runs.finish(len), state);
        }
        // Otherwise the scalar ended, and this line is a document line again.
    }

    let mut runs = Runs::new();
    let indent = indent_of(line);
    let mut at = indent;

    // The document markers, which are the whole line when they are there.
    let trimmed = line.trim_end();
    if trimmed == "---" || trimmed == "..." {
        runs.push(TokenKind::Keyword, at, trimmed.len());
        return (runs.finish(len), LineState::START);
    }

    if bytes.get(at) == Some(&b'#') {
        runs.push(TokenKind::Comment, at, len);
        return (runs.finish(len), LineState::START);
    }

    // Sequence indicators, however many of them are nested on this line.
    while bytes.get(at) == Some(&b'-') && matches!(bytes.get(at + 1), None | Some(b' ')) {
        at = skip_spaces(bytes, at + 1);
    }

    if let Some(colon) = key_end(line, at) {
        runs.push(TokenKind::Key, at, colon);
        at = skip_spaces(bytes, colon + 1);
    }

    // A block scalar is introduced where a value would go: after the colon of a
    // mapping entry, or after the dash of a sequence one.
    if let Some(end) = block_scalar(line, at) {
        runs.push(TokenKind::Keyword, at, end);
        let rest = skip_spaces(bytes, end);
        if bytes.get(rest) == Some(&b'#') {
            runs.push(TokenKind::Comment, rest, len);
        }
        let introduced_at = u16::try_from(indent).unwrap_or(u16::MAX);
        return (
            runs.finish(len),
            LineState(Carry::BlockScalar(introduced_at)),
        );
    }

    while at < len {
        let byte = bytes[at];
        match byte {
            // YAML wants whitespace before an inline `#`, which is what stops a
            // URL's fragment from turning the rest of the line grey.
            b'#' if at == 0 || matches!(bytes.get(at - 1), Some(b' ' | b'\t')) => {
                runs.push(TokenKind::Comment, at, len);
                at = len;
            }
            b'\'' | b'"' => match quote_body(line, at + 1, byte, byte == b'"') {
                Some(end) => {
                    runs.push(TokenKind::String, at, end);
                    at = end;
                }
                None => {
                    // A flow scalar left open is not carried to the next line;
                    // see the module documentation.
                    runs.push(TokenKind::String, at, len);
                    at = len;
                }
            },
            // An anchor, an alias, or a tag: all three name something elsewhere
            // in the document, which is what the variable colour is for.
            b'&' | b'*' | b'!' if word_boundary(bytes, at) => {
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
                at = end;
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

    (runs.finish(len), LineState::START)
}

/// The offset of the `:` that ends the key starting at `at`, if this line is a
/// mapping entry.
///
/// A key ends at a `:` that is followed by a space or by the end of the line —
/// which is what keeps a `http://host` in a value from being read as one — and
/// a `#` comment or a quote that runs to the end of the line says there is no
/// key here at all.
fn key_end(line: &str, at: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut scan = at;
    while scan < bytes.len() {
        match bytes[scan] {
            b'#' if scan > at && matches!(bytes.get(scan - 1), Some(b' ' | b'\t')) => return None,
            quote @ (b'\'' | b'"') => scan = quote_body(line, scan + 1, quote, quote == b'"')?,
            b':' if matches!(bytes.get(scan + 1), None | Some(b' ' | b'\t')) => {
                return (scan > at).then_some(scan);
            }
            _ => scan += char_step(line, scan),
        }
    }
    None
}

/// The end of a block scalar header — `|`, `>`, and the chomping and indentation
/// indicators after it — when `at` is one, and nothing when it is not.
///
/// The header has to be the last thing on the line apart from a comment;
/// anything else means the `|` was a plain scalar that happens to start with a
/// pipe.
fn block_scalar(line: &str, at: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    if !matches!(bytes.get(at), Some(b'|' | b'>')) {
        return None;
    }
    let mut end = at + 1;
    while matches!(bytes.get(end), Some(byte) if *byte == b'+' || *byte == b'-' || byte.is_ascii_digit())
    {
        end += 1;
    }
    let rest = skip_spaces(bytes, end);
    if rest >= bytes.len() || bytes[rest] == b'#' {
        Some(end)
    } else {
        None
    }
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
    fn the_literal_table_is_sorted() {
        assert!(LITERALS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_mapping_entry_splits_into_key_and_value() {
        assert_eq!(
            kinds("  port: 22", &lex("  port: 22")),
            [
                (TokenKind::Plain, "  "),
                (TokenKind::Key, "port"),
                (TokenKind::Plain, ": "),
                (TokenKind::Number, "22"),
            ]
        );
    }

    #[test]
    fn a_sequence_entry_still_has_a_key() {
        assert_eq!(
            kinds("  - name: web", &lex("  - name: web")),
            [
                (TokenKind::Plain, "  - "),
                (TokenKind::Key, "name"),
                (TokenKind::Plain, ": web"),
            ]
        );
    }

    #[test]
    fn a_colon_inside_a_value_does_not_end_a_key() {
        // The regression this rule exists for: `url: http://host/x` has one key
        // and not two.
        let line = "url: http://host/x";
        let keys: Vec<_> = lex(line)
            .into_iter()
            .filter(|token| token.kind == TokenKind::Key)
            .map(|token| line[token.start..token.end].to_owned())
            .collect();
        assert_eq!(keys, ["url"]);
    }

    #[test]
    fn a_quoted_key_keeps_its_quotes() {
        assert_eq!(
            kinds(r#""a: b": c"#, &lex(r#""a: b": c"#)),
            [(TokenKind::Key, r#""a: b""#), (TokenKind::Plain, ": c")]
        );
    }

    #[test]
    fn booleans_and_nulls_are_literals() {
        let line = "enabled: true";
        assert!(
            lex(line)
                .iter()
                .any(|token| token.kind == TokenKind::Literal)
        );
        assert!(lex("x: null").iter().any(|t| t.kind == TokenKind::Literal));
        assert!(lex("x: yes").iter().any(|t| t.kind == TokenKind::Literal));
        // A word that merely contains one is not one.
        assert!(
            !lex("x: trueish")
                .iter()
                .any(|t| t.kind == TokenKind::Literal)
        );
    }

    #[test]
    fn a_comment_needs_whitespace_before_it() {
        assert!(
            lex("a: b # why")
                .iter()
                .any(|t| t.kind == TokenKind::Comment)
        );
        assert!(!lex("a: b#c").iter().any(|t| t.kind == TokenKind::Comment));
        assert_eq!(lex("# whole line")[0].kind, TokenKind::Comment);
    }

    #[test]
    fn a_block_scalar_takes_everything_indented_under_it() {
        let (tokens, after) = lex_line("  script: |", LineState::START);
        assert_eq!(tokens[1].kind, TokenKind::Key);
        assert_eq!(after, LineState(Carry::BlockScalar(2)));

        let (body, still) = lex_line("    echo hi: not a key", after);
        assert_eq!(body[0].kind, TokenKind::String);
        assert_eq!(still, after);

        // A blank line is part of the body.
        assert_eq!(lex_line("", after).1, after);

        // And a line back at the introducer's indentation closes it.
        let (next, closed) = lex_line("  other: 1", after);
        assert!(closed.is_start());
        assert_eq!(next[1].kind, TokenKind::Key);
    }

    #[test]
    fn a_chomping_indicator_is_part_of_the_header() {
        assert_eq!(
            lex_line("a: >-", LineState::START).1,
            LineState(Carry::BlockScalar(0))
        );
        // A pipe with something after it is a scalar that starts with a pipe.
        assert!(lex_line("a: | b", LineState::START).1.is_start());
    }

    #[test]
    fn anchors_and_aliases_point_somewhere() {
        let line = "base: &defaults";
        assert!(lex(line).iter().any(|t| t.kind == TokenKind::Variable));
        assert!(
            lex("x: *defaults")
                .iter()
                .any(|t| t.kind == TokenKind::Variable)
        );
    }

    #[test]
    fn document_markers_stand_alone() {
        assert_eq!(lex("---")[0].kind, TokenKind::Keyword);
        assert_eq!(lex("...")[0].kind, TokenKind::Keyword);
    }

    #[test]
    fn nothing_here_panics_on_anything() {
        for line in [
            "",
            ":",
            "-",
            "- ",
            "&",
            "*",
            "|",
            "'",
            "\"",
            "한글: 값 # 주석",
            "🙂: 🙂",
        ] {
            let (tokens, _) = lex_line(line, LineState::START);
            tiles(line, &tokens);
        }
    }
}
