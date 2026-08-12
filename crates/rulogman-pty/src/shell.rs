//! Naming the user's login shell.
//!
//! The UI labels a local session with the shell's name, so it needs the same
//! answer the pty layer acts on. `alacritty_terminal` resolves the shell
//! internally and does not expose it, so the lookup is repeated here — `$SHELL`
//! first, then the passwd entry — in the same order, so that the label cannot
//! disagree with the shell that actually starts.

use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::ptr;

/// Last-resort name, for the pathological case of a user with no `$SHELL` and
/// no readable passwd entry.
const FALLBACK: &str = "sh";

/// Basename of the user's login shell (`$SHELL`, falling back to the passwd
/// entry), e.g. `"zsh"`.
pub fn login_shell_name() -> String {
    let path = std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .or_else(passwd_shell);

    match path.as_deref().map(basename) {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => FALLBACK.to_owned(),
    }
}

/// Everything after the last `/`, which for a shell path is its name.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The login shell recorded in this user's passwd entry.
///
/// Consulted only when `$SHELL` is unset, which happens in daemon-launched and
/// desktop-session contexts more often than one would like.
fn passwd_shell() -> Option<String> {
    // `getpwuid_r` writes the strings into a caller-provided buffer; 1 KiB is
    // what alacritty uses for the same call, and is ample for a passwd line.
    let mut buffer = [0; 1024];
    let mut entry = MaybeUninit::<libc::passwd>::uninit();
    let mut found: *mut libc::passwd = ptr::null_mut();

    // SAFETY: `entry` and `buffer` are live and correctly sized for the call,
    // and `found` receives either null or a pointer into `entry`.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() {
        return None;
    }

    // SAFETY: a non-null `found` means the entry was filled in.
    let entry = unsafe { entry.assume_init() };
    if entry.pw_shell.is_null() {
        return None;
    }

    // SAFETY: `pw_shell` points into `buffer`, which is still in scope, and
    // the string is copied out before it goes away.
    let shell = unsafe { CStr::from_ptr(entry.pw_shell) };
    shell
        .to_str()
        .ok()
        .filter(|shell| !shell.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_keeps_the_last_component() {
        assert_eq!(basename("/usr/bin/zsh"), "zsh");
        assert_eq!(basename("bash"), "bash");
    }

    #[test]
    fn a_trailing_slash_leaves_no_name() {
        // Nonsense as a shell path, but it must not panic — and the empty
        // result is what sends `login_shell_name` to its fallback.
        assert_eq!(basename("/bin/"), "");
    }
}
