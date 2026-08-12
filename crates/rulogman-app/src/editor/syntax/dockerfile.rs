//! Dockerfiles, which are one instruction per line until a `\` says otherwise.
//!
//! The instruction is the whole of the structure — everything else on the line
//! is an argument to it — so that is what this finds first, and it only looks
//! for one where a line can start. A line the previous one ended with a `\` is
//! a continuation, and its first word is an argument like any other; the
//! [`Carry::Continued`] state is what tells them apart.
//!
//! # What is given up
//!
//! The body of a `RUN` is shell and is not lexed as shell. Handing the rest of
//! the line to the shell lexer would mean carrying its quote and heredoc states
//! through this one, for a file whose shell fragments are usually a single
//! command; what is here instead is the part of shell that a Dockerfile
//! actually leans on — quoting and `$` expansion — inlined.

use super::{
    Carry, LineState, Runs, Token, TokenKind, char_step, number, quote_body, skip_spaces,
    word_boundary, word_end,
};

/// Everything the builder accepts at the head of a line.
///
/// Compared case-insensitively — a lower-case `from` is legal — but written the
/// way a Dockerfile writes it. Short enough that a scan beats a binary search
/// and, more to the point, beats upper-casing the word to look it up.
const INSTRUCTIONS: &[&str] = &[
    "ADD",
    "ARG",
    "CMD",
    "COPY",
    "ENTRYPOINT",
    "ENV",
    "EXPOSE",
    "FROM",
    "HEALTHCHECK",
    "LABEL",
    "MAINTAINER",
    "ONBUILD",
    "RUN",
    "SHELL",
    "STOPSIGNAL",
    "USER",
    "VOLUME",
    "WORKDIR",
];

/// The words that mean something inside an instruction rather than at the head
/// of one: the `AS` of a named build stage, and `ONBUILD`'s own argument form.
const MODIFIERS: &[&str] = &["AS"];

/// The tokens of one line of a Dockerfile, and the state it leaves behind.
pub fn lex_line(line: &str, state: LineState) -> (Vec<Token>, LineState) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut runs = Runs::new();
    let head = skip_spaces(bytes, 0);

    // A `# syntax=` directive is a comment to everything except the builder,
    // and colouring it as one says the right thing about what it does to the
    // image: nothing.
    if bytes.get(head) == Some(&b'#') {
        runs.push(TokenKind::Comment, head, len);
        return (runs.finish(len), LineState::START);
    }

    let mut at = head;
    if !matches!(state.0, Carry::Continued) {
        let end = word_end(bytes, at);
        if end > at
            && INSTRUCTIONS
                .iter()
                .any(|word| word.eq_ignore_ascii_case(&line[at..end]))
        {
            runs.push(TokenKind::Keyword, at, end);
            at = end;
        }
    }

    while at < len {
        let byte = bytes[at];
        match byte {
            b'"' | b'\'' => {
                let end = quote_body(line, at + 1, byte, byte == b'"').unwrap_or(len);
                runs.push(TokenKind::String, at, end);
                at = end;
            }
            b'$' => {
                let end = if bytes.get(at + 1) == Some(&b'{') {
                    let mut scan = at + 2;
                    while scan < len && bytes[scan] != b'}' {
                        scan += char_step(line, scan);
                    }
                    if scan < len { scan + 1 } else { len }
                } else {
                    word_end(bytes, at + 1)
                };
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
                let word = &line[at..end];
                if MODIFIERS
                    .iter()
                    .any(|other| other.eq_ignore_ascii_case(word))
                {
                    runs.push(TokenKind::Keyword, at, end);
                } else if bytes.get(end) == Some(&b'=') {
                    // `ENV k=v`, `ARG k=v`, `LABEL k=v`: the name being bound
                    // is the key of a mapping wherever it appears.
                    runs.push(TokenKind::Key, at, end);
                }
                at = end;
            }
            _ => at += char_step(line, at),
        }
    }

    // A trailing `\` joins this line to the next, so the next one must not go
    // looking for an instruction at its head.
    let state = if line.trim_end().ends_with('\\') {
        LineState(Carry::Continued)
    } else {
        LineState::START
    };
    (runs.finish(len), state)
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
    fn every_instruction_is_recognised_in_either_case() {
        for instruction in INSTRUCTIONS {
            let line = format!("{instruction} x");
            assert_eq!(lex(&line)[0].kind, TokenKind::Keyword, "{instruction}");
            let lowered = format!("{} x", instruction.to_ascii_lowercase());
            assert_eq!(lex(&lowered)[0].kind, TokenKind::Keyword, "{lowered}");
        }
        // Kept in alphabetical order so that a reader can find one.
        assert!(INSTRUCTIONS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn an_instruction_leads_the_line() {
        assert_eq!(
            kinds("FROM debian:12 AS build", &lex("FROM debian:12 AS build")),
            [
                (TokenKind::Keyword, "FROM"),
                (TokenKind::Plain, " debian:"),
                (TokenKind::Number, "12"),
                (TokenKind::Plain, " "),
                (TokenKind::Keyword, "AS"),
                (TokenKind::Plain, " build"),
            ]
        );
    }

    #[test]
    fn a_lower_case_instruction_is_still_one() {
        assert_eq!(lex("from debian")[0].kind, TokenKind::Keyword);
    }

    #[test]
    fn a_word_that_only_looks_like_an_instruction_is_not_one() {
        assert!(
            !lex("  RUNNER x")
                .iter()
                .any(|t| t.kind == TokenKind::Keyword)
        );
    }

    #[test]
    fn a_continuation_has_no_instruction_of_its_own() {
        let (_, after) = lex_line("RUN apt-get update \\", LineState::START);
        assert_eq!(after, LineState(Carry::Continued));

        // `RUN` here is an argument to the line above, not a new instruction.
        let (tokens, closed) = lex_line("  && run_it", after);
        assert!(!tokens.iter().any(|token| token.kind == TokenKind::Keyword));
        assert!(closed.is_start());
    }

    #[test]
    fn a_binding_names_a_key() {
        assert_eq!(
            kinds("ENV PATH=/usr/bin", &lex("ENV PATH=/usr/bin")),
            [
                (TokenKind::Keyword, "ENV"),
                (TokenKind::Plain, " "),
                (TokenKind::Key, "PATH"),
                (TokenKind::Plain, "=/usr/bin"),
            ]
        );
    }

    #[test]
    fn expansions_and_strings_survive() {
        let line = r#"RUN echo "$HOME" ${TARGET:-x}"#;
        let tokens = lex(line);
        assert!(tokens.iter().any(|token| token.kind == TokenKind::String));
        assert_eq!(
            tokens
                .iter()
                .filter(|token| token.kind == TokenKind::Variable)
                .map(|token| &line[token.start..token.end])
                .collect::<Vec<_>>(),
            ["${TARGET:-x}"],
            "the one inside the quotes stays part of the string"
        );
    }

    #[test]
    fn a_directive_is_a_comment() {
        assert_eq!(
            lex("# syntax=docker/dockerfile:1")[0].kind,
            TokenKind::Comment
        );
        // And a comment does not continue, whatever it ends with.
        assert!(lex_line("# a \\", LineState::START).1.is_start());
    }

    #[test]
    fn nothing_here_panics_on_anything() {
        for line in ["", "\\", "$", "${", "\"", "RUN", "ENV 키=값", "🙂"] {
            let (tokens, _) = lex_line(line, LineState::START);
            tiles(line, &tokens);
        }
    }
}
