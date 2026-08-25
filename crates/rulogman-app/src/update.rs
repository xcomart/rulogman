//! The two things `ruui-shell`'s self-updater cannot know about rulogman.
//!
//! The check, the download, the digest, the unpacking, the swap and the
//! rollback are all [`ruui_shell::update`]'s, and so is the registry write that
//! keeps winget's idea of the installed version in step with what is on disk.
//! What the shell has to be told is injected in [`main`](crate::main) as an
//! [`AppIdentity`](ruui_shell::AppIdentity); what is left here is the part of
//! that identity with a test attached, and the one thing the shell's
//! `Installed` does not carry.
//!
//! # The uninstall key
//!
//! [`ARP_KEY`] is one corner of a triangle that has to agree — the Inno Setup
//! script, the winget manifests and this constant — and
//! [`the_uninstall_key_is_the_one_the_installer_writes`] checks the first side
//! of it mechanically.
//!
//! # Where a restart points
//!
//! The swap renames the installed copy aside and moves the new one into the
//! name it had, so the running image ends up living at `rulogman.old` while the
//! path it was launched from holds the new build. On Linux `current_exe()`
//! follows the *image*, which is to say the renamed-aside old binary, and that
//! is what gpui restarts into when it is given no path of its own — the old
//! build, every time. [`ruui_shell::update::Installed`] carries no path, so the
//! answer is recorded here before anything can move it: [`record`] runs at
//! start-up and [`restart_path`] hands the result to `cx.set_restart_path`.

#[cfg(any(target_os = "macos", test))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The "Apps & features" entry the Windows installer leaves behind, relative to
/// `HKEY_CURRENT_USER` or `HKEY_LOCAL_MACHINE`.
///
/// The GUID in the middle of it is one corner of a triangle that has to agree,
/// and it is a published identifier rather than an implementation detail:
///
/// * `packaging/windows/rulogman.iss` sets it as Inno Setup's `AppId`, and Inno
///   derives this key's name from it by appending `_is1`;
/// * the manifests under `packaging/winget/*/` record the same string, braces
///   and suffix included, as the package's `ProductCode`;
/// * and the shell's updater is what finds the entry again by it, to correct
///   the `DisplayVersion` after a self-update.
///
/// Move any one corner without the other two and winget stops recognising an
/// installed rulogman: `winget list` finds nothing, `winget upgrade` offers a
/// fresh install to sit beside the existing one, and `winget uninstall` has
/// nothing to remove — all silently, because a key that is not there is
/// indistinguishable from a copy that was never installed. None of the three
/// ever changes; see the README in `packaging/winget/`.
pub const ARP_KEY: &str = concat!(
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\",
    "{D6066CD8-5F5D-4B13-AB5B-DAD7965FF725}_is1"
);

/// Where a restart after a self-update has to be pointed; see the module docs.
static RESTART_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Works out where a restart would have to point, and remembers it.
///
/// Call once at start-up. Doing it later would be too late on the one platform
/// that needs it: after the swap `current_exe()` answers the renamed-aside
/// copy, which is the wrong build to come back on.
pub fn record() {
    let _ = RESTART_PATH.set(install_target());
}

/// The path a restart should name, or `None` when it could not be worked out.
///
/// `None` leaves gpui to its own fallback, which is the right answer when there
/// is nothing better to offer and the wrong one only on Linux after a swap —
/// which is exactly the case [`record`] exists to have already covered.
pub fn restart_path() -> Option<PathBuf> {
    RESTART_PATH.get().cloned().flatten()
}

/// What this run of rulogman would replace: the executable, or on macOS the
/// bundle containing it.
///
/// The same answer `ruui_shell::update`'s install plan starts from, worked out
/// the same way — one entry, because rulogman ships a single file and resolves
/// nothing beside it.
fn install_target() -> Option<PathBuf> {
    let exe = std::env::current_exe()
        .inspect_err(|error| log::debug!("could not locate the running program: {error}"))
        .ok()?;

    #[cfg(target_os = "macos")]
    {
        bundle_root(&exe)
    }

    #[cfg(not(target_os = "macos"))]
    {
        Some(exe)
    }
}

/// The `.app` directory `exe` lives inside, if any.
///
/// `current_exe()` in a bundle is `<name>.app/Contents/MacOS/rulogman`, but the
/// depth is not worth relying on: the ancestor chain is walked until a component
/// wears the `app` extension.
#[cfg(any(target_os = "macos", test))]
fn bundle_root(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
        })
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uninstall_key_is_the_one_the_installer_writes() {
        // The triangle from `ARP_KEY`'s docs, one side of it checked
        // mechanically. Inno Setup names its uninstall key by appending `_is1`
        // to `AppId`, so if these two ever drift the updater goes on running and
        // silently stops correcting the version — the exact failure mode that is
        // hardest to notice. The third side, the `ProductCode` in the winget
        // manifests, is not checked here only because that directory is named
        // after a release and would have to be found rather than named.
        let script = include_str!("../../../packaging/windows/rulogman.iss");
        // The doubled brace is Inno's escape for a literal one, so what follows
        // it is the GUID with its own closing brace still attached.
        let app_id = script
            .lines()
            .find_map(|line| line.trim().strip_prefix("AppId={{"))
            .expect("an AppId in rulogman.iss");
        assert!(
            ARP_KEY.ends_with(&format!("{{{app_id}_is1")),
            "{ARP_KEY} is not the key Inno derives from AppId={{{app_id}"
        );
    }

    #[test]
    fn a_bundle_is_recognised_at_whatever_depth_it_sits() {
        assert_eq!(
            bundle_root(Path::new(
                "/Applications/rulogman.app/Contents/MacOS/rulogman"
            )),
            Some(PathBuf::from("/Applications/rulogman.app"))
        );
        // The extension is matched without regard to case, and a bare binary
        // sits inside no bundle at all.
        assert_eq!(
            bundle_root(Path::new("/opt/rulogman.APP/Contents/MacOS/rulogman")),
            Some(PathBuf::from("/opt/rulogman.APP"))
        );
        assert_eq!(bundle_root(Path::new("/usr/local/bin/rulogman")), None);
    }

    #[test]
    fn the_recorded_path_is_the_one_this_process_was_launched_from() {
        // Recorded once and never again, which is the whole point: after a swap
        // the answer would be the copy renamed aside.
        record();
        record();
        assert_eq!(restart_path(), install_target());
    }
}
