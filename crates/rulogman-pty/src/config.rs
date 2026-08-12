//! Startup settings for a local pty session.
//!
//! Everything the transport needs in order to put a shell on a fresh pty lives
//! in [`PtyConfig`]. There is nothing secret in here — unlike the SSH side,
//! a local shell needs no credentials — so the type derives its `Debug`.

use std::path::PathBuf;

/// Terminal type exported to the child by default.
///
/// Matches the value the SSH transport advertises, so that a local tab and a
/// remote tab are driven by the same terminfo and render identically.
pub const DEFAULT_TERM: &str = "xterm-256color";

/// How a local shell should be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyConfig {
    /// Initial width of the pty, in columns.
    pub cols: u16,
    /// Initial height of the pty, in rows.
    pub rows: u16,
    /// TERM value exported to the child.
    pub term: String,
    /// Directory the shell starts in; `None` means the process's own.
    pub cwd: Option<PathBuf>,
    /// Program and arguments to run instead of the user's login shell.
    ///
    /// The application leaves this `None` — the point of a local tab is the
    /// user's own shell, started as a login shell. The tests use it to run a
    /// deterministic command instead.
    pub command: Option<Vec<String>>,
}

impl PtyConfig {
    /// Settings for a login shell of the given size, in the current directory.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            term: DEFAULT_TERM.to_owned(),
            cwd: None,
            command: None,
        }
    }
}
