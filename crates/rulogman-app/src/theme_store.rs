//! The user's own UI themes and terminal color schemes, as files.
//!
//! rulogman ships six themes and six schemes; anything beyond that comes from a
//! `*.json` file dropped into [`rulogman_core::ui_themes_dir`] or
//! [`rulogman_core::schemes_dir`]. Each file's stem is the id the theme or scheme
//! is selected by, so `~/.config/rulogman/schemes/tokyo-night.json` is the scheme
//! `tokyo-night`.
//!
//! Only half of that is this module's own work. UI themes are a `rugpui` format
//! read by `rugpui`'s own store, which knows how to walk a directory of them and
//! is shared with every other application drawing with the same widgets; all
//! this module does for them is say where rulogman keeps its directory, through
//! [`theme_dirs`], and re-export the naming and file handling the rest of the
//! application calls. Terminal colour schemes have no counterpart there — a
//! widget kit has no terminal — so their half stays here, and
//! [`crate::scheme_catalog`] is what puts it in front of the shell's palette
//! editor.
//!
//! Reading is deliberately forgiving, for the same reason `settings.json` is:
//! these files are meant to be edited by hand, and one broken file must not
//! keep the others — or the application — from loading. A file that cannot be
//! parsed is logged and skipped, as is one whose name collides with a built-in
//! id, since such an entry could never be selected anyway.
//!
//! The formats are [`rugpui::ThemeFile`] and [`rulogman_term::SchemeFile`]; the
//! latter is Windows Terminal's, so published palettes work unchanged.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use gpui::App;
use rugpui::ThemeRegistry;
use rugpui::theme_store::ThemeDirs;
use rulogman_core::paths;
use rulogman_term::{CustomScheme, SchemeFile, TerminalTheme};
use serde::de::DeserializeOwned;

/// Naming, parsing and writing are the same job for both catalogues, and `rugpui`
/// already does it for the one it owns. Re-exported rather than wrapped so that
/// there is exactly one slug function in the process: an id computed two ways
/// is an id that eventually disagrees with itself.
pub use rugpui::theme_store::{FILE_EXTENSION, read_file, slug, write_file};

/// Prefix of the ids made up for a scheme whose name yields no slug.
pub const GENERATED_SCHEME_ID: &str = "scheme";

/// Where `rugpui` should look for the user's own UI themes.
///
/// Neither the widget library nor the shell above it has a configuration
/// directory of its own, and neither ever guesses at one, so every call into
/// the store carries this. rulogman names no editor theme directory: its editor
/// is coloured from the terminal scheme in force, not from a palette of its
/// own.
///
/// # Errors
///
/// Fails when the platform will not say where the configuration directory is.
pub fn theme_dirs() -> Result<ThemeDirs> {
    Ok(ThemeDirs {
        ui_themes: paths::ui_themes_dir()?,
        editor_themes: None,
    })
}

/// The same directory, empty where there is no configuration directory at all.
///
/// [`theme_dirs`] is the fallible answer, and everything about to *write* wants
/// it. This is for the catalogue the settings dialog builds at construction,
/// which has to exist before anyone asks it for anything: [`ThemeDirs`]'s own
/// default is the "no directory yet" one, which holds no themes and refuses
/// every write — the same outcome as the error, reported at the moment the
/// user can see it rather than while a dialog is being assembled.
pub fn theme_dirs_or_empty() -> ThemeDirs {
    theme_dirs().unwrap_or_else(|err| {
        log::warn!("cannot locate the theme directory: {err:#}");
        ThemeDirs::default()
    })
}

/// Reads both directories and installs what they hold.
///
/// Called once at start-up — after [`rugpui::init`] and before the configured
/// theme id is resolved, so that a theme of the user's own is already known by
/// the time the first frame is drawn — and again after every change rulogman
/// itself makes to the files, since both registries are swapped whole rather
/// than edited in place.
///
/// A configuration directory that cannot be located leaves the theme registry
/// holding nothing rather than holding what a previous reload put there: the
/// registries are replaced, never merged, and a stale custom theme surviving a
/// failed reload would be a palette the picker offers and the files no longer
/// describe.
pub fn reload(cx: &mut App) {
    match theme_dirs() {
        Ok(dirs) => rugpui::theme_store::reload(&dirs, cx),
        Err(err) => {
            log::warn!("cannot locate the theme directory: {err:#}");
            ThemeRegistry::set_custom(Vec::new(), cx);
        }
    }
    TerminalTheme::set_custom_schemes(load_schemes());
}

/// Every terminal color scheme the user has put in the `schemes` directory.
///
/// Never fails: a directory that does not exist yields no schemes, and so does
/// one that cannot be read.
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

/// Removes the scheme `id` from the `schemes` directory.
///
/// A scheme that is not there is not an error: the caller wanted it gone.
///
/// # Errors
///
/// Fails when `id` has no usable slug or the file cannot be removed.
pub fn delete_scheme(id: &str) -> Result<()> {
    let id = slug(id).with_context(|| format!("{id:?} is not a usable scheme id"))?;
    delete_json(&paths::schemes_dir()?, &id)
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

/// The id a scheme may be saved under, or why it may not be.
fn validated_id(id: &str, is_builtin: fn(&str) -> bool) -> Result<String> {
    let slug = slug(id).with_context(|| format!("{id:?} is not a usable id"))?;
    if is_builtin(&slug) {
        bail!("{slug} is the id of a scheme that ships with rulogman");
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
/// The scheme directory's counterpart of the walk `rugpui` does over the theme
/// directory, and forgiving in the same way: malformed files, unusable names
/// and ids that shadow a built-in one are logged and skipped. `kind` names what
/// is being loaded and appears in those messages. The result is ordered by id,
/// because `read_dir` reports no order of its own and a picker that reshuffles
/// itself between runs is worse than an arbitrary but stable one.
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

    /// Slugging, id generation and the theme half of the store are `rugpui`'s and
    /// are tested there. What is left here is the scheme half, which has no
    /// counterpart in a widget library.
    #[test]
    fn load_dir_reads_every_valid_scheme_and_skips_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();

        let write = |name: &str, contents: &[u8]| {
            fs::write(root.join(name), contents).expect("write");
        };
        let scheme = SchemeFile::from_theme("Zed Ish", &TerminalTheme::dracula());
        let json = serde_json::to_vec(&scheme).expect("serialize");

        write("Zed Ish.json", &json);
        // A leading byte order mark is what a Windows editor leaves behind.
        let mut with_bom = b"\xEF\xBB\xBF".to_vec();
        with_bom.extend_from_slice(&json);
        write("another.json", &with_bom);
        // Skipped: malformed, wrong extension, and a built-in id.
        write("broken.json", b"{ nope");
        write("notes.txt", &json);
        write("dracula.json", &json);

        let loaded = load_dir::<SchemeFile>(Ok(root), "scheme", is_builtin_scheme);
        let ids: Vec<&str> = loaded.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["another", "zed-ish"]);
        assert_eq!(loaded[0].1.name, "Zed Ish");
    }

    #[test]
    fn a_missing_scheme_directory_yields_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let absent = dir.path().join("never-created");
        assert!(load_dir::<SchemeFile>(Ok(absent), "scheme", is_builtin_scheme).is_empty());
    }

    #[test]
    fn a_scheme_saves_and_deletes_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("schemes");
        let file = SchemeFile::from_theme("Mine", &TerminalTheme::gruvbox_dark());

        let path = save_json(&root, "mine", &file).expect("save");
        assert!(path.exists());

        let loaded = load_dir::<SchemeFile>(Ok(root.clone()), "scheme", is_builtin_scheme);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, "mine");
        assert_eq!(loaded[0].1, file);

        delete_json(&root, "mine").expect("delete");
        assert!(!path.exists());
        // Deleting what is already gone is not an error.
        delete_json(&root, "mine").expect("delete again");
    }

    #[test]
    fn a_builtin_scheme_id_cannot_be_saved_over() {
        assert!(validated_id("one-dark", is_builtin_scheme).is_err());
        assert!(validated_id("   ", is_builtin_scheme).is_err());
        assert_eq!(
            validated_id("My Scheme", is_builtin_scheme).expect("id"),
            "my-scheme"
        );
    }
}
