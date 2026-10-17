//! Where a shell on a pty is standing, asked of the operating system rather
//! than of the shell.
//!
//! A shell configured to report its directory does so over `OSC 7`, and the
//! terminal on top of this crate reads that. Most shells are not configured
//! to, and for those the kernel still knows the answer: every process has a
//! working directory, and the one that matters is that of whatever is in the
//! *foreground* of the pty — the shell at its prompt, or the program it is
//! running, or a second shell started inside the first. The foreground is asked
//! for rather than the process this crate started, because on macOS that
//! process is `/usr/bin/login`, which never moves however far the shell under it
//! does.
//!
//! Linux and macOS only. Elsewhere — the BSDs, which would need `kvm` or
//! `sysctl` — the answer is simply `None`, which callers already have to cope
//! with for a shell that has just exited.

use std::path::PathBuf;

/// The working directory of the foreground process of the pty `shell` was
/// started on, or of `shell` itself when the foreground cannot be read.
pub(crate) fn current_dir(shell: u32) -> Option<PathBuf> {
    foreground_group(shell)
        .filter(|&leader| leader != shell)
        .and_then(cwd_of)
        .or_else(|| cwd_of(shell))
}

/// The foreground process group of the terminal `pid` is attached to — which
/// is also the process ID of that group's leader.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn foreground_group(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_tpgid(&stat)
}

/// The working directory of `pid`.
///
/// A directory removed since the process entered it reads back with
/// ` (deleted)` appended, which names nothing that exists; callers check the
/// answer before they use it.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn cwd_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// The `tpgid` field of a `/proc/<pid>/stat` line.
///
/// Counted from the *last* `)`, because the field before the ones wanted is the
/// command name in parentheses, and a command may put a space or a `)` of its
/// own in its name. `-1` — no controlling terminal — and `0` are no answer.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn parse_tpgid(stat: &str) -> Option<u32> {
    let (_, rest) = stat.rsplit_once(')')?;
    // state, ppid, pgrp, session, tty_nr, tpgid.
    let tpgid: i64 = rest.split_whitespace().nth(5)?.parse().ok()?;
    u32::try_from(tpgid).ok().filter(|&id| id > 0)
}

/// The foreground process group of the terminal `pid` is attached to.
#[cfg(target_os = "macos")]
fn foreground_group(pid: u32) -> Option<u32> {
    // SAFETY: `proc_bsdinfo` is plain old data, for which all-zero is a value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of_val(&info)).ok()?;
    // SAFETY: the buffer is a `proc_bsdinfo`, and `size` is its size.
    let written = unsafe {
        libc::proc_pidinfo(
            libc::c_int::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            size,
        )
    };
    (written == size)
        .then_some(info.e_tpgid)
        .filter(|&id| id > 0)
}

/// The working directory of `pid`.
#[cfg(target_os = "macos")]
fn cwd_of(pid: u32) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    // SAFETY: as above, for a `proc_vnodepathinfo`.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of_val(&info)).ok()?;
    // SAFETY: as above.
    let written = unsafe {
        libc::proc_pidinfo(
            libc::c_int::try_from(pid).ok()?,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // `libc` spells the `MAXPATHLEN` buffer as rows of 32; it is one C string.
    let path: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|&c| c as u8)
        .take_while(|&b| b != 0)
        .collect();
    (!path.is_empty()).then(|| PathBuf::from(OsString::from_vec(path)))
}

/// No way to ask on this platform.
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn foreground_group(_pid: u32) -> Option<u32> {
    None
}

/// No way to ask on this platform.
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn cwd_of(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_foreground_group_is_read_past_the_command_name() {
        let stat = "4242 (bash) S 4241 4242 4242 34816 5150 4194560 0 0";
        assert_eq!(parse_tpgid(stat), Some(5150));
        // A name with spaces and parentheses of its own.
        let stat = "4242 (my (odd) shell) S 4241 4242 4242 34816 5150 0";
        assert_eq!(parse_tpgid(stat), Some(5150));
    }

    #[test]
    fn a_process_with_no_terminal_has_no_foreground_group() {
        assert_eq!(parse_tpgid("1 (init) S 0 1 1 0 -1 4194560"), None);
        assert_eq!(parse_tpgid("garbage"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn this_process_is_where_it_says_it_is() {
        let here = std::env::current_dir().expect("the test runs somewhere");
        assert_eq!(cwd_of(std::process::id()), Some(here));
    }
}
