//! The filesystem of a WSL distribution, reached from Windows.
//!
//! A WSL tab is an ordinary pty running an ordinary command, and everything
//! about it is local except the one thing the file panel cares about: the shell
//! on the other end stands in a Linux filesystem, and the directory it reports
//! over `OSC 7` — `/home/ada` — names nothing in the Windows tree this process
//! sees. For as long as that was true the tab simply had no panel.
//!
//! What makes one possible is that Windows already exposes every running
//! distribution as a network share, `\\wsl.localhost\<distro>`, served by WSL's
//! own 9P server. So there is no new transport here and no second process to
//! talk to: this is [`LocalSource`](super::LocalSource) again, over `std::fs`
//! again, with one translation layer bolted on the outside.
//!
//! **The panel keeps living in Linux paths.** That is the decision the rest of
//! the module follows from. The alternative — handing the panel
//! `//wsl.localhost/Ubuntu/home/ada` — would have been less code here and wrong
//! everywhere else: the breadcrumbs would show four segments of plumbing before
//! the first real one, `..` out of `/` would walk into the share list, and the
//! path the panel showed would no longer be the path the shell beside it
//! prints. So [`to_windows`] and [`to_linux`] translate at the two edges of
//! every call, and nothing between the panel and the shell ever sees a UNC
//! path — apart from the sentence a failure carries, and even that is written
//! in Linux paths wherever the failing path was one.
//!
//! Windows-only, because `\\wsl.localhost` is: on Linux itself a distribution
//! is just this machine, and there is nothing to translate.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use futures::channel::mpsc::UnboundedSender;
use gpui::BackgroundExecutor;
use std::os::windows::process::CommandExt;

use super::local::{can_write, copy_file_as};
use super::{FileEntry, FileError, FileSource, RootAccess};
use crate::wsl::{CREATE_NO_WINDOW, decode_output};

/// The UNC server name every current build of Windows serves WSL shares under.
const SERVER: &str = "wsl.localhost";

/// The name the same shares answered to before Windows 10 21H1.
///
/// Still aliased on new builds, and still the only name on old ones, so it is
/// what [`unc_server`] falls back to when the modern name resolves nothing.
const LEGACY_SERVER: &str = "wsl$";

/// Which of the two names this machine actually serves the shares under.
///
/// Process-global rather than per source, and settled once rather than per
/// call, because the answer is a property of the *Windows build* and not of the
/// distribution: whichever name works for the first share works for all of
/// them. The probe behind it opens a path on the 9P share, which starts a
/// stopped distribution and can take seconds, so paying it once per run matters
/// — and it is why [`unc_server`] is only ever called from inside a blocking
/// closure, never from a constructor.
static UNC_SERVER: OnceLock<&'static str> = OnceLock::new();

/// Server names a path handed *back* to us may legitimately carry.
///
/// Both are accepted on the way in whatever [`unc_server`] chose on the way
/// out, because the two are aliases of one share and a canonicalised path may
/// come back under either.
const KNOWN_SERVERS: [&str; 2] = [SERVER, LEGACY_SERVER];

/// A source over one WSL distribution's filesystem.
///
/// Holds no handle and starts no process: everything it needs is the
/// distribution's name and somewhere to run blocking work. That is deliberate
/// rather than incidental — [`Session::files`](crate::session::Session::files)
/// builds one of these on every terminal notification, so a constructor that
/// probed anything would probe it on every frame the shell produced output on.
pub struct WslSource {
    /// Where the blocking `std::fs` and `wsl.exe` calls are sent.
    ///
    /// Cloned from the application's, as [`LocalSource`](super::LocalSource)
    /// does and for the same reason: [`FileSource`]'s futures are `?Send` and
    /// so are polled on the UI thread, where a `std::fs` call over a network
    /// redirector would hold up a repaint for as long as the share took.
    executor: BackgroundExecutor,
    /// The distribution this source browses, as `wsl.exe -l -q` named it.
    ///
    /// Also the share name, which is why it is kept as written rather than
    /// folded to a case: the user sees it in error messages.
    distro: String,
}

impl WslSource {
    /// A source over `distro`, running its filesystem work on `executor`.
    pub fn new(executor: BackgroundExecutor, distro: String) -> Self {
        Self { executor, distro }
    }

    /// Runs one blocking piece of work off the UI thread.
    ///
    /// The first call of a run may be slow in a way no local one is: reaching a
    /// share of a stopped distribution starts that distribution. There is no
    /// timeout on it on purpose — cutting the wait short would report a failure
    /// for a distribution that was about to answer perfectly well, and the
    /// panel is already drawn as busy while this runs.
    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T, FileError> + Send + 'static,
    ) -> Result<T, FileError> {
        self.executor.spawn(async move { work() }).await
    }
}

#[async_trait::async_trait(?Send)]
impl FileSource for WslSource {
    /// The home directory of the user the distribution's shells run as.
    ///
    /// Asked of the distribution itself rather than guessed at from the share,
    /// because only it knows: the default user is configurable, `/etc/passwd`
    /// may point anywhere, and `\\wsl.localhost\Ubuntu\home` is frequently a
    /// directory with several names in it.
    ///
    /// `--cd ~` is the same flag the welcome screen starts a WSL tab with, so
    /// this answers with the directory that tab's shell actually opened in, and
    /// `-e pwd` runs a Linux binary directly rather than through a shell — its
    /// standard output is that binary's own bytes, UTF-8, and not the UTF-16
    /// `wsl.exe` writes its *own* listings in.
    ///
    /// A failure is reported rather than softened into `/`. Falling back to the
    /// root was the original shape of this bug — a panel that opens on `/etc`
    /// and `/proc` instead of on the user's files looks like it works and is
    /// showing the wrong place, which is worse than a sentence saying the
    /// distribution could not be asked.
    async fn home(&self) -> Result<String, FileError> {
        let distro = self.distro.clone();
        self.blocking(move || {
            let output = Command::new("wsl.exe")
                .args(["-d", &distro, "--cd", "~", "-e", "pwd"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .map_err(|error| {
                    FileError::Local(format!("could not run wsl.exe for {distro}: {error}"))
                })?;

            if !output.status.success() {
                // The bytes on stderr are not decodable without knowing which
                // side wrote them — `wsl.exe`'s own errors are UTF-16, a Linux
                // binary's are UTF-8 — so the status stands in the sentence and
                // the rest goes to the log for whoever is debugging it.
                log::debug!(
                    "wsl.exe -d {distro} --cd ~ -e pwd wrote {} bytes to stderr",
                    output.stderr.len()
                );
                return Err(FileError::Backend(format!(
                    "{distro} did not say where its home directory is ({})",
                    output.status
                )));
            }

            let home = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !home.starts_with('/') {
                return Err(FileError::Backend(format!(
                    "{distro} reported {home:?} as its home directory, which is not a path"
                )));
            }
            Ok(home)
        })
        .await
    }

    /// Canonicalises `path` — and does it on the Windows side.
    ///
    /// The panel's only use of this is `<current>/..`, and answering it here
    /// rather than by trimming the string is what makes a symbolic link out of
    /// the distribution's own tree resolve to where it really goes. The round
    /// trip through the share is what costs: `canonicalize` answers in the
    /// verbatim UNC form, which [`to_linux`] has to strip back down.
    async fn realpath(&self, path: &str) -> Result<String, FileError> {
        let path = path.to_owned();
        let distro = self.distro.clone();
        self.blocking(move || {
            let resolved = std::fs::canonicalize(to_windows(&distro, &path)?)
                .map_err(|error| FileError::Local(format!("could not resolve {path}: {error}")))?;
            to_linux(&distro, &resolved).ok_or_else(|| {
                FileError::Path(format!("{path} resolves to somewhere outside {distro}"))
            })
        })
        .await
    }

    /// Lists `path`, to exactly the rules
    /// [`LocalSource::read_dir`](super::LocalSource::read_dir) lists by — a
    /// link whose target cannot be stat'ed is listed as a non-directory, an
    /// entry that cannot be read at all is dropped with a line in the log, and
    /// a name that is not valid UTF-8 is dropped because the panel would
    /// otherwise aim its next rename at a different file than the one it drew.
    ///
    /// One more name is dropped here than there, for the same reason one more
    /// can be: a Linux file name may contain a backslash, and a Windows path
    /// cannot. Such a name is unreachable through the share at all — there is
    /// no path this source could build that would name it — so listing it would
    /// draw a row that fails whatever the user did with it.
    async fn read_dir(&self, path: &str) -> Result<Vec<FileEntry>, FileError> {
        let path = path.to_owned();
        let distro = self.distro.clone();
        self.blocking(move || {
            let listing = std::fs::read_dir(to_windows(&distro, &path)?)
                .map_err(|error| FileError::Local(format!("could not list {path}: {error}")))?;

            let mut entries = Vec::new();
            for entry in listing {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        log::debug!("skipping an entry of {path}: {error}");
                        continue;
                    }
                };
                let Ok(name) = entry.file_name().into_string() else {
                    log::debug!("skipping an entry of {path}: its name is not valid UTF-8");
                    continue;
                };
                if name.contains('\\') {
                    log::debug!("skipping {path}/{name}: its name cannot be spelled as a path");
                    continue;
                }

                let child = entry.path();
                // The link itself, so that this is the real "is it a
                // directory?" test rather than the target's answer.
                let link = match std::fs::symlink_metadata(&child) {
                    Ok(link) => link,
                    Err(error) => {
                        log::debug!("skipping {path}/{name}: {error}");
                        continue;
                    }
                };
                let is_symlink = link.is_symlink();
                let (is_dir, size) = if is_symlink {
                    match std::fs::metadata(&child) {
                        Ok(target) => (target.is_dir(), target.len()),
                        Err(error) => {
                            log::debug!("could not resolve the symlink {path}/{name}: {error}");
                            (false, link.len())
                        }
                    }
                } else {
                    (link.is_dir(), link.len())
                };

                entries.push(FileEntry {
                    name,
                    is_dir,
                    is_symlink,
                    size,
                });
            }
            Ok(entries)
        })
        .await
    }

    /// Creates `path`, treating "it is already a directory" as success — the
    /// same non-recursive, existing-is-fine contract
    /// [`LocalSource::mkdir`](super::LocalSource::mkdir) keeps, and which the
    /// panel's recursive copy leans on.
    async fn mkdir(&self, path: &str) -> Result<(), FileError> {
        let path = path.to_owned();
        let distro = self.distro.clone();
        self.blocking(move || {
            let target = to_windows(&distro, &path)?;
            let outcome = std::fs::create_dir(&target);
            let Err(error) = outcome else {
                return Ok(());
            };
            if std::fs::metadata(&target).is_ok_and(|metadata| metadata.is_dir()) {
                return Ok(());
            }
            Err(FileError::Local(format!(
                "could not create the directory {path}: {error}"
            )))
        })
        .await
    }

    async fn remove_file(&self, path: &str) -> Result<(), FileError> {
        let path = path.to_owned();
        let distro = self.distro.clone();
        self.blocking(move || {
            std::fs::remove_file(to_windows(&distro, &path)?)
                .map_err(|error| FileError::Local(format!("could not delete {path}: {error}")))
        })
        .await
    }

    /// Deletes the empty directory at `path`; non-recursive, as the trait says.
    async fn remove_dir(&self, path: &str) -> Result<(), FileError> {
        let path = path.to_owned();
        let distro = self.distro.clone();
        self.blocking(move || {
            std::fs::remove_dir(to_windows(&distro, &path)?).map_err(|error| {
                FileError::Local(format!("could not delete the directory {path}: {error}"))
            })
        })
        .await
    }

    async fn rename(&self, old: &str, new: &str) -> Result<(), FileError> {
        let old = old.to_owned();
        let new = new.to_owned();
        let distro = self.distro.clone();
        self.blocking(move || {
            let (from, to) = (to_windows(&distro, &old)?, to_windows(&distro, &new)?);
            std::fs::rename(from, to)
                .map_err(|error| FileError::Local(format!("could not rename {old}: {error}")))
        })
        .await
    }

    /// Copies `local` — a path on the Windows side — into the distribution's
    /// directory `dir`, keeping its file name, and answers the Linux path it
    /// was written to.
    ///
    /// The destination is spelled by joining the *Linux* paths and translating
    /// the result, not by [`Path::join`]ing onto the share: the answer goes
    /// back to the panel, which navigates by it, and a UNC path there would be
    /// a path no other call of this source accepts.
    async fn copy_in(
        &self,
        local: PathBuf,
        dir: &str,
        progress: Option<UnboundedSender<u64>>,
    ) -> Result<String, FileError> {
        let dir = dir.to_owned();
        let distro = self.distro.clone();
        self.blocking(move || {
            let written = join(&dir, &file_name(&local)?);
            let target = to_windows(&distro, &written)?;
            copy_file_as(
                &local,
                &target,
                &local.display().to_string(),
                &written,
                progress.as_ref(),
            )?;
            Ok(written)
        })
        .await
    }

    /// Copies the distribution's file at `path` to `local` on the Windows side.
    async fn copy_out(
        &self,
        path: &str,
        local: PathBuf,
        progress: Option<UnboundedSender<u64>>,
    ) -> Result<(), FileError> {
        let path = path.to_owned();
        let distro = self.distro.clone();
        self.blocking(move || {
            let from = to_windows(&distro, &path)?;
            let shown = local.display().to_string();
            copy_file_as(&from, &local, &path, &shown, progress.as_ref())
        })
        .await
    }

    /// Opens the file through the share the way a save would, and reports which
    /// of the two answers the attempt was.
    ///
    /// The same [`can_write`] probe the local source makes, on the same
    /// `std::fs`, and it tests exactly what a save will do: writing a file into
    /// this distribution is a plain copy onto `\\wsl.localhost`, so whatever
    /// the 9P server has to say about permission it says here first.
    ///
    /// A path this source cannot spell answers `true` rather than `false`. That
    /// is not the filesystem refusing — nothing was asked of it — and the trait
    /// keeps the fail-open rule for exactly such an answerless case; such a path
    /// could not have been read into the editor to begin with.
    async fn writable(&self, path: &str) -> bool {
        let path = path.to_owned();
        let distro = self.distro.clone();
        self.blocking(move || {
            Ok(match to_windows(&distro, &path) {
                Ok(target) => can_write(&target),
                Err(_) => true,
            })
        })
        .await
        .unwrap_or(true)
    }

    /// Always [`RootAccess::Granted`]: `wsl.exe -u root` starts a process as
    /// root inside the distribution and asks for no password to do it.
    ///
    /// That is not a hole this module opens. A distribution's root is not the
    /// machine's — it owns nothing outside the distribution's own filesystem —
    /// and anybody who can start a WSL shell at all can already type the same
    /// flag into one. What the editor adds is a button in the place where the
    /// alternative was closing the pane and doing exactly this by hand.
    ///
    /// Answered without asking the distribution anything, which is why this
    /// costs nothing however often it is called: the flag is a property of WSL
    /// itself and not of the machine's configuration.
    async fn root_access(&self) -> RootAccess {
        RootAccess::Granted
    }

    /// Always `Ok`, because there is nothing to unlock: no password to check,
    /// nothing to keep, and no timestamp that can expire between this call and
    /// the save that follows it.
    ///
    /// Implemented rather than inherited all the same. The default refuses, and
    /// a source that has a root to write as must not answer a question about
    /// that root with "there is none" — the editor takes this call's verdict as
    /// the one that unlocks the buffer.
    async fn unlock_root(&self, _password: Option<&str>, _remember: bool) -> Result<(), FileError> {
        Ok(())
    }

    /// Writes `local` into the distribution's `dir` as root, and answers the
    /// Linux path it landed at.
    ///
    /// Not through the share, which is the whole of the point: `\\wsl.localhost`
    /// serves a distribution's files as that distribution's *default user*, and
    /// that account is precisely the one [`FileSource::writable`] has already
    /// said may not write this file. So the bytes go in through the
    /// distribution instead — a process started as root inside it, with the
    /// staging file on its standard input.
    ///
    /// That process is `tee` rather than a shell redirection, for two reasons
    /// that both matter. `--exec` runs the binary directly with no shell in
    /// between, so the destination travels as a single argv element and nothing
    /// in it — a space, a quote, a `$`, a newline — can be read as syntax;
    /// `sh -c "cat > …"` would have needed quoting rules guessed at this end,
    /// and a guess that is wrong once is wrong about somebody's `/etc`. And
    /// `tee` *truncates* the file that is there rather than replacing it, so
    /// the inode survives the write and with it the file's owner, group and
    /// mode: editing `/etc/hosts` as root does not hand `/etc/hosts` to root.
    ///
    /// Neither of [`unc_path`]'s two refusals applies here, because nothing is
    /// translated: a Linux name carrying a backslash is a name this call could
    /// perfectly well write. It cannot arrive, though — the name comes off
    /// `local`, which is a file on a Windows filesystem that has no such name
    /// to give.
    ///
    /// The exit status is the whole verdict, and it is trusted. `tee` that
    /// could not open its file says so and exits non-zero; one that exits zero
    /// has written every byte it was handed. Reading the file back to check
    /// would cost a second crossing to learn what the status already said.
    ///
    /// `password` is ignored, and the parameter is not dead weight for it: a
    /// distribution's root costs nothing, and the parameter is there for the
    /// backend whose root does — an SSH session's `sudo`, which reads the
    /// account's password off the command's standard input. A source that has
    /// no use for one simply has none.
    async fn copy_in_as_root(
        &self,
        local: PathBuf,
        dir: &str,
        _password: Option<&str>,
    ) -> Result<String, FileError> {
        let dir = dir.to_owned();
        let distro = self.distro.clone();
        self.blocking(move || {
            let written = join(&dir, &file_name(&local)?);
            let staged = std::fs::File::open(&local).map_err(|error| {
                FileError::Local(format!("{} could not be read: {error}", local.display()))
            })?;

            let output = Command::new("wsl.exe")
                .args(["-d", &distro, "-u", "root", "--exec", "tee", &written])
                .stdin(Stdio::from(staged))
                // `tee` echoes everything it writes, and there is nobody on
                // this end to echo a file to.
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .map_err(|error| {
                    FileError::Local(format!("could not run wsl.exe for {distro}: {error}"))
                })?;

            if !output.status.success() {
                // Either side of `wsl.exe` may be the one refusing — it, in
                // UTF-16, or `tee` inside the distribution, in UTF-8 — so the
                // bytes are decoded by [`crate::wsl::decode_output`], which is
                // where that guess lives. A refusal with nothing to say falls
                // back to the status, which is the only other thing known.
                let reason = decode_output(&output.stderr);
                let reason = reason.trim();
                return Err(FileError::Backend(if reason.is_empty() {
                    format!(
                        "{distro} would not write {written} as root ({})",
                        output.status
                    )
                } else {
                    format!("{distro} would not write {written} as root: {reason}")
                }));
            }
            Ok(written)
        })
        .await
    }

    /// Always `true`, and the wording it picks is the accurate one: a WSL
    /// distribution runs on this computer, its share is served by a process on
    /// this computer, and moving a file across it copies bytes between two
    /// filesystems this machine already has mounted. Answering `false` would
    /// have the panel offer to "upload" a file to a place with no network
    /// between it and here.
    fn is_local(&self) -> bool {
        true
    }
}

/// The name `local` will be given on the distribution's side.
///
/// Shared by the two calls that copy a file *in*, so that both spell their
/// destination the same way and refuse the same paths: one with no final
/// component names a directory rather than a file, and one whose name is not
/// valid UTF-8 cannot be joined onto a Linux directory at all — the panel's
/// paths are [`String`]s.
fn file_name(local: &Path) -> Result<String, FileError> {
    local
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| FileError::Path(format!("{} has no file name to copy", local.display())))
}

/// Appends `name` to the Linux directory `dir`.
///
/// A separator is added only when there is not one already, which matters for
/// exactly one directory and matters absolutely there: `/` already ends in its
/// separator, and `//etc` is not a path POSIX promises means `/etc`.
fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The Linux path `path`, spelled as a path into this machine's WSL share.
///
/// The one place a path leaves the panel's world, and the only place
/// [`UNC_SERVER`] is consulted — which is why every caller of this is already
/// inside a blocking closure.
fn to_windows(distro: &str, path: &str) -> Result<PathBuf, FileError> {
    unc_path(unc_server(distro), distro, path)
}

/// Which server name this machine's WSL shares answer to, probed once per run.
///
/// The probe is a `stat` of the distribution's root: on a build that serves
/// `wsl.localhost` it succeeds, and on an older one it fails with "the network
/// path was not found", which is the whole question being asked. A distribution
/// that is merely *broken* also fails it, and then the fallback is chosen and
/// every call fails on its own merits under the old name — no worse an outcome
/// than failing under the new one, and not worth a second probe to distinguish.
fn unc_server(distro: &str) -> &'static str {
    UNC_SERVER.get_or_init(|| {
        if std::fs::metadata(format!(r"\\{SERVER}\{distro}")).is_ok() {
            SERVER
        } else {
            log::debug!(r"\\{SERVER}\{distro} could not be reached; using \\{LEGACY_SERVER}");
            LEGACY_SERVER
        }
    })
}

/// Builds `\\server\distro\...` out of an absolute Linux path.
///
/// Separated from [`to_windows`] so that the translation can be tested without
/// a machine that has WSL on it — the server name is the only part of it that
/// has to be discovered.
///
/// Two paths are refused rather than translated:
///
/// * **a relative one**, because there is nothing to resolve it against: this
///   process's working directory is a Windows one, and pasting the two together
///   would name a place in neither tree.
/// * **one containing a backslash**, because Windows would read it as a
///   separator. A Linux file may legitimately be called `a\b`, and no path
///   through the share can name it — the same wall [`FileSource::read_dir`]
///   drops such a name at, hit from the other side.
fn unc_path(server: &str, distro: &str, path: &str) -> Result<PathBuf, FileError> {
    if !path.starts_with('/') {
        return Err(FileError::Path(format!(
            "{path} is not an absolute path in {distro}"
        )));
    }
    if path.contains('\\') {
        return Err(FileError::Path(format!(
            "{path} contains a backslash, which no path into {distro} can carry"
        )));
    }
    Ok(PathBuf::from(format!(
        r"\\{server}\{distro}{}",
        path.replace('/', r"\")
    )))
}

/// The Linux path `path` names inside `distro`, or `None` if it names nothing
/// there.
///
/// The inverse of [`unc_path`], and it has more shapes to undo than that one
/// produces, because what comes back through here is whatever
/// [`canonicalize`](std::fs::canonicalize) answered:
///
/// * `\\?\UNC\wsl.localhost\Ubuntu\home\ada` — the verbatim form of a share,
///   which is what canonicalising a UNC path actually returns;
/// * `\\wsl.localhost\Ubuntu\home\ada` — the plain form, which is what went in;
/// * `\\?\C:\Users\ada` — a *drive*, which is what canonicalising a path that
///   walked out through `/mnt/c` or a symbolic link can return, and which is
///   not in the distribution's tree at all.
///
/// `None` for that last one and for a path under another distribution, so a
/// caller reports a failure rather than handing the panel a Linux path that
/// resolves somewhere else entirely. Server and distribution are compared
/// without regard to ASCII case because Windows resolves them that way and will
/// hand back whichever spelling it prefers; the path below them is not, because
/// Linux does not.
fn to_linux(distro: &str, path: &Path) -> Option<String> {
    // Forward slashes are separators to Windows and may survive a round trip,
    // so both spellings are folded to one before anything is stripped.
    let text = path.to_str()?.replace('/', r"\");

    let rest = match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => rest,
        // A verbatim path that is not a share is a drive on this machine.
        None if text.starts_with(r"\\?\") => return None,
        None => text.strip_prefix(r"\\")?,
    };

    let (server, rest) = split_head(rest);
    if !KNOWN_SERVERS
        .iter()
        .any(|known| server.eq_ignore_ascii_case(known))
    {
        return None;
    }
    let (share, rest) = split_head(rest);
    if !share.eq_ignore_ascii_case(distro) {
        return None;
    }

    // The share root is the distribution's root, and it is reached with the
    // trailing separator still on it as often as not.
    let inside = rest.replace('\\', "/");
    let inside = inside.trim_end_matches('/');
    Some(if inside.is_empty() {
        "/".to_owned()
    } else {
        format!("/{inside}")
    })
}

/// Splits `path` at its first separator, with everything after it — possibly
/// nothing — as the remainder.
fn split_head(path: &str) -> (&str, &str) {
    match path.split_once('\\') {
        Some((head, rest)) => (head, rest),
        None => (path, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The distribution the integration tests below run against.
    ///
    /// One name rather than a discovered list: these tests are about whether
    /// the translation holds against a real 9P share, and one share proves that
    /// as well as five do.
    const TEST_DISTRO: &str = "Ubuntu";

    /// A source over [`TEST_DISTRO`], for the ignored tests below.
    fn source(executor: BackgroundExecutor) -> WslSource {
        WslSource::new(executor, TEST_DISTRO.to_owned())
    }

    /// The entry named `name`, or a failure naming what the listing did hold.
    fn find<'a>(entries: &'a [FileEntry], name: &str) -> &'a FileEntry {
        entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| {
                let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
                panic!("{name} is not in the listing: {names:?}")
            })
    }

    /// A directory under `/var/tmp` no other run of these tests shares, so that
    /// a leftover from a crashed run cannot make the next one pass or fail.
    ///
    /// `/var/tmp` rather than `/tmp`, and it is the one test that reaches the
    /// distribution *both* ways — through the share and through `wsl.exe` — that
    /// forces the choice. A distribution that has to be started serves its share
    /// before its init has finished, and an init that finishes by mounting a
    /// tmpfs over `/tmp` leaves a directory created in that window addressable
    /// from Windows and gone from inside: the elevated write then fails for a
    /// reason that has nothing to do with what is being tested, and only on the
    /// first run after a boot. `/var/tmp` is on the distribution's own disk and
    /// nothing is mounted over it, which is also why the removals at the end of
    /// each test matter more than they did.
    fn scratch(seed: u32) -> String {
        format!("/var/tmp/rulogman-wsl-test-{}-{seed}", std::process::id())
    }

    #[test]
    fn a_linux_path_becomes_a_share_path() {
        assert_eq!(
            unc_path(SERVER, "Ubuntu", "/home/ada").expect("an absolute path must translate"),
            PathBuf::from(r"\\wsl.localhost\Ubuntu\home\ada")
        );
        // The root is the share root, and keeps the separator that says so.
        assert_eq!(
            unc_path(SERVER, "Ubuntu", "/").expect("the root must translate"),
            PathBuf::from(r"\\wsl.localhost\Ubuntu\")
        );
        assert_eq!(
            unc_path(LEGACY_SERVER, "Ubuntu", "/etc").expect("the old name must translate too"),
            PathBuf::from(r"\\wsl$\Ubuntu\etc")
        );
    }

    #[test]
    fn a_path_windows_cannot_spell_is_refused_rather_than_mangled() {
        // Relative: there is nothing on this side to resolve it against.
        assert!(matches!(
            unc_path(SERVER, "Ubuntu", "home/ada"),
            Err(FileError::Path(_))
        ));
        // A backslash in a Linux name is a separator to Windows, so this would
        // silently name `/home/ada/notes` instead of a file called `ada\notes`.
        assert!(matches!(
            unc_path(SERVER, "Ubuntu", r"/home/ada\notes"),
            Err(FileError::Path(_))
        ));
    }

    #[test]
    fn a_share_path_becomes_a_linux_path_in_every_shape_it_comes_back_in() {
        // What `canonicalize` answers with.
        assert_eq!(
            to_linux(
                "Ubuntu",
                Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\home\ada")
            ),
            Some("/home/ada".to_owned())
        );
        // What went in.
        assert_eq!(
            to_linux("Ubuntu", Path::new(r"\\wsl.localhost\Ubuntu\home\ada")),
            Some("/home/ada".to_owned())
        );
        // The old server name, and forward slashes, both of which Windows
        // accepts and so may hand back.
        assert_eq!(
            to_linux("Ubuntu", Path::new(r"\\wsl$\Ubuntu\etc")),
            Some("/etc".to_owned())
        );
        assert_eq!(
            to_linux("Ubuntu", Path::new("//wsl.localhost/Ubuntu/etc")),
            Some("/etc".to_owned())
        );
    }

    #[test]
    fn the_share_root_is_the_distributions_root() {
        assert_eq!(
            to_linux("Ubuntu", Path::new(r"\\wsl.localhost\Ubuntu\")),
            Some("/".to_owned())
        );
        assert_eq!(
            to_linux("Ubuntu", Path::new(r"\\wsl.localhost\Ubuntu")),
            Some("/".to_owned())
        );
        assert_eq!(
            to_linux("Ubuntu", Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\")),
            Some("/".to_owned())
        );
    }

    #[test]
    fn a_path_outside_the_distribution_translates_to_nothing() {
        // A drive: what a link out through `/mnt/c` canonicalises to.
        assert_eq!(to_linux("Ubuntu", Path::new(r"\\?\C:\Users\ada")), None);
        assert_eq!(to_linux("Ubuntu", Path::new(r"C:\Users\ada")), None);
        // Another distribution's share, and another server's entirely.
        assert_eq!(
            to_linux("Ubuntu", Path::new(r"\\wsl.localhost\Debian\etc")),
            None
        );
        assert_eq!(to_linux("Ubuntu", Path::new(r"\\build\share\etc")), None);
    }

    #[test]
    fn the_server_and_the_distribution_are_matched_without_regard_to_case() {
        // Windows resolves both case-insensitively and hands back whichever
        // spelling it feels like, so refusing one would be refusing our own
        // path back.
        assert_eq!(
            to_linux("Ubuntu", Path::new(r"\\WSL.LOCALHOST\ubuntu\etc")),
            Some("/etc".to_owned())
        );
        // The path below the share is Linux's, and Linux is case-sensitive.
        assert_eq!(
            to_linux("Ubuntu", Path::new(r"\\wsl.localhost\Ubuntu\ETC")),
            Some("/ETC".to_owned())
        );
    }

    #[test]
    fn a_name_is_joined_onto_a_directory_without_doubling_the_separator() {
        assert_eq!(join("/home/ada", "notes.txt"), "/home/ada/notes.txt");
        assert_eq!(join("/", "etc"), "/etc");
    }

    /// The two navigation calls the panel opens a WSL tab with.
    ///
    /// `home` is the one this feature exists for: before it, a WSL tab had no
    /// panel at all, and the obvious wrong implementation of it — fall back to
    /// `/` — opens the panel on `/proc` and `/sys` instead of on the user's
    /// files.
    #[gpui::test]
    #[ignore = "needs a real WSL distribution on this machine"]
    async fn navigation_starts_in_the_distributions_home(executor: BackgroundExecutor) {
        let source = source(executor);

        let home = source.home().await.expect("the distribution must answer");
        assert!(
            home.starts_with('/'),
            "{home} is not an absolute Linux path"
        );
        assert!(
            !home.contains('\\'),
            "{home} carries a Windows separator into the panel"
        );

        // `<current>/..` is the only way the panel goes up.
        let up = source
            .realpath(&format!("{home}/.."))
            .await
            .expect("the parent must resolve");
        assert!(
            home.starts_with(&format!("{up}/")) || up == "/",
            "{home} is not below {up}"
        );

        // And the root resolves to itself rather than out of the share.
        assert_eq!(
            source.realpath("/").await.expect("the root must resolve"),
            "/"
        );
    }

    /// The root of a distribution, listed: what the panel draws on the first
    /// frame of a tab whose shell reported `/`.
    #[gpui::test]
    #[ignore = "needs a real WSL distribution on this machine"]
    async fn the_root_lists_the_directories_every_distribution_has(executor: BackgroundExecutor) {
        let entries = source(executor)
            .read_dir("/")
            .await
            .expect("the root must list");
        assert!(find(&entries, "etc").is_dir);
        assert!(find(&entries, "home").is_dir);
        assert!(find(&entries, "usr").is_dir);
    }

    /// The four calls the panel's own commands are made of, run against the
    /// real share — which is the only place the translation can be shown to
    /// survive a directory that is created, entered, renamed and removed.
    #[gpui::test]
    #[ignore = "needs a real WSL distribution on this machine"]
    async fn directories_are_created_renamed_and_removed_through_the_share(
        executor: BackgroundExecutor,
    ) {
        let source = source(executor);
        let root = scratch(1);
        let renamed = format!("{root}-renamed");

        source.mkdir(&root).await.expect("the directory is created");
        source
            .mkdir(&root)
            .await
            .expect("an existing directory is not an error");

        let inside = join(&root, "logs");
        source.mkdir(&inside).await.expect("the child is created");
        assert!(
            source.remove_dir(&root).await.is_err(),
            "a directory with a child in it must not be removed"
        );

        source
            .rename(&root, &renamed)
            .await
            .expect("the rename must succeed");
        let entries = source
            .read_dir(&renamed)
            .await
            .expect("the renamed directory must list");
        assert!(find(&entries, "logs").is_dir);

        source
            .remove_dir(&join(&renamed, "logs"))
            .await
            .expect("the child is removed");
        source
            .remove_dir(&renamed)
            .await
            .expect("the emptied directory is removed");
        assert!(source.read_dir(&renamed).await.is_err());
    }

    /// A file across the share in both directions, long enough to take several
    /// chunks: the bytes have to arrive intact and the status line has to be
    /// told about them on the way.
    #[gpui::test]
    #[ignore = "needs a real WSL distribution on this machine"]
    async fn a_file_crosses_the_share_and_comes_back_unchanged(executor: BackgroundExecutor) {
        use futures::StreamExt;

        let source = source(executor);
        let here = tempfile::tempdir().expect("the temporary tree must be created");
        let there = scratch(2);
        source
            .mkdir(&there)
            .await
            .expect("the directory is created");

        let size = 3 * 64 * 1024 + 7;
        let body: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
        let original = here.path().join("app.log");
        std::fs::write(&original, &body).expect("the file must be written");

        let (sender, receiver) = futures::channel::mpsc::unbounded();
        let written = source
            .copy_in(original, &there, Some(sender))
            .await
            .expect("the copy in must succeed");
        // The answer is a Linux path, because that is what the panel navigates
        // by — a UNC one here would be a path no later call accepts.
        assert_eq!(written, join(&there, "app.log"));
        let counts: Vec<u64> = receiver.collect().await;
        assert_eq!(counts.last().copied(), Some(size as u64), "saw {counts:?}");

        let entries = source.read_dir(&there).await.expect("the listing succeeds");
        let entry = find(&entries, "app.log");
        assert!(!entry.is_dir);
        assert_eq!(entry.size, size as u64);

        let back = here.path().join("returned.log");
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        source
            .copy_out(&written, back.clone(), Some(sender))
            .await
            .expect("the copy out must succeed");
        assert_eq!(
            std::fs::read(&back).expect("the copy must be readable"),
            body
        );
        let counts: Vec<u64> = receiver.collect().await;
        assert_eq!(counts.last().copied(), Some(size as u64), "saw {counts:?}");

        source
            .remove_file(&written)
            .await
            .expect("the file is removed");
        source
            .remove_dir(&there)
            .await
            .expect("the directory is removed");
    }

    /// What `stat` inside the distribution says about `path`, in `format`.
    ///
    /// Asked as root, so that a file in a directory the tests made is readable
    /// whoever ends up owning it, and run through `--exec` so the answer is the
    /// binary's own UTF-8 rather than `wsl.exe`'s UTF-16.
    fn stat(path: &str, format: &str) -> String {
        let output = Command::new("wsl.exe")
            .args([
                "-d",
                TEST_DISTRO,
                "-u",
                "root",
                "--exec",
                "stat",
                "-c",
                format,
                path,
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("wsl.exe must run");
        assert!(
            output.status.success(),
            "stat {format} {path} failed: {}",
            decode_output(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// The write the share cannot make, made through the distribution instead.
    ///
    /// Two files, because the two halves of the claim are different files. A
    /// name that is not there yet has to appear with the right bytes in it; a
    /// name that *is* there has to keep its identity across the write, which is
    /// the whole reason `tee` was chosen — an elevated save that quietly moved
    /// a config file into root's name would be a worse bug than the one this
    /// feature fixes, and it is invisible until something else fails to write
    /// the file later.
    #[gpui::test]
    #[ignore = "needs a real WSL distribution on this machine"]
    async fn a_file_written_as_root_arrives_without_changing_hands(executor: BackgroundExecutor) {
        let source = source(executor);
        let here = tempfile::tempdir().expect("the temporary tree must be created");
        let there = scratch(3);
        source
            .mkdir(&there)
            .await
            .expect("the directory is created");

        // A file only root writes: nothing of that name exists on the other
        // side until this call puts it there.
        let fresh = here.path().join("fresh.conf");
        std::fs::write(&fresh, b"written by root\n").expect("the staging file must be written");
        let created = source
            // No password, and none wanted: a distribution's root is free.
            .copy_in_as_root(fresh, &there, None)
            .await
            .expect("the elevated write must succeed");
        assert_eq!(created, join(&there, "fresh.conf"));
        assert_eq!(
            std::fs::read(to_windows(TEST_DISTRO, &created).expect("the path must translate"))
                .expect("the new file must be readable"),
            b"written by root\n"
        );
        assert_eq!(stat(&created, "%U"), "root");

        // And a file that already belongs to somebody. It goes in through the
        // share first, so it is the default user's, and the elevated write over
        // it must leave it the default user's.
        let staged = here.path().join("owned.conf");
        std::fs::write(&staged, b"first\n").expect("the staging file must be written");
        let owned = source
            .copy_in(staged, &there, None)
            .await
            .expect("the ordinary copy must succeed");
        let (owner, mode) = (stat(&owned, "%U"), stat(&owned, "%a"));

        let again = here.path().join("owned.conf");
        std::fs::write(&again, b"second, as root\n").expect("the staging file must be written");
        assert_eq!(
            source
                .copy_in_as_root(again, &there, None)
                .await
                .expect("the elevated overwrite must succeed"),
            owned
        );
        assert_eq!(
            std::fs::read(to_windows(TEST_DISTRO, &owned).expect("the path must translate"))
                .expect("the overwritten file must be readable"),
            b"second, as root\n"
        );
        assert_eq!(stat(&owned, "%U"), owner, "the write took the file over");
        assert_eq!(
            stat(&owned, "%a"),
            mode,
            "the write changed the file's mode"
        );

        source
            .remove_file(&created)
            .await
            .expect("the new file is removed");
        source
            .remove_file(&owned)
            .await
            .expect("the overwritten file is removed");
        source
            .remove_dir(&there)
            .await
            .expect("the directory is removed");
    }

    /// The flag the wording hangs off: a distribution runs on this machine, so
    /// the panel says "Copy" rather than offering to upload across a network
    /// that is not there.
    #[gpui::test]
    async fn a_wsl_source_says_it_is_local(executor: BackgroundExecutor) {
        assert!(source(executor).is_local());
    }
}
