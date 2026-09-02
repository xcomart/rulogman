//! Regex rules that recolour what a followed file shows.
//!
//! A tailed log is read the way a person reads a wall of text: not word by
//! word, but by looking for the line that is different. Nothing in the byte
//! stream says which line that is — `tail -f` hands over the same undifferen-
//! tiated characters for a routine heartbeat and for the stack trace that
//! explains the outage — so the difference has to be *put back*, and a regex
//! over each line is the only description of "interesting" that survives being
//! written down once and applied to every log format the user follows.
//!
//! What lives here is only the persisted model and the pure functions over it:
//! the rules themselves, the vocabulary their colours are spelled in, the
//! built-in [`preset`], and the resolution that decides which list a given pane
//! runs with. Compiling a [`HighlightRule::pattern`] into something that can
//! match is deliberately *not* here — that would put the `regex` crate in the
//! dependency list of the layer whose whole point is that it can be exercised
//! with nothing but serde, and the app layer already has to own the compiled
//! form anyway because it is the thing holding a cache of it per pane. Core
//! stores the source text; the app compiles it and reports a bad one to the
//! user. A pattern that does not compile is therefore not an error here: it is
//! a rule that never matches, which is exactly what a half-typed regex in a
//! settings dialog should do.
//!
//! Colours are strings for the same reason. A highlight has to keep working
//! when the user switches colour scheme, so the normal way to name one is a
//! *slot* of the scheme — `"red"`, `"bright_yellow"` — resolved against
//! whatever palette the session is running, which `rulogman-term` owns and this
//! crate has never depended on. [`HighlightColor::parse`] turns the stored
//! string into the small enum the renderer needs without either end having to
//! agree on a colour type.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::settings::HighlightSettings;

/// How much of the line a matching rule recolours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HighlightScope {
    /// Only the span the pattern matched, leaving the rest of the line alone.
    ///
    /// The default, because it is the conservative answer: a rule that turns
    /// out to match more than its author expected has spoiled a few characters
    /// rather than a screenful.
    #[default]
    Match,
    /// The whole line the match was found on.
    Line,
}

/// Whether a flag defaulting to "on" is on, so serde can omit it.
///
/// Written out rather than borrowed from `std` because the `skip_serializing_if`
/// predicate is handed a `&bool` and the obvious spelling — `std::convert::identity`
/// — takes its argument by value. The `false`-defaulting counterpart needs no
/// such helper: `std::ops::Not::not` is already implemented for `&bool`.
fn is_true(value: &bool) -> bool {
    *value
}

/// Default for the two flags a rule is useless without: on.
fn default_true() -> bool {
    true
}

/// One regex rule and the colours a line it matches is drawn in.
///
/// Rules are held in an ordered list and the renderer gives the **first**
/// matching rule precedence, so the order in a list is a severity order: see
/// [`preset`], which is sorted that way on purpose. This is why the model is a
/// `Vec` and not a map keyed by pattern — the sequence carries meaning that a
/// set would throw away.
///
/// Every field but the pattern is omitted from the JSON when it holds its
/// default, so a hand-written rule stays as short as the thing it describes:
/// `{"pattern": "\\bOOM\\b", "foreground": "bright_red"}` is a complete rule.
/// The one field written even when it is the default is [`scope`](Self::scope),
/// because "match or line" is the fact a person re-reading their own rules most
/// wants to see, and unlike a flag its absence has no natural reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightRule {
    /// Regular expression source, compiled by the app layer.
    ///
    /// Stored verbatim — not trimmed, not validated. Leading and trailing
    /// whitespace can be significant in a regex (` ERROR ` is a deliberately
    /// different pattern from `ERROR`), so nothing here touches it; a rule
    /// whose pattern is *entirely* blank is dropped by
    /// [`HighlightSettings::sanitize`](crate::HighlightSettings) instead, since
    /// that one can only ever be a leftover empty row from the dialog.
    pub pattern: String,
    /// Text colour, in the vocabulary [`HighlightColor::parse`] accepts.
    ///
    /// `None` leaves the foreground the log's own escape sequences chose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<String>,
    /// Background colour, in the vocabulary [`HighlightColor::parse`] accepts.
    ///
    /// `None` leaves the background alone, which is what all but the loudest
    /// rules want: a filled background is legible exactly because so little
    /// else uses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// Draw the highlighted span bold.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    /// How much of the line the colours cover.
    #[serde(default)]
    pub scope: HighlightScope,
    /// Match without regard to case. On unless the file says otherwise.
    ///
    /// On by default because the words worth highlighting are written every
    /// way a log format's author felt like writing them — `ERROR`, `Error`,
    /// `error` — and a user typing one of them into the dialog means all three.
    /// Turning it off is how a rule distinguishes `WARN` the level from `warn`
    /// the English word in a message.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub ignore_case: bool,
    /// Whether the rule is applied at all.
    ///
    /// A rule the user switched off is kept rather than deleted, because the
    /// pattern is the part that took work to write and switching it back on is
    /// the whole point of having the flag. Omitted from the JSON while it is
    /// on, so only a disabled rule carries the key.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

/// A colour a rule can be drawn in, resolved from its stored spelling.
///
/// Deliberately not serde-aware: what is persisted is the *string*, and this is
/// what the renderer gets after [`HighlightColor::parse`] has read it. Keeping
/// the two apart is what lets a spelling this build does not understand survive
/// a round trip through it — the string is stored, `parse` answers `None`, the
/// rule draws in the default colour, and a newer build that knows the spelling
/// draws it properly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightColor {
    /// A literal 24-bit colour, from a `#rrggbb` or `#rgb` spelling.
    Rgb {
        /// Red channel.
        r: u8,
        /// Green channel.
        g: u8,
        /// Blue channel.
        b: u8,
    },
    /// One of the sixteen ANSI slots of the session's colour scheme, `0..16` in
    /// the order [`HIGHLIGHT_COLOR_NAMES`] lists them.
    Slot(u8),
    /// The scheme's default text colour.
    Foreground,
    /// The scheme's default background colour.
    Background,
}

/// The sixteen ANSI slot names, in slot order.
///
/// `purple` rather than `magenta`, matching the scheme files `rulogman-term`
/// loads, which in turn match Windows Terminal's naming — the format those
/// files were first written in. `magenta` is accepted as an alias on the way
/// in; see [`HighlightColor::parse`].
const SLOT_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "purple",
    "cyan",
    "white",
    "bright_black",
    "bright_red",
    "bright_green",
    "bright_yellow",
    "bright_blue",
    "bright_purple",
    "bright_cyan",
    "bright_white",
];

/// Every colour name [`HighlightColor::parse`] accepts, canonically spelled.
///
/// The sixteen scheme slots in slot order, then the two scheme defaults. Public
/// so the settings dialog can list them in its hint text without keeping a
/// second copy that could drift; the aliases `parse` also takes are not here on
/// purpose, since the UI should teach the spelling the files use.
pub const HIGHLIGHT_COLOR_NAMES: [&str; 18] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "purple",
    "cyan",
    "white",
    "bright_black",
    "bright_red",
    "bright_green",
    "bright_yellow",
    "bright_blue",
    "bright_purple",
    "bright_cyan",
    "bright_white",
    "foreground",
    "background",
];

/// Expand a single hex digit into the byte `#rgb` means by it.
///
/// `f` is `0xff`, not `0xf0`: the short form is a scaled-down long form, so the
/// digit is repeated (`n * 0x11`) rather than shifted, which is what keeps
/// `#fff` white instead of a dark grey.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Parse the body of a `#`-prefixed colour, in either the 3- or 6-digit form.
fn parse_hex(body: &str) -> Option<HighlightColor> {
    let bytes = body.as_bytes();
    match bytes.len() {
        3 => {
            let r = hex_digit(bytes[0])? * 0x11;
            let g = hex_digit(bytes[1])? * 0x11;
            let b = hex_digit(bytes[2])? * 0x11;
            Some(HighlightColor::Rgb { r, g, b })
        }
        6 => {
            let r = hex_digit(bytes[0])? * 16 + hex_digit(bytes[1])?;
            let g = hex_digit(bytes[2])? * 16 + hex_digit(bytes[3])?;
            let b = hex_digit(bytes[4])? * 16 + hex_digit(bytes[5])?;
            Some(HighlightColor::Rgb { r, g, b })
        }
        _ => None,
    }
}

impl HighlightColor {
    /// Read a stored colour string.
    ///
    /// Surrounding whitespace is ignored, and every name is matched without
    /// regard to case. Accepted:
    ///
    /// * `#rrggbb` and the short `#rgb`, in either case of hex digit;
    /// * the sixteen scheme slots of [`HIGHLIGHT_COLOR_NAMES`], plus `magenta`
    ///   and `bright_magenta` as aliases for `purple` and `bright_purple` —
    ///   the scheme files spell that slot Windows Terminal's way, but every
    ///   ANSI reference in the world calls it magenta, and a user typing the
    ///   name they know should not get a rule that silently does nothing;
    /// * `foreground` and `background`, the scheme's own two defaults.
    ///
    /// Anything else — including an empty string — is `None`, which the
    /// renderer reads as "leave this channel alone". Nothing is rejected at
    /// *storage* time on the strength of this: see the type's own docs for why
    /// an unrecognised spelling is kept on disk rather than dropped.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if let Some(body) = text.strip_prefix('#') {
            return parse_hex(body);
        }
        if text.eq_ignore_ascii_case("foreground") {
            return Some(Self::Foreground);
        }
        if text.eq_ignore_ascii_case("background") {
            return Some(Self::Background);
        }
        if text.eq_ignore_ascii_case("magenta") {
            return Some(Self::Slot(5));
        }
        if text.eq_ignore_ascii_case("bright_magenta") {
            return Some(Self::Slot(13));
        }
        SLOT_NAMES
            .iter()
            .position(|name| text.eq_ignore_ascii_case(name))
            .map(|index| Self::Slot(index as u8))
    }
}

/// Build one preset rule, keeping the table below readable.
fn rule(
    pattern: &str,
    scope: HighlightScope,
    foreground: Option<&str>,
    background: Option<&str>,
    bold: bool,
) -> HighlightRule {
    HighlightRule {
        pattern: pattern.to_string(),
        foreground: foreground.map(str::to_string),
        background: background.map(str::to_string),
        bold,
        scope,
        ignore_case: true,
        enabled: true,
    }
}

/// The rules a pane uses when nobody has configured any.
///
/// Five severity levels, in the order the renderer resolves them — first match
/// wins — so `fatal` cannot be stolen by the `error` rule underneath it. The
/// vocabulary is the intersection of what the log formats people actually
/// follow print: syslog levels, Java and Python stack traces, Go's `log`, and
/// the handful of words (`panic`, `traceback`, `exception`) that mean an
/// unhandled failure whatever produced them. Every pattern is anchored on word
/// boundaries, so `error` does not light up `terror` or a path called
/// `/errors/`.
///
/// Two shapes of decision are worth spelling out.
///
/// **Slot names, not hex.** Each colour here names a slot of the user's colour
/// scheme, so a preset rule drawn over Solarized Light and over One Dark is
/// legible in both — a hard-coded `#ff5555` would be a rule that looks right in
/// whichever scheme it was picked against and muddy everywhere else. Hex is
/// there for the rule a user writes against a colour their scheme has no slot
/// for, which is a choice they are making with their own scheme in front of
/// them.
///
/// **Line scope for the severities, match scope for info.** A log line is one
/// record: when it is an error, the *whole record* is what the reader wants
/// pulled out of the wall of text, and colouring the five characters of the
/// word `ERROR` inside an otherwise grey line buries exactly the message that
/// matters. `info` is the opposite case — it is the level most lines already
/// are, so colouring every one of them would mean colouring nothing. There the
/// word itself is tinted, which marks the column without claiming the line.
pub fn preset() -> Vec<HighlightRule> {
    vec![
        rule(
            r"\b(fatal|panic|critical|emerg)\b",
            HighlightScope::Line,
            Some("bright_white"),
            Some("red"),
            true,
        ),
        rule(
            r"\b(error|err|exception|traceback|failed|failure)\b",
            HighlightScope::Line,
            Some("bright_red"),
            None,
            false,
        ),
        rule(
            r"\b(warn|warning)\b",
            HighlightScope::Line,
            Some("yellow"),
            None,
            false,
        ),
        rule(
            r"\b(debug|trace)\b",
            HighlightScope::Line,
            Some("bright_black"),
            None,
            false,
        ),
        rule(
            r"\b(info)\b",
            HighlightScope::Match,
            Some("green"),
            None,
            false,
        ),
    ]
}

/// Whether `rules` is exactly the built-in [`preset`].
///
/// The settings dialog uses this on the way *out*: a user who opened the
/// highlight editor, looked at the preset and changed nothing should have
/// `None` written back rather than a frozen copy of today's preset, so that
/// improvements to the built-in list keep reaching them. Only an equality
/// check, and deliberately so — a list that differs from the preset by one
/// disabled rule is a list the user built, and freezing it is the correct
/// reading of what they did.
pub fn is_preset(rules: &[HighlightRule]) -> bool {
    rules == preset()
}

/// The rules a pane following one file actually runs with.
///
/// Both levels are three-valued, and the third value is the reason either is an
/// `Option` rather than a `Vec`:
///
/// * `None` — nothing was said here; inherit from the level below.
/// * `Some(empty)` — highlighting is explicitly **off**. This is not the same
///   as `None`: a user who deleted every rule for one noisy file meant "stop
///   colouring this", and a `Vec` alone could not tell that apart from "I never
///   configured this file" and would silently hand them the preset back.
/// * `Some(rules)` — use exactly these.
///
/// `file` wins outright when it is `Some`, including when it is empty; a global
/// [`HighlightSettings::rules`](crate::HighlightSettings) that is `Some` is next;
/// and a global `None` — what a fresh install and every `settings.json` written
/// before highlighting existed both say — yields [`preset`].
///
/// The return is a [`Cow`] because the two configured cases can borrow the list
/// that is already in memory and only the preset has to be built; a caller that
/// needs to keep it can [`Cow::into_owned`] it.
pub fn effective_highlights<'a>(
    global: &'a HighlightSettings,
    file: Option<&'a [HighlightRule]>,
) -> Cow<'a, [HighlightRule]> {
    if let Some(rules) = file {
        return Cow::Borrowed(rules);
    }
    match global.rules.as_deref() {
        Some(rules) => Cow::Borrowed(rules),
        None => Cow::Owned(preset()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colours_parse_in_both_lengths_and_either_case() {
        assert_eq!(
            HighlightColor::parse("#ff8000"),
            Some(HighlightColor::Rgb {
                r: 0xff,
                g: 0x80,
                b: 0x00
            })
        );
        assert_eq!(
            HighlightColor::parse("#FF8000"),
            Some(HighlightColor::Rgb {
                r: 0xff,
                g: 0x80,
                b: 0x00
            })
        );
        // The short form repeats each digit rather than shifting it, so #fff
        // is white and not a dark grey.
        assert_eq!(
            HighlightColor::parse("#fff"),
            Some(HighlightColor::Rgb {
                r: 0xff,
                g: 0xff,
                b: 0xff
            })
        );
        assert_eq!(
            HighlightColor::parse("#0a3"),
            Some(HighlightColor::Rgb {
                r: 0x00,
                g: 0xaa,
                b: 0x33
            })
        );
    }

    #[test]
    fn every_slot_name_parses_to_its_own_index() {
        for (index, name) in SLOT_NAMES.iter().enumerate() {
            assert_eq!(
                HighlightColor::parse(name),
                Some(HighlightColor::Slot(index as u8)),
                "{name}"
            );
        }
        // Spot-check the two ends and the boundary between the halves.
        assert_eq!(
            HighlightColor::parse("black"),
            Some(HighlightColor::Slot(0))
        );
        assert_eq!(
            HighlightColor::parse("white"),
            Some(HighlightColor::Slot(7))
        );
        assert_eq!(
            HighlightColor::parse("bright_black"),
            Some(HighlightColor::Slot(8))
        );
        assert_eq!(
            HighlightColor::parse("bright_white"),
            Some(HighlightColor::Slot(15))
        );
    }

    #[test]
    fn magenta_is_an_alias_for_the_purple_slot() {
        // The scheme files spell it Windows Terminal's way; every ANSI table in
        // the world spells it the other. Both have to reach the same slot.
        assert_eq!(
            HighlightColor::parse("magenta"),
            HighlightColor::parse("purple")
        );
        assert_eq!(
            HighlightColor::parse("bright_magenta"),
            HighlightColor::parse("bright_purple")
        );
        assert_eq!(
            HighlightColor::parse("MAGENTA"),
            Some(HighlightColor::Slot(5))
        );
    }

    #[test]
    fn the_scheme_defaults_parse_by_name() {
        assert_eq!(
            HighlightColor::parse("foreground"),
            Some(HighlightColor::Foreground)
        );
        assert_eq!(
            HighlightColor::parse("Background"),
            Some(HighlightColor::Background)
        );
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(
            HighlightColor::parse("  bright_red \n"),
            Some(HighlightColor::Slot(9))
        );
        assert_eq!(
            HighlightColor::parse("\t#abc"),
            Some(HighlightColor::Rgb {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc
            })
        );
    }

    #[test]
    fn junk_is_none() {
        for junk in [
            "",
            "   ",
            "reddish",
            "#",
            "#12",
            "#12345",
            "#1234567",
            "#gggggg",
            // `u8::from_str_radix` would happily take a signed digit pair, so
            // the parser has to check the digits itself.
            "#+1+2+3",
            "0xff0000",
            "rgb(1,2,3)",
        ] {
            assert_eq!(HighlightColor::parse(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn the_published_name_list_is_the_slots_then_the_defaults() {
        assert_eq!(&HIGHLIGHT_COLOR_NAMES[..16], &SLOT_NAMES[..]);
        assert_eq!(HIGHLIGHT_COLOR_NAMES[16], "foreground");
        assert_eq!(HIGHLIGHT_COLOR_NAMES[17], "background");
        for name in HIGHLIGHT_COLOR_NAMES {
            assert!(HighlightColor::parse(name).is_some(), "{name}");
        }
    }

    #[test]
    fn the_preset_is_usable_as_written() {
        let preset = preset();
        assert!(!preset.is_empty());
        for rule in &preset {
            assert!(!rule.pattern.trim().is_empty());
            assert!(rule.enabled);
            assert!(rule.ignore_case);
            for colour in [&rule.foreground, &rule.background].into_iter().flatten() {
                assert!(
                    HighlightColor::parse(colour).is_some(),
                    "{colour} in {}",
                    rule.pattern
                );
            }
            // A rule that sets no colour and no weight would be invisible.
            assert!(rule.foreground.is_some() || rule.background.is_some() || rule.bold);
        }
    }

    #[test]
    fn the_preset_is_ordered_by_severity() {
        // First match wins in the renderer, so `fatal` has to be reachable:
        // were the `error` rule above it, a line saying "FATAL: ..." from a
        // logger that also prints "error" would take the wrong colours.
        let preset = preset();
        let index = |needle: &str| {
            preset
                .iter()
                .position(|rule| rule.pattern.contains(needle))
                .unwrap_or_else(|| panic!("no rule matching {needle}"))
        };
        assert!(index("fatal") < index("error"));
        assert!(index("error") < index("warn"));
        assert!(index("warn") < index("debug"));
        assert!(index("debug") < index("info"));
    }

    #[test]
    fn the_preset_colours_the_whole_line_except_for_info() {
        let preset = preset();
        for rule in &preset {
            let expected = if rule.pattern.contains("info") {
                HighlightScope::Match
            } else {
                HighlightScope::Line
            };
            assert_eq!(rule.scope, expected, "{}", rule.pattern);
        }
    }

    #[test]
    fn effective_highlights_falls_back_to_the_preset() {
        let global = HighlightSettings::default();
        assert_eq!(global.rules, None);
        assert_eq!(effective_highlights(&global, None).as_ref(), &preset()[..]);
    }

    #[test]
    fn effective_highlights_prefers_the_global_list_over_the_preset() {
        let mine = vec![rule("boom", HighlightScope::Line, Some("red"), None, false)];
        let global = HighlightSettings {
            rules: Some(mine.clone()),
        };
        assert_eq!(effective_highlights(&global, None).as_ref(), &mine[..]);
    }

    #[test]
    fn a_file_list_wins_over_the_global_one() {
        let global = HighlightSettings {
            rules: Some(vec![rule(
                "global",
                HighlightScope::Line,
                Some("red"),
                None,
                false,
            )]),
        };
        let per_file = vec![rule(
            "mine",
            HighlightScope::Match,
            Some("cyan"),
            None,
            false,
        )];
        assert_eq!(
            effective_highlights(&global, Some(&per_file)).as_ref(),
            &per_file[..]
        );
    }

    #[test]
    fn an_empty_file_list_turns_highlighting_off_rather_than_inheriting() {
        // The whole reason the per-file field is an Option: "I deleted every
        // rule for this file" must not read as "I never configured this file".
        let global = HighlightSettings {
            rules: Some(vec![rule(
                "global",
                HighlightScope::Line,
                Some("red"),
                None,
                false,
            )]),
        };
        let empty: Vec<HighlightRule> = Vec::new();
        assert!(effective_highlights(&global, Some(&empty)).is_empty());

        // And it beats the preset the same way.
        let unset = HighlightSettings::default();
        assert!(effective_highlights(&unset, Some(&empty)).is_empty());
    }

    #[test]
    fn effective_highlights_only_allocates_for_the_preset() {
        let global = HighlightSettings::default();
        assert!(matches!(effective_highlights(&global, None), Cow::Owned(_)));

        let configured = HighlightSettings {
            rules: Some(preset()),
        };
        assert!(matches!(
            effective_highlights(&configured, None),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn is_preset_recognises_only_the_untouched_list() {
        assert!(is_preset(&preset()));

        let mut edited = preset();
        edited[0].enabled = false;
        assert!(!is_preset(&edited));

        let mut reordered = preset();
        reordered.reverse();
        assert!(!is_preset(&reordered));

        assert!(!is_preset(&[]));
    }

    #[test]
    fn a_rule_writes_only_what_is_not_the_default() {
        let rule = HighlightRule {
            pattern: r"\bOOM\b".to_string(),
            foreground: Some("bright_red".to_string()),
            background: None,
            bold: false,
            scope: HighlightScope::Match,
            ignore_case: true,
            enabled: true,
        };
        let value = serde_json::to_value(&rule).expect("serialize");
        let object = value.as_object().expect("an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        // `scope` is written even at its default; the flags are not.
        assert_eq!(keys, ["foreground", "pattern", "scope"]);
        assert_eq!(value["scope"], serde_json::json!("match"));
    }

    #[test]
    fn a_minimal_rule_parses_with_the_documented_defaults() {
        let rule: HighlightRule =
            serde_json::from_str(r#"{"pattern": "boom", "foreground": "red"}"#).expect("parse");
        assert_eq!(rule.pattern, "boom");
        assert_eq!(rule.foreground.as_deref(), Some("red"));
        assert_eq!(rule.background, None);
        assert!(!rule.bold);
        assert_eq!(rule.scope, HighlightScope::Match);
        assert!(rule.ignore_case);
        assert!(rule.enabled);
    }

    #[test]
    fn the_off_switches_survive_a_round_trip() {
        let rule = HighlightRule {
            pattern: "boom".to_string(),
            foreground: None,
            background: Some("#102030".to_string()),
            bold: true,
            scope: HighlightScope::Line,
            ignore_case: false,
            enabled: false,
        };
        let json = serde_json::to_string(&rule).expect("serialize");
        assert!(json.contains("\"ignore_case\":false"), "{json}");
        assert!(json.contains("\"enabled\":false"), "{json}");
        assert_eq!(
            serde_json::from_str::<HighlightRule>(&json).expect("parse"),
            rule
        );
    }

    #[test]
    fn scope_serializes_in_snake_case() {
        assert_eq!(
            serde_json::to_value(HighlightScope::Line).unwrap(),
            serde_json::json!("line")
        );
        assert_eq!(
            serde_json::from_str::<HighlightScope>("\"match\"").unwrap(),
            HighlightScope::Match
        );
    }

    #[test]
    fn an_unknown_key_in_a_rule_is_ignored() {
        // Forward compatibility, exactly as everywhere else in this crate: a
        // rule written by a newer build still opens here.
        let rule: HighlightRule =
            serde_json::from_str(r#"{"pattern": "boom", "underline": true}"#).expect("parse");
        assert_eq!(rule.pattern, "boom");
    }
}
