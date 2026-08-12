//! Markdown, coloured the way a reader skims it rather than the way CommonMark
//! parses it.
//!
//! Markdown is the one format here whose *content* is prose, so the job is not
//! to tell code from comments but to make the structure findable: where a
//! section starts, where a code block starts and stops, what is a list and what
//! is quoted from somewhere else. That is a shape a line scanner can see almost
//! all of, because Markdown's block structure is written at the head of a line
//! on purpose — `#`, `>`, `-`, a fence — and only the inline spans need looking
//! along the line for.
//!
//! # What each thing is coloured as
//!
//! The eight [`TokenKind`]s are shared with six config formats, so the mapping
//! is by role rather than by name:
//!
//! * A heading is [`TokenKind::Key`] — the left-hand-side colour. A heading is a
//!   section header, which is exactly what `[section]` is in a `.conf`, and it
//!   takes the whole line because a heading *is* the line.
//! * A fenced code block's body is [`TokenKind::String`]: it is literal text
//!   that the document is quoting rather than saying, which is what a string is
//!   in every other language here. The fence lines themselves are
//!   [`TokenKind::Comment`], being markup about the block rather than part of
//!   it — including the info string, which is spelled the way a comment is read.
//! * A blockquote is [`TokenKind::Comment`], the quiet colour: it is context
//!   carried in from elsewhere, and it should recede the way a comment does.
//! * A horizontal rule is [`TokenKind::Comment`] for the same reason — it is a
//!   mark on the page with no text of its own.
//! * A list marker is [`TokenKind::Keyword`] and *only* the marker, so that the
//!   item's text stays prose and the column of bullets stands out down the left.
//! * Inline code is [`TokenKind::String`], to agree with the fenced kind.
//! * Strong is [`TokenKind::Keyword`] and emphasis is [`TokenKind::Literal`]:
//!   two weights of "this word matters more than its neighbours", the stronger
//!   colour on the stronger markup.
//! * A link's text is [`TokenKind::Variable`] — it names something that lives
//!   elsewhere, which is what the variable colour means everywhere else here —
//!   and its target is [`TokenKind::String`], the target being a literal.
//! * An HTML comment is [`TokenKind::Comment`], which needs no argument.
//!
//! # What is given up
//!
//! * **Indented code blocks.** Four spaces at the head of a line means code
//!   only when nothing else is open, and a line scanner cannot tell that from
//!   the second paragraph of a list item, which is indented exactly as far.
//!   Colouring both would put half the lists in a document in the string colour;
//!   colouring neither is wrong only where fences are not used, and fences are
//!   what people write.
//! * **YAML front matter.** A `---` on the first line of a file opens it, and
//!   the first line is the one thing this lexer cannot recognise: it is handed
//!   [`LineState::START`], and so is every line after a blank one. Front matter
//!   is therefore not expressible without a state the framework does not have,
//!   and the opening `---` reads as the horizontal rule it also is.
//! * **Setext headings** — a line underlined with `===` or `---` — for the same
//!   reason in reverse: the underline is seen a line too late to recolour the
//!   text above it, and the underline itself already reads as a rule.
//! * **Inline spans do not cross lines.** An emphasis or a code span left open
//!   at the end of a line stays plain rather than swallowing the paragraph, and
//!   an HTML comment left open colours its first line only. A fence is the one
//!   inline-looking thing that carries, and it carries because it is a block.
//! * **Double-backtick code spans** are read as two empty spans around plain
//!   text. `` `` `` is written to put a backtick *inside* code, which is rare
//!   enough not to be worth a second scanner.
//! * **Reference links** — `[text][label]` and the `[label]: url` line that
//!   defines them — are not coloured; only the inline `[text](url)` form is.
//!   The label form resolves against the whole document, and a line scanner
//!   would be guessing.
//! * A heading's line is one run: inline spans inside it are not looked for,
//!   because a heading already has the colour that says what it is.

use super::{
    Carry, LineState, Runs, Token, TokenKind, char_step, indent_of, skip_spaces, word_boundary,
};

/// The tokens of one line of Markdown, and the state it leaves behind.
pub fn lex_line(line: &str, state: LineState) -> (Vec<Token>, LineState) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut runs = Runs::new();

    // Inside a fenced block nothing else is markup, which is the whole point of
    // one: the body is what the author wanted shown verbatim.
    if let Carry::Fenced(fence) = state.0 {
        if closes_fence(bytes, fence) {
            runs.push(TokenKind::Comment, indent_of(line), len);
            return (runs.finish(len), LineState::START);
        }
        runs.push(TokenKind::String, 0, len);
        return (runs.finish(len), state);
    }

    let at = indent_of(line);

    // A fence first, because everything after this point would read the block's
    // first line as prose.
    for fence in *b"`~" {
        if fence_run(bytes, fence).is_some() {
            runs.push(TokenKind::Comment, at, len);
            return (runs.finish(len), LineState(Carry::Fenced(fence)));
        }
    }

    if heading(bytes, at) {
        runs.push(TokenKind::Key, at, len);
        return (runs.finish(len), LineState::START);
    }

    // The rule is tried before the list marker, because `* * *` and `- - -` are
    // both, and what they mean is the rule.
    if horizontal_rule(line) || bytes.get(at) == Some(&b'>') {
        runs.push(TokenKind::Comment, at, len);
        return (runs.finish(len), LineState::START);
    }

    let mut from = at;
    if let Some(end) = list_marker(bytes, at) {
        runs.push(TokenKind::Keyword, at, end);
        from = end;
    }

    inline(&mut runs, line, from);
    (runs.finish(len), LineState::START)
}

/// The end of a run of three or more `fence` bytes at the head of `line`, past
/// whatever indentation it has.
///
/// Three is the minimum a fence may be and there is no maximum, so this counts
/// rather than compares. Which character opened the block is remembered in
/// [`Carry::Fenced`] so that a ``` inside a `~~~` block stays code.
fn fence_run(bytes: &[u8], fence: u8) -> Option<usize> {
    let at = skip_spaces(bytes, 0);
    let mut end = at;
    while bytes.get(end) == Some(&fence) {
        end += 1;
    }
    (end - at >= 3).then_some(end)
}

/// Whether `bytes` is the line that closes a `fence` block.
///
/// A closing fence is a run of the opening character with nothing after it. The
/// specification also asks that it be no shorter than the opening run; that is
/// not tracked, because the case it separates — a four-backtick block closed by
/// three — is vanishingly rare beside the case it costs, which is remembering a
/// length in a state that has to stay `Copy` and small.
fn closes_fence(bytes: &[u8], fence: u8) -> bool {
    fence_run(bytes, fence).is_some_and(|end| skip_spaces(bytes, end) >= bytes.len())
}

/// Whether `line` is a horizontal rule: three or more of `-`, `*` or `_`, alone
/// on the line apart from the spaces that may be sprinkled between them.
fn horizontal_rule(line: &str) -> bool {
    let mut marks = line
        .bytes()
        .filter(|byte| !matches!(byte, b' ' | b'\t' | b'\r'));
    let Some(first) = marks.next() else {
        return false;
    };
    if !matches!(first, b'-' | b'*' | b'_') {
        return false;
    }
    let mut count = 1;
    for byte in marks {
        if byte != first {
            return false;
        }
        count += 1;
    }
    count >= 3
}

/// Whether an ATX heading starts at `at`.
///
/// One to six `#`, and then a space or the end of the line. The space is what
/// keeps a `#tag` in prose — and a shell comment in a file somebody mis-set the
/// language of — from turning into a heading.
fn heading(bytes: &[u8], at: usize) -> bool {
    let mut end = at;
    while bytes.get(end) == Some(&b'#') {
        end += 1;
    }
    (1..=6).contains(&(end - at)) && matches!(bytes.get(end), None | Some(b' ' | b'\t'))
}

/// The end of the list marker at `at`, when there is one.
///
/// A bullet is `-`, `*` or `+`; an ordered marker is digits and then `.` or `)`.
/// Either way a space must follow, which is what stops a `-1` in prose and a
/// `1.5` at the head of a line from becoming bullets.
fn list_marker(bytes: &[u8], at: usize) -> Option<usize> {
    match bytes.get(at)? {
        b'-' | b'*' | b'+' => {
            matches!(bytes.get(at + 1), None | Some(b' ' | b'\t')).then_some(at + 1)
        }
        byte if byte.is_ascii_digit() => {
            let mut end = at;
            while matches!(bytes.get(end), Some(digit) if digit.is_ascii_digit()) {
                end += 1;
            }
            if !matches!(bytes.get(end), Some(b'.' | b')')) {
                return None;
            }
            end += 1;
            matches!(bytes.get(end), None | Some(b' ' | b'\t')).then_some(end)
        }
        _ => None,
    }
}

/// Scans the prose of a line for the spans that are not prose.
///
/// Everything here closes on the same line or does not count, so the loop can
/// look forward freely and fall back to stepping over one character. All four
/// delimiters are ASCII, so an index found by comparing bytes is always a
/// character boundary.
fn inline(runs: &mut Runs, line: &str, from: usize) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut at = from;

    while at < len {
        let byte = bytes[at];
        match byte {
            b'`' => match find(bytes, at + 1, b'`') {
                Some(end) => {
                    runs.push(TokenKind::String, at, end + 1);
                    at = end + 1;
                }
                None => at += 1,
            },
            b'*' | b'_' => {
                let width = if bytes.get(at + 1) == Some(&byte) {
                    2
                } else {
                    1
                };
                match emphasis(bytes, at, byte, width) {
                    Some(end) => {
                        // Two markers, two weights: `**` is the louder of them
                        // and gets the louder colour.
                        let kind = if width == 2 {
                            TokenKind::Keyword
                        } else {
                            TokenKind::Literal
                        };
                        runs.push(kind, at, end);
                        at = end;
                    }
                    None => at += 1,
                }
            }
            b'[' => match link(bytes, at) {
                Some((text, target)) => {
                    runs.push(TokenKind::Variable, at, text);
                    runs.push(TokenKind::String, text, target);
                    at = target;
                }
                None => at += 1,
            },
            b'<' if bytes[at..].starts_with(b"<!--") => {
                // Unclosed, it colours this line and no more; see the module
                // documentation for why nothing is carried.
                let end = html_comment_end(bytes, at + 4).unwrap_or(len);
                runs.push(TokenKind::Comment, at, end);
                at = end;
            }
            _ => at += char_step(line, at),
        }
    }
}

/// The end of the emphasis span opened by `width` copies of `byte` at `at`, when
/// there is one on this line.
///
/// Two of the flanking rules are worth keeping because each of them stops a
/// common false positive: a marker followed by a space is not an opener, so
/// arithmetic and a bullet in the middle of a sentence stay plain; and an `_`
/// inside a word is not an opener, so `snake_case_names` do not go italic from
/// the middle. The rest of the flanking rules are dropped — they decide cases
/// that are ambiguous to a person reading the source too.
fn emphasis(bytes: &[u8], at: usize, byte: u8, width: usize) -> Option<usize> {
    if !matches!(bytes.get(at + width), Some(next) if !matches!(next, b' ' | b'\t')) {
        return None;
    }
    if byte == b'_' && !word_boundary(bytes, at) {
        return None;
    }
    let mut scan = at + width;
    while scan < bytes.len() {
        if bytes[scan] == byte {
            if width == 1 {
                return Some(scan + 1);
            }
            if bytes.get(scan + 1) == Some(&byte) {
                return Some(scan + 2);
            }
        }
        scan += 1;
    }
    None
}

/// Where the text and the target of an inline link starting at `at` end, when
/// `at` opens one.
///
/// The `]` must be followed immediately by a `(`, which is what tells a link
/// from the `[label]` of a reference and from a `[WARN]` in a pasted log. The
/// target ends at the first `)`, so a URL containing one is cut short — the
/// escape that would fix it is rarer than the URLs it would break.
fn link(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let close = find(bytes, at + 1, b']')?;
    if bytes.get(close + 1) != Some(&b'(') {
        return None;
    }
    let paren = find(bytes, close + 2, b')')?;
    Some((close + 1, paren + 1))
}

/// The end of an HTML comment whose body starts at `from`, past its `-->`.
fn html_comment_end(bytes: &[u8], from: usize) -> Option<usize> {
    let last = bytes.len().checked_sub(2)?;
    (from..last)
        .find(|at| &bytes[*at..*at + 3] == b"-->")
        .map(|at| at + 3)
}

/// The offset of the first `byte` at or after `from`.
fn find(bytes: &[u8], from: usize, byte: u8) -> Option<usize> {
    (from..bytes.len()).find(|at| bytes[*at] == byte)
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
    fn a_heading_is_the_whole_line() {
        for line in ["# Title", "###### Deep", "  ## Indented"] {
            let tokens = lex(line);
            assert_eq!(
                tokens.last().map(|token| token.kind),
                Some(TokenKind::Key),
                "{line:?}"
            );
        }
        // Seven is too many, and a `#` with no space is a tag rather than a
        // heading.
        assert!(!lex("####### Nope").iter().any(|t| t.kind == TokenKind::Key));
        assert!(!lex("#tag").iter().any(|t| t.kind == TokenKind::Key));
    }

    #[test]
    fn a_fence_carries_its_body_until_it_closes() {
        let (open, after) = lex_line("```rust", LineState::START);
        tiles("```rust", &open);
        assert_eq!(open[0].kind, TokenKind::Comment);
        assert_eq!(after, LineState(Carry::Fenced(b'`')));

        // Everything inside is literal, markup included.
        let (body, still) = lex_line("# not a heading", after);
        assert_eq!(body[0].kind, TokenKind::String);
        assert_eq!(still, after);
        assert_eq!(lex_line("", after).1, after);

        let (close, closed) = lex_line("```", after);
        assert_eq!(close[0].kind, TokenKind::Comment);
        assert!(closed.is_start());
    }

    #[test]
    fn a_fence_is_closed_only_by_its_own_character() {
        let (_, after) = lex_line("~~~", LineState::START);
        assert_eq!(after, LineState(Carry::Fenced(b'~')));
        // A backtick fence inside a tilde block is body.
        let (body, still) = lex_line("```", after);
        assert_eq!(body[0].kind, TokenKind::String);
        assert_eq!(still, after);
        assert!(lex_line("~~~~", after).1.is_start());
    }

    #[test]
    fn a_fence_that_never_closes_keeps_carrying() {
        let mut state = lex_line("```", LineState::START).1;
        for line in ["one", "", "  two", "## three"] {
            let (tokens, next) = lex_line(line, state);
            tiles(line, &tokens);
            assert_eq!(next, state, "{line:?}");
            state = next;
        }
        assert!(!state.is_start());
    }

    #[test]
    fn a_closing_fence_may_not_carry_an_info_string() {
        let after = lex_line("```", LineState::START).1;
        // Text after the run means this is body, not the end of the block.
        assert_eq!(lex_line("``` js", after).1, after);
        // Trailing spaces are forgiven.
        assert!(lex_line("```   ", after).1.is_start());
    }

    #[test]
    fn a_list_marker_is_coloured_and_its_text_is_not() {
        assert_eq!(
            kinds("- item", &lex("- item")),
            [(TokenKind::Keyword, "-"), (TokenKind::Plain, " item")]
        );
        assert_eq!(
            kinds("  1. first", &lex("  1. first")),
            [
                (TokenKind::Plain, "  "),
                (TokenKind::Keyword, "1."),
                (TokenKind::Plain, " first"),
            ]
        );
        assert_eq!(lex("12) twelfth")[0].kind, TokenKind::Keyword);
        // A marker needs the space after it.
        assert!(
            !lex("-1 degree")
                .iter()
                .any(|t| t.kind == TokenKind::Keyword)
        );
        assert!(
            !lex("1.5 units")
                .iter()
                .any(|t| t.kind == TokenKind::Keyword)
        );
    }

    #[test]
    fn the_text_of_a_list_item_is_still_lexed() {
        let line = "- see `run.sh`";
        assert_eq!(
            kinds(line, &lex(line)),
            [
                (TokenKind::Keyword, "-"),
                (TokenKind::Plain, " see "),
                (TokenKind::String, "`run.sh`"),
            ]
        );
    }

    #[test]
    fn a_blockquote_recedes() {
        assert_eq!(lex("> quoted")[0].kind, TokenKind::Comment);
        assert_eq!(
            kinds("  > > deep", &lex("  > > deep")),
            [(TokenKind::Plain, "  "), (TokenKind::Comment, "> > deep")]
        );
    }

    #[test]
    fn a_rule_is_a_rule_before_it_is_a_bullet() {
        for line in ["---", "***", "___", "- - -", "*****"] {
            assert_eq!(
                lex(line).last().map(|token| token.kind),
                Some(TokenKind::Comment),
                "{line:?}"
            );
        }
        // Two is not enough, and a mixture is not one at all.
        assert_ne!(lex("--")[0].kind, TokenKind::Comment);
        assert_ne!(lex("-*-")[0].kind, TokenKind::Comment);
    }

    #[test]
    fn inline_code_keeps_its_backticks() {
        let line = "run `ls -l` now";
        assert_eq!(
            kinds(line, &lex(line)),
            [
                (TokenKind::Plain, "run "),
                (TokenKind::String, "`ls -l`"),
                (TokenKind::Plain, " now"),
            ]
        );
    }

    #[test]
    fn strong_and_emphasis_are_two_weights() {
        let line = "a **bold** and *thin* word";
        let spans = kinds(line, &lex(line));
        assert!(spans.contains(&(TokenKind::Keyword, "**bold**")));
        assert!(spans.contains(&(TokenKind::Literal, "*thin*")));
        assert!(kinds("__x__", &lex("__x__")).contains(&(TokenKind::Keyword, "__x__")));
        assert!(kinds("_x_", &lex("_x_")).contains(&(TokenKind::Literal, "_x_")));
    }

    #[test]
    fn an_unclosed_marker_stays_plain() {
        for line in ["a * b", "half *open", "2 * 3 = 6", "snake_case_name"] {
            assert_eq!(
                kinds(line, &lex(line)),
                [(TokenKind::Plain, line)],
                "{line:?}"
            );
        }
    }

    #[test]
    fn a_link_names_a_target() {
        let line = "see [the guide](docs/x.md) first";
        assert_eq!(
            kinds(line, &lex(line)),
            [
                (TokenKind::Plain, "see "),
                (TokenKind::Variable, "[the guide]"),
                (TokenKind::String, "(docs/x.md)"),
                (TokenKind::Plain, " first"),
            ]
        );
        // A bracket with no target after it is prose.
        assert_eq!(
            kinds("[WARN] hi", &lex("[WARN] hi")),
            [(TokenKind::Plain, "[WARN] hi")]
        );
        assert_eq!(
            kinds("[a][b]", &lex("[a][b]")),
            [(TokenKind::Plain, "[a][b]")]
        );
    }

    #[test]
    fn an_html_comment_is_a_comment() {
        let line = "text <!-- hidden --> more";
        assert!(kinds(line, &lex(line)).contains(&(TokenKind::Comment, "<!-- hidden -->")));
        // Left open it takes this line and stops there.
        let (tokens, state) = lex_line("<!-- open", LineState::START);
        tiles("<!-- open", &tokens);
        assert_eq!(tokens[0].kind, TokenKind::Comment);
        assert!(state.is_start());
    }

    #[test]
    fn a_heading_is_not_scanned_for_spans() {
        // One run, so that a heading reads as a heading rather than as a line
        // with holes in it.
        let line = "# A `code` heading";
        assert_eq!(kinds(line, &lex(line)), [(TokenKind::Key, line)]);
    }

    #[test]
    fn nothing_here_panics_on_anything() {
        for line in [
            "",
            "   ",
            "#",
            "`",
            "*",
            "**",
            "___",
            "[",
            "[]",
            "[](",
            "<!--",
            "-",
            "1.",
            "~~~",
            "제목 `코드` **굵게**",
            "🙂 *🙂* [🙂](🙂)",
        ] {
            let (tokens, _) = lex_line(line, LineState::START);
            tiles(line, &tokens);
        }
    }
}
