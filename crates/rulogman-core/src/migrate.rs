//! One-time adoption of the configuration left behind by `logman`.
//!
//! rulogman was called `logman` until the rename, and both of the identifiers
//! that decide where user data lives were derived from that name. The
//! configuration directory came from the `com.aihouse.logman` triple, so an
//! updated build looks at a brand new — and empty — directory:
//!
//! | | before | after |
//! |---|---|---|
//! | Windows | `%APPDATA%\aihouse\logman\config` | `%APPDATA%\aihouse\rulogman\config` |
//! | macOS | `~/Library/Application Support/com.aihouse.logman` | `~/Library/Application Support/com.aihouse.rulogman` |
//! | Linux | `~/.config/logman` | `~/.config/rulogman` |
//!
//! [`migrate_from_logman`] copies the old directory across on the first launch
//! of a renamed build, so profiles, trusted host keys, settings and every
//! user-supplied theme, scheme and syntax survive the update. The copy is
//! deliberately one-way: the old directory is left exactly as it was, so a user
//! who reinstalls the previous release finds their configuration intact.
//!
//! The keychain half of the rename is handled elsewhere, in
//! [`crate::secrets`], and for a different reason — see the module comment
//! there.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use directories::ProjectDirs;

/// The name rulogman was published under before the rename.
const LEGACY_APPLICATION: &str = "logman";

/// Copy the whole of `from` into `to`, creating `to` and its parents.
///
/// Directories are recreated and their contents copied entry by entry; symbolic
/// links are followed, since [`fs::copy`] reads through them. Nothing in `to` is
/// removed, so an entry that already exists there is overwritten and any extra
/// one is kept.
///
/// Returns the number of files copied — directories are not counted.
///
/// # Errors
///
/// Fails when `from` cannot be listed, when `to` cannot be created, or when any
/// single file cannot be copied. A failure part-way through leaves whatever was
/// already copied in place.
fn copy_dir_recursive(from: &Path, to: &Path) -> Result<usize> {
    fs::create_dir_all(to)
        .with_context(|| format!("failed to create directory {}", to.display()))?;

    let entries = fs::read_dir(from)
        .with_context(|| format!("failed to read directory {}", from.display()))?;

    let mut copied = 0;
    for entry in entries {
        let entry = entry
            .with_context(|| format!("failed to read an entry of directory {}", from.display()))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", source.display()))?;

        if file_type.is_dir() {
            copied += copy_dir_recursive(&source, &target)?;
        } else {
            fs::copy(&source, &target).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source.display(),
                    target.display()
                )
            })?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// Copy `legacy` to `current` unless either directory says the move is done.
///
/// The migration is skipped — reporting `Ok(None)` — when `current` already
/// exists, which is the case on every launch after the first, and also when
/// `legacy` does not exist, which is the case for a fresh install. Otherwise the
/// number of files copied is reported.
///
/// # Errors
///
/// Fails when the copy itself fails; see [`copy_dir_recursive`].
fn migrate_config_dir(legacy: &Path, current: &Path) -> Result<Option<usize>> {
    if current.exists() {
        return Ok(None);
    }
    if !legacy.is_dir() {
        return Ok(None);
    }
    copy_dir_recursive(legacy, current).map(Some)
}

/// Adopt the configuration directory of the pre-rename `logman` release.
///
/// Call this once at start-up, before anything reads a configuration file. It
/// does nothing at all when rulogman already has a configuration directory of
/// its own, so it is safe — and cheap — to call on every launch.
///
/// # Errors
///
/// Fails when no home directory can be determined for the current user, or when
/// the copy does not go through. Neither is fatal to the application: the caller
/// is expected to log the failure and carry on with an empty configuration,
/// which is no worse than not having migrated at all.
pub fn migrate_from_logman() -> Result<()> {
    let current = crate::paths::config_dir()?;
    let legacy = ProjectDirs::from("com", "aihouse", LEGACY_APPLICATION)
        .context("could not determine a home directory for the current user")?
        .config_dir()
        .to_path_buf();

    if let Some(copied) = migrate_config_dir(&legacy, &current)? {
        log::info!(
            "migrated {copied} file(s) from {} to {}",
            legacy.display(),
            current.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_nested_directories_and_files() {
        let root = tempfile::tempdir().expect("tempdir");
        let legacy = root.path().join("logman");
        let current = root.path().join("rulogman");

        fs::create_dir_all(legacy.join("themes")).expect("themes dir");
        fs::create_dir_all(legacy.join("syntaxes").join("nested")).expect("nested dir");
        fs::write(legacy.join("profiles.json"), b"[]").expect("profiles");
        fs::write(legacy.join("themes").join("night.json"), b"{}").expect("theme");
        fs::write(
            legacy.join("syntaxes").join("nested").join("toml.yml"),
            b"x",
        )
        .expect("syntax");

        let copied = migrate_config_dir(&legacy, &current).expect("migrate");
        assert_eq!(copied, Some(3));

        assert_eq!(
            fs::read_to_string(current.join("profiles.json")).unwrap(),
            "[]"
        );
        assert_eq!(
            fs::read_to_string(current.join("themes").join("night.json")).unwrap(),
            "{}"
        );
        assert_eq!(
            fs::read_to_string(current.join("syntaxes").join("nested").join("toml.yml")).unwrap(),
            "x"
        );

        // The old directory is left untouched, so downgrading still works.
        assert!(legacy.join("profiles.json").is_file());
    }

    #[test]
    fn existing_destination_is_left_alone() {
        let root = tempfile::tempdir().expect("tempdir");
        let legacy = root.path().join("logman");
        let current = root.path().join("rulogman");

        fs::create_dir_all(&legacy).expect("legacy dir");
        fs::write(legacy.join("profiles.json"), b"old").expect("legacy profiles");
        fs::create_dir_all(&current).expect("current dir");
        fs::write(current.join("profiles.json"), b"new").expect("current profiles");

        assert_eq!(
            migrate_config_dir(&legacy, &current).expect("migrate"),
            None
        );
        assert_eq!(
            fs::read_to_string(current.join("profiles.json")).unwrap(),
            "new"
        );
    }

    #[test]
    fn missing_source_is_not_an_error() {
        let root = tempfile::tempdir().expect("tempdir");
        let legacy = root.path().join("logman");
        let current = root.path().join("rulogman");

        assert_eq!(
            migrate_config_dir(&legacy, &current).expect("migrate"),
            None
        );
        assert!(!current.exists(), "nothing may be created out of nothing");
    }

    #[test]
    fn migration_is_idempotent() {
        let root = tempfile::tempdir().expect("tempdir");
        let legacy = root.path().join("logman");
        let current = root.path().join("rulogman");

        fs::create_dir_all(&legacy).expect("legacy dir");
        fs::write(legacy.join("settings.json"), b"{}").expect("settings");

        assert_eq!(
            migrate_config_dir(&legacy, &current).expect("first run"),
            Some(1)
        );
        // A second launch finds the destination in place and does nothing, even
        // if the user has since edited the copy.
        fs::write(current.join("settings.json"), b"edited").expect("edit");
        assert_eq!(
            migrate_config_dir(&legacy, &current).expect("second run"),
            None
        );
        assert_eq!(
            fs::read_to_string(current.join("settings.json")).unwrap(),
            "edited"
        );
    }
}
