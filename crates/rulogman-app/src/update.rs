//! The one thing `rugpui-shell`'s self-updater cannot know about rulogman.
//!
//! The check, the download, the digest, the unpacking, the swap and the
//! rollback are all [`rugpui_shell::update`]'s, and so is the registry write that
//! keeps winget's idea of the installed version in step with what is on disk.
//! What the shell has to be told is injected in [`main`](crate::main) as an
//! [`AppIdentity`](rugpui_shell::AppIdentity); what is left here is the part of
//! that identity with a test attached.
//!
//! # The uninstall key
//!
//! [`ARP_KEY`] is one corner of a triangle that has to agree — the Inno Setup
//! script, the winget manifests and this constant — and
//! [`the_uninstall_key_is_the_one_the_installer_writes`] checks the first side
//! of it mechanically.

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
}
