//! The user's own UI themes and terminal color schemes, as files.
//!
//! rulogman ships six themes and six schemes; anything beyond that comes from a
//! `*.json` file dropped into [`rulogman_core::ui_themes_dir`] or
//! [`rulogman_core::schemes_dir`]. Each file's stem is the id the theme or scheme
//! is selected by, so `~/.config/rulogman/schemes/tokyo-night.json` is the scheme
//! `tokyo-night`.
//!
//! Reading is deliberately forgiving, for the same reason `settings.json` is:
//! these files are meant to be edited by hand, and one broken file must not
//! keep the others — or the application — from loading. A file that cannot be
//! parsed is logged and skipped, as is one whose name collides with a built-in
//! id, since such an entry could never be selected anyway.
//!
//! The formats are [`crate::ui::ThemeFile`] and [`rulogman_term::SchemeFile`];
//! the latter is Windows Terminal's, so published palettes work unchanged.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use gpui::App;
use rulogman_core::paths;
use rulogman_term::{CustomScheme, SchemeFile, TerminalTheme};
use serde::de::DeserializeOwned;

use crate::ui::{CustomUiTheme, ThemeFile, ThemeRegistry};

/// Extension every theme and scheme file carries.
pub const FILE_EXTENSION: &str = "json";

/// Prefix of the ids made up for a theme whose name yields no slug.
pub const GENERATED_THEME_ID: &str = "theme";

/// Prefix of the ids made up for a scheme whose name yields no slug.
pub const GENERATED_SCHEME_ID: &str = "scheme";

/// Reads both directories and installs what they hold.
///
/// Called once at start-up — after [`crate::ui::init`] and before the configured
/// theme id is resolved, so that a theme of the user's own is already known by
/// the time the first frame is drawn — and again after every change rulogman
/// itself makes to the files, since both registries are swapped whole rather
/// than edited in place.
pub fn reload(cx: &mut App) {
    ThemeRegistry::set_custom(load_ui_themes(), cx);
    TerminalTheme::set_custom_schemes(load_schemes());
}

/// Every UI theme the user has put in the `themes` directory.
///
/// Never fails: a directory that does not exist yields no themes, and so does
/// one that cannot be read.
pub fn load_ui_themes() -> Vec<CustomUiTheme> {
    load_dir::<ThemeFile>(paths::ui_themes_dir(), "theme", ThemeRegistry::is_builtin)
        .into_iter()
        .map(|(id, file)| CustomUiTheme {
            name: display_name(&file.name, &id),
            theme: file.to_theme(),
            id,
        })
        .collect()
}

/// Every terminal color scheme the user has put in the `schemes` directory.
///
/// Never fails, for the same reasons [`load_ui_themes`] does not.
pub fn load_schemes() -> Vec<CustomScheme> {
    load_dir::<SchemeFile>(paths::schemes_dir(), "scheme", is_builtin_scheme)
        .into_iter()
        .map(|(id, file)| CustomScheme {
            name: display_name(&file.name, &id),
            theme: file.to_theme(),
            id,
        })
        .collect()
}

/// Writes `file` to the `themes` directory as the theme `id`.
///
/// # Errors
///
/// Fails when `id` has no usable slug, names a built-in theme, or the file
/// cannot be written.
pub fn save_ui_theme(id: &str, file: &ThemeFile) -> Result<PathBuf> {
    let id = validated_id(id, ThemeRegistry::is_builtin)?;
    save_json(&paths::ui_themes_dir()?, &id, file)
}

/// Writes `file` to the `schemes` directory as the scheme `id`.
///
/// # Errors
///
/// Fails when `id` has no usable slug, names a built-in scheme, or the file
/// cannot be written.
pub fn save_scheme(id: &str, file: &SchemeFile) -> Result<PathBuf> {
    let id = validated_id(id, is_builtin_scheme)?;
    save_json(&paths::schemes_dir()?, &id, file)
}

/// Removes the theme `id` from the `themes` directory.
///
/// A theme that is not there is not an error: the caller wanted it gone.
///
/// # Errors
///
/// Fails when `id` has no usable slug or the file cannot be removed.
pub fn delete_ui_theme(id: &str) -> Result<()> {
    let id = slug(id).with_context(|| format!("{id:?} is not a usable theme id"))?;
    delete_json(&paths::ui_themes_dir()?, &id)
}

/// Removes the scheme `id` from the `schemes` directory.
///
/// A scheme that is not there is not an error, as with [`delete_ui_theme`].
///
/// # Errors
///
/// Fails when `id` has no usable slug or the file cannot be removed.
pub fn delete_scheme(id: &str) -> Result<()> {
    let id = slug(id).with_context(|| format!("{id:?} is not a usable scheme id"))?;
    delete_json(&paths::schemes_dir()?, &id)
}

/// Turns a file stem or a typed name into an id.
///
/// Ids are lowercase and hold only `a`-`z`, `0`-`9` and `-`, so that the same
/// theme resolves whatever a platform's file system did to the case of its
/// name. Every other character becomes a separator, runs of separators
/// collapse, and leading and trailing ones are dropped. A name that leaves
/// nothing behind — one written entirely in a non-Latin script, say — answers
/// `None`, and the caller has to ask the user for a different one.
pub fn slug(value: &str) -> Option<String> {
    let mut slug = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    (!slug.is_empty()).then(|| slug.to_string())
}

/// The first id derived from `names` that nothing in `taken` answers to.
///
/// The candidates are tried in order and the first one with a usable slug wins
/// — a duplicated theme offers the copy's name, an imported file offers the
/// `name` its JSON carries and then its file stem — after which a `-2`, `-3`, …
/// suffix is appended until the id is free. When *no* candidate slugs, which is
/// what a name written entirely in a non-Latin script leaves behind, the id is
/// made up instead: `prefix-1`, `prefix-2`, and so on, again until one is free.
///
/// `taken` holds every id already spoken for, built-in and custom alike, and is
/// compared case-insensitively for the same reason ids are lowercased in the
/// first place: two files whose names differ only in case are one theme on a
/// case-insensitive file system.
pub fn unique_id(names: &[&str], prefix: &str, taken: &[String]) -> String {
    let free = |candidate: &str| !taken.iter().any(|id| id.eq_ignore_ascii_case(candidate));

    if let Some(base) = names.iter().find_map(|name| slug(name)) {
        if free(&base) {
            return base;
        }
        return (2u32..)
            .map(|suffix| format!("{base}-{suffix}"))
            .find(|candidate| free(candidate))
            .expect("an unbounded sequence always has a free id");
    }

    (1u32..)
        .map(|suffix| format!("{prefix}-{suffix}"))
        .find(|candidate| free(candidate))
        .expect("an unbounded sequence always has a free id")
}

/// Parses one theme or scheme file, wherever it sits.
///
/// Used by the import, which reads files the user picked from anywhere on the
/// disk rather than the ones already in the configuration directory. Tolerates
/// a leading byte order mark for the same reason [`load_dir`] does.
///
/// # Errors
///
/// Fails when the file cannot be read or does not parse as `T`.
pub fn read_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let data = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(paths::strip_bom(&data))
        .with_context(|| format!("failed to parse {}", path.display()))
}

/// Writes `value` to `path` as the pretty JSON a theme or scheme file is.
///
/// The counterpart of [`read_file`]: where the import reads from anywhere, the
/// export writes to anywhere, so this takes a whole path instead of an id.
///
/// # Errors
///
/// Fails when `value` cannot be serialized or the file cannot be written.
pub fn write_file<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let json = serde_json::to_vec_pretty(value)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    paths::write_atomic(path, &json)
}

/// Whether `id` names a scheme that ships with rulogman.
fn is_builtin_scheme(id: &str) -> bool {
    TerminalTheme::builtin()
        .iter()
        .any(|scheme| scheme.id.eq_ignore_ascii_case(id))
}

/// The name to show for a file, falling back to its id when it carries none.
fn display_name(name: &str, id: &str) -> String {
    if name.trim().is_empty() {
        id.to_string()
    } else {
        name.trim().to_string()
    }
}

/// The id a file may be saved under, or why it may not be.
fn validated_id(id: &str, is_builtin: fn(&str) -> bool) -> Result<String> {
    let slug = slug(id).with_context(|| format!("{id:?} is not a usable id"))?;
    if is_builtin(&slug) {
        bail!("{slug} is the id of a theme that ships with rulogman");
    }
    Ok(slug)
}

/// Serializes `value` into `dir/id.json`, atomically.
fn save_json<T: serde::Serialize>(dir: &Path, id: &str, value: &T) -> Result<PathBuf> {
    let path = dir.join(format!("{id}.{FILE_EXTENSION}"));
    write_file(&path, value)?;
    Ok(path)
}

/// Removes `dir/id.json`, treating an absent file as success.
fn delete_json(dir: &Path, id: &str) -> Result<()> {
    let path = dir.join(format!("{id}.{FILE_EXTENSION}"));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// Parses every `*.json` file in `dir`, paired with the id of its file name.
///
/// `kind` names what is being loaded and appears in the log messages; it is the
/// only thing that differs between the theme and the scheme directory.
/// Malformed files, unusable names and ids that shadow a built-in one are
/// logged and skipped. The result is ordered by id, because `read_dir` reports
/// no order of its own and a picker that reshuffles itself between runs is
/// worse than an arbitrary but stable one.
fn load_dir<T: DeserializeOwned>(
    dir: Result<PathBuf>,
    kind: &str,
    is_builtin: fn(&str) -> bool,
) -> Vec<(String, T)> {
    let dir = match dir {
        Ok(dir) => dir,
        Err(err) => {
            log::warn!("cannot locate the {kind} directory: {err:#}");
            return Vec::new();
        }
    };
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        // A user who has never added one simply has no directory.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            log::warn!("cannot read {}: {err}", dir.display());
            return Vec::new();
        }
    };

    let mut loaded: Vec<(String, T)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case(FILE_EXTENSION))
        {
            continue;
        }

        let Some(id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(slug)
        else {
            log::warn!("skipping {}: its name yields no usable id", path.display());
            continue;
        };
        if is_builtin(&id) {
            log::warn!(
                "skipping {}: {id} is the id of a {kind} that ships with rulogman",
                path.display()
            );
            continue;
        }
        if loaded.iter().any(|(loaded, _)| *loaded == id) {
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
        match serde_json::from_slice::<T>(paths::strip_bom(&data)) {
            Ok(value) => loaded.push((id, value)),
            Err(err) => log::warn!("skipping {}: {err}", path.display()),
        }
    }

    loaded.sort_by(|(left, _), (right, _)| left.cmp(right));
    loaded
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ui::Theme;

    #[test]
    fn slugs_are_lowercase_and_hyphenated() {
        assert_eq!(slug("Tokyo Night").as_deref(), Some("tokyo-night"));
        assert_eq!(slug("my_theme.v2").as_deref(), Some("my-theme-v2"));
        assert_eq!(
            slug("--Solarized--Dark--").as_deref(),
            Some("solarized-dark")
        );
        assert_eq!(slug("ONE").as_deref(), Some("one"));
    }

    #[test]
    fn a_name_with_nothing_to_slug_has_no_id() {
        assert_eq!(slug(""), None);
        assert_eq!(slug("   "), None);
        assert_eq!(slug("---"), None);
        assert_eq!(slug("테마"), None);
    }

    #[test]
    fn load_dir_reads_every_valid_file_and_skips_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();

        let write = |name: &str, contents: &[u8]| {
            fs::write(root.join(name), contents).expect("write");
        };
        let theme = ThemeFile::from_theme("Zed Ish", &Theme::dracula());
        let json = serde_json::to_vec(&theme).expect("serialize");

        write("Zed Ish.json", &json);
        // A leading byte order mark is what a Windows editor leaves behind.
        let mut with_bom = b"\xEF\xBB\xBF".to_vec();
        with_bom.extend_from_slice(&json);
        write("another.json", &with_bom);
        // Skipped: malformed, wrong extension, and a built-in id.
        write("broken.json", b"{ nope");
        write("notes.txt", &json);
        write("dracula.json", &json);

        let loaded = load_dir::<ThemeFile>(Ok(root), "theme", ThemeRegistry::is_builtin);
        let ids: Vec<&str> = loaded.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["another", "zed-ish"]);
        assert_eq!(loaded[0].1.name, "Zed Ish");
    }

    #[test]
    fn a_missing_directory_yields_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let absent = dir.path().join("never-created");
        assert!(load_dir::<ThemeFile>(Ok(absent), "theme", ThemeRegistry::is_builtin).is_empty());
    }

    #[test]
    fn save_and_delete_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("themes");
        let file = ThemeFile::from_theme("Mine", &Theme::light());

        let path = save_json(&root, "mine", &file).expect("save");
        assert!(path.exists());

        let loaded = load_dir::<ThemeFile>(Ok(root.clone()), "theme", ThemeRegistry::is_builtin);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, "mine");
        assert_eq!(loaded[0].1, file);

        delete_json(&root, "mine").expect("delete");
        assert!(!path.exists());
        // Deleting what is already gone is not an error.
        delete_json(&root, "mine").expect("delete again");
    }

    #[test]
    fn a_free_id_is_used_as_it_stands() {
        let taken = ["one-dark".to_string(), "dracula".to_string()];
        assert_eq!(unique_id(&["Tokyo Night"], "theme", &taken), "tokyo-night");
        assert_eq!(unique_id(&["Tokyo Night"], "theme", &[]), "tokyo-night");
    }

    #[test]
    fn a_taken_id_gains_the_first_free_suffix() {
        let taken = [
            "dracula".to_string(),
            "dracula-2".to_string(),
            "dracula-3".to_string(),
        ];
        assert_eq!(unique_id(&["Dracula"], "theme", &taken), "dracula-4");
        // The comparison ignores case, since the ids themselves do.
        assert_eq!(
            unique_id(&["Dracula"], "theme", &["DRACULA".to_string()]),
            "dracula-2"
        );
    }

    #[test]
    fn the_first_candidate_that_slugs_wins() {
        // What an import does: the file's own `name` first, its stem second.
        assert_eq!(unique_id(&["테마", "my-file"], "theme", &[]), "my-file");
        assert_eq!(unique_id(&["", "  ", "Kept"], "theme", &[]), "kept");
    }

    #[test]
    fn a_name_with_nothing_to_slug_gets_a_generated_id() {
        assert_eq!(unique_id(&["테마"], "theme", &[]), "theme-1");
        assert_eq!(unique_id(&["테마", "---"], "scheme", &[]), "scheme-1");
        let taken = ["theme-1".to_string(), "theme-2".to_string()];
        assert_eq!(unique_id(&["테마"], "theme", &taken), "theme-3");
        // No candidates at all is the same situation as no usable one.
        assert_eq!(unique_id(&[], "scheme", &[]), "scheme-1");
    }

    #[test]
    fn a_picked_file_is_parsed_however_it_was_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = ThemeFile::from_theme("Imported", &Theme::gruvbox_dark());
        let json = serde_json::to_vec(&file).expect("serialize");

        let plain = dir.path().join("plain.json");
        fs::write(&plain, &json).expect("write");
        assert_eq!(read_file::<ThemeFile>(&plain).expect("plain"), file);

        // A byte order mark is what a Windows editor leaves behind, and a
        // published palette is as likely to carry one as a hand-written file.
        let marked = dir.path().join("marked.json");
        let mut with_bom = b"\xEF\xBB\xBF".to_vec();
        with_bom.extend_from_slice(&json);
        fs::write(&marked, &with_bom).expect("write");
        assert_eq!(read_file::<ThemeFile>(&marked).expect("marked"), file);

        // A file that is not a theme at all is an error rather than a panic.
        let broken = dir.path().join("broken.json");
        fs::write(&broken, b"{ nope").expect("write");
        assert!(read_file::<ThemeFile>(&broken).is_err());
        assert!(read_file::<ThemeFile>(&dir.path().join("absent.json")).is_err());
    }

    #[test]
    fn an_exported_file_reads_back_as_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("exported").join("mine.json");
        let file = ThemeFile::from_theme("Mine", &Theme::solarized_light());

        write_file(&path, &file).expect("write");
        assert_eq!(read_file::<ThemeFile>(&path).expect("read"), file);
    }

    #[test]
    fn a_builtin_id_cannot_be_saved_over() {
        assert!(validated_id("Dracula", ThemeRegistry::is_builtin).is_err());
        assert!(validated_id("one-dark", is_builtin_scheme).is_err());
        assert!(validated_id("   ", ThemeRegistry::is_builtin).is_err());
        assert_eq!(
            validated_id("My Theme", ThemeRegistry::is_builtin).expect("id"),
            "my-theme"
        );
    }
}
