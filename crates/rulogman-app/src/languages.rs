//! Which languages this application can colour a file as, and how a file is
//! matched to one.
//!
//! [`rugpui_editor`] knows how to *lex* eighteen languages and how to keep a
//! table of them — [`LanguageRegistry`] is its value type, deliberately a value
//! and not a global, because where the table lives is the application's
//! question. This module is rulogman's answer: one registry, built once at
//! start-up out of three sources and installed as a gpui global that every
//! editor pane and the status bar's picker read.
//!
//! # The three sources, in the order they are searched
//!
//! 1. **The widget's own built-in table.** The configuration formats a file
//!    panel over a server reaches every day, and the languages with a lexer of
//!    their own. Nothing can take one of these over: a definition dropped into
//!    the `syntaxes` directory can *add* a language but a `yaml.yml` of
//!    somebody's own does not change what a `.yaml` file is.
//! 2. **The user's definitions**, one `*.yml` (or `*.yaml`) file per language
//!    in [`paths::syntaxes_dir`]. The file's stem is the language's id, so
//!    `nginx.yml` defines `nginx`.
//! 3. **The definitions rulogman ships** — [`SHIPPED`] — for the languages the
//!    widget has no lexer for. They are ordinary definition files with no
//!    privileges of their own: one whose id a built-in or a user file has
//!    already claimed is dropped rather than registered a second time, which is
//!    how `python.yml` of the user's own replaces ours outright.
//!
//! Within 2 and 3 together the search order is alphabetical by the language's
//! *name*, which is [`LanguageRegistry::register`]'s doing and is the order the
//! picker lists them in: a list of thirty formats is read that way or not at
//! all. Two definitions claiming the same extension therefore resolve the same
//! way on every machine and every launch.
//!
//! **The directory is read once, at start-up.** Adding, changing or removing a
//! definition takes effect on the next launch. Reading is forgiving, exactly as
//! [`crate::theme_store`] is with themes and schemes: a file that does not
//! parse is logged and skipped, and so is a single rule inside a file that
//! cannot be honoured. One broken definition must not cost the user the others.
//!
//! The schema, whole, is documented at the head of `rugpui_editor::lang::custom`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use gpui::{App, Global, SharedString};
use rugpui_editor::LanguageEntry;
use rugpui_editor::lang::LanguageRegistry;
use rugpui_editor::lang::custom::Definition;
use rulogman_core::paths;

use crate::i18n::ts;

/// The id of the entry that colours nothing, which is the widget's first row
/// and the one this module translates.
pub const PLAIN: &str = "plain";

/// What a definition file may be called.
const FILE_EXTENSIONS: [&str; 2] = ["yml", "yaml"];

/// The definitions rulogman ships, `(id, source)`.
///
/// Compiled in rather than written to disk, so there is no copy to edit and
/// none to go stale, and so that a fresh installation colours a `.c` file
/// before anybody has put anything anywhere. Ten files, of which the ones the
/// widget has since grown a real lexer for are skipped at registration — see
/// [`assembled`] — leaving them here as what they also are: the corpus the
/// "our own definitions need no forgiving" test runs over.
const SHIPPED: [(&str, &str); 10] = [
    ("c", include_str!("../syntaxes/c.yml")),
    ("cpp", include_str!("../syntaxes/cpp.yml")),
    ("csharp", include_str!("../syntaxes/csharp.yml")),
    ("go", include_str!("../syntaxes/go.yml")),
    ("java", include_str!("../syntaxes/java.yml")),
    ("javascript", include_str!("../syntaxes/javascript.yml")),
    ("python", include_str!("../syntaxes/python.yml")),
    ("rust", include_str!("../syntaxes/rust.yml")),
    ("sql", include_str!("../syntaxes/sql.yml")),
    ("typescript", include_str!("../syntaxes/typescript.yml")),
];

/// The one registry, held where every window can reach it.
///
/// An [`Arc`] rather than the registry itself so that a pane can hold the table
/// it looked its language up in for as long as it is open, without borrowing
/// the application context to read a name.
struct Languages(Arc<LanguageRegistry>);

impl Global for Languages {}

/// Reads the `syntaxes` directory and installs the registry over it.
///
/// The one call, made at start-up beside the theme and scheme load. Never
/// fails: a directory that is not there, or cannot be read, leaves the built-in
/// and shipped languages alone and the editor keeps every language it was born
/// with.
pub fn init(cx: &mut App) {
    let registry = assembled(load(paths::syntaxes_dir()));
    cx.set_global(Languages(Arc::new(registry)));
}

/// The registry [`init`] installed.
///
/// Falls back to the widget's own table when nothing has been installed, which
/// is what a test that never called [`init`] sees rather than a panic.
pub fn registry(cx: &App) -> Arc<LanguageRegistry> {
    cx.try_global::<Languages>()
        .map_or_else(|| Arc::new(LanguageRegistry::builtin()), |it| it.0.clone())
}

/// What the status bar and its picker call `entry`.
///
/// Every name but one comes from the registry, because every name but one is a
/// proper name: `YAML` is `YAML` in every locale, and a definition's name is
/// whatever its author wrote. Plain text is the exception — it describes a file
/// rather than naming a format, and a reader of a translated interface should
/// find it in their own language — so that one row is looked up here, where the
/// strings are.
pub fn language_label(entry: &LanguageEntry) -> SharedString {
    if entry.id == PLAIN {
        ts!("editor.language_plain")
    } else {
        entry.name.clone()
    }
}

/// The registry built out of the widget's table, `user`'s definitions and
/// whatever of [`SHIPPED`] is left over.
///
/// A shipped definition whose id is already spoken for is dropped rather than
/// registered beside the one that has it. That is one rule serving two
/// purposes: a `python.yml` of the user's own *replaces* ours instead of
/// standing next to a second language also called Python, and the seven
/// languages the widget has since grown a hand-written lexer for — Java, SQL,
/// Go, Rust, Python, C# and TypeScript — are lexed by that rather than by a
/// keyword list of ours, with no duplicate row in the picker to choose wrongly
/// from.
fn assembled(user: Vec<Definition>) -> LanguageRegistry {
    let mut registry = LanguageRegistry::builtin();
    for definition in user {
        registry.register(definition.into_entry());
    }
    for (id, source) in SHIPPED {
        if registry.get(id).is_some() {
            continue;
        }
        match Definition::parse_with_warnings(source) {
            Ok((mut definition, warnings)) => {
                // Our own file, so a complaint here is a bug in this repository
                // rather than something the user can fix. The test over the
                // shipped set is what keeps it from ever being logged.
                for warning in warnings {
                    log::error!("the shipped {id} definition: {warning}");
                }
                definition.id = id.to_string();
                registry.register(definition.into_entry());
            }
            Err(err) => log::error!("the shipped {id} definition does not parse: {err:#}"),
        }
    }
    registry
}

/// Every definition file in `dir`, in id order.
///
/// Forgiving throughout: a missing directory, an unreadable file, a name that
/// yields no id and a file that is not a definition at all are each logged and
/// skipped. The one thing that is not tolerated is two files claiming one id,
/// where the second is dropped — the registry would otherwise hold two entries
/// [`LanguageRegistry::get`] could not tell apart.
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
        let text = String::from_utf8_lossy(paths::strip_bom(&data));
        match Definition::parse_with_warnings(&text) {
            Ok((mut definition, warnings)) => {
                for warning in warnings {
                    log::warn!("{}: {warning}", path.display());
                }
                // The file's stem, not whatever the file said its id was: it is
                // the half of the definition the user can see and rename.
                definition.id = id;
                loaded.push(definition);
            }
            Err(err) => log::warn!("skipping {}: {err:#}", path.display()),
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
mod tests {
    use super::*;

    /// The ids of [`SHIPPED`] the widget's own table already claims, and which
    /// [`assembled`] is therefore expected to drop.
    fn shadowed() -> Vec<&'static str> {
        let builtin = LanguageRegistry::builtin();
        SHIPPED
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| builtin.get(id).is_some())
            .collect()
    }

    #[test]
    fn every_shipped_definition_parses_and_needs_no_forgiving() {
        // All ten, including the ones `assembled` goes on to drop: a definition
        // that stopped parsing would be a bug in this repository, and this is
        // where it is caught. A warning is a rule that could not be honoured,
        // which in a file of our own is the same kind of bug.
        for (id, source) in SHIPPED {
            let (definition, warnings) =
                Definition::parse_with_warnings(source).unwrap_or_else(|err| {
                    panic!("the shipped {id} definition does not parse: {err:#}")
                });
            assert!(
                warnings.is_empty(),
                "the shipped {id} definition was forgiven {warnings:?}"
            );
            assert!(
                !definition.name.is_empty(),
                "the shipped {id} definition names itself nothing"
            );
        }
    }

    #[test]
    fn the_shipped_definitions_fill_the_gaps_and_never_double_a_language() {
        let registry = assembled(Vec::new());
        let shadowed = shadowed();
        // Every id is registered exactly once, whatever the source.
        let mut ids: Vec<&str> = registry
            .all()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two entries share an id");
        // What the widget does not lex is here, and is lexed.
        for (id, _) in SHIPPED {
            let entry = registry
                .get(id)
                .unwrap_or_else(|| panic!("{id} is missing"));
            assert!(entry.highlighter.is_some(), "{id} colours nothing");
            if shadowed.contains(&id) {
                // The widget's own lexer answers for it, not our keyword list.
                assert_eq!(
                    entry.name,
                    LanguageRegistry::builtin().get(id).unwrap().name,
                    "{id} was taken over by a shipped definition"
                );
            }
        }
        // And the two the widget has no lexer at all for are the ones this
        // table is really for.
        assert_eq!(registry.detect("main.c", "").id, "c");
        assert_eq!(registry.detect("main.cpp", "").id, "cpp");
    }

    #[test]
    fn a_definition_of_the_user_s_own_is_registered_and_a_broken_one_is_skipped() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(
            dir.path().join("nginx.yml"),
            "name: Nginx\nfiles:\n  extensions: [nginx]\ncomment: \"#\"\n",
        )
        .expect("the good definition");
        // Not YAML at all, which is the failure that must not be fatal.
        std::fs::write(dir.path().join("broken.yml"), "name: [unterminated\n")
            .expect("the broken definition");
        // A definition claiming an id the widget already has. It is registered
        // — a definition may add a language — but it may not take `.yaml` over.
        std::fs::write(
            dir.path().join("yaml.yml"),
            "name: Not YAML\nfiles:\n  extensions: [yaml, yml]\n",
        )
        .expect("the shadowing definition");
        // Not a definition file, and not to be read as one.
        std::fs::write(dir.path().join("notes.txt"), "not a language\n").expect("the stray file");

        let loaded = load(Ok(dir.path().to_path_buf()));
        let ids: Vec<&str> = loaded.iter().map(|it| it.id.as_str()).collect();
        assert_eq!(ids, ["nginx", "yaml"]);

        let registry = assembled(loaded);
        assert_eq!(registry.detect("site.nginx", "").id, "nginx");
        // A `.conf` still belongs to the built-in table, which is searched
        // first: a definition may add a language and may not take one over.
        assert_eq!(registry.detect("nginx.conf", "").id, "conf");
        // The built-in table is searched first, so the shadowing definition
        // added a row to the picker and took nothing over.
        assert_eq!(registry.detect("compose.yaml", "").id, "yaml");
        assert_eq!(
            registry.get("yaml").map(|it| it.name.as_ref()),
            Some("YAML")
        );
        assert!(registry.all().iter().any(|it| it.name == "Not YAML"));
    }

    #[test]
    fn a_syntaxes_directory_that_is_not_there_costs_nothing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        assert!(load(Ok(dir.path().join("absent"))).is_empty());
        assert!(load(Err(anyhow::anyhow!("no configuration directory"))).is_empty());
        // And the registry is still every language the application ships.
        assert!(assembled(Vec::new()).get("c").is_some());
    }
}
