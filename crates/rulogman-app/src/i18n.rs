//! Interface language: which locales exist, and which one is active.
//!
//! The translations themselves live in `crates/rulogman-app/locales/<tag>.yml`
//! and are compiled into the binary by `rust_i18n::i18n!` in [`crate`]'s root,
//! so nothing here touches the filesystem. This module only decides *which*
//! locale `t!` should read from, and bridges it to the widget layer: [`ts!`]
//! gives back the [`SharedString`][gpui::SharedString] every gpui builder wants
//! and `t!` does not, and [`input_menu_labels`] is the wording `ruui`'s text
//! field asks for, since no widget there holds a string of its own.
//!
//! Resolution order, applied by [`apply`] at start-up and again whenever the
//! settings dialog saves:
//!
//! 1. the tag stored in `settings.json`, when rulogman ships that language;
//! 2. the operating system's locale, matched loosely (see [`match_tag`]);
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

/// Locale used when neither the settings nor the system offer a supported one.
///
/// Must stay in step with the `fallback` argument of `rust_i18n::i18n!`.
pub const FALLBACK: &str = "en";

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
pub fn input_menu_labels(_cx: &gpui::App) -> ruui::InputMenuLabels {
    ruui::InputMenuLabels {
        cut: ts!("input.menu_cut"),
        copy: ts!("input.menu_copy"),
        paste: ts!("input.menu_paste"),
        select_all: ts!("input.menu_select_all"),
    }
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
pub fn display_name(tag: &str) -> Option<SharedString> {
    supported()
        .iter()
        .find(|(code, _)| *code == tag)
        .map(|(_, name)| name.clone())
}

/// The locale to render the UI in, given the configured `language`.
///
/// `None`, a blank string, or a tag rulogman has no translation for all fall
/// through to the system locale, and from there to [`FALLBACK`].
pub fn resolve(language: Option<&str>) -> &'static str {
    if let Some(tag) = language.and_then(match_tag) {
        return tag;
    }
    sys_locale::get_locale()
        .as_deref()
        .and_then(match_tag)
        .unwrap_or(FALLBACK)
}

/// Make [`resolve`]'s answer the locale `t!` reads from.
pub fn apply(language: Option<&str>) {
    rust_i18n::set_locale(resolve(language));
}

/// Matches one locale identifier against the shipped locales.
///
/// Deliberately forgiving, because the string can come from a hand-edited
/// `settings.json` or from a platform that spells locales its own way:
/// case is ignored, the POSIX `_` separator is accepted alongside `-`, and any
/// trailing encoding or modifier suffix (`ko_KR.UTF-8`, `de_DE@euro`) is cut
/// off.
///
/// A tag with no exact match falls back to the first shipped locale — first in
/// tag order — sharing its primary subtag, so `ko-KR` finds `ko`, `en-GB` finds
/// `en`, and `zh-TW` finds `zh-CN` for as long as Simplified Chinese is the
/// only Chinese translation. Shipping `zh-TW.yml` would take over that exact
/// tag, while the remaining `zh-*` regions would keep landing on `zh-CN`.
fn match_tag(tag: &str) -> Option<&'static str> {
    let normalized: String = tag
        .trim()
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-")
        .to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }

    let codes = || supported().iter().map(|(code, _)| *code);
    if let Some(exact) = codes().find(|code| code.eq_ignore_ascii_case(&normalized)) {
        return Some(exact);
    }

    let primary = normalized.split('-').next().unwrap_or_default();
    codes().find(|code| {
        code.split('-')
            .next()
            .is_some_and(|shipped| shipped.eq_ignore_ascii_case(primary))
    })
}

#[cfg(test)]
mod tests {
    use rust_i18n::t;

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
    fn the_shipped_languages_are_the_compiled_in_locales_in_tag_order() {
        let mut expected = rust_i18n::available_locales!();
        expected.sort();
        let tags: Vec<_> = supported().iter().map(|(tag, _)| *tag).collect();
        assert_eq!(tags, expected);
        assert!(
            tags.contains(&FALLBACK),
            "the fallback locale ships no file of its own"
        );
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
            assert_eq!(match_tag(tag), Some(*tag), "tag {tag}");
            assert_eq!(display_name(tag).as_ref(), Some(name), "name of {tag}");
        }
    }

    #[test]
    fn matching_ignores_case_and_the_posix_separator() {
        assert_eq!(match_tag("KO"), Some("ko"));
        assert_eq!(match_tag("  ja  "), Some("ja"));
        assert_eq!(match_tag("zh_cn"), Some("zh-CN"));
        assert_eq!(match_tag("ZH-Hans-CN"), Some("zh-CN"));
    }

    #[test]
    fn matching_falls_back_to_the_primary_subtag() {
        assert_eq!(match_tag("ko-KR"), Some("ko"));
        assert_eq!(match_tag("en-GB"), Some("en"));
        assert_eq!(match_tag("es-419"), Some("es"));
        assert_eq!(match_tag("fr_CA.UTF-8"), Some("fr"));
        assert_eq!(match_tag("de_DE@euro"), Some("de"));
    }

    #[test]
    fn a_region_with_no_file_of_its_own_takes_the_first_of_its_language() {
        // No `zh.yml` and no `zh-TW.yml`, so every Chinese tag reaches the one
        // Chinese translation there is through the primary-subtag rule.
        assert_eq!(match_tag("zh"), Some("zh-CN"));
        assert_eq!(match_tag("zh-TW"), Some("zh-CN"));
        assert_eq!(match_tag("zh-Hant-HK"), Some("zh-CN"));
    }

    #[test]
    fn an_unknown_or_empty_tag_matches_nothing() {
        assert_eq!(match_tag(""), None);
        assert_eq!(match_tag("   "), None);
        assert_eq!(match_tag("xx-YZ"), None);
        assert_eq!(match_tag("kor"), None);
        // A prefix of a supported tag is still a different language.
        assert_eq!(match_tag("e"), None);
    }

    #[test]
    fn a_configured_language_wins_over_the_system_locale() {
        // The only branch of `resolve` that can be asserted without controlling
        // the environment: a supported tag never consults `sys_locale`.
        assert_eq!(resolve(Some("ru")), "ru");
        assert_eq!(resolve(Some("zh_TW")), "zh-CN");
    }

    #[test]
    fn resolve_always_answers_with_a_supported_locale() {
        // Covers the system-locale and fallback branches without assuming what
        // the machine running the tests is set to.
        for language in [None, Some(""), Some("xx-YZ")] {
            let resolved = resolve(language);
            assert!(
                supported().iter().any(|(code, _)| *code == resolved),
                "resolve({language:?}) returned {resolved}"
            );
        }
    }
}
