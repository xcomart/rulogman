//! Whether the shell of an [`SshSession`](crate::SshSession) is waiting at its
//! prompt, read over the session's own exec channel.
//!
//! The local counterpart in `rulogman-pty` reads the foreground process group
//! of the shell's pty straight out of this machine's process table. A remote
//! shell's process table is on the far side, so the same question is asked
//! there, with `ps`, over an exec channel of the same connection — and the
//! shared connection is what makes the shell findable at all. A server forks
//! every channel of one connection from the one process that serves it
//! (`sshd: user@pts/0` in OpenSSH, `sshd-session: user@pts/0` from 9.8 on), so
//! the parent of the command run here is the parent of the session's shell
//! too. Among that parent's children the shell is the one with a controlling
//! terminal: the command itself, an `sftp-server` beside it and any other exec
//! in flight have none, and report a terminal process group of `-1` (Linux) or
//! `0` (macOS and the BSDs).
//!
//! The shell is at its prompt exactly while the terminal's foreground process
//! group is its own, for the reason `rulogman-pty` gives: a shell with job
//! control hands the terminal to every command it runs, and takes it back when
//! the command ends. A job sent to the background leaves it with the shell.
//!
//! Anything that keeps the answer from being read — a `ps` without `tpgid`
//! (BusyBox), a host with no `ps` at all (Windows), a forced command, a
//! connection that has gone quiet — is `None`, and what an unknown is worth is
//! the caller's to decide.

use crate::ExecClient;

/// What the exec channel runs.
///
/// Handed to the account's login shell, which may be fish or tcsh and read
/// none of `sh`'s syntax; `exec sh -c '…'` is the one line all of them read
/// alike, and it replaces the login shell rather than running under it, so
/// that the `$PPID` the script reports is the server's connection process and
/// not the login shell. One line because tcsh refuses a newline inside quotes,
/// and no backslash or single quote inside the quotes because fish reads both
/// there.
///
/// The parent is printed after [`PARENT_MARKER`] rather than on a line of its
/// own, because a login shell's startup files run before the command and may
/// well print something first.
const COMMAND: &str =
    "exec sh -c 'echo rulogman-parent $PPID; exec ps -A -o pid= -o ppid= -o tpgid='";

/// The word [`COMMAND`] prints the connection process after.
const PARENT_MARKER: &str = "rulogman-parent";

/// Reads, over `exec`, whether the session's shell is at its prompt: see the
/// module documentation.
pub(crate) async fn read(exec: ExecClient) -> Option<bool> {
    let output = exec.run(COMMAND.to_owned(), Vec::new()).await.ok()?;
    if output.exit_status != Some(0) {
        return None;
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

/// The answer [`COMMAND`]'s output gives: whether every child of the
/// connection process on a terminal is its terminal's foreground, or `None`
/// when the output names no such child.
fn parse(output: &str) -> Option<bool> {
    let mut lines = output.lines();
    let parent: i64 = lines
        .by_ref()
        .find_map(|line| line.trim().strip_prefix(PARENT_MARKER)?.trim().parse().ok())?;
    let mut shells = lines
        .filter_map(row)
        .filter(|row| row.ppid == parent && row.tpgid > 0)
        .peekable();
    shells.peek()?;
    Some(shells.all(|row| row.tpgid == row.pid))
}

/// One line of `ps -o pid= -o ppid= -o tpgid=`.
struct Row {
    pid: i64,
    ppid: i64,
    tpgid: i64,
}

/// `line` read as a [`Row`], or `None` for anything that is not three numbers.
fn row(line: &str) -> Option<Row> {
    let mut fields = line.split_whitespace().map(str::parse::<i64>);
    let row = Row {
        pid: fields.next()?.ok()?,
        ppid: fields.next()?.ok()?,
        tpgid: fields.next()?.ok()?,
    };
    fields.next().is_none().then_some(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What OpenSSH's process tree looks like from the exec channel, with the
    /// shell's terminal handed to `foreground`: the listener, the privileged
    /// monitor, the connection process `37`, the login shell `38` on `pts/0`,
    /// and `ps` itself beside the shell.
    fn tree(foreground: i64) -> String {
        format!(
            "rulogman-parent 37\n\
             1 0 -1\n\
             26 1 -1\n\
             37 26 -1\n\
             38 37 {foreground}\n\
             43 37 -1\n"
        )
    }

    #[test]
    fn a_shell_holding_its_own_terminal_is_at_its_prompt() {
        assert_eq!(parse(&tree(38)), Some(true));
    }

    #[test]
    fn a_shell_that_handed_its_terminal_on_is_running_something() {
        // `sleep 300` started from the prompt, in a process group of its own.
        let output = format!("{}     44      38      44\n", tree(44));
        assert_eq!(parse(&output), Some(false));
    }

    #[test]
    fn a_background_job_leaves_the_shell_at_its_prompt() {
        let output = format!("{}     44      38      38\n", tree(38));
        assert_eq!(parse(&output), Some(true));
    }

    #[test]
    fn what_a_login_shell_prints_before_the_answer_is_passed_over() {
        let output = format!("Welcome to fish\n12 34 56\n{}", tree(38));
        assert_eq!(parse(&output), Some(true));
    }

    #[test]
    fn processes_without_a_terminal_are_not_the_shell() {
        // macOS and the BSDs report `0` where Linux reports `-1`, and an
        // `sftp-server` beside the shell has no terminal either way.
        let output = "rulogman-parent 15\n15 13 0\n16 15 0\n17 15 -1\n";
        assert_eq!(parse(output), None);
    }

    #[test]
    fn one_busy_shell_among_several_counts() {
        let output = "rulogman-parent 15\n16 15 16\n17 15 30\n";
        assert_eq!(parse(output), Some(false));
    }

    #[test]
    fn output_that_is_not_the_answer_reads_as_unknown() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("ps: unknown option -- o\n"), None);
        assert_eq!(parse("38 37 38\n"), None, "no parent was reported");
        assert_eq!(parse("rulogman-parent\n38 37 38\n"), None);
        assert_eq!(parse("rulogman-parent 37\n38 37\n"), None);
        assert_eq!(parse("rulogman-parent 37\n38 37 38 4\n"), None);
    }
}
