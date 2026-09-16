//! Crash-conscious replacement for files edited through the Windows local source.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetFileSecurityW, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SetFileSecurityW,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle, MoveFileW, REPLACE_FILE_FLAGS,
    ReplaceFileW,
};
use windows::core::PCWSTR;

use super::FileError;

const MAX_SNAPSHOT: u64 = 10 * 1024 * 1024;
const ERROR_UNABLE_TO_MOVE_REPLACEMENT_2: u32 = 1177;
static NEXT_NAME: AtomicU64 = AtomicU64::new(0);

/// Replaces an existing regular file while retaining its metadata and ACLs.
/// `ReplaceFileW` commits the successful replacement and supplies the documented
/// failure states that this module recovers or leaves available for recovery.
pub(super) fn replace(path: &Path, expected: &[u8], replacement: &[u8]) -> Result<(), FileError> {
    if expected.len() as u64 > MAX_SNAPSHOT {
        return Err(FileError::Backend(format!(
            "{} is larger than the 10 MiB safe-save limit",
            path.display()
        )));
    }

    // Resolve the destination, rather than replacing a symlink itself. The
    // directory entry at `path` therefore remains a link after a successful save.
    let target = std::fs::canonicalize(path).map_err(|error| local("resolve", path, error))?;
    let mut original = File::open(&target).map_err(|error| local("open", &target, error))?;
    let original_info = file_info(&original, &target)?;
    if original_info.nNumberOfLinks != 1 {
        return Err(FileError::Backend(format!(
            "{} has multiple hard links and cannot be replaced safely",
            target.display()
        )));
    }
    if !original
        .metadata()
        .map_err(|error| local("inspect", &target, error))?
        .is_file()
    {
        return Err(FileError::Path(format!(
            "{} is not a regular file",
            target.display()
        )));
    }
    if read_snapshot(&mut original, &target)? != expected {
        return Err(FileError::Conflict);
    }
    drop(original);

    let parent = target
        .parent()
        .ok_or_else(|| FileError::Path(format!("{} has no parent directory", target.display())))?;
    let name = target
        .file_name()
        .ok_or_else(|| FileError::Path(format!("{} has no file name", target.display())))?;
    let (staged_path, mut staged) = create_staged(parent, name)?;
    if let Err(error) = copy_dacl(&target, &staged_path) {
        drop(staged);
        let _ = std::fs::remove_file(&staged_path);
        return Err(error);
    }
    if let Err(error) = staged
        .write_all(replacement)
        .and_then(|()| staged.sync_all())
    {
        drop(staged);
        let _ = std::fs::remove_file(&staged_path);
        return Err(local("stage", &target, error));
    }
    drop(staged);

    let backup_path = match unused_name(parent, name, "backup") {
        Ok(path) => path,
        Err(error) => {
            let _ = std::fs::remove_file(&staged_path);
            return Err(error);
        }
    };
    let outcome = replace_staged(
        &target,
        path,
        &staged_path,
        &backup_path,
        expected,
        &original_info,
    );
    if outcome.is_err() && !backup_path.exists() {
        // In all documented failure states except 1177 the original still has
        // its name, so the unconsumed staging file is disposable.
        let _ = std::fs::remove_file(&staged_path);
    }
    outcome
}

fn replace_staged(
    target: &Path,
    requested_path: &Path,
    staged: &Path,
    backup: &Path,
    expected: &[u8],
    original_info: &BY_HANDLE_FILE_INFORMATION,
) -> Result<(), FileError> {
    if std::fs::canonicalize(requested_path)
        .map_err(|error| local("recheck", requested_path, error))?
        != target
    {
        return Err(FileError::Conflict);
    }
    let mut current = File::open(target).map_err(|error| local("recheck", target, error))?;
    let current_info = file_info(&current, target)?;
    if current_info.nNumberOfLinks != 1
        || !same_file(original_info, &current_info)
        || read_snapshot(&mut current, target)? != expected
    {
        return Err(FileError::Conflict);
    }
    drop(current);

    let target_wide = wide(target)?;
    let staged_wide = wide(staged)?;
    let backup_wide = wide(backup)?;
    let result = unsafe {
        ReplaceFileW(
            PCWSTR(target_wide.as_ptr()),
            PCWSTR(staged_wide.as_ptr()),
            PCWSTR(backup_wide.as_ptr()),
            REPLACE_FILE_FLAGS(0),
            None,
            None,
        )
    };
    match result {
        Ok(()) => {
            // The save is already committed. A backup cleanup problem must not
            // make the editor retain the old snapshot and overwrite later work.
            if let Err(error) = std::fs::remove_file(backup) {
                log::warn!(
                    "saved {}, but could not remove backup {}: {error}",
                    target.display(),
                    backup.display()
                );
            }
            Ok(())
        }
        Err(error) => {
            let code = (error.code().0 as u32) & 0xffff;
            if code == ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 {
                // Windows documents this state precisely: the original is at
                // the backup name and the staging file remains at its own name.
                match restore_backup(backup, target) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(staged);
                        Err(FileError::Backend(format!(
                            "could not replace {}; the original was restored from {}: {error}",
                            target.display(),
                            backup.display()
                        )))
                    }
                    Err(restore_error) => Err(FileError::Backend(format!(
                        "save failed and the original could not be restored; recover it from {} (staged data remains at {}): {error}; restore failed: {restore_error}",
                        backup.display(),
                        staged.display()
                    ))),
                }
            } else {
                Err(FileError::Local(format!(
                    "could not replace {} (recovery backup path {}): {error}",
                    target.display(),
                    backup.display()
                )))
            }
        }
    }
}

fn read_snapshot(file: &mut File, path: &Path) -> Result<Vec<u8>, FileError> {
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| local("read", path, error))?;
    if bytes.len() as u64 > MAX_SNAPSHOT {
        return Err(FileError::Backend(format!(
            "{} is larger than the 10 MiB safe-save limit",
            path.display()
        )));
    }
    Ok(bytes)
}

fn file_info(file: &File, path: &Path) -> Result<BY_HANDLE_FILE_INFORMATION, FileError> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info).map_err(|error| {
            FileError::Local(format!("could not inspect {}: {error}", path.display()))
        })?;
    }
    Ok(info)
}

fn copy_dacl(source: &Path, destination: &Path) -> Result<(), FileError> {
    let source_wide = wide(source)?;
    let destination_wide = wide(destination)?;
    let mut required = 0u32;
    unsafe {
        let _ = GetFileSecurityW(
            PCWSTR(source_wide.as_ptr()),
            DACL_SECURITY_INFORMATION.0,
            None,
            0,
            &mut required,
        );
    }
    if required == 0 {
        return Err(FileError::Local(format!(
            "could not read the access controls of {}: {}",
            source.display(),
            std::io::Error::last_os_error()
        )));
    }
    let mut descriptor = vec![0u8; required as usize];
    let descriptor_ptr = PSECURITY_DESCRIPTOR(descriptor.as_mut_ptr().cast());
    unsafe {
        GetFileSecurityW(
            PCWSTR(source_wide.as_ptr()),
            DACL_SECURITY_INFORMATION.0,
            Some(descriptor_ptr),
            required,
            &mut required,
        )
        .ok()
        .map_err(|error| {
            FileError::Local(format!(
                "could not read the access controls of {}: {error}",
                source.display()
            ))
        })?;
        SetFileSecurityW(
            PCWSTR(destination_wide.as_ptr()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor_ptr,
        )
        .ok()
        .map_err(|error| {
            FileError::Local(format!(
                "could not protect staging file {}: {error}",
                destination.display()
            ))
        })?;
    }
    Ok(())
}

fn restore_backup(backup: &Path, target: &Path) -> Result<(), std::io::Error> {
    let backup_wide = wide(backup).map_err(file_error_to_io)?;
    let target_wide = wide(target).map_err(file_error_to_io)?;
    unsafe { MoveFileW(PCWSTR(backup_wide.as_ptr()), PCWSTR(target_wide.as_ptr())) }
        .map_err(std::io::Error::from)
}

fn file_error_to_io(error: FileError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
}

fn same_file(a: &BY_HANDLE_FILE_INFORMATION, b: &BY_HANDLE_FILE_INFORMATION) -> bool {
    a.dwVolumeSerialNumber == b.dwVolumeSerialNumber
        && a.nFileIndexHigh == b.nFileIndexHigh
        && a.nFileIndexLow == b.nFileIndexLow
}

fn create_staged(parent: &Path, name: &std::ffi::OsStr) -> Result<(PathBuf, File), FileError> {
    for _ in 0..128 {
        let path = candidate(parent, name, "staged");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(local("create a staging file beside", parent, error)),
        }
    }
    Err(FileError::Local(format!(
        "could not choose a staging name beside {}",
        parent.display()
    )))
}

fn unused_name(parent: &Path, name: &std::ffi::OsStr, kind: &str) -> Result<PathBuf, FileError> {
    for _ in 0..128 {
        let path = candidate(parent, name, kind);
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(path),
            Ok(_) => continue,
            Err(error) => return Err(local("inspect a backup name beside", parent, error)),
        }
    }
    Err(FileError::Local(format!(
        "could not choose a backup name beside {}",
        parent.display()
    )))
}

fn candidate(parent: &Path, name: &std::ffi::OsStr, kind: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
    let mut generated = std::ffi::OsString::from(".");
    generated.push(name);
    generated.push(format!(
        ".rulogman-{kind}-{}-{nonce:x}-{sequence:x}",
        std::process::id()
    ));
    parent.join(generated)
}

fn wide(path: &Path) -> Result<Vec<u16>, FileError> {
    let mut encoded: Vec<u16> = path.as_os_str().encode_wide().collect();
    if encoded.contains(&0) {
        return Err(FileError::Path(format!(
            "{} contains a null character",
            path.display()
        )));
    }
    encoded.push(0);
    Ok(encoded)
}

fn local(action: &str, path: &Path, error: std::io::Error) -> FileError {
    FileError::Local(format!("could not {action} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("note.txt");
        std::fs::write(&path, b"before").unwrap();

        replace(&path, b"before", b"after").unwrap();

        assert_eq!(std::fs::read(path).unwrap(), b"after");
    }

    #[test]
    fn stale_snapshot_does_not_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("note.txt");
        std::fs::write(&path, b"external edit").unwrap();

        assert_eq!(
            replace(&path, b"old editor copy", b"editor edit"),
            Err(FileError::Conflict)
        );
        assert_eq!(std::fs::read(path).unwrap(), b"external edit");
    }

    #[test]
    fn rejects_multiple_hard_links() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("note.txt");
        let alias = directory.path().join("alias.txt");
        std::fs::write(&path, b"before").unwrap();
        std::fs::hard_link(&path, &alias).unwrap();

        assert!(matches!(
            replace(&path, b"before", b"after"),
            Err(FileError::Backend(_))
        ));
        assert_eq!(std::fs::read(path).unwrap(), b"before");
        assert_eq!(std::fs::read(alias).unwrap(), b"before");
    }

    #[test]
    fn read_only_failure_preserves_original() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("note.txt");
        std::fs::write(&path, b"before").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();

        assert!(replace(&path, b"before", b"after").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"before");

        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn saves_through_symlink_when_creation_is_permitted() {
        use std::os::windows::fs::symlink_file;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.txt");
        let link = directory.path().join("link.txt");
        std::fs::write(&target, b"before").unwrap();
        if symlink_file(&target, &link).is_err() {
            return;
        }

        replace(&link, b"before", b"after").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"after");
        assert!(
            std::fs::symlink_metadata(link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn file_identity_uses_volume_and_full_index() {
        let mut a = BY_HANDLE_FILE_INFORMATION::default();
        a.dwVolumeSerialNumber = 7;
        a.nFileIndexHigh = 8;
        a.nFileIndexLow = 9;
        let mut b = a;
        assert!(same_file(&a, &b));
        b.nFileIndexLow += 1;
        assert!(!same_file(&a, &b));
    }

    #[test]
    fn restore_does_not_clobber_a_new_target() {
        let directory = tempfile::tempdir().unwrap();
        let backup = directory.path().join("backup.txt");
        let target = directory.path().join("target.txt");
        std::fs::write(&backup, b"original").unwrap();
        std::fs::write(&target, b"new external file").unwrap();

        assert!(restore_backup(&backup, &target).is_err());
        assert_eq!(std::fs::read(backup).unwrap(), b"original");
        assert_eq!(std::fs::read(target).unwrap(), b"new external file");
    }
}
