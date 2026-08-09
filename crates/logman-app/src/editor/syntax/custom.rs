//! Languages defined by a YAML file rather than by code.
//!
//! The seven lexers beside this one are hand-written because they are worth
//! writing by hand: they are the formats a file panel over a server reaches
//! every day. Everything else — the Python script, the `.sql` somebody is
//! debugging, whatever the machine happens to be running — is served by one
//! general lexer driven by data, because an eighth hand-written scanner would
//! be the same shape as the seventh and there is no end to the list.
//!
//! Ten of those definitions ship with logman and are compiled into the binary:
//! C, C++, C#, Go, Java, JavaScript, Python, Rust, SQL and TypeScript. They are
//! ordinary definition files that happen to live in `crates/logman-app/syntaxes`
//! — [`SHIPPED`] — so there is nothing they can do that a file of the user's
//! cannot.
//!
//! # Where the files go, and when they are read
//!
//! One `*.yml` (or `*.yaml`) file per language in [`logman_core::syntaxes_dir`]
//! — `~/.config/logman/syntaxes/` on Linux, `syntaxes` beside `settings.json`
//! everywhere else. The file's stem is the language's id, so `python.yml`
//! defines `python`.
//!
//! # Which definition answers for a file
//!
//! The seven built-in languages first, then the user's definitions, then the
//! shipped ones — and a user file whose stem matches a shipped id replaces that
//! definition outright. [`Language::detect`] holds the first half of that rule
//! and [`assembled`] the second, each with the reasoning behind it.
//!
//! **The directory is read once, at start-up.** Adding, changing or removing a
//! definition takes effect on the next launch. That is not laziness about file
//! watching: [`Language::Custom`] is an *index* into the registry this module
//! keeps, and an open editor holds one — swapping the registry underneath it
//! would repaint a buffer with another language's rules. Everything downstream
//! is allowed to assume the registry is written once and only read afterwards.
//!
//! Reading is forgiving, exactly as [`crate::theme_store`] is with themes and
//! schemes: a file that does not parse is logged and skipped, and so is a
//! single rule inside a file that cannot be honoured. One broken definition
//! must not cost the user the others.
//!
//! # The schema, whole
//!
//! ```yaml
//! name: Python                 # what the language is called; the stem is its id
//! files:
//!   extensions: [py, pyi]      # no dot, matched without regard to case
//!   names: [SConstruct]        # exact file names, for what has no extension
//!   shebangs: [python]         # matches when the `#!` interpreter ends with this
//! comment: "#"                 # line comment, and what the comment toggle writes
//! block_comment: ["/*", "*/"]  # a comment that may cross lines
//! strings:                     # tried longest opener first
//!   - quote: "'"               # one character; never crosses a line
//!     escape: false            # whether a `\` escapes the next character (default true)
//!   - quote: '"'
//!   - pair: ['"""', '"""']     # an open/close pair, which does cross lines
//! keywords:                    # group name -> how the words in it are coloured
//!   keyword: [def, class, if, else, for, while, return, import]
//!   literal: ["True", "False", "None"]
//! keywords_ignore_case: false  # match keywords whatever their case (default false)
//! variables: ["$"]             # sigils: `$NAME` and `${...}` become variables
//! sections: false              # colour a leading `[section]` as a key
//! keys: none                   # none | colon | equals: colour `key:` / `key=`
//! numbers: true                # colour numeric literals (default true)
//! ```
//!
//! Every key is optional. A file holding nothing but `name` and `files` is
//! legal and gives a language that is matched and drawn in one colour, which is
//! a perfectly good way to start.
//!
//! The four groups `keywords` may name are the [`TokenKind`]s a word can
//! reasonably be: `keyword`, `literal`, `key` and `variable`. A group by any
//! other name is warned about and ignored rather than failing the file.
//! `keywords_ignore_case` covers the whole definition rather than one group,
//! because the languages that need it — SQL above all, where `SELECT` and
//! `select` are the same word — need it everywhere or nowhere.
//!
//! A word YAML would otherwise resolve to something else — `true`, `null`,
//! `NULL` — still arrives as the word it looks like, because the reader hands
//! over the text of a plain scalar wherever a string is wanted. The shipped
//! definitions quote them anyway, since a `literal` list reading
//! `["true", "false", "null"]` says what it means to the next person.
//!
//! # What this cannot express
//!
//! The line is drawn at what a *line-at-a-time* scanner can carry, which is a
//! block comment and a multi-line string and nothing else — see [`Carry`].
//! Beyond that:
//!
//! * No regular expressions, and no context. A word is a keyword wherever it
//!   stands, and `key:` is a key only at the head of its line, so the keys of
//!   an inline `{a: 1}` are not coloured.
//! * Nesting is not tracked. A block comment ends at the first closing
//!   delimiter, whatever opened in between; the same goes for a `pair` string,
//!   inside which a backslash escapes nothing.
//! * One kind of block comment and one line comment per language.
//! * No heredocs, no indentation-delimited block scalars, no interpolation
//!   coloured inside a string. The languages that need those are the seven
//!   that have a lexer of their own.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use anyhow::Result;
use logman_core::paths;
use serde::Deserialize;

use super::{
    Carry, Language, LineState, Runs, Token, TokenKind, char_step, number, quote_body,
    shebang_interpreter, skip_spaces, word_boundary, word_end,
};

/// The extensions a definition file may carry.
const FILE_EXTENSIONS: [&str; 2] = ["yml", "yaml"];

/// The definitions logman ships, by id, compiled into the binary.
///
/// They live in `crates/logman-app/syntaxes/` — beside `locales/`, and in the
/// same spirit: a language logman knows is part of the build, not an asset to
/// install. Nothing is ever written to the user's directory, so there is no
/// half-upgraded copy of a definition to go stale, and a person who wants to
/// change one writes their own file of the same name — see [`assembled`].
///
/// In id order, which is the order they are searched in.
const SHIPPED: [(&str, &str); 10] = [
    ("c", include_str!("../../../syntaxes/c.yml")),
    ("cpp", include_str!("../../../syntaxes/cpp.yml")),
    ("csharp", include_str!("../../../syntaxes/csharp.yml")),
    ("go", include_str!("../../../syntaxes/go.yml")),
    ("java", include_str!("../../../syntaxes/java.yml")),
    (
        "javascript",
        include_str!("../../../syntaxes/javascript.yml"),
    ),
    ("python", include_str!("../../../syntaxes/python.yml")),
    ("rust", include_str!("../../../syntaxes/rust.yml")),
    ("sql", include_str!("../../../syntaxes/sql.yml")),
    (
        "typescript",
        include_str!("../../../syntaxes/typescript.yml"),
    ),
];

/// How many string rules one definition may have.
///
/// [`Carry::CustomString`] carries which of them is open in a `u8`, since a
/// [`LineState`] is stored per line and has to stay small. A definition with
/// more rules than this keeps the first [`STRING_LIMIT`] and is warned about; a
/// language that spells strings thirty-two ways is a language this module was
/// not built for anyway.
const STRING_LIMIT: usize = 32;

/// A language read from a definition file, compiled into what the lexer wants.
///
/// Every list is stored in the form the matcher compares against — file names,
/// extensions and shebangs lowercased, keywords sorted — so that detection and
/// lexing do no work per line that could have been done once at start-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// The file's stem, slugged. Unique within the registry.
    pub id: String,
    /// What the definition calls itself, or its id when it calls itself
    /// nothing.
    pub name: String,
    /// Extensions, lowercased, without the dot.
    extensions: Vec<String>,
    /// Whole file names, lowercased.
    names: Vec<String>,
    /// Shebang interpreter suffixes, lowercased.
    shebangs: Vec<String>,
    /// The line comment prefix, which is also what the comment toggle writes.
    comment: Option<String>,
    /// The open and close delimiters of a block comment.
    block: Option<(String, String)>,
    /// String rules, longest opener first so that `"""` is tried before `"`.
    strings: Vec<StringRule>,
    /// Words that are not plain, sorted by the word for a binary search. All
    /// lowercase when `ignore_case`.
    keywords: Vec<(String, TokenKind)>,
    /// Whether a word matches a keyword whatever its case, which is what SQL
    /// needs and what nothing with a compiler wants.
    ignore_case: bool,
    /// The bytes that introduce a variable.
    sigils: Vec<u8>,
    /// Whether a leading `[section]` is a key.
    sections: bool,
    /// Which separator makes the word at the head of a line a key.
    keys: KeyStyle,
    /// Whether numeric literals are coloured.
    numbers: bool,
}

/// One way of writing a string.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StringRule {
    /// A single-character quote, which closes on the line it opened.
    Quote {
        /// The quote character.
        quote: u8,
        /// Whether a `\` escapes the character after it.
        escape: bool,
    },
    /// A delimiter pair, which may cross lines.
    Pair {
        /// What opens it.
        open: String,
        /// What closes it. May be the same as `open`, as `"""` is.
        close: String,
    },
}

impl StringRule {
    /// What has to be matched for this rule to apply.
    fn opener(&self) -> &[u8] {
        match self {
            Self::Quote { quote, .. } => std::slice::from_ref(quote),
            Self::Pair { open, .. } => open.as_bytes(),
        }
    }
}

/// What makes the word at the head of a line a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum KeyStyle {
    /// Nothing does; the language has no mappings.
    #[default]
    None,
    /// `key: value`.
    Colon,
    /// `key = value`.
    Equals,
}

impl KeyStyle {
    /// The style `value` names, or `None` when it names none.
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "colon" => Some(Self::Colon),
            "equals" => Some(Self::Equals),
            _ => None,
        }
    }

    /// The byte that has to follow the word.
    const fn separator(self) -> Option<u8> {
        match self {
            Self::None => None,
            Self::Colon => Some(b':'),
            Self::Equals => Some(b'='),
        }
    }
}

// --- the registry ------------------------------------------------------------

/// The definitions read from the user's `syntaxes` directory.
///
/// Process-wide for the same reason [`logman_term::TerminalTheme`]'s custom
/// schemes are: the alternative is threading a registry through every editor,
/// every pane and the detector they share, for a list written once at start-up.
///
/// The slice is `'static` — [`set_custom_syntaxes`] leaks the vector — so that
/// a reader takes the lock only long enough to copy a fat pointer out of it.
/// That is what lets [`line_comment`] hand back a `&'static str` and the lexer
/// run without holding a lock. The leak is bounded by the number of calls, and
/// the only call outside the tests is [`install`], once, at start-up.
static CUSTOM: OnceLock<RwLock<&'static [Definition]>> = OnceLock::new();

/// The registry, empty on first use.
fn registry() -> &'static RwLock<&'static [Definition]> {
    CUSTOM.get_or_init(|| RwLock::new(&[]))
}

/// Every definition currently registered, in id order.
pub fn definitions() -> &'static [Definition] {
    *registry()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Replaces the registry with `definitions`.
///
/// Whole-list replacement, as with the color schemes: re-reading the directory
/// cannot leave behind a language whose file no longer defines it. Callers
/// other than [`install`] and the tests would break the assumption every
/// [`Language::Custom`] index rests on — see this module's header.
pub fn set_custom_syntaxes(definitions: Vec<Definition>) {
    let leaked: &'static [Definition] = Vec::leak(definitions);
    let mut registry = registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *registry = leaked;
}

/// Reads the `syntaxes` directory and registers it over the shipped set.
///
/// The one call, made at start-up beside the theme and scheme load. Never
/// fails: a directory that is not there, or cannot be read, leaves the shipped
/// definitions alone and the editor keeps every language it was born with.
pub fn install() {
    set_custom_syntaxes(assembled(load(paths::syntaxes_dir())));
}

/// The registry in the order it is searched: the user's definitions, then the
/// shipped ones no file of the user's has taken the name of.
///
/// Two decisions live here, and they are the same decision twice. A user file
/// is *ahead of* a shipped one, so a definition of the user's own claiming
/// `.py` wins over the one logman ships even though both are custom. And a user
/// file whose stem matches a shipped id — `python.yml` over `python.yml` —
/// replaces it outright rather than sitting in front of it, so that a person
/// who dislikes what we ship gets their file *instead of* ours and not two
/// languages both called Python. Overriding by name is the only way to *remove*
/// something shipped, which is why replacement is whole rather than a merge.
///
/// (The built-in seven are ahead of all of this: [`Language::detect`] never
/// reaches the registry until they have all declined.)
fn assembled(mut definitions: Vec<Definition>) -> Vec<Definition> {
    for (id, source) in SHIPPED {
        if definitions.iter().any(|definition| definition.id == id) {
            continue;
        }
        match serde_norway::from_str::<SyntaxFile>(source) {
            Ok(file) => {
                let (definition, warnings) = compile(id, file);
                // Our own file, so a complaint here is a bug in this repository
                // rather than something the user can fix. The test over the
                // shipped set is what keeps it from ever being logged.
                for warning in warnings {
                    log::error!("the shipped {id} definition: {warning}");
                }
                definitions.push(definition);
            }
            Err(err) => log::error!("the shipped {id} definition does not parse: {err}"),
        }
    }
    definitions
}

// --- detection ---------------------------------------------------------------

/// The language of a file called `lower` (already lowercased, already reduced
/// to its last path segment) whose first line is `first_line`.
///
/// Consulted only after the built-in table has answered [`Language::Plain`], so
/// a definition can add a language but never take one over: dropping a
/// `yaml.yml` into the directory does not change what a `.yaml` file is. Within
/// the registry the first match wins, and the registry is ordered by
/// [`assembled`] — the user's definitions in id order, then the shipped ones in
/// theirs. Id order is alphabetical by file stem, so which of two definitions
/// claiming the same extension answers is the same on every machine and every
/// launch.
///
/// The three rules run in the order [`Language::detect`] runs its own, and for
/// the same reasons: the whole name, then the extension, and the shebang only
/// for a name that has no extension to go on.
pub(super) fn detect(lower: &str, first_line: &str) -> Option<Language> {
    let definitions = definitions();
    let found = |index: usize| Some(Language::Custom(index));

    if let Some(index) = definitions
        .iter()
        .position(|definition| definition.names.iter().any(|name| name == lower))
    {
        return found(index);
    }
    // The leading dots come off before the split for the reason
    // `Language::builtin` takes them off: a hidden-file marker is not an
    // extension separator, but the name behind it can still carry one.
    if let Some((_, extension)) = lower.trim_start_matches('.').rsplit_once('.') {
        return definitions
            .iter()
            .position(|definition| definition.extensions.iter().any(|known| known == extension))
            .and_then(found);
    }

    let interpreter = shebang_interpreter(first_line)?.to_ascii_lowercase();
    definitions
        .iter()
        .position(|definition| {
            definition
                .shebangs
                .iter()
                .any(|shebang| interpreter.ends_with(shebang.as_str()))
        })
        .and_then(found)
}

/// What the registered language `index` calls itself, for a list the user picks
/// from.
///
/// `&'static str` for the same reason [`line_comment`] hands one back: the
/// definitions are leaked, so a name borrowed here outlives the read and can be
/// put straight into a menu row without a copy. An index the registry does not
/// answer to has no name, and a picker built from [`definitions`] cannot produce
/// one — the empty string is what an editor set to a language that has since
/// been unregistered would show, which cannot happen while the registry is
/// written once.
pub(super) fn name(index: usize) -> &'static str {
    definitions()
        .get(index)
        .map_or("", |definition| definition.name.as_str())
}

/// The comment prefix of the registered language `index`, if it has one.
///
/// A definition with no `comment` disables the toggle and greys the menu row,
/// exactly as JSON's absence of comment syntax does.
pub(super) fn line_comment(index: usize) -> Option<&'static str> {
    definitions()
        .get(index)
        .and_then(|definition| definition.comment.as_deref())
}

/// Whether the registered language `index` can leave a line unfinished.
///
/// True of a definition with a block comment or a `pair` string, and of nothing
/// else: those are the only two things [`Carry`] carries for a custom language.
pub(super) fn carries_state(index: usize) -> bool {
    definitions().get(index).is_some_and(|definition| {
        definition.block.is_some()
            || definition
                .strings
                .iter()
                .any(|rule| matches!(rule, StringRule::Pair { .. }))
    })
}

// --- the lexer ---------------------------------------------------------------

/// The tokens of one line of the registered language `index`.
///
/// An index the registry does not answer to draws the line plain rather than
/// panicking. It cannot happen while the registry is written once — the
/// assumption this module rests on — and if that assumption is ever broken this
/// is what breaking it should cost.
pub(super) fn lex_line(line: &str, state: LineState, index: usize) -> (Vec<Token>, LineState) {
    match definitions().get(index) {
        Some(definition) => lex(line, state, definition),
        None => (super::plain(line), LineState::START),
    }
}

/// The tokens of one line of `definition`, and the state it leaves behind.
fn lex(line: &str, state: LineState, definition: &Definition) -> (Vec<Token>, LineState) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut runs = Runs::new();
    let mut at = 0;
    // Whether the head-of-line rules — a section header, a key — still apply.
    // They do not to the remainder of a line that began inside something.
    let mut head_rules = true;

    match state.0 {
        Carry::CustomComment => {
            head_rules = false;
            let close = definition
                .block
                .as_ref()
                .map(|(_, close)| close.as_bytes())
                .unwrap_or_default();
            match find_end(bytes, 0, close) {
                Some(end) => {
                    runs.push(TokenKind::Comment, 0, end);
                    at = end;
                }
                None => {
                    runs.push(TokenKind::Comment, 0, len);
                    return (runs.finish(len), state);
                }
            }
        }
        Carry::CustomString(rule) => {
            head_rules = false;
            let close = match definition.strings.get(usize::from(rule)) {
                Some(StringRule::Pair { close, .. }) => close.as_bytes(),
                _ => &[],
            };
            match find_end(bytes, 0, close) {
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
        _ => {}
    }

    if head_rules {
        at = head(&mut runs, line, definition);
    }

    while at < len {
        // A block comment beats a line comment when its opener is the longer
        // match, so a language spelling one `#` and the other `#|` is not cut
        // short by the shorter rule. A tie goes to the line comment, which is
        // the simpler reading of an ambiguous pair.
        let line_open = definition
            .comment
            .as_deref()
            .filter(|prefix| starts_at(bytes, at, prefix.as_bytes()));
        let block_open = definition
            .block
            .as_ref()
            .filter(|(open, _)| starts_at(bytes, at, open.as_bytes()));

        if let Some((open, close)) = block_open
            && line_open.is_none_or(|prefix| open.len() > prefix.len())
        {
            match find_end(bytes, at + open.len(), close.as_bytes()) {
                Some(end) => {
                    runs.push(TokenKind::Comment, at, end);
                    at = end;
                }
                None => {
                    runs.push(TokenKind::Comment, at, len);
                    return (runs.finish(len), LineState(Carry::CustomComment));
                }
            }
            continue;
        }
        if line_open.is_some() {
            runs.push(TokenKind::Comment, at, len);
            break;
        }

        if let Some((index, rule)) = string_at(definition, bytes, at) {
            match rule {
                StringRule::Quote { quote, escape } => {
                    // An unterminated one-character quote takes the rest of the
                    // line and nothing more: only a `pair` crosses one.
                    let end = quote_body(line, at + 1, *quote, *escape).unwrap_or(len);
                    runs.push(TokenKind::String, at, end);
                    at = end.max(at + 1);
                }
                StringRule::Pair { open, close } => {
                    match find_end(bytes, at + open.len(), close.as_bytes()) {
                        // Past both delimiters, so `at` moves however long they
                        // are and lands on a character boundary either way.
                        Some(end) => {
                            runs.push(TokenKind::String, at, end);
                            at = end;
                        }
                        None => {
                            runs.push(TokenKind::String, at, len);
                            let carry = Carry::CustomString(index as u8);
                            return (runs.finish(len), LineState(carry));
                        }
                    }
                }
            }
            continue;
        }

        let byte = bytes[at];
        if definition.sigils.contains(&byte) {
            match variable_end(line, at) {
                Some(end) => {
                    runs.push(TokenKind::Variable, at, end);
                    at = end.max(at + 1);
                }
                None => at += 1,
            }
            continue;
        }
        if definition.numbers && byte.is_ascii_digit() && word_boundary(bytes, at) {
            let end = number(line, at);
            runs.push(TokenKind::Number, at, end);
            at = end.max(at + 1);
            continue;
        }
        if (byte.is_ascii_alphabetic() || byte == b'_') && word_boundary(bytes, at) {
            let end = word_end(bytes, at);
            if let Some(kind) = definition.keyword(&line[at..end]) {
                runs.push(kind, at, end);
            }
            at = end.max(at + 1);
            continue;
        }
        at += char_step(line, at);
    }

    (runs.finish(len), LineState::START)
}

/// Applies the head-of-line rules, and answers where the rest of the line
/// starts.
///
/// A `[section]` runs to the last `]` on the line, as it does in the flat
/// configuration formats — which colours a `]` inside a trailing comment as
/// part of the header, and is the reading that survives the header being typed.
fn head(runs: &mut Runs, line: &str, definition: &Definition) -> usize {
    let bytes = line.as_bytes();
    let head = skip_spaces(bytes, 0);

    if definition.sections && bytes.get(head) == Some(&b'[') {
        let end = line.rfind(']').map_or(bytes.len(), |at| at + 1);
        runs.push(TokenKind::Key, head, end);
        return end.max(head);
    }
    if let Some(separator) = definition.keys.separator() {
        let end = word_end(bytes, head);
        if end > head && bytes.get(skip_spaces(bytes, end)) == Some(&separator) {
            runs.push(TokenKind::Key, head, end);
            return end;
        }
    }
    0
}

/// The end of the `$NAME` or `${...}` whose sigil is at `at`.
///
/// `None` when the sigil introduces nothing, which leaves it as plain text
/// rather than colouring a bare `$` at the end of a line.
fn variable_end(line: &str, at: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    if bytes.get(at + 1) == Some(&b'{') {
        let mut end = at + 2;
        while end < len && bytes[end] != b'}' {
            end += char_step(line, end);
        }
        return Some(if end < len { end + 1 } else { len });
    }
    let end = word_end(bytes, at + 1);
    (end > at + 1).then_some(end)
}

/// The string rule that opens at `at`, and its index.
///
/// The rules are held longest-opener-first, so the first match is the longest
/// one: a definition with both `"""` and `"` opens the triple quote on `"""`
/// rather than an empty string followed by a quote.
fn string_at<'a>(
    definition: &'a Definition,
    bytes: &[u8],
    at: usize,
) -> Option<(usize, &'a StringRule)> {
    definition
        .strings
        .iter()
        .enumerate()
        .find(|(_, rule)| starts_at(bytes, at, rule.opener()))
}

/// Whether `needle` sits at `at` in `haystack`.
///
/// Byte-wise, and safe on any `at`: both ends of a match are character
/// boundaries whenever `at` is one, since `needle` came from a `str`.
fn starts_at(haystack: &[u8], at: usize, needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .get(at..)
            .is_some_and(|rest| rest.starts_with(needle))
}

/// The offset just past the first `needle` at or after `from`.
///
/// An empty needle never matches, which is what keeps a definition that somehow
/// carried one from closing everything immediately. Loading rejects those, so
/// this is the second lock on the same door.
fn find_end(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len())
        .find(|at| haystack[*at..].starts_with(needle))
        .map(|at| at + needle.len())
}

impl Definition {
    /// How `word` is coloured, when the definition says anything about it.
    ///
    /// Called once per word of every line drawn, so neither branch allocates. A
    /// case-insensitive dictionary is stored lowercased and its search folds
    /// the needle byte by byte as it compares — `str` ordering *is* byte
    /// ordering, so the folded comparison agrees with the order the dictionary
    /// was sorted in. Only ASCII is folded, which is all a keyword can be:
    /// [`word_end`] stops at anything that is not `[A-Za-z0-9_]`.
    fn keyword(&self, word: &str) -> Option<TokenKind> {
        let found = if self.ignore_case {
            self.keywords.binary_search_by(|(known, _)| {
                known
                    .bytes()
                    .cmp(word.bytes().map(|byte| byte.to_ascii_lowercase()))
            })
        } else {
            self.keywords
                .binary_search_by(|(known, _)| known.as_str().cmp(word))
        };
        found.ok().map(|index| self.keywords[index].1)
    }
}

// --- the file format ---------------------------------------------------------

/// A definition file, as it is written.
///
/// Every field is optional, and unknown fields are ignored — serde's default —
/// so a definition written against a later version of this schema loses the
/// keys this build does not know rather than failing outright.
#[derive(Debug, Clone, Default, Deserialize)]
struct SyntaxFile {
    /// What to call the language.
    #[serde(default)]
    name: String,
    /// What the language is recognised by.
    #[serde(default)]
    files: FileMatchers,
    /// The line comment prefix.
    #[serde(default)]
    comment: Option<String>,
    /// The open and close delimiters of a block comment, in that order.
    #[serde(default)]
    block_comment: Option<Vec<String>>,
    /// How strings are written.
    #[serde(default)]
    strings: Vec<StringField>,
    /// Group name to the words in it.
    #[serde(default)]
    keywords: BTreeMap<String, Vec<String>>,
    /// Whether `keywords` matches whatever the case. Absent means no.
    #[serde(default)]
    keywords_ignore_case: Option<bool>,
    /// The sigils that introduce a variable.
    #[serde(default)]
    variables: Vec<String>,
    /// Whether a leading `[section]` is a key.
    #[serde(default)]
    sections: bool,
    /// `none`, `colon` or `equals`. A string rather than an enum so that an
    /// unknown value is a warning about one key instead of a rejected file.
    #[serde(default)]
    keys: Option<String>,
    /// Whether numeric literals are coloured. Absent means yes.
    #[serde(default)]
    numbers: Option<bool>,
}

/// What a definition is recognised by.
#[derive(Debug, Clone, Default, Deserialize)]
struct FileMatchers {
    /// Extensions, with no leading dot.
    #[serde(default)]
    extensions: Vec<String>,
    /// Whole file names.
    #[serde(default)]
    names: Vec<String>,
    /// What the `#!` interpreter may end with.
    #[serde(default)]
    shebangs: Vec<String>,
}

/// One entry of `strings`: either a `quote` or a `pair`, never both.
#[derive(Debug, Clone, Default, Deserialize)]
struct StringField {
    /// A one-character quote.
    #[serde(default)]
    quote: Option<String>,
    /// An open and close delimiter, in that order.
    #[serde(default)]
    pair: Option<Vec<String>>,
    /// Whether a `\` escapes the next character. Absent means yes.
    #[serde(default)]
    escape: Option<bool>,
}

/// The [`TokenKind`] a `keywords` group name asks for.
///
/// Only the kinds a *word* can sensibly be. Colouring a word as a comment or a
/// number is not a thing anybody wants, and leaving them out keeps the list of
/// legal group names short enough to remember.
fn group_kind(group: &str) -> Option<TokenKind> {
    match group.trim().to_ascii_lowercase().as_str() {
        "keyword" => Some(TokenKind::Keyword),
        "literal" => Some(TokenKind::Literal),
        "key" => Some(TokenKind::Key),
        "variable" => Some(TokenKind::Variable),
        _ => None,
    }
}

/// Turns a parsed file into the definition the lexer runs on, with everything
/// that had to be dropped along the way.
///
/// Nothing here fails. A rule that cannot be honoured — an empty delimiter, a
/// quote that is not one character, a `keywords` group nobody has heard of — is
/// dropped and described, leaving the rest of the definition working. The
/// complaints are returned rather than logged so that the caller can name the
/// file they belong to, and so that the test over the definitions logman ships
/// can insist there are none.
fn compile(id: &str, file: SyntaxFile) -> (Definition, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let lowercase = |values: Vec<String>| -> Vec<String> {
        values
            .into_iter()
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .collect()
    };

    let name = match file.name.trim() {
        "" => id.to_string(),
        name => name.to_string(),
    };

    let block = file.block_comment.and_then(|pair| match pair.as_slice() {
        [open, close] if !open.is_empty() && !close.is_empty() => {
            Some((open.clone(), close.clone()))
        }
        _ => {
            warnings
                .push("block_comment needs an open and a close delimiter, both non-empty".into());
            None
        }
    });

    let mut strings = Vec::new();
    for rule in file.strings {
        match (rule.quote, rule.pair) {
            (Some(quote), None) => {
                let bytes = quote.as_bytes();
                if bytes.len() == 1 && bytes[0].is_ascii() {
                    strings.push(StringRule::Quote {
                        quote: bytes[0],
                        escape: rule.escape.unwrap_or(true),
                    });
                } else {
                    warnings.push(format!(
                        "{quote:?} is not a one-character quote; write it as a pair instead"
                    ));
                }
            }
            (None, Some(pair)) => match pair.as_slice() {
                [open, close] if !open.is_empty() && !close.is_empty() => {
                    strings.push(StringRule::Pair {
                        open: open.clone(),
                        close: close.clone(),
                    });
                }
                _ => warnings.push("a string pair needs an open and a close delimiter".into()),
            },
            _ => warnings
                .push("a string rule is either a quote or a pair, not both or neither".into()),
        }
    }
    if strings.len() > STRING_LIMIT {
        warnings.push(format!(
            "only the first {STRING_LIMIT} string rules are used"
        ));
        strings.truncate(STRING_LIMIT);
    }
    // Longest opener first, so that the `"""` of a definition that also spells
    // `"` is the rule that matches. `sort_by` is stable, so rules of equal
    // length stay in the order they were written.
    strings.sort_by_key(|rule| std::cmp::Reverse(rule.opener().len()));

    // A case-insensitive definition holds its words lowercased, which is what
    // lets the lookup fold the *needle* instead of the dictionary and so stay
    // allocation-free on a path that runs once per word of every drawn line.
    let ignore_case = file.keywords_ignore_case.unwrap_or(false);
    let mut keywords: Vec<(String, TokenKind)> = Vec::new();
    for (group, words) in file.keywords {
        let Some(kind) = group_kind(&group) else {
            warnings.push(format!("no such keyword group as {group:?}, ignoring it"));
            continue;
        };
        for word in words {
            if word.is_empty() {
                continue;
            }
            let word = if ignore_case {
                word.to_ascii_lowercase()
            } else {
                word
            };
            if keywords.iter().any(|(known, _)| *known == word) {
                warnings.push(format!(
                    "{word:?} is claimed by more than one keyword group"
                ));
                continue;
            }
            keywords.push((word, kind));
        }
    }
    keywords.sort_by(|(left, _), (right, _)| left.cmp(right));

    let mut sigils = Vec::new();
    for sigil in file.variables {
        match sigil.as_bytes() {
            [byte] if byte.is_ascii() && !byte.is_ascii_alphanumeric() => sigils.push(*byte),
            _ => warnings.push(format!("{sigil:?} is not a usable variable sigil")),
        }
    }

    let keys = file.keys.map_or(KeyStyle::None, |value| {
        KeyStyle::parse(&value).unwrap_or_else(|| {
            warnings.push(format!("keys is none, colon or equals, not {value:?}"));
            KeyStyle::None
        })
    });

    let definition = Definition {
        id: id.to_string(),
        name,
        extensions: lowercase(file.files.extensions)
            .into_iter()
            .map(|extension| extension.trim_start_matches('.').to_string())
            .filter(|extension| !extension.is_empty())
            .collect(),
        names: lowercase(file.files.names),
        shebangs: lowercase(file.files.shebangs),
        comment: file.comment.filter(|comment| !comment.is_empty()),
        block,
        strings,
        keywords,
        ignore_case,
        sigils,
        sections: file.sections,
        keys,
        numbers: file.numbers.unwrap_or(true),
    };
    (definition, warnings)
}

/// Every definition in `dir`, in id order.
///
/// Shaped like [`crate::theme_store`]'s reader of the theme and scheme
/// directories, and forgiving in the same three places: a name that yields no
/// id, a file that cannot be read, and a file that does not parse are each
/// logged and skipped. The order is by id rather than whatever `read_dir`
/// reports, because id order is what decides which of two definitions claiming
/// the same extension wins, and that must not change between launches.
fn load(dir: Result<PathBuf>) -> Vec<Definition> {
    let dir = match dir {
        Ok(dir) => dir,
        Err(err) => {
            log::warn!("cannot locate the syntaxes directory: {err:#}");
            return Vec::new();
        }
    };
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        // A user who has never defined a language simply has no directory.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            log::warn!("cannot read {}: {err}", dir.display());
            return Vec::new();
        }
    };

    let mut loaded: Vec<Definition> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_definition_file(&path) {
            continue;
        }
        let Some(id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(crate::theme_store::slug)
        else {
            log::warn!("skipping {}: its name yields no usable id", path.display());
            continue;
        };
        if loaded.iter().any(|definition| definition.id == id) {
            log::warn!("skipping {}: {id} is already defined", path.display());
            continue;
        }
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(err) => {
                log::warn!("skipping {}: {err}", path.display());
                continue;
            }
        };
        // The byte order mark a Windows editor leaves behind is not YAML, and
        // the parser says so in a way that reads like a syntax error.
        match serde_norway::from_slice::<SyntaxFile>(paths::strip_bom(&data)) {
            Ok(file) => {
                let (definition, warnings) = compile(&id, file);
                for warning in warnings {
                    log::warn!("{}: {warning}", path.display());
                }
                loaded.push(definition);
            }
            Err(err) => log::warn!("skipping {}: {err}", path.display()),
        }
    }

    loaded.sort_by(|left, right| left.id.cmp(&right.id));
    loaded
}

/// Whether `path` is named like a definition file.
fn is_definition_file(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        FILE_EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::super::tests::{kinds, tiles};
    use super::*;

    /// Held by every test that touches the registry.
    ///
    /// The registry is process-wide and the test harness is threaded, so a test
    /// that installs a language would otherwise race one that asserts a name is
    /// *not* a language. Every test on either side of that takes this.
    static REGISTRY: Mutex<()> = Mutex::new(());

    /// Locks the registry for the duration of a test, and empties it.
    pub(in crate::editor::syntax) fn lock_registry() -> MutexGuard<'static, ()> {
        let guard = REGISTRY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        set_custom_syntaxes(Vec::new());
        guard
    }

    /// A python-shaped definition: the schema exercised end to end.
    const PYTHON: &str = r##"
name: Python
files:
  extensions: [py, PYI]
  names: [SConstruct]
  shebangs: [python, python3]
comment: "#"
strings:
  - quote: "'"
  - quote: '"'
  - pair: ['"""', '"""']
keywords:
  keyword: [def, class, return, import]
  literal: [True, False, None]
"##;

    /// The definition `yaml` describes, compiled as `id`, with nothing dropped.
    fn definition(id: &str, yaml: &str) -> Definition {
        let (definition, warnings) = compiled(id, yaml);
        assert_eq!(
            warnings,
            Vec::<String>::new(),
            "{id} was compiled with complaints"
        );
        definition
    }

    /// The definition `yaml` describes and everything that could not be
    /// honoured in it.
    fn compiled(id: &str, yaml: &str) -> (Definition, Vec<String>) {
        let file = serde_norway::from_str::<SyntaxFile>(yaml).expect("a valid definition");
        compile(id, file)
    }

    #[test]
    fn a_word_that_looks_like_a_yaml_scalar_is_still_a_word() {
        // What the `literal` group of every real definition is made of. If the
        // reader ever resolves these to a boolean or a null instead of handing
        // over their text, every shipped definition loses its literals — so
        // this is the test that would say so.
        let scalars = definition(
            "scalars",
            "keywords:\n  literal: [\"true\", \"False\", \"null\", \"NULL\", \"on\", \"~\"]\n",
        );
        for word in ["true", "False", "null", "NULL", "on"] {
            assert_eq!(scalars.keyword(word), Some(TokenKind::Literal), "{word}");
        }

        // And unquoted, which is what a person writes first: the reader hands
        // over the text of a plain scalar wherever a string is wanted, so a
        // word that YAML would otherwise resolve to a boolean or a null still
        // arrives as the word it looks like.
        let bare = definition(
            "bare",
            "keywords:
  literal: [true, False, None, null, NULL]
",
        );
        for word in ["true", "False", "None", "null", "NULL"] {
            assert_eq!(bare.keyword(word), Some(TokenKind::Literal), "{word}");
        }
    }

    /// The tokens of `line` from a clean state, checked for tiling.
    fn lex_from_start(definition: &Definition, line: &str) -> Vec<Token> {
        let (tokens, _) = lex(line, LineState::START, definition);
        tiles(line, &tokens);
        tokens
    }

    #[test]
    fn a_whole_definition_parses() {
        let python = definition("python", PYTHON);

        assert_eq!(python.id, "python");
        assert_eq!(python.name, "Python");
        assert_eq!(python.extensions, ["py", "pyi"]);
        assert_eq!(python.names, ["sconstruct"]);
        assert_eq!(python.shebangs, ["python", "python3"]);
        assert_eq!(python.comment.as_deref(), Some("#"));
        assert!(python.numbers);
        assert_eq!(python.keys, KeyStyle::None);
        // The triple quote is first, so it is tried before the single one.
        assert_eq!(python.strings.len(), 3);
        assert_eq!(python.strings[0].opener(), b"\"\"\"");
        assert_eq!(python.keyword("def"), Some(TokenKind::Keyword));
        assert_eq!(python.keyword("None"), Some(TokenKind::Literal));
        assert_eq!(python.keyword("none"), None);
    }

    #[test]
    fn a_definition_may_say_almost_nothing() {
        let bare = definition("bare", "files:\n  extensions: [bare]\n");
        // The id stands in for a name nobody wrote, and numbers default on.
        assert_eq!(bare.name, "bare");
        assert!(bare.numbers);
        assert_eq!(bare.comment, None);
        assert!(bare.strings.is_empty());
        assert!(!carries_state_of(&bare));

        // And a file that says nothing at all is still a definition, just one
        // nothing is ever detected as. All it has left is the numbers.
        let empty = definition("empty", "{}");
        assert!(empty.extensions.is_empty());
        assert_eq!(
            kinds("x = 1", &lex_from_start(&empty, "x = 1")),
            [(TokenKind::Plain, "x = "), (TokenKind::Number, "1")]
        );
    }

    #[test]
    fn a_rule_that_cannot_be_honoured_is_dropped_and_the_rest_kept() {
        let (mixed, warnings) = compiled(
            "mixed",
            r#"
name: Mixed
comment: "//"
block_comment: ["/*"]
strings:
  - quote: "''"
  - quote: "'"
  - pair: ["<<", ">>"]
  - {}
keywords:
  keyword: [ok]
  nonsense: [dropped]
variables: ["$", "not a sigil", "a"]
keys: sideways
"#,
        );

        // The block comment was a one-element list, so there is none.
        assert_eq!(mixed.block, None);
        // Two of the four string rules survive: the two-character quote is not
        // a quote, and the empty rule is neither a quote nor a pair.
        assert_eq!(mixed.strings.len(), 2);
        // The unknown keyword group is gone; the known one is not.
        assert_eq!(mixed.keyword("ok"), Some(TokenKind::Keyword));
        assert_eq!(mixed.keyword("dropped"), None);
        // Only the one-character, non-alphanumeric sigil is usable.
        assert_eq!(mixed.sigils, [b'$']);
        // An unknown key style is no key style.
        assert_eq!(mixed.keys, KeyStyle::None);
        // And what was well-formed still works.
        assert_eq!(mixed.comment.as_deref(), Some("//"));
        // One complaint each for the seven things dropped: the block comment,
        // two string rules, the keyword group, two sigils and the key style.
        assert_eq!(warnings.len(), 7, "{warnings:?}");
    }

    #[test]
    fn a_file_that_is_not_a_definition_is_an_error_rather_than_a_panic() {
        assert!(serde_norway::from_str::<SyntaxFile>("name: [not, a, string]").is_err());
        assert!(serde_norway::from_str::<SyntaxFile>("\tnot: yaml: at: all").is_err());
        // An unknown key is not an error: a file written for a later schema
        // loses the key rather than the whole definition.
        let ahead = definition("ahead", "name: Ahead\nfuture_key: [1, 2]\n");
        assert_eq!(ahead.name, "Ahead");
    }

    #[test]
    fn a_directory_of_definitions_loads_in_id_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let write = |name: &str, contents: &[u8]| {
            fs::write(root.join(name), contents).expect("write");
        };

        write("Zeta.yml", b"name: Zeta\nfiles:\n  extensions: [z]\n");
        let mut with_bom = b"\xEF\xBB\xBF".to_vec();
        with_bom.extend_from_slice(b"name: Alpha\n");
        write("alpha.yaml", &with_bom);
        // Skipped: unparseable, and not a definition file at all.
        write("broken.yml", b"strings: 3\n");
        write("notes.txt", b"name: Notes\n");

        let loaded = load(Ok(root));
        let ids: Vec<&str> = loaded.iter().map(|one| one.id.as_str()).collect();
        assert_eq!(ids, ["alpha", "zeta"]);
        assert_eq!(loaded[0].name, "Alpha");
    }

    #[test]
    fn every_shipped_definition_compiles_with_nothing_dropped() {
        assert_eq!(SHIPPED.len(), 10);
        let mut ids: Vec<&str> = SHIPPED.iter().map(|(id, _)| *id).collect();
        let sorted = {
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            sorted
        };
        assert_eq!(ids, sorted, "SHIPPED is searched in id order");
        ids.dedup();
        assert_eq!(
            ids.len(),
            SHIPPED.len(),
            "two shipped definitions share an id"
        );

        for (id, source) in SHIPPED {
            // `definition` fails the test on any complaint, which is the point:
            // a file logman ships must not need the forgiving path.
            let shipped = definition(id, source);
            assert!(!shipped.name.is_empty(), "{id} has no name");
            assert!(
                !shipped.extensions.is_empty(),
                "{id} is not recognised by anything"
            );
            assert!(shipped.comment.is_some(), "{id} has no line comment");
            assert!(!shipped.keywords.is_empty(), "{id} has no keywords");
            assert!(
                shipped
                    .keywords
                    .iter()
                    .any(|(_, kind)| *kind == TokenKind::Literal),
                "{id} has no literals"
            );
            assert!(shipped.numbers, "{id} does not colour numbers");
        }
    }

    #[test]
    fn the_shipped_definitions_are_registered_behind_the_users_own() {
        let _guard = lock_registry();
        let registry = assembled(Vec::new());
        let ids: Vec<&str> = registry.iter().map(|one| one.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "c",
                "cpp",
                "csharp",
                "go",
                "java",
                "javascript",
                "python",
                "rust",
                "sql",
                "typescript"
            ]
        );

        // A user's definition of another language goes in front of all of them.
        let registry = assembled(vec![definition("lua", "files:\n  extensions: [lua]\n")]);
        assert_eq!(registry.len(), SHIPPED.len() + 1);
        assert_eq!(registry[0].id, "lua");
    }

    #[test]
    fn a_user_file_of_the_same_name_replaces_a_shipped_definition() {
        let _guard = lock_registry();
        let mine = definition(
            "python",
            "name: My Python\nfiles:\n  extensions: [py, pie]\n",
        );
        let registry = assembled(vec![mine]);

        // One Python, and it is the user's: replaced whole rather than shadowed,
        // so what the shipped file claimed is gone with it.
        assert_eq!(registry.len(), SHIPPED.len());
        let pythons: Vec<&Definition> = registry.iter().filter(|one| one.id == "python").collect();
        assert_eq!(pythons.len(), 1);
        assert_eq!(pythons[0].name, "My Python");

        set_custom_syntaxes(registry);
        assert_eq!(
            Language::detect("main.pie", ""),
            Language::detect("main.py", "")
        );
        // The shipped Python's keywords went with the file it replaced.
        let index = match Language::detect("main.py", "") {
            Language::Custom(index) => index,
            other => panic!("expected a custom language, got {other:?}"),
        };
        assert_eq!(definitions()[index].keyword("def"), None);
    }

    #[test]
    fn a_registered_language_offers_the_name_its_file_gave_it() {
        let _guard = lock_registry();
        set_custom_syntaxes(vec![
            definition("zeta", "name: Zeta\nfiles:\n  extensions: [z]\n"),
            definition("alpha", "name: Alpha\nfiles:\n  extensions: [a]\n"),
        ]);

        assert_eq!(Language::Custom(0).name(), "Zeta");
        assert_eq!(Language::Custom(1).name(), "Alpha");
        // An index nothing answers to cannot come out of a picker built from
        // the registry, and is a blank rather than a panic if it ever did.
        assert_eq!(Language::Custom(2).name(), "");
    }

    #[test]
    fn the_picker_lists_registered_languages_after_the_built_in_ones_by_name() {
        let _guard = lock_registry();
        // Registered in search order — a user's own definitions first — which is
        // not an order anybody reads a list of formats in.
        set_custom_syntaxes(vec![
            definition("zeta", "name: Zeta\nfiles:\n  extensions: [z]\n"),
            definition("alpha", "name: Alpha\nfiles:\n  extensions: [a]\n"),
        ]);

        let listed = Language::all();
        assert_eq!(listed.len(), 10);
        assert_eq!(listed[0], Language::Plain, "plain text leads the list");
        assert_eq!(listed[7], Language::Markdown, "the built-in eight first");
        // Sorted by name, so the index each one carries — its place in the
        // registry — is not the place it appears in.
        assert_eq!(&listed[8..], &[Language::Custom(1), Language::Custom(0)]);
        assert_eq!(listed[8].name(), "Alpha");
    }

    #[test]
    fn each_shipped_language_answers_for_its_own_extensions() {
        let _guard = lock_registry();
        set_custom_syntaxes(assembled(Vec::new()));

        let named = |name: &str, first_line: &str| -> String {
            match Language::detect(name, first_line) {
                Language::Custom(index) => definitions()[index].id.clone(),
                other => format!("{other:?}"),
            }
        };
        for (name, expected) in [
            ("main.py", "python"),
            ("types.pyi", "python"),
            ("SConstruct", "python"),
            ("index.js", "javascript"),
            ("server.mjs", "javascript"),
            ("bundle.cjs", "javascript"),
            ("app.ts", "typescript"),
            ("view.tsx", "typescript"),
            ("Main.java", "java"),
            ("parser.c", "c"),
            ("parser.h", "c"),
            ("engine.cpp", "cpp"),
            ("engine.cc", "cpp"),
            ("engine.cxx", "cpp"),
            ("engine.hpp", "cpp"),
            ("engine.hh", "cpp"),
            ("engine.hxx", "cpp"),
            ("Program.cs", "csharp"),
            ("main.go", "go"),
            ("lib.rs", "rust"),
            ("schema.sql", "sql"),
        ] {
            assert_eq!(named(name, ""), expected, "{name}");
        }

        // A shebang, for the scripts that carry no extension at all.
        assert_eq!(named("run", "#!/usr/bin/env python3"), "python");
        assert_eq!(named("serve", "#!/usr/bin/node"), "javascript");
        // And the seven built-in languages are still ahead of every one of them.
        assert_eq!(named("compose.yml", ""), "Yaml");
        assert_eq!(named("deploy.sh", ""), "Shell");
    }

    #[test]
    fn sql_matches_its_keywords_whatever_their_case() {
        let sql = definition("sql", SHIPPED[8].1);
        assert_eq!(SHIPPED[8].0, "sql");
        assert!(sql.ignore_case);

        for word in ["select", "SELECT", "Select", "sELECT"] {
            assert_eq!(sql.keyword(word), Some(TokenKind::Keyword), "{word}");
        }
        assert_eq!(sql.keyword("NULL"), Some(TokenKind::Literal));
        // A word nobody claimed is still nobody's, in any case.
        assert_eq!(sql.keyword("Selected"), None);
        assert_eq!(sql.keyword("SEL"), None);

        let line = "SELECT id FROM users WHERE name = 'a''b' -- why";
        let spans = kinds(line, &lex_from_start(&sql, line));
        assert_eq!(spans[0], (TokenKind::Keyword, "SELECT"));
        assert!(spans.contains(&(TokenKind::Keyword, "FROM")));
        assert!(spans.contains(&(TokenKind::Keyword, "WHERE")));
        assert!(spans.contains(&(TokenKind::Comment, "-- why")));
        assert!(spans.iter().any(|(kind, _)| *kind == TokenKind::String));

        // The case-sensitive definitions did not become case-insensitive.
        let python = definition("python", SHIPPED[6].1);
        assert_eq!(SHIPPED[6].0, "python");
        assert!(!python.ignore_case);
        assert_eq!(python.keyword("def"), Some(TokenKind::Keyword));
        assert_eq!(python.keyword("DEF"), None);
        assert_eq!(python.keyword("True"), Some(TokenKind::Literal));
        assert_eq!(python.keyword("true"), None);
    }

    #[test]
    fn the_shipped_definitions_lex_a_line_of_their_own_language() {
        let _guard = lock_registry();
        let compiled: Vec<Definition> = assembled(Vec::new());
        let by_id = |id: &str| -> Definition {
            compiled
                .iter()
                .find(|one| one.id == id)
                .expect("a shipped definition")
                .clone()
        };

        // A block comment and a keyword, in the five that spell them the same.
        for id in ["c", "cpp", "csharp", "go", "java", "javascript", "rust"] {
            let definition = by_id(id);
            let line = "/* aside */ x // trailing";
            let spans = kinds(line, &lex_from_start(&definition, line));
            assert_eq!(spans[0], (TokenKind::Comment, "/* aside */"), "{id}");
            assert_eq!(
                spans.last(),
                Some(&(TokenKind::Comment, "// trailing")),
                "{id}"
            );
            assert!(
                definition.block.is_some() && carries_state_of(&definition),
                "{id}"
            );
        }

        // Python's triple quote crosses a line; nothing else it has does.
        let python = by_id("python");
        let (_, after) = lex(r#"doc = """open"#, LineState::START, &python);
        assert_eq!(after, LineState(Carry::CustomString(0)));

        // Go's raw string and JavaScript's template literal are the same rule.
        for id in ["go", "javascript", "typescript"] {
            let definition = by_id(id);
            let (_, after) = lex("const a = `open", LineState::START, &definition);
            assert!(!after.is_start(), "{id} does not carry its backtick string");
        }

        // Rust leaves `'a` alone rather than reading it as a string that never
        // closes, which is the one deliberate hole in its definition.
        let rust = by_id("rust");
        let line = "fn get<'a>(x: &'a str) -> u32 { 1 }";
        let spans = kinds(line, &lex_from_start(&rust, line));
        assert!(!spans.iter().any(|(kind, _)| *kind == TokenKind::String));
        assert_eq!(spans[0], (TokenKind::Keyword, "fn"));
    }

    #[test]
    fn nothing_shipped_panics_on_a_line_from_another_language() {
        let _guard = lock_registry();
        // Every shipped definition over lines drawn from all of them, threaded
        // so that each line is also lexed from whatever the last one left open.
        // What is being asserted is not the colours but that the runs add up.
        let lines = [
            "",
            "#include <stdio.h>",
            "def f(): return '''x",
            "SELECT * FROM t WHERE a = 'b' -- c",
            "const s = `a${b}c`;",
            "/* open",
            "*/ closed",
            "let x = r#\"raw\"#;",
            "@Override public void f() {}",
            "x := `raw",
            "\"\"\"",
            "한글 = \"값\" // 주석",
            "🙂🙂🙂",
        ];
        for definition in assembled(Vec::new()) {
            let mut state = LineState::START;
            for line in lines {
                let (tokens, next) = lex(line, state, &definition);
                tiles(line, &tokens);
                state = next;
            }
        }
    }

    #[test]
    fn a_missing_directory_yields_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load(Ok(dir.path().join("never-created"))).is_empty());
        assert!(load(Err(anyhow::anyhow!("no home directory"))).is_empty());
    }

    #[test]
    fn a_comment_runs_to_the_end_of_the_line() {
        let python = definition("python", PYTHON);
        let line = "x = 1  # why";
        assert_eq!(
            kinds(line, &lex_from_start(&python, line)),
            [
                (TokenKind::Plain, "x = "),
                (TokenKind::Number, "1"),
                (TokenKind::Plain, "  "),
                (TokenKind::Comment, "# why"),
            ]
        );
    }

    #[test]
    fn keywords_are_coloured_by_the_group_they_are_in() {
        let python = definition("python", PYTHON);
        let line = "def run(): return None";
        let spans = kinds(line, &lex_from_start(&python, line));
        assert_eq!(spans[0], (TokenKind::Keyword, "def"));
        assert!(spans.contains(&(TokenKind::Keyword, "return")));
        assert!(spans.contains(&(TokenKind::Literal, "None")));
        // A word nobody claimed is plain, and a keyword inside another word is
        // not a keyword.
        assert!(!spans.iter().any(|(kind, text)| *kind == TokenKind::Keyword
            && *text != "def"
            && *text != "return"));
        let line = "undefined";
        assert_eq!(
            kinds(line, &lex_from_start(&python, line)),
            [(TokenKind::Plain, "undefined")]
        );
    }

    #[test]
    fn both_quotes_close_on_their_own_line() {
        let python = definition("python", PYTHON);
        let line = r#"a = 'one' + "two" # done"#;
        let spans = kinds(line, &lex_from_start(&python, line));
        assert!(spans.contains(&(TokenKind::String, "'one'")));
        assert!(spans.contains(&(TokenKind::String, r#""two""#)));
        assert!(spans.contains(&(TokenKind::Comment, "# done")));

        // An unterminated quote takes the rest of the line and carries nothing.
        let (tokens, after) = lex(r#"a = "open"#, LineState::START, &python);
        tiles(r#"a = "open"#, &tokens);
        assert!(after.is_start());
    }

    #[test]
    fn a_pair_string_carries_until_it_closes() {
        let python = definition("python", PYTHON);

        let opened = r#"doc = """first"#;
        let (tokens, after) = lex(opened, LineState::START, &python);
        tiles(opened, &tokens);
        assert_eq!(
            kinds(opened, &tokens).last(),
            Some(&(TokenKind::String, r#""""first"#))
        );
        assert_eq!(after, LineState(Carry::CustomString(0)));

        // The body is a string whatever it holds — a `#` in there is not a
        // comment — and the state does not move.
        let (body, still) = lex("second # not a comment", after, &python);
        tiles("second # not a comment", &body);
        assert_eq!(body[0].kind, TokenKind::String);
        assert_eq!(still, after);

        let closing = r#"third""" # a comment"#;
        let (last, closed) = lex(closing, after, &python);
        tiles(closing, &last);
        assert!(closed.is_start());
        assert_eq!(last[0].kind, TokenKind::String);
        assert_eq!(last.last().expect("tokens").kind, TokenKind::Comment);

        // Opened and closed on one line carries nothing.
        assert!(
            lex(r#"d = """one""""#, LineState::START, &python)
                .1
                .is_start()
        );
    }

    #[test]
    fn a_block_comment_carries_until_it_closes() {
        let c = definition(
            "c",
            r#"
name: C-ish
comment: "//"
block_comment: ["/*", "*/"]
keywords:
  keyword: [int]
"#,
        );

        let (tokens, after) = lex("int x; /* open", LineState::START, &c);
        tiles("int x; /* open", &tokens);
        assert_eq!(tokens[0].kind, TokenKind::Keyword);
        assert_eq!(after, LineState(Carry::CustomComment));

        let (body, still) = lex("int is not a keyword in here", after, &c);
        assert_eq!(body.len(), 1);
        assert_eq!(body[0].kind, TokenKind::Comment);
        assert_eq!(still, after);

        let closing = "*/ int y;";
        let (last, closed) = lex(closing, after, &c);
        tiles(closing, &last);
        assert!(closed.is_start());
        assert_eq!(kinds(closing, &last)[0], (TokenKind::Comment, "*/"));
        assert!(kinds(closing, &last).contains(&(TokenKind::Keyword, "int")));

        // A block comment that opens and closes on one line leaves the rest of
        // the line alone.
        let line = "int /* aside */ y;";
        let spans = kinds(line, &lex_from_start(&c, line));
        assert!(spans.contains(&(TokenKind::Comment, "/* aside */")));
        assert_eq!(spans[0], (TokenKind::Keyword, "int"));
    }

    #[test]
    fn only_the_definitions_with_something_to_remember_carry_state() {
        let _guard = lock_registry();
        let python = definition("python", PYTHON);
        let plain = definition("plain", "comment: \"#\"\n");
        let block = definition("block", "block_comment: [\"<!--\", \"-->\"]\n");

        // A `pair` string or a block comment, and nothing else.
        set_custom_syntaxes(vec![block.clone(), plain.clone(), python.clone()]);
        assert!(carries_state(0), "a block comment carries");
        assert!(!carries_state(1), "quotes alone do not");
        assert!(carries_state(2), "a pair string carries");
        // An index nothing answers to is not a panic.
        assert!(!carries_state(9));

        assert_eq!(line_comment(1), Some("#"));
        assert_eq!(line_comment(0), None, "no comment key, no toggle");
        assert_eq!(line_comment(9), None);
    }

    /// Whether `definition` would carry state, without going through the
    /// registry.
    fn carries_state_of(definition: &Definition) -> bool {
        definition.block.is_some()
            || definition
                .strings
                .iter()
                .any(|rule| matches!(rule, StringRule::Pair { .. }))
    }

    #[test]
    fn sections_keys_variables_and_numbers_are_opt_in() {
        let flat = definition(
            "flat",
            r#"
sections: true
keys: equals
variables: ["%"]
numbers: false
"#,
        );

        let line = "[group]";
        assert_eq!(
            kinds(line, &lex_from_start(&flat, line)),
            [(TokenKind::Key, "[group]")]
        );
        let line = "  path = %HOME% 12";
        let spans = kinds(line, &lex_from_start(&flat, line));
        assert_eq!(spans[1], (TokenKind::Key, "path"));
        assert!(spans.contains(&(TokenKind::Variable, "%HOME")));
        // `numbers: false` leaves a number alone.
        assert!(!spans.iter().any(|(kind, _)| *kind == TokenKind::Number));

        // A colon definition does not read an `=` as a mapping, and neither
        // reads a key anywhere but at the head of its line.
        let mapped = definition("mapped", "keys: colon\n");
        assert!(
            !lex_from_start(&mapped, "a = 1")
                .iter()
                .any(|token| token.kind == TokenKind::Key)
        );
        let line = "key: {inner: 1}";
        let spans = kinds(line, &lex_from_start(&mapped, line));
        assert_eq!(spans[0], (TokenKind::Key, "key"));
        assert_eq!(
            spans
                .iter()
                .filter(|(kind, _)| *kind == TokenKind::Key)
                .count(),
            1
        );
    }

    #[test]
    fn a_longer_comment_opener_wins_over_a_shorter_one() {
        let both = definition("both", "comment: \"#\"\nblock_comment: [\"#|\", \"|#\"]\n");
        let line = "#| block |# and # line";
        let spans = kinds(line, &lex_from_start(&both, line));
        assert_eq!(spans[0], (TokenKind::Comment, "#| block |#"));
        assert_eq!(spans.last(), Some(&(TokenKind::Comment, "# line")));
    }

    #[test]
    fn nothing_here_panics_on_anything() {
        let definitions = [
            definition("python", PYTHON),
            definition(
                "everything",
                r##"
comment: "#"
block_comment: ["/*", "*/"]
strings:
  - quote: "'"
  - pair: ["<<", ">>"]
variables: ["$"]
sections: true
keys: colon
"##,
            ),
            definition("empty", "{}"),
        ];
        let lines = [
            "",
            "   ",
            "#",
            "/*",
            "*/",
            "<<",
            "\"\"\"",
            "'",
            "$",
            "${",
            "[",
            "]",
            ":",
            "키: \"값\" # 주석",
            "🙂🙂🙂",
            "\\\"'`$${}[]<<>>::==",
        ];
        for definition in &definitions {
            // Threaded, so every line is also lexed from whatever the line
            // before it left open.
            let mut state = LineState::START;
            for line in lines {
                let (tokens, next) = lex(line, state, definition);
                tiles(line, &tokens);
                state = next;
            }
        }
    }

    #[test]
    fn a_definition_is_detected_only_where_no_builtin_answers() {
        let _guard = lock_registry();
        set_custom_syntaxes(vec![
            definition("python", PYTHON),
            // Claims YAML's extension, which it must not get.
            definition("yaml-ish", "files:\n  extensions: [yml]\n"),
        ]);

        assert_eq!(Language::detect("main.py", ""), Language::Custom(0));
        assert_eq!(Language::detect("MAIN.PYI", ""), Language::Custom(0));
        assert_eq!(Language::detect("SConstruct", ""), Language::Custom(0));
        assert_eq!(
            Language::detect("/srv/app/tasks.py", ""),
            Language::Custom(0)
        );
        // The built-in table answers first, so the definition never sees it.
        assert_eq!(Language::detect("compose.yml", ""), Language::Yaml);
        // And a shebang, only for a name with no extension of its own.
        assert_eq!(
            Language::detect("build", "#!/usr/bin/env python3"),
            Language::Custom(0)
        );
        assert_eq!(
            Language::detect("build.txt", "#!/usr/bin/python"),
            Language::Plain
        );
        assert_eq!(Language::detect("notes.txt", ""), Language::Plain);

        // The comment toggle and the cache follow the definition.
        assert_eq!(Language::Custom(0).line_comment(), Some("#"));
        assert!(Language::Custom(0).carries_state());
    }

    #[test]
    fn the_first_definition_in_id_order_wins_a_shared_extension() {
        let _guard = lock_registry();
        // As `load` would hand them over: sorted by id.
        set_custom_syntaxes(vec![
            definition("aaa", "files:\n  extensions: [shared]\n"),
            definition("bbb", "files:\n  extensions: [shared]\n"),
        ]);
        assert_eq!(Language::detect("x.shared", ""), Language::Custom(0));
    }

    #[test]
    fn an_unregistered_index_is_drawn_plain() {
        let _guard = lock_registry();
        let (tokens, state) = super::lex_line("anything", LineState::START, 7);
        assert_eq!(kinds("anything", &tokens), [(TokenKind::Plain, "anything")]);
        assert!(state.is_start());
    }
}
