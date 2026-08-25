//! Interface language: which locales exist, and which one is active.
//!
//! The translations themselves live in `crates/rulogman-app/locales/<tag>.yml`
//! and are compiled into the binary by `rust_i18n::i18n!` in [`crate`]'s root,
//! so nothing here touches the filesystem at run time. This module only decides
//! *which* locale `t!` should read from, and bridges it to the widget layer:
//! [`ts!`] gives back the [`SharedString`][gpui::SharedString] every gpui
//! builder wants and `t!` does not, and [`input_menu_labels`] is the wording
//! `ruui`'s text field asks for, since no widget there holds a string of its
//! own.
//!
//! The arithmetic under all of that — matching one tag against the set that
//! ships, and the resolution order — is [`ruui_shell::locale`], because it is
//! the same in all three applications. What cannot move is the table:
//! `rust-i18n` compiles a crate's locale files into *that* crate and keeps the
//! active locale in a process global, so the `i18n!` invocation, `ts!` and
//! `available_locales!()` stay here — which is also why the shell looks its own
//! words up through [`ruui_shell::Strings`] rather than translating anything
//! itself.
//!
//! Resolution order, applied by [`apply`] at start-up and again whenever the
//! settings dialog saves:
//!
//! 1. the tag stored in `settings.json`, when rulogman ships that language;
//! 2. the operating system's locale, matched loosely (see
//!    [`ruui_shell::locale::match_tag`]);
//! 3. English.
//!
//! Step 3 is also `rust-i18n`'s compile-time `fallback`, so a key missing from
//! a translation falls back per-key rather than switching the whole UI.
//!
//! # Adding a language
//!
//! Drop a `<BCP 47 tag>.yml` next to the others, translate every key of
//! `en.yml` — `language.name` included, since that is the endonym the settings
//! dialog lists the language under — and rebuild. No source file mentions the
//! set of languages, so none needs editing.

use std::sync::OnceLock;

use gpui::SharedString;
use ruui_shell::locale;

/// Translates a key and hands the result back as a [`SharedString`].
///
/// `rust-i18n` yields a `Cow<str>`, which no gpui builder accepts; every call
/// site would otherwise repeat the same conversion. Takes exactly the arguments
/// [`rust_i18n::t`] takes, interpolation included:
///
/// ```ignore
/// ts!("settings.title")
/// ts!("settings.save_failed", error = format!("{err:#}"))
/// ```
///
/// [`SharedString`]: gpui::SharedString
macro_rules! ts {
    ($($args:tt)*) => {
        ::gpui::SharedString::from(::rust_i18n::t!($($args)*).into_owned())
    };
}

pub(crate) use ts;

/// The four rows of the menu a right-click in a text field opens, in whatever
/// language is in force at the moment it is asked for.
///
/// `ruui`'s text field carries no words of its own — that is what lets a widget
/// kit be shared by applications that do not agree on a locale — so every
/// [`TextInput`](ruui::TextInput) rulogman builds is handed this and would
/// otherwise open no menu at all. Written as a function rather than a value
/// because [`TextInput::context_menu`](ruui::TextInput::context_menu) calls it
/// each time the menu opens, which is what keeps a field that was built before
/// the settings dialog changed the language from offering the old wording.
///
/// The wording itself is [`ruui_shell::input_menu_labels`], which reads the
/// same four `input.menu_*` keys through the [`Strings`](ruui_shell::Strings)
/// `main` installed — so the fields rulogman builds and the fields the shell
/// builds cannot come to disagree about what "Paste" is called.
pub fn input_menu_labels(cx: &gpui::App) -> ruui::InputMenuLabels {
    ruui_shell::input_menu_labels(cx)
}

/// The tags of the locale files compiled into the binary, sorted.
///
/// `available_locales!` hands back `Cow`s; owning them once in a `OnceLock`
/// turns them into the `&'static str`s the rest of the module passes around.
fn tags() -> &'static [String] {
    static TAGS: OnceLock<Vec<String>> = OnceLock::new();
    TAGS.get_or_init(|| {
        let mut tags: Vec<String> = rust_i18n::available_locales!()
            .into_iter()
            .map(std::borrow::Cow::into_owned)
            .collect();
        tags.sort();
        tags
    })
}

/// The shipped tags as the borrowed slice [`ruui_shell::locale`] takes.
fn codes() -> &'static [&'static str] {
    static CODES: OnceLock<Vec<&'static str>> = OnceLock::new();
    CODES.get_or_init(|| tags().iter().map(String::as_str).collect())
}

/// The locales rulogman ships translations for, as `(BCP 47 tag, endonym)`,
/// ordered by tag.
///
/// Derived from the locale files themselves rather than from a list kept in
/// this module, so shipping one more language is a matter of adding one more
/// file. The endonym comes from that file's `language.name`; it is written in
/// the language it names and is deliberately not translated, so caching it is
/// safe — unlike most lookups it does not depend on the active locale.
pub fn supported() -> &'static [(&'static str, SharedString)] {
    static SUPPORTED: OnceLock<Vec<(&'static str, SharedString)>> = OnceLock::new();
    SUPPORTED.get_or_init(|| {
        tags()
            .iter()
            .map(|tag| {
                let tag = tag.as_str();
                (tag, ts!("language.name", locale = tag))
            })
            .collect()
    })
}

/// The endonym of `tag`, or `None` when rulogman ships no such translation.
///
/// The lookup itself is the shell's, over the very table [`supported`] already
/// holds: it is generic over the endonym's string type, so a `SharedString`
/// goes through it without a copy being collected first.
pub fn display_name(tag: &str) -> Option<SharedString> {
    locale::display_name(supported(), tag).map(SharedString::new_static)
}

/// Make the resolved locale the one `t!` reads from.
///
/// `None`, a blank string, or a tag rulogman has no translation for all fall
/// through to the system locale, and from there to
/// [`FALLBACK`](ruui_shell::locale::FALLBACK).
pub fn apply(language: Option<&str>) {
    let system = sys_locale::get_locale();
    rust_i18n::set_locale(&locale::resolve(codes(), language, system.as_deref()));
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rust_i18n::t;
    use ruui_shell::locale::FALLBACK;

    use super::*;

    /// One key per top-level namespace of `en.yml`, chosen so that no
    /// translation of it legitimately coincides with the English wording.
    const PROBES: [&str; 15] = [
        "language.name",
        "common.save",
        "menu.new_session",
        "tab.close",
        "files.title",
        "settings.title",
        "empty.saved_profiles",
        "statusbar.idle",
        "connection.connect",
        "session.connecting",
        "terminal.menu_clear_scrollback",
        "input.menu_select_all",
        "editor.find",
        "about.title",
        "update.ignore",
    ];

    #[test]
    fn the_locale_files_carry_the_same_keys_and_no_yaml_traps() {
        // Both mistakes are invisible in a running application: a key missing
        // from a translation is answered in English by the per-key fallback,
        // and a value YAML swallows loads as an empty string. The shell owns
        // the check; the files are ours.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
        locale::check_locale_dir(&dir, codes());
    }

    #[test]
    fn the_shipped_languages_are_the_compiled_in_locales_in_tag_order() {
        let mut expected = rust_i18n::available_locales!();
        expected.sort();
        let tags: Vec<_> = supported().iter().map(|(tag, _)| *tag).collect();
        assert_eq!(tags, expected);
    }

    #[test]
    fn every_locale_translates_every_namespace() {
        // A key missing from a translation is answered in English by the
        // `fallback = "en"` of `i18n!`, so a silently mis-nested key would look
        // like a working lookup. Asserting that a non-English locale answers
        // with something *other* than English is what catches it.
        for (tag, _) in supported().iter().filter(|(tag, _)| *tag != FALLBACK) {
            for key in PROBES {
                assert_ne!(
                    t!(key, locale = *tag),
                    t!(key, locale = FALLBACK),
                    "{key} is untranslated in {tag}"
                );
            }
        }
    }

    #[test]
    fn every_language_names_itself_distinctly() {
        // `language.name` is what the settings dialog lists a language under,
        // so a file that omits it would show up as "English" — the per-key
        // fallback — and two entries would be indistinguishable. The
        // `every_locale_translates_every_namespace` probe already catches the
        // leak; this catches the collision, including one between two locales
        // that both spell out a name of their own.
        let mut seen: Vec<&SharedString> = Vec::new();
        for (tag, name) in supported() {
            assert!(!name.is_empty(), "{tag} names itself with an empty string");
            assert!(
                !seen.contains(&name),
                "{tag} shares the display name {name:?} with another locale"
            );
            seen.push(name);
        }
    }

    #[test]
    fn every_supported_tag_matches_itself() {
        for (tag, name) in supported() {
            assert_eq!(locale::match_tag(codes(), tag), Some(*tag), "tag {tag}");
            assert_eq!(display_name(tag).as_ref(), Some(name), "name of {tag}");
        }
    }

    #[test]
    fn a_region_with_no_file_of_its_own_takes_the_first_of_its_language() {
        // No `zh.yml` and no `zh-TW.yml`, so every Chinese tag reaches the one
        // Chinese translation there is through the primary-subtag rule; `ko-KR`
        // and `de_DE@euro` reach theirs the same way. The rule itself is the
        // shell's and is tested there; what is asserted here is that *these*
        // eight files are the set it is being asked about.
        let codes = codes();
        assert_eq!(locale::match_tag(codes, "zh"), Some("zh-CN"));
        assert_eq!(locale::match_tag(codes, "zh-TW"), Some("zh-CN"));
        assert_eq!(locale::match_tag(codes, "ko-KR"), Some("ko"));
        assert_eq!(locale::match_tag(codes, "de_DE@euro"), Some("de"));
        assert_eq!(locale::match_tag(codes, "xx-YZ"), None);
    }

    #[test]
    fn resolving_always_answers_with_a_supported_locale() {
        // Covers the system-locale and fallback branches without assuming what
        // the machine running the tests is set to.
        for language in [None, Some(""), Some("xx-YZ")] {
            let resolved = locale::resolve(codes(), language, None);
            assert!(
                supported().iter().any(|(code, _)| *code == resolved),
                "resolving {language:?} returned {resolved}"
            );
        }
        // A configured language wins over the system locale, and a region with
        // no file of its own still lands on its language.
        assert_eq!(locale::resolve(codes(), Some("ru"), Some("ja")), "ru");
        assert_eq!(locale::resolve(codes(), Some("zh_TW"), None), "zh-CN");
    }
}
