//! Shell: `sh`, `bash`, `zsh`, and the rc files they read.
//!
//! The two things that make a script hard to read without colour are quoting
//! and expansion, so those are what this is careful about: a `'` and a `"`
//! behave differently, an unterminated one runs on to the next line, and
//! `${...}` is not the same as the text around it. Everything else is a word
//! list.
//!
//! # What is given up
//!
//! * A `$VAR` inside a double-quoted string stays part of the string. Splitting
//!   the run would be easy and would make every quoted path in the file flicker
//!   between two colours; a string that reads as one thing is worth more.
//! * Only the first heredoc on a line is tracked. `cmd <<A <<B` is legal and
//!   nobody writes it.
//! * A backslash at the end of a line joins it to the next one, which this does
//!   not follow. It costs nothing: the next line is lexed from the start state,
//!   and outside a quote that is where a continued command is anyway.

use super::{
    Carry, Heredoc, LineState, Runs, Token, TokenKind, char_step, number, quote_body, skip_spaces,
    word_boundary, word_end,
};

/// The words that give a script its shape, and the builtins that do the work.
///
/// One table rather than two because they land in the same colour: the
/// distinction between `if` and `export` is real to a shell and invisible to
/// someone scanning a file for what it does. Sorted, so the lookup is a binary
/// search; a test holds the order.
const KEYWORDS: &[&str] = &[
    "alias", "break", "case", "cd", "continue", "declare", "do", "done", "echo", "elif", "else",
    "esac", "eval", "exec", "exit", "export", "fi", "for", "function", "if", "in", "local",
    "printf", "read", "readonly", "return", "select", "set", "shift", "source", "then", "time",
    "trap", "typeset", "umask", "unalias", "unset", "until", "wait", "while",
];

/// The words that are values rather than commands.
const LITERALS: &[&str] = &["false", "true"];

/// The tokens of one line of shell, and the state it leaves behind.
pub fn lex_line(line: &str, state: LineState) -> (Vec<Token>, LineState) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut runs = Runs::new();
    let mut at = 0;

    match state.0 {
        Carry::Heredoc(heredoc) => {
            if !heredoc.terminates(line) {
                runs.push(TokenKind::String, 0, len);
                return (runs.finish(len), state);
            }
            // The terminator is code again, and falls through to the loop.
        }
        Carry::Quote(quote) => match quote_body(line, 0, quote, quote == b'"') {
            Some(end) => {
                runs.push(TokenKind::String, 0, end);
                at = end;
            }
            None => {
                runs.push(TokenKind::String, 0, len);
                return (runs.finish(len), state);
            }
        },
        _ => {}
    }

    // Set when a `<<TAG` is seen, and handed to the next line at the end.
    let mut opened = None;

    while at < len {
        let byte = bytes[at];
        match byte {
            // A `#` only opens a comment where a word could start, so `x#y` is
            // one word and not a comment. The other place a `#` is ordinary —
            // inside a `${name#prefix}` — never reaches here, because the `$`
            // arm below has already swallowed the whole expansion.
            b'#' if word_boundary(bytes, at) => {
                runs.push(TokenKind::Comment, at, len);
                at = len;
            }
            b'\'' | b'"' => {
                // Single quotes take no escapes at all, which is the whole
                // reason a script uses them.
                match quote_body(line, at + 1, byte, byte == b'"') {
                    Some(end) => {
                        runs.push(TokenKind::String, at, end);
                        at = end;
                    }
                    None => {
                        runs.push(TokenKind::String, at, len);
                        return (runs.finish(len), LineState(Carry::Quote(byte)));
                    }
                }
            }
            b'$' => {
                let end = expansion(line, at);
                if end > at {
                    runs.push(TokenKind::Variable, at, end);
                    at = end;
                } else {
                    // `$(` and a bare `$`: step over it so that what is inside a
                    // command substitution is lexed as the code it is.
                    at += 1;
                }
            }
            // `<<<` is a here-string, and the whole operator has to be stepped
            // over at once: leaving the last two bytes to the arm below would
            // read them as the `<<` they are not.
            b'<' if bytes.get(at + 1) == Some(&b'<') && bytes.get(at + 2) == Some(&b'<') => {
                at += 3;
            }
            // `<<` opens a heredoc; a lone `<` is a redirect.
            b'<' if bytes.get(at + 1) == Some(&b'<') => match heredoc_tag(line, at + 2) {
                Some((start, end, heredoc)) => {
                    runs.push(TokenKind::String, start, end);
                    opened = Some(heredoc);
                    at = end;
                }
                None => at += 2,
            },
            b'0'..=b'9' if word_boundary(bytes, at) => {
                let end = number(line, at);
                runs.push(TokenKind::Number, at, end);
                at = end;
            }
            _ if (byte.is_ascii_alphabetic() || byte == b'_') && word_boundary(bytes, at) => {
                let end = word_end(bytes, at);
                let word = &line[at..end];
                if LITERALS.contains(&word) {
                    runs.push(TokenKind::Literal, at, end);
                } else if KEYWORDS.binary_search(&word).is_ok() {
                    runs.push(TokenKind::Keyword, at, end);
                } else if bytes.get(end) == Some(&b'=') {
                    // `PORT=22`, and `local x=1` after the keyword: the name
                    // being assigned reads as the key of a mapping, because
                    // that is what it is.
                    runs.push(TokenKind::Key, at, end);
                }
                at = end;
            }
            _ => at += char_step(line, at),
        }
    }

    let state = opened.map_or(LineState::START, |heredoc| {
        LineState(Carry::Heredoc(heredoc))
    });
    (runs.finish(len), state)
}

/// The end of the expansion whose `$` is at `at`, or `at` when there is none.
fn expansion(line: &str, at: usize) -> usize {
    let bytes = line.as_bytes();
    let Some(byte) = bytes.get(at + 1) else {
        return at;
    };
    match byte {
        b'{' => {
            // To the closing brace, or to the end of the line: a `${` that never
            // closes is a broken script, and colouring the rest of the line as
            // the expansion it was meant to be says so more usefully than
            // colouring nothing.
            let mut end = at + 2;
            while end < bytes.len() && bytes[end] != b'}' {
                end += char_step(line, end);
            }
            if end < bytes.len() {
                end + 1
            } else {
                bytes.len()
            }
        }
        // `$(...)` is a command substitution: the caller steps over the `$` so
        // that what is inside is lexed as code.
        b'(' => at,
        // The positional and special parameters, each exactly one byte.
        b'?' | b'!' | b'#' | b'$' | b'@' | b'*' | b'-' | b'0'..=b'9' => at + 2,
        byte if byte.is_ascii_alphabetic() || *byte == b'_' => word_end(bytes, at + 1),
        _ => at,
    }
}

/// The tag of a heredoc introduced at `at`, which is just past the `<<`.
///
/// Answers the span to colour — the tag with its quotes, if it has any — and
/// the state to carry. `None` when what follows is not a tag, which is what
/// `x << 2` looks like.
fn heredoc_tag(line: &str, at: usize) -> Option<(usize, usize, Heredoc)> {
    let bytes = line.as_bytes();
    let mut at = at;
    let dash = bytes.get(at) == Some(&b'-');
    if dash {
        at += 1;
    }
    let start = skip_spaces(bytes, at);
    match bytes.get(start) {
        // `<<'EOF'` and `<<"EOF"` turn expansion off inside the body, which
        // this does not colour differently; the quotes are part of the span
        // either way.
        Some(quote @ (b'\'' | b'"')) => {
            let end = quote_body(line, start + 1, *quote, false)?;
            let tag = line.get(start + 1..end - 1)?;
            Some((start, end, Heredoc::new(tag, dash)?))
        }
        Some(byte) if byte.is_ascii_alphabetic() || *byte == b'_' => {
            let end = word_end(bytes, start);
            Some((start, end, Heredoc::new(&line[start..end], dash)?))
        }
        _ => None,
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
    fn the_keyword_table_is_sorted() {
        assert!(KEYWORDS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_comment_runs_to_the_end_of_the_line() {
        assert_eq!(
            kinds("echo hi # and then", &lex("echo hi # and then")),
            [
                (TokenKind::Keyword, "echo"),
                (TokenKind::Plain, " hi "),
                (TokenKind::Comment, "# and then"),
            ]
        );
    }

    #[test]
    fn a_hash_inside_a_word_is_not_a_comment() {
        // The two places a `#` is ordinary: mid-word, and inside a `${}`.
        assert!(!lex("id=a#b").iter().any(|t| t.kind == TokenKind::Comment));
        assert!(
            !lex("echo ${name#prefix}")
                .iter()
                .any(|t| t.kind == TokenKind::Comment)
        );
    }

    #[test]
    fn the_two_quotes_behave_differently() {
        // A backslash escapes inside `"` and is a plain byte inside `'`.
        let line = r#"echo "a\"b" 'c\'"#;
        let tokens = lex(line);
        let strings: Vec<_> = tokens
            .iter()
            .filter(|token| token.kind == TokenKind::String)
            .map(|token| &line[token.start..token.end])
            .collect();
        assert_eq!(strings, [r#""a\"b""#, r"'c\'"]);
    }

    #[test]
    fn an_open_quote_carries_to_the_next_line() {
        let (_, after) = lex_line("echo \"one", LineState::START);
        assert!(!after.is_start());
        let (tokens, closed) = lex_line("two\" done", after);
        assert_eq!(tokens[0].kind, TokenKind::String);
        assert_eq!(tokens[0].end, 4, "the string ends with the closing quote");
        assert!(closed.is_start());
    }

    #[test]
    fn expansions_come_out_whole() {
        let line = "cp $src ${dst:-/tmp} $1 $? $HOME/x";
        let tokens = lex(line);
        let variables: Vec<_> = tokens
            .iter()
            .filter(|token| token.kind == TokenKind::Variable)
            .map(|token| &line[token.start..token.end])
            .collect();
        assert_eq!(variables, ["$src", "${dst:-/tmp}", "$1", "$?", "$HOME"]);
    }

    #[test]
    fn a_command_substitution_is_lexed_as_code() {
        // The point of not swallowing `$(`: the `echo` inside is still a word.
        let line = "x=$(echo hi)";
        assert!(lex(line).iter().any(
            |token| token.kind == TokenKind::Keyword && &line[token.start..token.end] == "echo"
        ));
    }

    #[test]
    fn an_unclosed_brace_takes_the_rest_of_the_line() {
        let line = "echo ${broken";
        let tokens = lex(line);
        let last = tokens.last().expect("a line with text has tokens");
        assert_eq!((last.kind, last.end), (TokenKind::Variable, line.len()));
    }

    #[test]
    fn an_assignment_names_a_key() {
        assert_eq!(
            kinds("PORT=22", &lex("PORT=22")),
            [
                (TokenKind::Key, "PORT"),
                (TokenKind::Plain, "="),
                (TokenKind::Number, "22"),
            ]
        );
    }

    #[test]
    fn a_heredoc_body_is_a_string_until_its_tag() {
        let (_, after) = lex_line("cat <<EOF", LineState::START);
        assert!(!after.is_start());

        let (body, still) = lex_line("  anything at all # not a comment", after);
        assert_eq!(body[0].kind, TokenKind::String);
        assert_eq!(still, after, "the body does not change the state");

        let (_, closed) = lex_line("EOF", after);
        assert!(closed.is_start());
    }

    #[test]
    fn a_dash_heredoc_forgives_an_indented_terminator() {
        let (_, after) = lex_line("cat <<-'END'", LineState::START);
        assert!(lex_line("\t\tEND", after).1.is_start());
        // And a plain one does not.
        let (_, strict) = lex_line("cat <<END", LineState::START);
        assert!(!lex_line("\t\tEND", strict).1.is_start());
    }

    #[test]
    fn a_here_string_and_a_shift_are_not_heredocs() {
        assert!(lex_line("cat <<<\"$x\"", LineState::START).1.is_start());
        assert!(lex_line("n=$(( 1 << 2 ))", LineState::START).1.is_start());
    }

    #[test]
    fn a_tag_too_long_to_carry_is_not_tracked() {
        // Documented behaviour rather than a silent truncation: the body is
        // lexed as shell, which is wrong in colour and in nothing else.
        let line = "cat <<A_TAG_NOBODY_WOULD_EVER_WRITE";
        assert!(lex_line(line, LineState::START).1.is_start());
    }

    #[test]
    fn nothing_here_panics_on_anything() {
        for line in [
            "",
            "\"",
            "'",
            "$",
            "${",
            "<<",
            "<<-",
            "<<''",
            "\\",
            "한글 $변수 \"열린",
            "🙂 <<🙂",
        ] {
            let (tokens, _) = lex_line(line, LineState::START);
            tiles(line, &tokens);
        }
    }
}
