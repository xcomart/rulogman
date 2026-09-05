//! What the launch itself asked for: the paths handed to the application when
//! it starts, and where each of them says to put a shell.
//!
//! Two things arrive here, and they are the same thing wearing different
//! clothes. A path on the command line — `rulogman /var/log`, or the `%F` a
//! Linux desktop entry expands when a file manager's *Open with* runs it — and
//! the `file://` URLs macOS delivers to `application:openURLs:` instead of an
//! argv, which is how the Finder's own *Open with* speaks. Both end as a
//! directory to start a local shell in, so both go through [`start_dirs`].
//!
//! A third source is Linux-only, and carries no argument at all: KDE's *Open
//! Terminal Here* — and any launcher that treats rulogman as the desktop's
//! default terminal — runs the desktop entry's `Exec=` line unchanged and
//! communicates the folder only by setting the child's working directory,
//! because that is the one field `KTerminalLauncherJob` sets for a terminal it
//! does not otherwise know how to drive. A launch that named no paths but
//! started somewhere other than the user's home is that request arriving as a
//! process attribute instead of an argv entry, and [`implicit_start_dir`]
//! reads it back out.
//!
//! The same launcher has one more thing to say, and this time it does say it in
//! argv: a desktop entry marked `Terminal=true` — the *Run in terminal* box, and
//! everything `KTerminalLauncherJob` runs that way — is started by appending
//! `-e <command…>` to the terminal's own command line. That is the whole of the
//! protocol. KDE writes `--noclose` and `--workdir` for konsole, nothing at all
//! for a terminal it does not recognise, and reads none of the `X-Terminal*`
//! keys a desktop entry could have offered instead, so `-e` is the only thing
//! rulogman is ever handed and it has to mean what it means everywhere else.
//! The command is word-split before it is passed, arriving as several argv
//! entries, which is why [`split_launch_args`] takes *everything* after the flag
//! as the command rather than only the word following it.
//!
//! One thing on the command line is not a path at all: `--dashboard <name>`
//! asks for a saved dashboard to be opened as the window comes up, the same
//! arrangement the welcome screen lists. It is read off the argv by
//! [`split_launch_args`] before [`start_dirs`] ever sees it, because the two
//! kinds of request are answered in different places — a directory becomes a
//! local shell, a name becomes a lookup in the dashboard store — and the only
//! thing they share is the vector they arrived in.
//!
//! The same request has a second spelling, `rulogman://dashboard/<name>`, and it
//! exists because of what a *second* launch can carry. On macOS `open -a
//! rulogman` hands a running application no argv at all — only URLs reach it,
//! through `application:openURLs:` — so a flag can only ever be read by the
//! launch that starts the process, while a URL is heard by one that is already
//! up. Every platform can open a URL (`open`, `xdg-open`, `start`), so the one
//! spelling works everywhere and rulogman registers the scheme in all three
//! packagings. [`dashboard_url`] is the whole of the grammar; both doors into
//! this module — [`split_launch_args`] for an argv and [`split_open_urls`] for a
//! batch of URLs — put what it accepts in the same list `--dashboard` fills.
//!
//! Everything here is pure but for the one question only the filesystem can
//! answer — is this a directory, or a file in one — which is why it is a module
//! of free functions with its own tests rather than a step inside `main`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The long option that names a dashboard to open, in its separated spelling.
const DASHBOARD_FLAG: &str = "--dashboard";

/// The same option in its `--dashboard=NAME` spelling, which is the one a
/// desktop entry's `Exec=` line tends to carry because it survives being
/// word-split by anything that word-splits.
const DASHBOARD_FLAG_EQ: &str = "--dashboard=";

/// The option every terminal emulator has answered to since xterm: run this,
/// and nothing else.
///
/// Spelt with one dash because that is how the desktops spell it. Nothing asks
/// rulogman what it accepts — KDE appends `-e` to whatever command line is
/// configured as the terminal — so the flag is not ours to name.
const EXEC_FLAG: &str = "-e";

/// rulogman's own URL scheme, the one registered with the three desktops.
const URL_SCHEME: &str = "rulogman";

/// The only authority the scheme takes, and the one that says what the rest of
/// the URL names. Spelt as a host rather than as a path segment because
/// `rulogman://dashboard/Morning` is what every URL parser — and every user
/// reading it — already knows how to take apart.
const DASHBOARD_HOST: &str = "dashboard";

/// What a URL in this scheme has to look like, said once so that every place
/// that turns one down says the same thing.
const URL_FORM: &str =
    "a rulogman URL is rulogman://dashboard/<name>, with the name percent-encoded";

/// What a launch asked for, once the three kinds of request in an argv have
/// been told apart.
///
/// Named rather than a tuple because the three are answered in three different
/// places — a directory becomes a local shell, a name becomes a lookup in the
/// dashboard store, a command becomes a shell that runs it — and a caller
/// reading `.paths` is spared counting to three.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LaunchArgs {
    /// The paths, in the order they were given.
    pub paths: Vec<OsString>,
    /// The names of the dashboards asked for, in the order they were asked for.
    pub dashboards: Vec<String>,
    /// The command `-e` named — the program first, then its arguments — or
    /// `None` if the launch named no command.
    ///
    /// [`String`] rather than [`OsString`], which the paths keep, because a
    /// command line is carried through the session layer as strings and down
    /// into the pty as strings. The conversion is total or nothing: a command
    /// with a word that is not valid UTF-8 is dropped whole, since
    /// `to_string_lossy` would put `U+FFFD` where those bytes were and so
    /// silently run a *different* program than the one the launcher named.
    /// Refusing says so in the log; guessing would not.
    pub command: Option<Vec<String>>,
}

/// The launch arguments split by kind: the paths, in the order they were given,
/// the names of the dashboards asked for, in the order they were asked for, and
/// the command a `-e` named.
///
/// Both spellings of the option are accepted — `--dashboard morning` and
/// `--dashboard=morning` — because both are spellings a user will type and a
/// `.desktop` file will carry. Repeating the option asks for another dashboard
/// rather than replacing the first: several may be open at once, so several may
/// be named.
///
/// A `rulogman://dashboard/<name>` URL among the arguments is the third way of
/// asking for the same thing, and joins the names rather than the paths: that
/// is the form `xdg-open` and the Windows registry hand over, and the form a
/// macOS *first* launch may carry in its argv. See [`dashboard_url`], which is
/// also what turns a malformed one down — so a URL in this scheme never reaches
/// the path parser to be reported as a missing directory.
///
/// `-e` ends the parse: everything after it is the command, and nothing after
/// it is read as a path, a dashboard or a flag of rulogman's. That is the
/// xterm and konsole rule, and it has to be, because the command carries its
/// own arguments — `rulogman -e ssh --dashboard prod` is asking *ssh* for a
/// dashboard — and because a desktop word-splits the command before passing it,
/// so its words arrive as ordinary argv entries with nothing to mark where they
/// end. Whatever stood before the flag is still a path or a dashboard; only
/// what follows it is claimed. See the module doc comment for why `-e` is all
/// KDE ever says.
///
/// Everything that is not the option, its value or such a URL passes through
/// untouched, including anything else that looks like a flag. This is not an argument
/// parser and rulogman has no other options; a stray `-x` is left to
/// [`start_dirs`] to drop with the same warning it drops a missing path with,
/// which is a better answer than a usage message the user cannot see because
/// the window it would have printed to does not exist.
///
/// Ways of asking badly are dropped with a warning, the stance the whole module
/// takes: a trailing `--dashboard` that names nothing, because there is no name
/// to look up; a name that is not valid UTF-8, because a dashboard is named by
/// typing into a text field and no such name can ever match; a trailing `-e`,
/// because there is no command to run; and a command whose words are not valid
/// UTF-8, for the reason given on [`LaunchArgs::command`]. A launch that is
/// wrong about one thing still opens a window, and the warning says which thing.
pub fn split_launch_args<A>(args: A) -> LaunchArgs
where
    A: IntoIterator,
    A::Item: Into<OsString>,
{
    let mut paths = Vec::new();
    let mut dashboards = Vec::new();
    let mut command = None;
    let mut args = args.into_iter().map(Into::into);

    while let Some(arg) = args.next() {
        // The end of the parse rather than one more case in it: what follows
        // belongs to the command, including anything that looks like one of
        // rulogman's own flags.
        if arg.as_os_str() == OsStr::new(EXEC_FLAG) {
            command = exec_command(args.by_ref().collect());
            break;
        }
        // Compared as an `OsStr` rather than through `to_str`, so an argument
        // the platform allows and UTF-8 does not is not even asked about: it
        // cannot equal an ASCII flag, and it is a path.
        if arg.as_os_str() == OsStr::new(DASHBOARD_FLAG) {
            match args.next() {
                Some(name) => push_dashboard(name, &mut dashboards),
                None => log::warn!("ignoring a trailing {DASHBOARD_FLAG}: it names no dashboard"),
            }
            continue;
        }
        // `as_encoded_bytes` is only ever split on an ASCII prefix here, which
        // is the use its documentation blesses: the encoding is
        // self-synchronising, so a prefix of ASCII bytes cannot fall inside
        // anything else.
        let Some(name) = arg
            .as_encoded_bytes()
            .strip_prefix(DASHBOARD_FLAG_EQ.as_bytes())
        else {
            // Not a flag: either rulogman's own URL, or a path. The URL is
            // claimed here rather than in `start_dirs` because it names a
            // dashboard, and a dashboard is not somewhere to put a shell.
            match arg.to_str().filter(|text| is_rulogman_url(text)) {
                Some(url) => dashboards.extend(dashboard_url(url)),
                None => paths.push(arg),
            }
            continue;
        };
        match std::str::from_utf8(name) {
            Ok(name) => push_dashboard(OsString::from(name), &mut dashboards),
            Err(_) => log::warn!(
                "ignoring {DASHBOARD_FLAG_EQ}…: a dashboard name is typed into a text field, so it is valid UTF-8"
            ),
        }
    }

    LaunchArgs {
        paths,
        dashboards,
        command,
    }
}

/// The command the arguments after an `-e` name, or `None` with the reason
/// logged.
///
/// Taken as the whole tail rather than word by word, since that is what the
/// flag means; the two ways it can name nothing runnable — no words at all, and
/// a word this platform allows but UTF-8 does not — are the two warnings.
fn exec_command(argv: Vec<OsString>) -> Option<Vec<String>> {
    if argv.is_empty() {
        log::warn!("ignoring a trailing {EXEC_FLAG}: it names no command");
        return None;
    }

    let mut command = Vec::with_capacity(argv.len());
    for arg in argv {
        match arg.into_string() {
            Ok(arg) => command.push(arg),
            // The whole command, not just the word: running the rest without
            // it would be running something else entirely, and so would
            // running it with the bytes replaced.
            Err(raw) => {
                log::warn!(
                    "ignoring {EXEC_FLAG} and the command after it: {} is not valid UTF-8, and no guess at what it says can be run in its place",
                    raw.to_string_lossy()
                );
                return None;
            }
        }
    }
    Some(command)
}

/// The directory a command named by `-e` runs in: this process's own working
/// directory, whatever it happens to be.
///
/// Deliberately not [`implicit_start_dir`], which is the same question asked
/// for a different reason and answered differently. There the working directory
/// is a *signal* — the only way a file manager's *Open Terminal Here* can name
/// a folder — so the home directory has to be read as "no folder was meant".
/// Here nothing is being inferred: the launch said what to run, and this is
/// merely where to run it. A command started from the home directory runs in
/// the home directory, exactly as it would under any other terminal, and when
/// the desktop entry carried a `Path=` this is that directory.
#[cfg(unix)]
pub fn command_start_dir() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

/// Files one `--dashboard` value under the names to open, or drops it with the
/// reason logged.
fn push_dashboard(name: OsString, dashboards: &mut Vec<String>) {
    match name.into_string() {
        // An empty name is the `--dashboard=` and `--dashboard ""` spellings of
        // asking for nothing. Dropped here rather than left to the lookup so
        // that the warning names the argument that was wrong, which the lookup
        // — reporting a dashboard called "" — would not.
        Ok(name) if name.is_empty() => {
            log::warn!("ignoring {DASHBOARD_FLAG}: it names no dashboard");
        }
        Ok(name) => dashboards.push(name),
        Err(raw) => log::warn!(
            "ignoring {DASHBOARD_FLAG} {}: a dashboard name is typed into a text field, so it is valid UTF-8",
            raw.to_string_lossy()
        ),
    }
}

/// The URLs a launch delivered, split the way [`split_launch_args`] splits an
/// argv: everything [`start_dirs`] should look at, and the names of the
/// dashboards asked for.
///
/// This is the door macOS uses while the application is already running.
/// `application:openURLs:` is the only way a second launch can say anything at
/// all — `open -a rulogman` passes a running application no argv — so both
/// kinds of request arrive here as URLs: a `file://` for a folder, and a
/// `rulogman://dashboard/<name>` for a dashboard. Everything that is not the
/// latter is handed straight on, since [`start_dirs`] already knows what to do
/// with a `file://` URL and what to say about a scheme that is neither.
pub fn split_open_urls(urls: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut rest = Vec::new();
    let mut dashboards = Vec::new();

    for url in urls {
        if is_rulogman_url(&url) {
            dashboards.extend(dashboard_url(&url));
            continue;
        }
        rest.push(url);
    }

    (rest, dashboards)
}

/// The dashboard a `rulogman://dashboard/<name>` URL names, or `None` with the
/// reason logged.
///
/// The name is percent-encoded UTF-8, exactly as a URL's path component is
/// everywhere else — `rulogman://dashboard/Morning%20logs` asks for the
/// dashboard called *Morning logs* — which is what lets a name carry a space, a
/// slash or a language whose letters are not ASCII through a shell, a browser
/// and a registry entry unharmed. The scheme and the host are matched without
/// regard to case, because the things that hand URLs around lowercase both; the
/// name is matched exactly, since it is the one part that is data. A trailing
/// slash is tolerated, being what a URL bar tends to add.
///
/// Anything else in this scheme is dropped with the accepted form in the
/// warning: another host, no name at all, an escape that is not two hexadecimal
/// digits, or bytes that are not valid UTF-8 — no dashboard can be called any of
/// those, since a name is typed into a text field. A URL in some *other* scheme
/// is not this function's business and comes back `None` in silence, so that a
/// caller can go on to ask what else it might be.
pub fn dashboard_url(arg: &str) -> Option<String> {
    if !is_rulogman_url(arg) {
        return None;
    }
    let rest = arg.split_once("://").map(|(_, rest)| rest)?;

    let Some((host, name)) = rest.split_once('/') else {
        log::warn!("ignoring {arg}: {URL_FORM}");
        return None;
    };
    if !host.eq_ignore_ascii_case(DASHBOARD_HOST) {
        log::warn!("ignoring {arg}: {URL_FORM}");
        return None;
    }

    let name = name.strip_suffix('/').unwrap_or(name);
    let name = decode(name)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|name| !name.is_empty());
    if name.is_none() {
        log::warn!("ignoring {arg}: {URL_FORM}");
    }
    name
}

/// Whether `arg` is spelt in rulogman's own URL scheme, whatever it goes on to
/// say.
///
/// Asked before [`dashboard_url`] is trusted to have turned something down: a
/// URL this scheme claims is never also a path, so a caller that sees `None`
/// from a URL that answers `true` here should drop it rather than hand it on.
fn is_rulogman_url(arg: &str) -> bool {
    scheme(arg).is_some_and(|scheme| scheme.eq_ignore_ascii_case(URL_SCHEME))
}

/// The directories to open a local shell in, one per launch argument that
/// named somewhere real.
///
/// A directory stands for itself and a file stands for the directory holding
/// it, because *open this file with rulogman* can only sensibly mean *put me
/// where that file is*: the application has no notion of a shell "opened on" a
/// file. Anything else — a path that does not exist, a URL in a scheme this is
/// not, a `file://` URL naming another host — is dropped with a warning and
/// nothing more. The launch is not a command the user is waiting on an answer
/// to; it is a window opening, and a window that opens with one tab fewer than
/// asked for is a far better outcome than one that refuses to open at all.
///
/// The argument type is deliberately wide enough for both callers:
/// `std::env::args_os` yields [`OsString`], `on_open_urls` yields [`String`].
/// Taking the wider of the two also keeps a path the platform allows but UTF-8
/// does not — which unix filenames may well be — out of the panic
/// `std::env::args` would raise on it.
pub fn start_dirs<A>(args: A) -> Vec<PathBuf>
where
    A: IntoIterator,
    A::Item: Into<OsString>,
{
    args.into_iter()
        .filter_map(|arg| start_dir(&arg.into()))
        .collect()
}

/// The directory an argument-less Linux launch is implicitly starting in, or
/// `None` if it should open on the welcome screen instead.
///
/// Only meaningful once [`start_dirs`] has already come back empty: a launch
/// that named a path speaks for itself, and this is only ever the fallback
/// for the one that did not. See the module doc comment for why the process's
/// own working directory is the signal at all.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn implicit_start_dir() -> Option<PathBuf> {
    let home = directories::UserDirs::new().map(|dirs| dirs.home_dir().to_owned());
    implicit_start_dir_in(std::env::current_dir().ok(), home)
}

/// The pure decision behind [`implicit_start_dir`], taking the working
/// directory and the home directory as values so it can be tested without
/// touching either.
///
/// Compared lexically, like [`start_dir`]: this is a launcher telling us where
/// it put the child, not a path the user typed, so there is nothing here for a
/// symlink to have been resolved against in the first place. The desktop icon
/// and the application menu start rulogman in the home directory — or in `/`,
/// on a session that sets no working directory for what it launches — so both
/// are read as "no folder was actually meant" rather than as somewhere to open
/// a shell.
///
/// Gated with its caller rather than left to be dead code on the platforms
/// that never ask: the tests still exercise it everywhere.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn implicit_start_dir_in(cwd: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let cwd = cwd?;
    if home.is_some_and(|home| home == cwd) || cwd.parent().is_none() {
        return None;
    }
    Some(cwd)
}

/// Where a single launch argument says to start, or `None` with the reason
/// logged.
fn start_dir(arg: &OsString) -> Option<PathBuf> {
    // A `file://` URL is ASCII by construction — every byte outside the
    // unreserved set is percent-encoded — so an argument that is not valid
    // UTF-8 cannot be one, and there is nothing to parse: it is a path, and the
    // platform is welcome to keep whatever bytes it likes in it.
    let path = match arg.to_str() {
        Some(text) => parse(text)?,
        None => PathBuf::from(arg),
    };

    // Resolved against this process's working directory, which for a launch
    // from a shell is the directory the user typed the relative path in.
    // Lexical only: symlinks are left standing, so the shell opens in the path
    // the user named rather than in whatever it happens to point at.
    let path = std::path::absolute(&path).unwrap_or(path);
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => {
            log::warn!("ignoring {}: {error}", path.display());
            return None;
        }
    };

    if metadata.is_dir() {
        return Some(path);
    }
    match path.parent().map(Path::to_path_buf) {
        Some(parent) => Some(parent),
        // A file with no parent is not something a filesystem can produce
        // from an absolute path, but the type says it can.
        None => {
            log::warn!("ignoring {}: it is in no directory", path.display());
            None
        }
    }
}

/// The path a textual launch argument names, or `None` with the reason logged.
fn parse(arg: &str) -> Option<PathBuf> {
    if arg.is_empty() {
        return None;
    }
    match scheme(arg) {
        Some(scheme) if scheme.eq_ignore_ascii_case("file") => from_file_url(arg),
        // Both doors into this module claim rulogman's own scheme before the
        // paths are read, so this is only reachable by calling `start_dirs`
        // directly with one. It is answered here rather than left to the arm
        // below because "rulogman opens paths, not rulogman URLs" would be a
        // strange thing to tell anybody.
        Some(scheme) if scheme.eq_ignore_ascii_case(URL_SCHEME) => {
            log::warn!("ignoring {arg}: it names a dashboard rather than a path — {URL_FORM}");
            None
        }
        // Some other scheme entirely — `http://`, `ssh://`. Treating it as a
        // relative path would go looking for a directory called `http:` and
        // report *that* as missing, which tells the user nothing about what
        // was actually wrong with what they passed.
        Some(other) => {
            log::warn!("ignoring {arg}: rulogman opens paths, not {other} URLs");
            None
        }
        None => Some(PathBuf::from(arg)),
    }
}

/// The URL scheme `arg` begins with, or `None` if it begins with no scheme at
/// all.
///
/// Only the `scheme://` form counts. A bare `scheme:` is a valid URI but a
/// Windows path is `C:\...`, and one drive letter is not going to be read as a
/// protocol.
fn scheme(arg: &str) -> Option<&str> {
    let (scheme, _) = arg.split_once("://")?;
    let mut chars = scheme.chars();
    let valid = chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(scheme)
}

/// The path a `file://` URL names, or `None` with the reason logged.
fn from_file_url(url: &str) -> Option<PathBuf> {
    let rest = url.split_once("://").map(|(_, rest)| rest)?;
    // `file:///path` has an empty authority, `file://localhost/path` names this
    // machine explicitly, and both mean the same local path. Anything else
    // names a host whose filesystem this process cannot see.
    let path = match rest.strip_prefix('/') {
        Some(path) => path,
        None => {
            let (host, path) = rest.split_once('/')?;
            if !host.eq_ignore_ascii_case("localhost") {
                log::warn!("ignoring {url}: it names the filesystem of another host");
                return None;
            }
            path
        }
    };

    let mut bytes = decode(path).or_else(|| {
        log::warn!("ignoring {url}: it is not a well-formed file URL");
        None
    })?;
    // The authority is gone but the path's own leading slash is not, since it
    // was the separator that ended the authority.
    bytes.insert(0, b'/');
    // ...except on Windows, where a URL keeps the root slash in front of the
    // drive letter — `file:///C:/Users` — and a path does not.
    #[cfg(windows)]
    if matches!(bytes.get(2), Some(b':')) {
        bytes.remove(0);
    }

    let path = path_from_bytes(bytes);
    (!path.as_os_str().is_empty()).then_some(path)
}

/// Percent-decodes the path component of a URL, or `None` if an escape in it
/// is not two hexadecimal digits.
///
/// Bytes rather than characters throughout: a percent-escape encodes one byte
/// of the path, and a multi-byte character reaches this as several of them.
fn decode(path: &str) -> Option<Vec<u8>> {
    let source = path.as_bytes();
    let mut out = Vec::with_capacity(source.len());
    let mut index = 0;
    while index < source.len() {
        match source[index] {
            b'%' => {
                let digits = source.get(index + 1..index + 3)?;
                let high = (digits[0] as char).to_digit(16)?;
                let low = (digits[1] as char).to_digit(16)?;
                out.push((high * 16 + low) as u8);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    Some(out)
}

/// The decoded bytes as a path, keeping whatever the platform's filenames are
/// allowed to be.
#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;

    PathBuf::from(OsString::from_vec(bytes))
}

/// The decoded bytes as a path.
///
/// Windows filenames are UTF-16 and have no byte form to hand back, so this is
/// the one place a `file://` URL can be rejected for its contents: bytes that
/// are not UTF-8 name nothing this platform could have produced.
#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    match String::from_utf8(bytes) {
        Ok(text) => PathBuf::from(text),
        Err(_) => {
            log::warn!("ignoring a file URL: its path is not valid UTF-8");
            PathBuf::new()
        }
    }
}

/// What a launch argument resolves to, which is the whole of what this module
/// decides.
///
/// The cases worth pinning down are the ones a user can actually produce: a
/// directory and a file, since a file manager's *Open with* offers both; a path
/// that has gone since the launcher last saw it; and the `file://` spelling
/// macOS and the freedesktop `%U` field code use, percent-escapes and all. Each
/// runs against a real temporary tree rather than a mocked filesystem, because
/// the one thing this module cannot decide on its own is whether a path is a
/// directory, and that is exactly what the tests are here to check it asks.
#[cfg(test)]
mod tests {
    use super::*;

    /// A directory with one file in it, which is all the cases below need from
    /// the filesystem.
    ///
    /// The returned root goes through [`std::path::absolute`] on the way out so
    /// that it compares equal to what [`start_dirs`] will have made of the same
    /// path: on macOS a temporary directory sits under a symlinked `/var`, and
    /// nothing on either side resolves that.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let guard = tempfile::tempdir().expect("could not create a temporary directory");
        let root = std::path::absolute(guard.path()).expect("could not absolutise the fixture");
        let file = root.join("nginx.conf");
        std::fs::write(&file, b"server {}\n").expect("could not write the fixture file");
        (guard, root, file)
    }

    /// `path` as the `file://` URL a desktop environment would hand over for
    /// it, spelt the same way on both platforms: an empty authority, forward
    /// slashes, and the one character in these fixtures that has to be escaped.
    fn file_url(path: &Path) -> String {
        let text = path
            .to_str()
            .expect("the fixture path is not valid UTF-8")
            .replace('\\', "/");
        format!(
            "file:///{}",
            text.trim_start_matches('/').replace(' ', "%20")
        )
    }

    #[test]
    fn a_directory_is_where_the_shell_starts() {
        let (_guard, root, _file) = fixture();

        assert_eq!(start_dirs([root.clone()]), vec![root]);
    }

    #[test]
    fn a_file_starts_the_shell_in_the_directory_holding_it() {
        let (_guard, root, file) = fixture();

        assert_eq!(start_dirs([file]), vec![root]);
    }

    #[test]
    fn a_path_that_is_not_there_is_dropped() {
        let (_guard, root, _file) = fixture();

        assert!(start_dirs([root.join("gone")]).is_empty());
    }

    #[test]
    fn every_argument_gets_its_own_directory_in_order() {
        let (_guard, root, file) = fixture();
        let other = root.join("sub");
        std::fs::create_dir(&other).expect("could not create the second directory");

        assert_eq!(
            start_dirs([other.clone(), root.join("gone"), file]),
            vec![other, root]
        );
    }

    #[test]
    fn a_file_url_names_the_same_directory_a_path_does() {
        let (_guard, root, file) = fixture();

        assert_eq!(start_dirs([file_url(&root)]), vec![root.clone()]);
        assert_eq!(start_dirs([file_url(&file)]), vec![root]);
    }

    #[test]
    fn a_file_url_is_percent_decoded() {
        let (_guard, root, _file) = fixture();
        let spaced = root.join("my logs");
        std::fs::create_dir(&spaced).expect("could not create the spaced directory");
        let url = file_url(&spaced);
        assert!(
            url.contains("%20"),
            "the fixture URL escapes nothing: {url}"
        );

        assert_eq!(start_dirs([url]), vec![spaced]);
    }

    #[test]
    fn a_file_url_may_name_this_machine_explicitly() {
        let (_guard, root, _file) = fixture();
        let url = file_url(&root).replacen("file://", "file://localhost", 1);

        assert_eq!(start_dirs([url]), vec![root]);
    }

    #[test]
    fn a_file_url_naming_another_host_is_dropped() {
        let (_guard, root, _file) = fixture();
        let url = file_url(&root).replacen("file://", "file://example.com", 1);

        assert!(start_dirs([url]).is_empty());
    }

    #[test]
    fn a_malformed_escape_is_dropped() {
        let (_guard, root, _file) = fixture();

        assert!(start_dirs([format!("{}%2", file_url(&root))]).is_empty());
    }

    #[test]
    fn a_url_in_another_scheme_is_dropped() {
        assert!(start_dirs(["https://example.com/var/log"]).is_empty());
    }

    #[test]
    fn an_empty_argument_is_dropped() {
        assert!(start_dirs([""]).is_empty());
    }

    /// The paths half of a split, as plain strings, so a failing assertion is
    /// readable.
    fn paths(args: &[&str]) -> Vec<String> {
        split_launch_args(args.iter().map(OsString::from))
            .paths
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// The dashboards of the same split.
    fn dashboards(args: &[&str]) -> Vec<String> {
        split_launch_args(args.iter().map(OsString::from)).dashboards
    }

    /// The command of the same split.
    fn command(args: &[&str]) -> Option<Vec<String>> {
        split_launch_args(args.iter().map(OsString::from)).command
    }

    #[test]
    fn a_launch_that_names_no_dashboard_hands_every_argument_on_as_a_path() {
        let args = ["/var/log", "file:///etc", "-x"];

        assert_eq!(paths(&args), args);
        assert!(dashboards(&args).is_empty());
    }

    #[test]
    fn both_spellings_of_the_option_name_a_dashboard() {
        assert_eq!(dashboards(&["--dashboard", "morning"]), ["morning"]);
        assert_eq!(dashboards(&["--dashboard=morning"]), ["morning"]);
    }

    #[test]
    fn the_option_may_stand_anywhere_among_the_paths() {
        let args = [
            "/var/log",
            "--dashboard",
            "morning",
            "/etc",
            "--dashboard=deploy",
            "/srv",
        ];

        assert_eq!(paths(&args), ["/var/log", "/etc", "/srv"]);
        assert_eq!(dashboards(&args), ["morning", "deploy"]);
    }

    #[test]
    fn repeating_the_option_asks_for_another_dashboard() {
        assert_eq!(
            dashboards(&["--dashboard", "morning", "--dashboard", "deploy"]),
            ["morning", "deploy"]
        );
    }

    #[test]
    fn the_same_dashboard_may_be_named_twice() {
        // Deduplication is the store lookup's business, not the parser's: this
        // reports what was asked for.
        assert_eq!(
            dashboards(&["--dashboard", "morning", "--dashboard=morning"]),
            ["morning", "morning"]
        );
    }

    #[test]
    fn a_trailing_option_with_no_name_is_dropped() {
        let args = ["/var/log", "--dashboard"];

        assert_eq!(paths(&args), ["/var/log"]);
        assert!(dashboards(&args).is_empty());
    }

    #[test]
    fn an_empty_name_is_dropped_in_either_spelling() {
        assert!(dashboards(&["--dashboard", ""]).is_empty());
        assert!(dashboards(&["--dashboard="]).is_empty());
    }

    #[test]
    fn the_value_of_the_option_is_never_read_as_a_path() {
        // The name of a dashboard may well also be the name of a directory, and
        // the argument after the option belongs to the option.
        let args = ["--dashboard", "/var/log"];

        assert!(paths(&args).is_empty());
        assert_eq!(dashboards(&args), ["/var/log"]);
    }

    #[test]
    fn an_argument_that_is_not_valid_utf8_is_a_path() {
        // The one case that cannot be spelt with a `&str`: a filename the
        // platform allows and UTF-8 does not must reach `start_dirs` intact
        // rather than being weighed against an ASCII flag.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            let raw = OsString::from_vec(vec![b'/', 0xff, b'/', b'l', b'o', b'g']);
            let split = split_launch_args([raw.clone()]);

            assert_eq!(split.paths, vec![raw]);
            assert!(split.dashboards.is_empty());
        }
    }

    #[test]
    fn a_dashboard_name_that_is_not_valid_utf8_is_dropped() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            let raw = OsString::from_vec(vec![0xff, 0xfe]);
            let split = split_launch_args([OsString::from("--dashboard"), raw]);

            assert!(split.paths.is_empty());
            assert!(split.dashboards.is_empty());
        }
    }

    #[test]
    fn everything_after_the_exec_flag_is_the_command() {
        // The desktop word-splits the command before passing it, so `htop
        // --utf-force` arrives as two arguments and both belong to htop.
        let args = ["-e", "htop", "--utf-force"];

        assert!(paths(&args).is_empty());
        assert_eq!(
            command(&args),
            Some(vec!["htop".into(), "--utf-force".into()])
        );
    }

    #[test]
    fn paths_and_dashboards_before_the_exec_flag_are_kept() {
        let args = ["/tmp", "--dashboard", "x", "-e", "btop"];

        assert_eq!(paths(&args), ["/tmp"]);
        assert_eq!(dashboards(&args), ["x"]);
        assert_eq!(command(&args), Some(vec!["btop".into()]));
    }

    #[test]
    fn a_dashboard_flag_after_the_exec_flag_belongs_to_the_command() {
        // rulogman stops reading at `-e`: what follows is somebody else's
        // command line, and its flags are that program's business.
        let args = ["-e", "foo", "--dashboard", "x"];

        assert!(paths(&args).is_empty());
        assert!(dashboards(&args).is_empty());
        assert_eq!(
            command(&args),
            Some(vec!["foo".into(), "--dashboard".into(), "x".into()])
        );
    }

    #[test]
    fn a_trailing_exec_flag_names_no_command_and_is_dropped() {
        let args = ["/var/log", "-e"];

        assert_eq!(paths(&args), ["/var/log"]);
        assert_eq!(command(&args), None);
    }

    #[test]
    fn a_launch_that_names_no_command_has_none() {
        assert_eq!(command(&["/var/log", "--dashboard", "morning"]), None);
    }

    #[test]
    fn a_command_word_that_is_not_valid_utf8_drops_the_whole_command() {
        // Lossy conversion would exec a program with `U+FFFD` where those
        // bytes were, which is a different program; there is nothing to run.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            let raw = OsString::from_vec(vec![b'/', 0xff, b'/', b'r', b'u', b'n']);
            let split = split_launch_args([OsString::from("-e"), raw, OsString::from("--follow")]);

            assert!(split.command.is_none());
            assert!(split.paths.is_empty());
        }
    }

    #[test]
    fn an_implicit_launch_from_home_opens_the_welcome_screen() {
        let home = PathBuf::from("/home/alice");

        assert_eq!(implicit_start_dir_in(Some(home.clone()), Some(home)), None);
    }

    #[test]
    fn an_implicit_launch_from_the_filesystem_root_opens_the_welcome_screen() {
        let root = PathBuf::from("/");

        assert_eq!(
            implicit_start_dir_in(Some(root), Some(PathBuf::from("/home/alice"))),
            None
        );
    }

    #[test]
    fn an_implicit_launch_from_elsewhere_starts_a_shell_there() {
        let project = PathBuf::from("/home/alice/projects/rulogman");

        assert_eq!(
            implicit_start_dir_in(Some(project.clone()), Some(PathBuf::from("/home/alice"))),
            Some(project)
        );
    }

    #[test]
    fn an_implicit_launch_with_no_known_working_directory_opens_the_welcome_screen() {
        assert_eq!(
            implicit_start_dir_in(None, Some(PathBuf::from("/home/alice"))),
            None
        );
    }

    #[test]
    fn a_dashboard_url_names_the_dashboard_in_its_path() {
        assert_eq!(
            dashboard_url("rulogman://dashboard/Morning"),
            Some("Morning".to_owned())
        );
    }

    #[test]
    fn a_dashboard_url_is_percent_decoded() {
        // A space and a name in a script that is not ASCII: both are what a
        // dashboard may actually be called, and both reach the scheme escaped.
        assert_eq!(
            dashboard_url("rulogman://dashboard/Morning%20logs"),
            Some("Morning logs".to_owned())
        );
        assert_eq!(
            dashboard_url("rulogman://dashboard/%EC%95%84%EC%B9%A8"),
            Some("아침".to_owned())
        );
    }

    #[test]
    fn a_trailing_slash_is_tolerated() {
        assert_eq!(
            dashboard_url("rulogman://dashboard/Morning/"),
            Some("Morning".to_owned())
        );
    }

    #[test]
    fn the_scheme_and_the_host_are_matched_without_regard_to_case() {
        // What a URL bar, a registry entry and `xdg-open` may each have
        // lowercased on the way. The name itself is data and is not touched.
        assert_eq!(
            dashboard_url("RuLogMan://DashBoard/Morning"),
            Some("Morning".to_owned())
        );
    }

    #[test]
    fn a_url_under_another_host_is_dropped() {
        assert_eq!(dashboard_url("rulogman://session/Morning"), None);
        assert_eq!(dashboard_url("rulogman://dashboard"), None);
    }

    #[test]
    fn a_url_naming_no_dashboard_is_dropped() {
        assert_eq!(dashboard_url("rulogman://dashboard/"), None);
    }

    #[test]
    fn a_malformed_escape_in_a_dashboard_url_is_dropped() {
        assert_eq!(dashboard_url("rulogman://dashboard/Morning%2"), None);
        assert_eq!(dashboard_url("rulogman://dashboard/%ff"), None);
    }

    #[test]
    fn a_url_in_another_scheme_is_not_a_dashboard() {
        assert_eq!(dashboard_url("https://dashboard/Morning"), None);
        assert_eq!(dashboard_url("file:///var/log"), None);
        assert_eq!(dashboard_url("/var/log"), None);
    }

    #[test]
    fn a_dashboard_url_on_the_command_line_names_a_dashboard() {
        let args = ["/var/log", "rulogman://dashboard/Morning%20logs"];

        assert_eq!(paths(&args), ["/var/log"]);
        assert_eq!(dashboards(&args), ["Morning logs"]);
    }

    #[test]
    fn a_malformed_dashboard_url_is_dropped_rather_than_read_as_a_path() {
        // The point of claiming the whole scheme: this must not go on to be
        // reported as a missing directory called `rulogman:`.
        let args = ["rulogman://dashboard/", "rulogman://elsewhere/Morning"];

        assert!(paths(&args).is_empty());
        assert!(dashboards(&args).is_empty());
    }

    #[test]
    fn open_urls_are_split_into_paths_and_dashboards() {
        let (rest, names) = split_open_urls(vec![
            "file:///var/log".to_owned(),
            "rulogman://dashboard/Morning".to_owned(),
            "/etc".to_owned(),
            "rulogman://dashboard/Night".to_owned(),
        ]);

        assert_eq!(rest, ["file:///var/log", "/etc"]);
        assert_eq!(names, ["Morning", "Night"]);
    }

    #[test]
    fn a_malformed_url_in_a_batch_takes_nothing_else_with_it() {
        let (rest, names) = split_open_urls(vec![
            "rulogman://dashboard/%2".to_owned(),
            "file:///var/log".to_owned(),
        ]);

        assert_eq!(rest, ["file:///var/log"]);
        assert!(names.is_empty());
    }
}
