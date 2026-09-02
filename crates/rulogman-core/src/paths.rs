//! Platform-specific locations of the files rulogman persists.
//!
//! Every path is derived from a single [`directories::ProjectDirs`] instance
//! built from the `com.aihouse.rulogman` triple, so the whole application agrees on
//! where its configuration lives:
//!
//! * Windows: `%APPDATA%\aihouse\rulogman\config`
//! * macOS: `~/Library/Application Support/com.aihouse.rulogman`
//! * Linux: `~/.config/rulogman`
//!
//! Most of what rulogman persists is a single file in that directory —
//! [`config_file`], [`dashboards_file`], [`known_hosts_file`],
//! [`settings_file`]. The kinds of file the user may supply any number of get a
//! subdirectory each instead: [`ui_themes_dir`] holds UI theme files,
//! [`schemes_dir`] terminal color scheme files, and [`syntaxes_dir`] the
//! editor's language definitions.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use directories::ProjectDirs;

/// Name of the file holding the serialized [`crate::ProfileStore`].
const PROFILES_FILE_NAME: &str = "profiles.json";

/// Name of the file holding the serialized [`crate::DashboardStore`].
const DASHBOARDS_FILE_NAME: &str = "dashboards.json";

/// Name of the file holding the trusted SSH host keys.
const KNOWN_HOSTS_FILE_NAME: &str = "known_hosts";

/// Name of the file holding the serialized [`crate::AppSettings`].
const SETTINGS_FILE_NAME: &str = "settings.json";

/// Name of the directory holding user-supplied UI theme files.
const UI_THEMES_DIR_NAME: &str = "themes";

/// Name of the directory holding user-supplied terminal color scheme files.
const SCHEMES_DIR_NAME: &str = "schemes";

/// Name of the directory holding user-supplied syntax definition files.
const SYNTAXES_DIR_NAME: &str = "syntaxes";

/// Byte order mark that Windows editors readily prepend to UTF-8 files.
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// Strip a leading UTF-8 byte order mark, if there is one.
///
/// Neither `serde_json` nor the `known_hosts` line parser tolerates a BOM: it
/// turns a perfectly valid file into a parse error, or silently glues itself to
/// the first host name. Since these files are meant to be editable by hand, and
/// several Windows editors add a BOM on save, every reader of one goes through
/// here — the theme and scheme files the app layer reads included.
pub fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes)
}

/// Resolve the project directories for rulogman.
fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("com", "aihouse", "rulogman")
        .context("could not determine a home directory for the current user")
}

/// Directory that holds every rulogman configuration file.
///
/// The directory is *not* created by this call; the writers in this crate create
/// it on demand.
///
/// # Errors
///
/// Fails when no home directory can be determined for the current user.
pub fn config_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

/// Full path of the session profile database (`profiles.json`).
///
/// # Errors
///
/// Fails when no home directory can be determined for the current user.
pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join(PROFILES_FILE_NAME))
}

/// Full path of the dashboard database (`dashboards.json`).
///
/// A file of its own rather than a key inside `profiles.json`, because a
/// dashboard spans profiles: it belongs to no one of them, and putting it
/// inside any would make removing that profile take the arrangement with it.
///
/// # Errors
///
/// Fails when no home directory can be determined for the current user.
pub fn dashboards_file() -> Result<PathBuf> {
    Ok(config_dir()?.join(DASHBOARDS_FILE_NAME))
}

/// Full path of the trusted host key database (`known_hosts`).
///
/// # Errors
///
/// Fails when no home directory can be determined for the current user.
pub fn known_hosts_file() -> Result<PathBuf> {
    Ok(config_dir()?.join(KNOWN_HOSTS_FILE_NAME))
}

/// Full path of the application settings file (`settings.json`).
///
/// # Errors
///
/// Fails when no home directory can be determined for the current user.
pub fn settings_file() -> Result<PathBuf> {
    Ok(config_dir()?.join(SETTINGS_FILE_NAME))
}

/// Directory holding the user's own UI theme files (`themes`).
///
/// One `*.json` file per theme, whose stem is the id the theme is selected by.
/// Like [`config_dir`], the directory is not created by this call; a user who
/// has never added a theme simply has none.
///
/// # Errors
///
/// Fails when no home directory can be determined for the current user.
pub fn ui_themes_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join(UI_THEMES_DIR_NAME))
}

/// Directory holding the user's own terminal color scheme files (`schemes`).
///
/// Laid out exactly like [`ui_themes_dir`]: one `*.json` file per scheme, named
/// after the id it is selected by.
///
/// # Errors
///
/// Fails when no home directory can be determined for the current user.
pub fn schemes_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join(SCHEMES_DIR_NAME))
}

/// Directory holding the editor's user-supplied language definitions
/// (`syntaxes`).
///
/// One `*.yml` or `*.yaml` file per language, whose stem is the language's id.
/// Unlike the two directories above these files are read and never written, so
/// nothing here creates the directory: a user who has never defined a language
/// simply has none.
///
/// # Errors
///
/// Fails when no home directory can be determined for the current user.
pub fn syntaxes_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join(SYNTAXES_DIR_NAME))
}

/// Build a unique temporary path next to `path`.
///
/// Keeping the temporary file in the same directory guarantees that the final
/// rename stays inside one filesystem, which is what makes it atomic.
fn temp_sibling(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("rulogman"));
    name.push(format!(".{}.{seq}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Write `contents` to `path`, replacing any previous file atomically.
///
/// Missing parent directories are created first. The data is written to a
/// temporary sibling file and then renamed over the destination, so a crash
/// mid-write can never leave a half-written configuration behind.
///
/// # Errors
///
/// Fails when the parent directory cannot be created, the temporary file cannot
/// be written, or the rename onto `path` does not go through.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    let temp = temp_sibling(path);
    fs::write(&temp, contents)
        .with_context(|| format!("failed to write temporary file {}", temp.display()))?;

    // `rename` replaces the destination on Unix and on Windows (`MoveFileEx`
    // with `MOVEFILE_REPLACE_EXISTING`). Should a platform ever refuse to
    // clobber an existing file, fall back to removing it first.
    if let Err(first) = fs::rename(&temp, path) {
        let _ = fs::remove_file(path);
        if let Err(second) = fs::rename(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(second).with_context(|| {
                format!(
                    "failed to move {} onto {} (first attempt: {first})",
                    temp.display(),
                    path.display()
                )
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_paths_share_the_config_directory() {
        let dir = config_dir().expect("config dir");
        let profiles = config_file().expect("config file");
        let dashboards = dashboards_file().expect("dashboards file");
        let hosts = known_hosts_file().expect("known hosts file");
        let settings = settings_file().expect("settings file");
        let themes = ui_themes_dir().expect("themes dir");
        let schemes = schemes_dir().expect("schemes dir");
        let syntaxes = syntaxes_dir().expect("syntaxes dir");

        assert_eq!(syntaxes.parent(), Some(dir.as_path()));
        assert_eq!(syntaxes.file_name().unwrap(), SYNTAXES_DIR_NAME);
        assert_eq!(profiles.parent(), Some(dir.as_path()));
        assert_eq!(dashboards.parent(), Some(dir.as_path()));
        assert_eq!(hosts.parent(), Some(dir.as_path()));
        assert_eq!(settings.parent(), Some(dir.as_path()));
        assert_eq!(themes.parent(), Some(dir.as_path()));
        assert_eq!(schemes.parent(), Some(dir.as_path()));
        assert_eq!(profiles.file_name().unwrap(), PROFILES_FILE_NAME);
        assert_eq!(dashboards.file_name().unwrap(), DASHBOARDS_FILE_NAME);
        assert_eq!(hosts.file_name().unwrap(), KNOWN_HOSTS_FILE_NAME);
        assert_eq!(settings.file_name().unwrap(), SETTINGS_FILE_NAME);
        assert_eq!(themes.file_name().unwrap(), UI_THEMES_DIR_NAME);
        assert_eq!(schemes.file_name().unwrap(), SCHEMES_DIR_NAME);
    }

    #[test]
    fn write_atomic_creates_parents_and_overwrites() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("deeper").join("data.txt");

        write_atomic(&path, b"first").expect("initial write");
        assert_eq!(fs::read_to_string(&path).unwrap(), "first");

        // Overwriting an existing destination must work on every platform.
        write_atomic(&path, b"second").expect("overwrite");
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");

        // No temporary leftovers.
        let stray: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(stray.is_empty(), "temporary files left behind: {stray:?}");
    }
}
