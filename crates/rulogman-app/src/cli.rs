//! The command-line questions answered without starting the app.
//!
//! One so far: `--version` — or `-V`, as most command-line tools spell it
//! short — prints the name and version and exits. It is answered before
//! anything else in `run`: before the logger, before the configuration is
//! migrated or read, before a window exists. A question about the binary
//! should come back at once and leave the machine exactly as it found it.
//!
//! Only the first argument is asked. Everything after `-e` belongs to the
//! program it names, so `rulogman -e foo --version` is `foo`'s flag rather
//! than ours; asking the first argument alone is what keeps the two apart
//! without this module having to know what `-e` is.

use std::ffi::{OsStr, OsString};

use crate::workspace::IDENTITY;

/// The spellings that ask for the version.
const VERSION_FLAGS: [&str; 2] = ["--version", "-V"];

/// Prints `rulogman <version>` and answers `true` if the launch asked for it.
///
/// `true` means the process has nothing left to do: the caller returns
/// without building an app.
pub fn answer_version_flag() -> bool {
    if !asks_for_version(std::env::args_os().skip(1)) {
        return false;
    }
    #[cfg(windows)]
    attach_parent_console();
    println!("{}", version_line());
    true
}

/// Whether the arguments after the program name ask for the version.
fn asks_for_version(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter().next().is_some_and(|first| {
        VERSION_FLAGS
            .iter()
            .any(|flag| first.as_os_str() == OsStr::new(flag))
    })
}

/// What `--version` prints: the name and the version, nothing else, so a
/// script can take the second word as it is.
fn version_line() -> String {
    format!("{} {}", IDENTITY.name, IDENTITY.version)
}

/// Lends the process the console of the shell that started it, so the line
/// lands there.
///
/// A release build is a GUI-subsystem program: Windows gives it no console and
/// no standard handles, so without this `println!` writes to nothing. The
/// console is the parent's and only ever borrowed — this never opens a window
/// of its own, and a launch from Explorer, which has no console to lend,
/// simply prints nowhere. Standard output that is already somewhere — a file
/// or a pipe the line was redirected to, or the console a debug build has — is
/// left alone, so the redirect keeps working.
#[cfg(windows)]
fn attach_parent_console() {
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
    };

    // SAFETY: both calls take their argument by value and write through no
    // pointer; the first only reads the process's standard output handle.
    let unset = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.map_or(true, |out| out.is_invalid());
    if unset {
        // SAFETY: as above. A failure leaves the process without a console,
        // which is where it already was.
        let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn either_spelling_asks_for_the_version() {
        assert!(asks_for_version(args(&["--version"])));
        assert!(asks_for_version(args(&["-V"])));
        // Whatever follows it is not read.
        assert!(asks_for_version(args(&["--version", "/tmp"])));
    }

    #[test]
    fn nothing_else_asks_for_the_version() {
        assert!(!asks_for_version(args(&[])));
        for word in ["-v", "--versions", "--version=1", "version", "/home/alice"] {
            assert!(!asks_for_version(args(&[word])), "{word} asked for it");
        }
    }

    /// A flag after the first argument is someone else's — after `-e`, the
    /// program's.
    #[test]
    fn only_the_first_argument_is_asked() {
        assert!(!asks_for_version(args(&["-e", "foo", "--version"])));
        assert!(!asks_for_version(args(&["/tmp", "-V"])));
    }

    #[test]
    fn the_line_is_the_name_and_the_version() {
        assert_eq!(
            version_line(),
            format!("rulogman {}", env!("CARGO_PKG_VERSION"))
        );
    }
}
