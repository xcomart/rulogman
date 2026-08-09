//! Detection, the invariants every lexer owes the renderer, and the helpers the
//! per-language tests are written with.

use super::custom::tests::lock_registry;
use super::*;

/// Asserts that `tokens` tile `line`: in order, no gaps, no overlaps, every
/// boundary on a character.
///
/// The invariant the renderer stands on. A gap would shorten a line's shaping
/// runs and slide every glyph after it; a boundary inside a character would
/// take the text system down. Every per-language test runs this over everything
/// it lexes, which is how six loops are held to one rule.
pub(super) fn tiles(line: &str, tokens: &[Token]) {
    let mut at = 0;
    for token in tokens {
        assert_eq!(token.start, at, "a gap or an overlap in {line:?}");
        assert!(token.end > token.start, "an empty token in {line:?}");
        assert!(
            line.is_char_boundary(token.start) && line.is_char_boundary(token.end),
            "a token boundary inside a character of {line:?}"
        );
        at = token.end;
    }
    assert_eq!(at, line.len(), "the tokens stopped short of {line:?}");
}

/// `tokens` as the pairs a test can read: what each one is, and what it covers.
pub(super) fn kinds<'a>(line: &'a str, tokens: &[Token]) -> Vec<(TokenKind, &'a str)> {
    tokens
        .iter()
        .map(|token| (token.kind, &line[token.start..token.end]))
        .collect()
}

/// Every built-in language, for the properties that hold of all of them.
///
/// A user-defined one is not here: what it does is decided by a file, and the
/// properties below are the ones this module decides. [`super::custom`] holds
/// the tests that install a definition and check it.
const ALL: [Language; 7] = [
    Language::Plain,
    Language::Shell,
    Language::Yaml,
    Language::Json,
    Language::Toml,
    Language::Conf,
    Language::Dockerfile,
];

#[test]
fn an_extension_names_the_language() {
    // Every test that expects a name to come out `Plain` holds the registry:
    // it is process-wide, and a definition another test installed would be
    // what answers for the names nothing built in claims.
    let _guard = lock_registry();
    for (name, expected) in [
        ("deploy.sh", Language::Shell),
        ("run.bash", Language::Shell),
        ("compose.yml", Language::Yaml),
        ("k8s.yaml", Language::Yaml),
        ("package.json", Language::Json),
        ("Cargo.toml", Language::Toml),
        ("php.ini", Language::Conf),
        ("nginx.conf", Language::Conf),
        ("app.cfg", Language::Conf),
        ("build.properties", Language::Conf),
        ("dev.env", Language::Conf),
        ("app.dockerfile", Language::Dockerfile),
        ("access.log", Language::Plain),
        ("README", Language::Plain),
    ] {
        assert_eq!(Language::detect(name, ""), expected, "{name}");
    }
}

#[test]
fn the_extension_is_matched_whatever_its_case() {
    assert_eq!(Language::detect("COMPOSE.YML", ""), Language::Yaml);
    assert_eq!(Language::detect("Setup.SH", ""), Language::Shell);
}

#[test]
fn a_name_with_no_extension_is_matched_whole() {
    for (name, expected) in [
        ("Dockerfile", Language::Dockerfile),
        ("dockerfile", Language::Dockerfile),
        ("Dockerfile.build", Language::Dockerfile),
        ("Containerfile", Language::Dockerfile),
        ("sshd_config", Language::Conf),
        (".bashrc", Language::Shell),
        (".zshrc", Language::Shell),
        (".profile", Language::Shell),
        (".env", Language::Conf),
        (".env.production", Language::Conf),
        (".gitconfig", Language::Conf),
    ] {
        assert_eq!(Language::detect(name, ""), expected, "{name}");
    }
}

#[test]
fn a_dotfile_is_not_all_extension() {
    let _guard = lock_registry();
    // The trap the whole-name table exists for: splitting `.bashrc` on its dot
    // leaves `bashrc`, which is not an extension anybody registers.
    assert_eq!(Language::detect(".bash_profile", ""), Language::Shell);
    // And an unknown one falls through to plain rather than to nonsense.
    assert_eq!(Language::detect(".unknownrc", ""), Language::Plain);
}

#[test]
fn a_hidden_file_still_gets_its_extension_read() {
    let _guard = lock_registry();
    // Only the leading dot is a hidden-file marker; the rest of the name works
    // the way it does on any other file, so `.claude.json` is as much JSON as
    // `claude.json` is.
    assert_eq!(Language::detect(".claude.json", ""), Language::Json);
    assert_eq!(Language::detect(".gitlab-ci.yml", ""), Language::Yaml);
    // An unknown extension on a hidden file is still just unknown.
    assert_eq!(Language::detect(".config.custom", ""), Language::Plain);
}

#[test]
fn a_shebang_speaks_only_for_a_name_with_no_extension() {
    let _guard = lock_registry();
    assert_eq!(Language::detect("deploy", "#!/bin/sh"), Language::Shell);
    assert_eq!(
        Language::detect("deploy", "#!/bin/bash -e"),
        Language::Shell
    );
    assert_eq!(
        Language::detect("deploy", "#!/usr/bin/env zsh"),
        Language::Shell
    );
    assert_eq!(
        Language::detect("deploy", "#!/usr/bin/python3"),
        Language::Plain
    );
    // A YAML file that opens with a shebang is still YAML.
    assert_eq!(Language::detect("play.yml", "#!/bin/sh"), Language::Yaml);
}

#[test]
fn a_path_is_read_from_its_last_segment() {
    assert_eq!(
        Language::detect("/etc/nginx/nginx.conf", ""),
        Language::Conf
    );
    assert_eq!(Language::detect(r"C:\src\bin.d\go.sh", ""), Language::Shell);
}

#[test]
fn only_json_refuses_the_comment_toggle() {
    for language in ALL {
        let comment = language.line_comment();
        if language == Language::Json {
            assert_eq!(comment, None);
        } else {
            assert_eq!(comment, Some("#"), "{language:?}");
        }
    }
}

#[test]
fn every_language_tiles_every_line_it_is_given() {
    // One pass over lines drawn from all six formats in each of them, which is
    // what an editor does the moment somebody opens the wrong file: the
    // guarantee is not that the colours are right but that the runs add up.
    let lines = [
        "",
        "   ",
        "# comment",
        "key: value # trailing",
        "[section]",
        r#"{"a": [1, 2.5, true, null]}"#,
        "RUN echo \"$HOME\" && exit 1",
        "text = \"\"\"open",
        "cat <<'EOF'",
        "a: |",
        "한글 = \"값\" # 주석",
        "🙂🙂🙂",
        "\\\"'`$${}[]<<>>::==",
    ];
    for language in ALL {
        // Threaded through, so that each line is also lexed from whatever state
        // the one before it left — which is where a lexer that mishandles its
        // own carry shows up.
        let mut state = LineState::START;
        for line in lines {
            let (tokens, next) = lex_line(line, state, language);
            tiles(line, &tokens);
            state = next;
        }
    }
}

#[test]
fn a_plain_buffer_is_one_run_a_line_and_carries_nothing() {
    let (tokens, state) = lex_line("anything at all", LineState::START, Language::Plain);
    assert_eq!(
        kinds("anything at all", &tokens),
        [(TokenKind::Plain, "anything at all")]
    );
    assert!(state.is_start());
    assert!(lex_line("", LineState::START, Language::Plain).0.is_empty());
}

#[test]
fn every_built_in_language_names_itself_for_the_picker() {
    // Proper names, spelled the way the formats spell themselves: these rows go
    // into a menu untranslated, so a lowercase `json` or a `Yaml` would be a
    // typo on screen in every locale at once.
    let names: Vec<&str> = ALL.iter().map(|language| language.name()).collect();
    assert_eq!(
        names,
        [
            "Plain Text",
            "Shell",
            "YAML",
            "JSON",
            "TOML",
            "Conf",
            "Dockerfile"
        ]
    );
}

#[test]
fn the_picker_lists_the_built_in_languages_first_and_in_order() {
    // Nothing registered, so the list is the built-in seven and stops there.
    // Plain text leads, being the answer to "colour none of this" rather than a
    // format among the others.
    let _guard = lock_registry();
    assert_eq!(Language::all(), ALL.to_vec());
}

#[test]
fn only_the_languages_with_something_to_remember_are_cached() {
    for language in ALL {
        assert_eq!(
            language.carries_state(),
            matches!(
                language,
                Language::Shell | Language::Yaml | Language::Toml | Language::Dockerfile
            ),
            "{language:?}"
        );
    }
}

#[test]
fn a_number_swallows_what_reads_as_part_of_it() {
    for (line, end) in [
        ("1", 1),
        ("1.5", 3),
        ("127.0.0.1", 9),
        ("2026-08-08", 10),
        ("12:00:00", 8),
        ("0xdeadBEEF", 10),
        ("1_000", 5),
        ("1e-3", 4),
        ("1.5e3x", 5),
    ] {
        assert_eq!(number(line, 0), end, "{line}");
    }
}

#[test]
fn stepping_over_a_character_never_lands_inside_one() {
    for line in ["a🙂b", "한글", "\u{1F1F0}\u{1F1F7}"] {
        let mut at = 0;
        while at < line.len() {
            assert!(line.is_char_boundary(at), "{line:?} at {at}");
            at += char_step(line, at);
        }
        assert_eq!(at, line.len());
    }
}

#[test]
fn a_heredoc_tag_that_does_not_fit_is_refused_rather_than_cut_short() {
    assert!(Heredoc::new("EOF", false).is_some());
    assert!(Heredoc::new("", false).is_none());
    assert!(Heredoc::new(&"x".repeat(TAG_LIMIT), false).is_some());
    assert!(Heredoc::new(&"x".repeat(TAG_LIMIT + 1), false).is_none());
}
