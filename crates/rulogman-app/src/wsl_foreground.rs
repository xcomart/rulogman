//! Whether the shell of a WSL tab is waiting at its prompt, asked of the
//! distribution it runs in.
//!
//! A local shell on unix is answered from this machine's process table — see
//! `PtySession::shell_in_foreground` — and so are `cmd` and PowerShell on
//! Windows. A WSL tab's pty holds `wsl.exe`, though, and the shell behind it is
//! a Linux process with no Windows process standing for it: whatever the user
//! runs there, `wsl.exe` looks the same. So the question is put to the
//! distribution itself, as the SSH transport puts it to a remote host, and
//! answered the way the unix pty answers it: the shell is at its prompt exactly
//! while its terminal's foreground process group is its own, because a shell
//! with job control hands the terminal to every command it runs and takes it
//! back when the command ends.
//!
//! What makes the shell findable is a tag. Every WSL tab's shell is started
//! with [`TAG_VARIABLE`] set to a value of its own, forwarded across by
//! `WSLENV`, and everything the shell starts inherits it. The topmost of the
//! processes carrying the tab's value is the shell `wsl.exe` started — its
//! parent is WSL's own relay, which does not carry it — and its `stat` says
//! whose the terminal is. A daemon the shell left behind carries the value too,
//! and is told apart by having no terminal at all.
//!
//! Anything that keeps the answer from being read — no tagged process, a
//! distribution that does not answer, a shell that has exited — is `None`, and
//! what an unknown is worth is the caller's to decide.

/// The variable a WSL tab's shell is started with, naming that one tab.
pub(crate) const TAG_VARIABLE: &str = "RULOGMAN_SESSION";

/// What `sh` runs in the distribution, with the tab's tag as `$1`: the `stat`
/// line of every process whose environment carries that tag.
///
/// `grep -l` reads the environment blocks whole, NUL separators and all, and
/// names the files that match; `-s` keeps quiet about the ones it may not read
/// — other users' processes — and about processes that exit while it reads.
/// The tag is a UUID of fixed length, so a match on `NAME=<tag>` inside one
/// block cannot be a longer value that merely starts with it. Nothing here
/// but `sh`, `grep` and `cat`, which BusyBox has as well as coreutils.
#[cfg(any(windows, test))]
const SCRIPT: &str = r#"for f in $(grep -lsF "RULOGMAN_SESSION=$1" /proc/[0-9]*/environ); do cat "${f%environ}stat"; done 2>/dev/null"#;

/// Asks distribution `distro` whether the shell tagged `tag` is at its prompt;
/// see the module documentation.
///
/// Starts `wsl.exe` and waits for it, so the caller runs this off the UI
/// thread, and gives up on it after a while of its own: a distribution that
/// has stopped answering is not waited on here.
#[cfg(windows)]
pub(crate) fn read(distro: &str, tag: &str) -> Option<bool> {
    use std::os::windows::process::CommandExt;

    let output = std::process::Command::new("wsl.exe")
        .args(["-d", distro, "--exec", "sh", "-c", SCRIPT, "sh", tag])
        .creation_flags(crate::wsl::CREATE_NO_WINDOW)
        .output();
    match output {
        // The status is not looked at: the loop's is that of its last `cat`,
        // which fails for a process that exited mid-read and says nothing
        // about the rest. What was printed is the whole answer.
        Ok(output) => parse(&String::from_utf8_lossy(&output.stdout)),
        Err(error) => {
            log::debug!("wsl.exe could not be run to read {distro}'s shell: {error}");
            None
        }
    }
}

/// The answer the `stat` lines of the tagged processes give: whether every
/// topmost one with a terminal is its terminal's foreground, or `None` when
/// there is no such process.
fn parse(output: &str) -> Option<bool> {
    let rows: Vec<Stat> = output.lines().filter_map(stat).collect();
    let mut shells = rows
        .iter()
        .filter(|row| row.tpgid > 0 && !rows.iter().any(|other| other.pid == row.ppid))
        .peekable();
    shells.peek()?;
    Some(shells.all(|row| row.tpgid == i64::from(row.pid)))
}

/// The fields of one `/proc/<pid>/stat` line that [`parse`] reads.
struct Stat {
    pid: u32,
    ppid: u32,
    /// The foreground process group of the process's terminal, `-1` when it
    /// has none.
    tpgid: i64,
}

/// `line` read as a [`Stat`], or `None` for anything that is not one.
///
/// The command name sits in parentheses after the ID and may hold spaces and
/// parentheses of its own, so the fields after it are counted from the last
/// `)`: state, parent, process group, session, terminal, and then the
/// terminal's foreground group.
fn stat(line: &str) -> Option<Stat> {
    let (head, tail) = line.rsplit_once(')')?;
    let pid = head.split_whitespace().next()?.parse().ok()?;
    let mut fields = tail.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse().ok()?;
    let tpgid = fields.nth(3)?.parse().ok()?;
    Some(Stat { pid, ppid, tpgid })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `stat` line as Linux writes it, cut after the fields [`stat`] reads
    /// and a few more.
    fn line(pid: u32, comm: &str, ppid: u32, tty: u32, tpgid: i64) -> String {
        format!("{pid} ({comm}) S {ppid} {pid} {pid} {tty} {tpgid} 4194560 1043 0 0")
    }

    /// What a WSL tab looks like with its terminal handed to `foreground`:
    /// `bash` (`412`) under WSL's relay (`411`, untagged), on `pts/0`.
    fn tab(foreground: i64) -> String {
        line(412, "bash", 411, 34816, foreground)
    }

    #[test]
    fn a_shell_holding_its_terminal_is_at_its_prompt() {
        assert_eq!(parse(&tab(412)), Some(true));
    }

    #[test]
    fn a_shell_that_handed_its_terminal_on_is_running_something() {
        // `vim` started from the prompt, in a process group of its own.
        let output = format!("{}\n{}\n", tab(530), line(530, "vim", 412, 34816, 530));
        assert_eq!(parse(&output), Some(false));
    }

    #[test]
    fn a_job_sent_to_the_background_does_not_count() {
        // `sleep 300 &`: the shell kept the terminal.
        let output = format!("{}\n{}\n", tab(412), line(530, "sleep", 412, 34816, 412));
        assert_eq!(parse(&output), Some(true));
    }

    #[test]
    fn a_daemon_the_shell_left_behind_is_not_a_second_shell() {
        // Reparented to init and with no terminal of its own: its `tpgid` is
        // `-1`, which leaves it out.
        let output = format!("{}\n{}\n", tab(412), line(600, "ssh-agent", 1, 0, -1));
        assert_eq!(parse(&output), Some(true));
    }

    #[test]
    fn a_command_name_with_parentheses_is_read_past() {
        let output = line(412, "odd) (name", 411, 34816, 412);
        assert_eq!(parse(&output), Some(true));
    }

    #[test]
    fn the_script_looks_for_the_variable_the_shell_is_started_with() {
        assert!(SCRIPT.contains(&format!("\"{TAG_VARIABLE}=$1\"")));
    }

    #[test]
    fn nothing_tagged_is_unknown() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("garbage\n"), None);
        // Only a process with no terminal: no shell to ask about.
        assert_eq!(parse(&line(600, "ssh-agent", 1, 0, -1)), None);
    }
}
