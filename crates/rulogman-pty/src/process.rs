//! Where a shell on a pty is standing, asked of the operating system rather
//! than of the shell.
//!
//! A shell configured to report its directory does so over `OSC 7`, and the
//! terminal on top of this crate reads that. Most shells are not configured
//! to, and for those the kernel still knows the answer: every process has a
//! working directory, and the one that matters is that of whatever is in the
//! *foreground* of the pty — the shell at its prompt, or the program it is
//! running, or a second shell started inside the first. The foreground is asked
//! for rather than the shell, which stays put however far what it runs moves.
//! On macOS the process this crate starts is not even the shell but
//! `/usr/bin/login`, which is root's and so closed to anyone else's questions;
//! there the shell under it is found and asked instead (see `shell_of`).
//!
//! Linux and macOS only. Elsewhere — the BSDs, which would need `kvm` or
//! `sysctl` — the answer is simply `None`, which callers already have to cope
//! with for a shell that has just exited.

use std::path::PathBuf;

/// The working directory of the foreground process of the pty the shell was
/// started on as `started`, or of the shell itself when the foreground cannot
/// be read.
pub(crate) fn current_dir(started: u32) -> Option<PathBuf> {
    let shell = shell_of(started);
    foreground_group(shell)
        .filter(|&leader| leader != shell)
        .and_then(cwd_of)
        .or_else(|| cwd_of(shell))
}

/// Whether the shell started on a pty as `started` is itself in the
/// foreground of that pty — at its prompt, with nothing running in front of
/// it — or `None` when the foreground cannot be read.
///
/// A shell with job control puts every command it runs in a process group of
/// its own and hands the terminal to that group, so the foreground group is the
/// shell's own exactly while it waits at the prompt. Jobs sent to the
/// background do not count: they are not what the user is looking at, and
/// prompt helpers (a `gitstatusd` beside zsh) would otherwise count as a
/// program forever.
pub(crate) fn shell_in_foreground(started: u32) -> Option<bool> {
    let shell = shell_of(started);
    foreground_group(shell).map(|leader| leader == shell)
}

/// The shell started on a pty as `started`: that process itself, except on
/// macOS for a default shell.
///
/// There the pty layer starts `/usr/bin/login`, which is setuid root and stays
/// behind as the shell's parent. The kernel tells one user only the outline of
/// another's process — its parent and its name, not its terminal or its
/// directory — so `login` reads as nothing at all, while the shell, its one
/// child, is the user's own. Before `login` has forked there is no shell to
/// find, and `login` itself is asked, which answers `None`, as a shell that
/// has not started does.
fn shell_of(started: u32) -> u32 {
    #[cfg(target_os = "macos")]
    if is_login(started) {
        return only_child(started).unwrap_or(started);
    }
    started
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

/// What the kernel keeps about `pid`: its terminal's foreground group among
/// the rest. Refused for a process of another user's, `login` among them.
#[cfg(target_os = "macos")]
fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
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
    (written == size).then_some(info)
}

/// The foreground process group of the terminal `pid` is attached to.
#[cfg(target_os = "macos")]
fn foreground_group(pid: u32) -> Option<u32> {
    bsd_info(pid).map(|info| info.e_tpgid).filter(|&id| id > 0)
}

/// The outline of `pid` the kernel gives anyone, whoever owns it: its parent
/// and its name, but not its terminal.
#[cfg(target_os = "macos")]
fn short_info(pid: u32) -> Option<libc::proc_bsdshortinfo> {
    // SAFETY: as above, for a `proc_bsdshortinfo`.
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of_val(&info)).ok()?;
    // SAFETY: as above.
    let written = unsafe {
        libc::proc_pidinfo(
            libc::c_int::try_from(pid).ok()?,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            size,
        )
    };
    (written == size).then_some(info)
}

/// The one child of `pid`, or `None` while it has none, or more than one,
/// which `login` never has.
#[cfg(target_os = "macos")]
fn only_child(pid: u32) -> Option<u32> {
    // Room for a second child, so that one can be told from two.
    let mut children: [libc::pid_t; 2] = [0; 2];
    let size = libc::c_int::try_from(std::mem::size_of_val(&children)).ok()?;
    // SAFETY: the buffer is `size` bytes of `pid_t`, which is what is listed
    // into it.
    let listed = unsafe {
        libc::proc_listchildpids(
            libc::pid_t::try_from(pid).ok()?,
            children.as_mut_ptr().cast(),
            size,
        )
    };
    if listed <= 0 {
        return None;
    }
    // Read off the entries rather than the count, so that nothing hangs on
    // whether the count is of pids or of bytes.
    match children {
        [child, 0] => u32::try_from(child).ok().filter(|&child| child > 0),
        _ => None,
    }
}

/// Whether `pid` is `/usr/bin/login`, which the pty layer starts a default
/// shell under on macOS.
#[cfg(target_os = "macos")]
fn is_login(pid: u32) -> bool {
    short_info(pid).is_some_and(|info| {
        let name: Vec<u8> = info
            .pbsi_comm
            .iter()
            .map(|&c| c as u8)
            .take_while(|&b| b != 0)
            .collect();
        name == b"login"
    })
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
