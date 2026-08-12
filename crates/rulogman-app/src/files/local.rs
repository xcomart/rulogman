//! The filesystem on the computer rulogman itself is running on.
//!
//! One [`FileSource`] implementation, for the session that runs a shell here
//! rather than reaching one over SSH. Everything the panel above does — listing,
//! renaming, deleting, copying in both directions — is `std::fs` on the other
//! side of this module, and the whole of it runs on a background executor:
//! [`FileSource`]'s futures are `?Send` and so are polled on the UI thread, and
//! a `std::fs` call left in one of them would hold up a repaint for as long as
//! the disk took.
//!
//! Written to answer exactly as [`SftpSource`](super::SftpSource) does, right
//! down to the cases where the two could reasonably disagree: a broken symbolic
//! link listed as a non-directory, an existing directory accepted by `mkdir`, a
//! non-empty one refused by `remove_dir`. The panel treats the two identically,
//! so a difference here would surface up there as a bug rather than as a choice.
//!
//! Nothing below is written twice for two platforms: every call is `std::fs`,
//! which is as cross-platform as this crate needs. The one place the platform
//! shows through is how a path is *spelled* on the way out — see
//! [`path_string`] — because the panel above does path arithmetic on strings
//! and expects one spelling whichever source produced them.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use futures::channel::mpsc::UnboundedSender;
use gpui::BackgroundExecutor;

use super::{FileEntry, FileError, FileSource};

/// How many bytes one read-and-write of a local copy moves.
///
/// The same 64 KiB the SFTP layer streams in, and for a related reason: it is
/// large enough that the per-call overhead disappears, and small enough that
/// the progress line moves often enough to look alive on a slow disk.
const COPY_CHUNK: usize = 64 * 1024;

/// A source over the filesystem rulogman itself is running on.
///
/// What a session running a shell on this machine browses. See the module
/// documentation for the two rules the whole type is written to: it matches the
/// SFTP source call for call, and it never blocks the UI thread.
pub struct LocalSource {
    /// Where the blocking `std::fs` calls are sent.
    ///
    /// Cloned from the application's, so this shares gpui's thread pool rather
    /// than standing one up per session.
    executor: BackgroundExecutor,
}

impl LocalSource {
    /// A source running its filesystem work on `executor`.
    pub fn new(executor: BackgroundExecutor) -> Self {
        Self { executor }
    }

    /// Runs one blocking piece of filesystem work off the UI thread.
    ///
    /// Every method below goes through this, which is what makes "nothing
    /// blocks a repaint" checkable by reading rather than by remembering. The
    /// closure owns everything it touches — the progress sender included — so
    /// whatever it holds is dropped when it returns, before the caller's
    /// `await` hands the answer back.
    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T, FileError> + Send + 'static,
    ) -> Result<T, FileError> {
        self.executor.spawn(async move { work() }).await
    }
}

#[async_trait::async_trait(?Send)]
impl FileSource for LocalSource {
    /// The user's home directory.
    ///
    /// The local counterpart of the login directory an SFTP server reports:
    /// where a shell started with no directory of its own would open.
    async fn home(&self) -> Result<String, FileError> {
        self.blocking(|| {
            let dirs = directories::UserDirs::new()
                .ok_or_else(|| FileError::Path("this account has no home directory".to_owned()))?;
            path_string(dirs.home_dir())
        })
        .await
    }

    async fn realpath(&self, path: &str) -> Result<String, FileError> {
        let path = path.to_owned();
        self.blocking(move || {
            let resolved = std::fs::canonicalize(&path)
                .map_err(|error| FileError::Local(format!("could not resolve {path}: {error}")))?;
            path_string(&resolved)
        })
        .await
    }

    /// Lists `path`, resolving symbolic links the way the SFTP shim does.
    ///
    /// Two failures are deliberately survivable, because failing the listing
    /// over either would leave the user staring at an error instead of at the
    /// other nine hundred files in the directory:
    ///
    /// * **a link whose target cannot be stat'ed** keeps the link's own type and
    ///   size, and so is listed as a non-directory — the documented meaning of
    ///   [`FileEntry::is_dir`](rulogman_ssh::RemoteEntry::is_dir);
    /// * **an entry that cannot be read at all** — removed between the `readdir`
    ///   and the `stat` — is dropped with a line in the log.
    ///
    /// A name that is not valid UTF-8 is dropped for a different and firmer
    /// reason: the panel does its path arithmetic on [`String`]s, so a lossy
    /// name would come back as a *different* path and aim the next rename or
    /// delete at whatever that path happens to name.
    async fn read_dir(&self, path: &str) -> Result<Vec<FileEntry>, FileError> {
        let path = path.to_owned();
        self.blocking(move || {
            let listing = std::fs::read_dir(&path)
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
                    log::debug!(
                        "skipping {}: its name is not valid UTF-8",
                        entry.path().display()
                    );
                    continue;
                };

                let child = entry.path();
                // The link itself, so that this is the real "is it a
                // directory?" test rather than the target's answer.
                let link = match std::fs::symlink_metadata(&child) {
                    Ok(link) => link,
                    Err(error) => {
                        log::debug!("skipping {}: {error}", child.display());
                        continue;
                    }
                };
                let is_symlink = link.is_symlink();
                let (is_dir, size) = if is_symlink {
                    match std::fs::metadata(&child) {
                        Ok(target) => (target.is_dir(), target.len()),
                        Err(error) => {
                            log::debug!(
                                "could not resolve the symlink {}: {error}",
                                child.display()
                            );
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

    /// Every drive letter Windows currently has mounted, as `C:/`, `D:/`, ….
    ///
    /// Windows is the one platform where the default single `/` would be a lie:
    /// each drive is a tree of its own, and a panel that started on `C:` could
    /// never reach `D:` by walking upwards. Overridden only here — the unix
    /// build keeps the default, because there the answer really is one root.
    ///
    /// Answered from the kernel's bitmask alone, with no [`std::fs::metadata`]
    /// call per letter. That is deliberate rather than lazy: touching a floppy
    /// or optical drive spins the hardware up, which takes seconds per empty
    /// bay, and the panel is asking this to fill a dropdown that opens under
    /// the pointer. Whether a listed drive actually has media in it is settled
    /// by the listing that follows the press, which reports its own failure.
    ///
    /// A mask of zero — how [`GetLogicalDrives`] reports its own failure —
    /// comes back as an empty list rather than an error. The panel reads "fewer
    /// than two roots" as "nothing to choose between" and falls back to the
    /// dropdown it has always shown, which is a better answer to a rare API
    /// failure than a notice about drive letters over a header the user pressed
    /// to navigate.
    #[cfg(windows)]
    async fn roots(&self) -> Result<Vec<String>, FileError> {
        self.blocking(|| {
            // SAFETY: the call takes no arguments and writes through no
            // pointer; it only reads the mask of drive letters in use.
            let mask = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
            Ok(drive_roots(mask))
        })
        .await
    }

    /// Creates `path`, treating "it is already a directory" as success.
    ///
    /// Non-recursive, like the SFTP call and unlike
    /// [`create_dir_all`](std::fs::create_dir_all): a caller reproducing a tree
    /// hands over the parents first, and quietly creating a missing one would
    /// hide a plan that had them in the wrong order. The existing-is-fine rule
    /// is checked rather than assumed from the error kind, because a name taken
    /// by a *file* raises the same [`AlreadyExists`](std::io::ErrorKind) and is
    /// a real collision.
    async fn mkdir(&self, path: &str) -> Result<(), FileError> {
        let path = path.to_owned();
        self.blocking(move || {
            let outcome = std::fs::create_dir(&path);
            let Err(error) = outcome else {
                return Ok(());
            };
            if std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                return Ok(());
            }
            Err(FileError::Local(format!(
                "could not create the directory {path}: {error}"
            )))
        })
        .await
    }

    /// Deletes the file — or the link — at `path`.
    ///
    /// [`std::fs::remove_file`] never follows a symbolic link, which is exactly
    /// the guarantee the panel needs: it calls this for a link to a directory
    /// too, and following one would empty the target and leave the link behind.
    async fn remove_file(&self, path: &str) -> Result<(), FileError> {
        let path = path.to_owned();
        self.blocking(move || {
            std::fs::remove_file(&path)
                .map_err(|error| FileError::Local(format!("could not delete {path}: {error}")))
        })
        .await
    }

    /// Deletes the empty directory at `path`.
    ///
    /// Non-recursive on purpose, and [`std::fs::remove_dir`] is what enforces
    /// it: the panel walks a tree itself and removes the children first, which
    /// is the only way its progress line can say how much is left — and the only
    /// way a delete that fails half way through has removed exactly what it
    /// reported removing.
    async fn remove_dir(&self, path: &str) -> Result<(), FileError> {
        let path = path.to_owned();
        self.blocking(move || {
            std::fs::remove_dir(&path).map_err(|error| {
                FileError::Local(format!("could not delete the directory {path}: {error}"))
            })
        })
        .await
    }

    async fn rename(&self, old: &str, new: &str) -> Result<(), FileError> {
        let old = old.to_owned();
        let new = new.to_owned();
        self.blocking(move || {
            std::fs::rename(&old, &new)
                .map_err(|error| FileError::Local(format!("could not rename {old}: {error}")))
        })
        .await
    }

    /// Copies `local` into the directory `dir`, keeping its file name.
    ///
    /// Both ends are this computer, so this is a plain copy — see [`copy_file`]
    /// for what it refuses and why. The destination is spelled with
    /// [`Path::join`] rather than by pasting a `/` in: it is a path on *this*
    /// filesystem and has to be usable as one, whatever separator the platform
    /// prefers. The path handed *back* is not that spelling but the panel's,
    /// because it goes through [`path_string`] like every other answer here.
    async fn copy_in(
        &self,
        local: PathBuf,
        dir: &str,
        progress: Option<UnboundedSender<u64>>,
    ) -> Result<String, FileError> {
        let dir = dir.to_owned();
        self.blocking(move || {
            let name = local.file_name().ok_or_else(|| {
                FileError::Path(format!("{} has no file name to copy", local.display()))
            })?;
            let target = Path::new(&dir).join(name);
            let written = path_string(&target)?;
            copy_file(&local, &target, progress.as_ref())?;
            Ok(written)
        })
        .await
    }

    /// Copies the file at `path` to `local`.
    ///
    /// The other direction of [`LocalSource::copy_in`], and the same copy: the
    /// panel offers it as "save this somewhere", and where it saves to happens
    /// to be the same filesystem it read from.
    async fn copy_out(
        &self,
        path: &str,
        local: PathBuf,
        progress: Option<UnboundedSender<u64>>,
    ) -> Result<(), FileError> {
        let path = path.to_owned();
        self.blocking(move || copy_file(Path::new(&path), &local, progress.as_ref()))
            .await
    }

    /// Asks the filesystem itself, by opening the file the way a save would.
    ///
    /// See [`can_write`] for what is asked and what each answer is taken to
    /// mean. The `Result` the executor hands back cannot be an error — the
    /// closure returns none — but folding it to `true` rather than unwrapping
    /// keeps the fail-open rule true of this method whatever happens to the
    /// thread the work ran on.
    async fn writable(&self, path: &str) -> bool {
        let path = path.to_owned();
        self.blocking(move || Ok(can_write(Path::new(&path))))
            .await
            .unwrap_or(true)
    }

    /// Always `true`: this is the filesystem the window is drawn on.
    fn is_local(&self) -> bool {
        true
    }
}

/// Whether this account may write the file at `path`, asked by opening it.
///
/// `write(true)` and nothing else: no `create`, so a path that names nothing is
/// not brought into existence by the question, and no `truncate`, so the file
/// the editor is about to show is still the file it was. The handle is closed
/// by the drop at the end of the match, which is the whole of the cleanup —
/// nothing was written, so there is nothing to flush.
///
/// [`PermissionDenied`](std::io::ErrorKind::PermissionDenied) is the only error
/// treated as a "no", because it is the only one that is a verdict about the
/// *account* rather than about the moment. A file that is not there, one
/// another process holds exclusively, one on a volume that has just gone away —
/// all of those may still be saved to seconds later, or may fail with a sentence
/// of their own, and none of them is a reason to hand the user a buffer they
/// cannot type in.
///
/// Shared with the WSL source rather than reimplemented there, for the same
/// reason [`copy_file_as`] is: a path through the `\\wsl.localhost` share is a
/// `std::fs` path on this machine, and the open it performs is this one.
pub(super) fn can_write(path: &Path) -> bool {
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::PermissionDenied,
    }
}

/// Turns [`GetLogicalDrives`]' bitmask into the roots the panel navigates by.
///
/// Bit 0 is `A:`, bit 1 is `B:`, and so on to bit 25 for `Z:`; every bit above
/// those is reserved and ignored. Each root keeps its trailing separator,
/// because `C:` alone names that drive's own current directory rather than its
/// top — the same distinction [`normalise`] preserves.
///
/// Split out from the call so the mapping can be tested against a mask this
/// machine does not happen to have.
///
/// [`GetLogicalDrives`]: windows::Win32::Storage::FileSystem::GetLogicalDrives
#[cfg(windows)]
fn drive_roots(mask: u32) -> Vec<String> {
    (0..26u32)
        .filter(|letter| mask & (1 << letter) != 0)
        .map(|letter| format!("{}:/", char::from(b'A' + letter as u8)))
        .collect()
}

/// Renders `path` as the [`String`] the panel navigates by.
///
/// The single point where a real [`Path`] becomes a path the panel holds, which
/// is why the platform's spelling is settled here and nowhere else.
///
/// A path that is not valid UTF-8 is refused rather than made lossy: the panel
/// joins, splits and compares these as strings, and a lossy spelling would name
/// a different file than the one it was read from — which is fine until a delete
/// is aimed at it.
fn path_string(path: &Path) -> Result<String, FileError> {
    path.to_str().map(normalise).ok_or_else(|| {
        FileError::Path(format!("{} is not a name that can be used", path.display()))
    })
}

/// Leaves `path` exactly as the filesystem spelled it.
///
/// POSIX paths are already the shape the panel expects, so there is nothing to
/// do — the counterpart below explains what "the shape the panel expects" is
/// and why the other platform has to be brought to it.
#[cfg(unix)]
fn normalise(path: &str) -> String {
    path.to_owned()
}

/// Rewrites a Windows path into the one spelling the panel understands.
///
/// Two changes, both of them only ever *out* of `std::fs` and never back into
/// it:
///
/// * **the verbatim prefix goes.** [`canonicalize`](std::fs::canonicalize) —
///   which is how `realpath` answers, and so how every `..` the panel walks is
///   resolved — returns `\\?\C:\Users\ada`. That prefix is an instruction to the
///   kernel to skip path parsing, not part of the name: showing it would put
///   four characters of noise at the head of every breadcrumb row, and the
///   `\\?\UNC\server\share` form of it would hide the fact that the path is a
///   network share at all. The UNC form therefore becomes `\\server\share`
///   rather than losing its leading separators, because those *are* part of the
///   name.
/// * **separators become `/`.** The panel splits paths on `/`, joins with `/`
///   and folds breadcrumbs on `/`, because SFTP paths are POSIX on the wire and
///   that arithmetic was written for them. Bringing local paths to the same
///   spelling is what lets one panel drive both without branching, and it costs
///   nothing on the way back: every Windows API — and so every `std::fs` call
///   below — accepts `/` as a separator, so the strings the panel hands back are
///   usable as paths exactly as they are.
#[cfg(windows)]
fn normalise(path: &str) -> String {
    // `\\?\UNC\server\share` and `\\server\share` name the same place, so the
    // marker is swapped for the two separators rather than simply dropped.
    let plain = match path.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(share) => format!(r"\\{share}"),
            None => rest.to_owned(),
        },
        None => path.to_owned(),
    };
    plain.replace('\\', "/")
}

/// Streams `from` onto `to`, announcing the running byte count.
///
/// Streamed rather than handed to [`std::fs::copy`] for the progress line's
/// sake: a copy call answers once, at the end, and a multi-gigabyte file would
/// sit at 0% until it finished.
///
/// Two things are refused before a single byte is written, because both would
/// destroy data rather than merely fail:
///
/// * **the same file on both ends.** The destination is created — that is,
///   truncated — before the source is read, so copying a file into the
///   directory it is already in would leave nothing but an empty file where it
///   used to be. This is not a theoretical case: "copy to…" offered on a
///   selection, answered with the directory it came from, is one click.
/// * **a directory as the source.** The contract is one regular file, as it is
///   over SFTP; a caller that wants a tree walks it and calls
///   [`FileSource::mkdir`] along the way. Opening a directory succeeds on some
///   platforms and fails on others, so this is checked rather than left to the
///   read.
///
/// An existing destination *file* is truncated and overwritten, which is what
/// the SFTP side does and what the panel promises.
///
/// Shared with the WSL source rather than reimplemented there: both ends of a
/// copy it performs are `std::fs` paths on this machine — one of them merely
/// spelled as a `\\wsl.localhost` share — so every refusal and every progress
/// message above is as true of it as it is here.
fn copy_file(
    from: &Path,
    to: &Path,
    progress: Option<&UnboundedSender<u64>>,
) -> Result<(), FileError> {
    copy_file_as(
        from,
        to,
        &from.display().to_string(),
        &to.display().to_string(),
        progress,
    )
}

/// [`copy_file`], with each end named as the panel spells it.
///
/// The paths a copy is *performed* on and the paths a failure is *reported*
/// with are the same thing here and are not for the WSL source, whose files are
/// reached through a `\\wsl.localhost` share and named by the Linux path the
/// user actually typed. Two extra arguments rather than two implementations:
/// everything a copy does, refuses and announces is identical, and only the
/// spelling in the sentence differs.
pub(super) fn copy_file_as(
    from: &Path,
    to: &Path,
    from_name: &str,
    to_name: &str,
    progress: Option<&UnboundedSender<u64>>,
) -> Result<(), FileError> {
    // Follows a link, matching the upload planner: dragging a symbolic link
    // into the panel means its target, which is what the shell would read too.
    let source = std::fs::metadata(from)
        .map_err(|error| FileError::Local(format!("could not read {from_name}: {error}")))?;
    if source.is_dir() {
        return Err(FileError::Local(format!(
            "{from_name} is a directory, not a file"
        )));
    }
    if is_same_file(from, to) {
        return Err(FileError::Local(format!(
            "{from_name} and {to_name} are the same file"
        )));
    }

    let mut reader = std::fs::File::open(from)
        .map_err(|error| FileError::Local(format!("could not open {from_name}: {error}")))?;
    let mut writer = std::fs::File::create(to)
        .map_err(|error| FileError::Local(format!("could not create {to_name}: {error}")))?;

    let mut buffer = vec![0u8; COPY_CHUNK];
    let mut moved = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| FileError::Local(format!("could not read {from_name}: {error}")))?;
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .map_err(|error| FileError::Local(format!("could not write {to_name}: {error}")))?;
        moved = moved.saturating_add(read as u64);
        // Unchecked on purpose: a receiver that has gone away means the panel
        // stopped watching, never that the copy should stop.
        if let Some(progress) = progress {
            let _ = progress.unbounded_send(moved);
        }
    }

    writer.flush().map_err(|error| {
        FileError::Local(format!("could not finish writing {to_name}: {error}"))
    })?;
    Ok(())
}

/// Whether `from` and `to` name the same file on disk.
///
/// Compared after [`canonicalize`](std::fs::canonicalize) rather than as
/// written, so that the same file reached through a symbolic link, a `..`, or
/// simply spelled differently still counts as itself. A destination that does
/// not exist yet cannot be canonicalised and cannot be the source either, so a
/// failure on either side answers "no" — the copy then proceeds and reports its
/// own failure if the path was unusable for some other reason.
fn is_same_file(from: &Path, to: &Path) -> bool {
    match (std::fs::canonicalize(from), std::fs::canonicalize(to)) {
        (Ok(from), Ok(to)) => from == to,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The entry named `name`, or a failure naming what the listing did hold —
    /// which is what a missing row usually needs explaining by.
    fn find<'a>(entries: &'a [FileEntry], name: &str) -> &'a FileEntry {
        entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| {
                let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
                panic!("{name} is not in the listing: {names:?}")
            })
    }

    /// The path `path` spelled the way the panel passes it around.
    ///
    /// The source's own renderer rather than a plain `to_str`, so that the
    /// expectations below are written in the spelling the panel actually sees —
    /// which on Windows is not the one [`Path`] produces.
    fn text(path: &Path) -> String {
        path_string(path).expect("the temporary path must be usable")
    }

    /// A file of `size` bytes with a recognisable, non-repeating body, so that a
    /// copy that dropped or reordered a chunk cannot pass by accident.
    fn write_file(path: &Path, size: usize) -> Vec<u8> {
        let body: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
        std::fs::write(path, &body).expect("the file must be written");
        body
    }

    /// The byte counts a transfer announced, in the order they arrived.
    async fn counts(receiver: futures::channel::mpsc::UnboundedReceiver<u64>) -> Vec<u64> {
        use futures::StreamExt;
        receiver.collect().await
    }

    /// The progress contract, asserted the way the status line depends on it:
    /// one running total per chunk, never going backwards, ending at the size
    /// of the file — anything else makes the bar jump or stop short.
    fn assert_progress(counts: &[u64], size: u64) {
        assert!(!counts.is_empty(), "a copy of {size} bytes said nothing");
        assert!(
            counts.windows(2).all(|pair| pair[0] < pair[1]),
            "the byte counts are not increasing: {counts:?}"
        );
        assert_eq!(counts.last().copied(), Some(size), "saw {counts:?}");
    }

    /// The two rows every listing is made of, and the two facts the panel draws
    /// them from: a directory can be entered, and a file has a size to show.
    ///
    /// Kept apart from the symbolic link test below because that one cannot run
    /// on Windows — creating a link there needs a privilege the machine running
    /// the tests may not have — and this half of the contract is the half that
    /// must hold on every platform a local session exists on.
    #[gpui::test]
    async fn a_listing_separates_directories_from_files(executor: BackgroundExecutor) {
        let root = tempfile::tempdir().expect("the temporary tree must be created");
        let source = LocalSource::new(executor);

        std::fs::create_dir(root.path().join("logs")).expect("the directory must be created");
        let body = write_file(&root.path().join("notes.txt"), 12);

        let entries = source
            .read_dir(&text(root.path()))
            .await
            .expect("the listing must succeed");
        assert_eq!(entries.len(), 2, "saw {entries:?}");

        let logs = find(&entries, "logs");
        assert!(logs.is_dir && !logs.is_symlink);

        let notes = find(&entries, "notes.txt");
        assert!(!notes.is_dir && !notes.is_symlink);
        assert_eq!(notes.size, body.len() as u64);
    }

    /// A listing has to describe four things the panel draws differently, and
    /// the two link cases are the ones worth pinning: a link to a directory is
    /// enterable, and a link to nothing at all must not be — while both keep the
    /// badge that says they are links.
    ///
    /// Unix only, and not because the code under it is: creating a symbolic
    /// link on Windows requires developer mode or an elevated process, so a
    /// test that made one would fail on the setup rather than on the behaviour
    /// it is about.
    #[cfg(unix)]
    #[gpui::test]
    async fn a_listing_separates_directories_files_and_both_kinds_of_link(
        executor: BackgroundExecutor,
    ) {
        let root = tempfile::tempdir().expect("the temporary tree must be created");
        let source = LocalSource::new(executor);

        std::fs::create_dir(root.path().join("logs")).expect("the directory must be created");
        let body = write_file(&root.path().join("notes.txt"), 12);
        std::os::unix::fs::symlink(root.path().join("logs"), root.path().join("to-logs"))
            .expect("the directory link must be created");
        std::os::unix::fs::symlink(root.path().join("notes.txt"), root.path().join("to-notes"))
            .expect("the file link must be created");
        std::os::unix::fs::symlink(root.path().join("gone"), root.path().join("dangling"))
            .expect("the broken link must be created");

        let entries = source
            .read_dir(&text(root.path()))
            .await
            .expect("the listing must succeed");
        assert_eq!(entries.len(), 5, "saw {entries:?}");

        let logs = find(&entries, "logs");
        assert!(logs.is_dir && !logs.is_symlink);

        let notes = find(&entries, "notes.txt");
        assert!(!notes.is_dir && !notes.is_symlink);
        assert_eq!(notes.size, body.len() as u64);

        // The target decides `is_dir`, so a link to a directory can be opened
        // and a link to a file cannot — and both are still marked as links.
        let to_logs = find(&entries, "to-logs");
        assert!(to_logs.is_dir && to_logs.is_symlink);
        let to_notes = find(&entries, "to-notes");
        assert!(!to_notes.is_dir && to_notes.is_symlink);
        assert_eq!(to_notes.size, body.len() as u64);

        // Nothing to stat, so the link's own type stands in — which is never a
        // directory, so a double click cannot walk into a hole.
        let dangling = find(&entries, "dangling");
        assert!(!dangling.is_dir && dangling.is_symlink);
    }

    /// The four calls the panel's own commands are made of, in the order it
    /// makes them, including the two rules the recursive copy and the recursive
    /// delete lean on: an existing directory is not a failure, and a directory
    /// with anything in it will not be removed.
    #[gpui::test]
    async fn directories_are_created_renamed_and_removed_the_way_the_panel_asks(
        executor: BackgroundExecutor,
    ) {
        let root = tempfile::tempdir().expect("the temporary tree must be created");
        let source = LocalSource::new(executor);
        let logs = text(&root.path().join("logs"));

        source
            .mkdir(&logs)
            .await
            .expect("the directory must be created");
        assert!(root.path().join("logs").is_dir());
        // The recursive copy creates every directory of a tree unconditionally,
        // so a second create of the same name has to be a no-op rather than a
        // failure that stops the batch.
        source
            .mkdir(&logs)
            .await
            .expect("an existing directory is not an error");

        write_file(&root.path().join("logs/app.log"), 4);
        assert!(
            source.remove_dir(&logs).await.is_err(),
            "a directory with a file in it must not be removed"
        );

        let renamed = text(&root.path().join("archive"));
        source
            .rename(&logs, &renamed)
            .await
            .expect("the rename must succeed");
        assert!(root.path().join("archive/app.log").is_file());

        source
            .remove_file(&text(&root.path().join("archive/app.log")))
            .await
            .expect("the file must be removed");
        source
            .remove_dir(&renamed)
            .await
            .expect("the emptied directory must be removed");
        assert!(!root.path().join("archive").exists());
    }

    /// Both directions of a copy, over a file long enough to take several
    /// chunks: the bytes have to arrive intact, and the status line has to be
    /// told about them on the way rather than only at the end.
    #[gpui::test]
    async fn a_copy_reproduces_the_file_and_reports_it_as_it_goes(executor: BackgroundExecutor) {
        let root = tempfile::tempdir().expect("the temporary tree must be created");
        let source = LocalSource::new(executor);
        let size = 3 * COPY_CHUNK + 7;
        let body = write_file(&root.path().join("app.log"), size);
        std::fs::create_dir(root.path().join("here")).expect("the directory must be created");

        let (sender, receiver) = futures::channel::mpsc::unbounded();
        let written = source
            .copy_in(
                root.path().join("app.log"),
                &text(&root.path().join("here")),
                Some(sender),
            )
            .await
            .expect("the copy in must succeed");

        assert_eq!(written, text(&root.path().join("here/app.log")));
        assert_eq!(
            std::fs::read(&written).expect("the copy must be readable"),
            body
        );
        assert_progress(&counts(receiver).await, size as u64);

        let (sender, receiver) = futures::channel::mpsc::unbounded();
        let out = root.path().join("saved.log");
        source
            .copy_out(&written, out.clone(), Some(sender))
            .await
            .expect("the copy out must succeed");
        assert_eq!(
            std::fs::read(&out).expect("the copy must be readable"),
            body
        );
        assert_progress(&counts(receiver).await, size as u64);
    }

    /// The one refusal that exists to protect data rather than to report a
    /// failure. Copying a file into the directory it is already in truncates the
    /// destination before it reads the source — and the destination *is* the
    /// source, so the file would be gone. It is a single click away, so it is
    /// refused before anything is opened.
    #[gpui::test]
    async fn a_file_copied_onto_itself_is_refused_and_left_whole(executor: BackgroundExecutor) {
        let root = tempfile::tempdir().expect("the temporary tree must be created");
        let source = LocalSource::new(executor);
        let body = write_file(&root.path().join("notes.txt"), 64);
        let here = text(root.path());

        let error = source
            .copy_in(root.path().join("notes.txt"), &here, None)
            .await
            .expect_err("copying a file into its own directory must be refused");
        assert!(matches!(error, FileError::Local(_)), "saw {error:?}");
        assert_eq!(
            std::fs::read(root.path().join("notes.txt")).expect("the file must still be there"),
            body,
            "the file was destroyed by a copy onto itself"
        );

        // The same hazard from the other side: "copy to…" answered with the
        // path the entry already has.
        let error = source
            .copy_out(
                &text(&root.path().join("notes.txt")),
                root.path().join("notes.txt"),
                None,
            )
            .await
            .expect_err("copying a file over itself must be refused");
        assert!(matches!(error, FileError::Local(_)), "saw {error:?}");
        assert_eq!(
            std::fs::read(root.path().join("notes.txt")).expect("the file must still be there"),
            body
        );
    }

    /// A directory is not a file, and the trait says so: the panel walks trees
    /// itself, so a copy pointed at one has to fail rather than half-succeed.
    #[gpui::test]
    async fn a_directory_is_not_copied_as_a_file(executor: BackgroundExecutor) {
        let root = tempfile::tempdir().expect("the temporary tree must be created");
        let source = LocalSource::new(executor);
        std::fs::create_dir(root.path().join("logs")).expect("the directory must be created");
        std::fs::create_dir(root.path().join("here")).expect("the directory must be created");

        let error = source
            .copy_in(
                root.path().join("logs"),
                &text(&root.path().join("here")),
                None,
            )
            .await
            .expect_err("a directory must not be copied as a file");
        assert!(matches!(error, FileError::Local(_)), "saw {error:?}");
        assert!(!root.path().join("here/logs").exists());
    }

    /// The two navigation calls the panel opens a session with: where to start,
    /// and how `..` is answered — which the panel never computes itself.
    #[gpui::test]
    async fn navigation_starts_at_home_and_resolves_a_parent(executor: BackgroundExecutor) {
        let root = tempfile::tempdir().expect("the temporary tree must be created");
        let source = LocalSource::new(executor);
        std::fs::create_dir_all(root.path().join("logs/old")).expect("the tree must be created");

        let home = source.home().await.expect("this account must have a home");
        assert!(Path::new(&home).is_absolute(), "{home} is not absolute");

        // `<current>/..` is the only way the panel goes up, so it has to land on
        // the parent as the filesystem itself spells it.
        let up = source
            .realpath(&format!("{}/..", text(&root.path().join("logs/old"))))
            .await
            .expect("the parent must resolve");
        let expected = std::fs::canonicalize(root.path().join("logs"))
            .expect("the parent must be canonicalisable");
        assert_eq!(up, text(&expected));

        let error = source
            .realpath(&text(&root.path().join("nowhere")))
            .await
            .expect_err("a path that is not there must not resolve");
        assert!(matches!(error, FileError::Local(_)), "saw {error:?}");
    }

    /// Turns the write permission of `path` on or off, in whichever of the two
    /// ways this platform has one: a single read-only attribute on Windows, the
    /// mode bits on unix. [`std::fs::Permissions::set_readonly`] is the one call
    /// that spells both.
    ///
    /// The restoring direction exists for the temporary directory's sake: a
    /// read-only file cannot be deleted on Windows, so a test that left one
    /// behind would leak the whole tree it was in.
    fn set_writable(path: &Path, writable: bool) {
        let mut permissions = std::fs::metadata(path)
            .expect("the file must be there to have its permissions changed")
            .permissions();
        permissions.set_readonly(!writable);
        std::fs::set_permissions(path, permissions).expect("the permissions must be settable");
    }

    /// The probe the editor opens a file with, over the three answers it has to
    /// tell apart: a file this account may write, one it may not, and a path
    /// that is not there at all.
    ///
    /// The last is the fail-open rule, and it is the one worth pinning: every
    /// ambiguous outcome has to answer "writable", because a save that turns out
    /// to be impossible says so in a sentence while a wrongly locked buffer says
    /// nothing at all.
    ///
    /// Assumes the tests are not running as a superuser, which is what the
    /// permission bits are addressed to; root writes a read-only file whatever
    /// they say, and the middle case would then be measuring the account rather
    /// than the code.
    #[gpui::test]
    async fn a_file_this_account_cannot_write_says_so_and_everything_else_fails_open(
        executor: BackgroundExecutor,
    ) {
        let root = tempfile::tempdir().expect("the temporary tree must be created");
        let source = LocalSource::new(executor);

        let plain = root.path().join("notes.txt");
        write_file(&plain, 12);
        assert!(source.writable(&text(&plain)).await);

        let locked = root.path().join("locked.txt");
        let body = write_file(&locked, 12);
        set_writable(&locked, false);
        assert!(!source.writable(&text(&locked)).await);
        // Asking must not have been a write: no truncation, no creation, no
        // change of any kind to the file the editor is about to show.
        assert_eq!(
            std::fs::read(&locked).expect("the file must still be readable"),
            body
        );
        set_writable(&locked, true);

        // Nothing there to refuse anything, so nothing has said no.
        assert!(source.writable(&text(&root.path().join("gone.txt"))).await);
    }

    /// The other half of "asking is not writing": a path that does not exist
    /// must not exist afterwards either. `create` was left off the open for
    /// exactly this, and it is the kind of flag that gets added back by someone
    /// making the probe "work" on a missing file.
    #[gpui::test]
    async fn probing_a_missing_file_does_not_create_it(executor: BackgroundExecutor) {
        let root = tempfile::tempdir().expect("the temporary tree must be created");
        let source = LocalSource::new(executor);
        let missing = root.path().join("gone.txt");

        assert!(source.writable(&text(&missing)).await);
        assert!(
            !missing.exists(),
            "the probe created the file it asked about"
        );
    }

    /// The flag the wording hangs off. Nothing branches on it but the sentences,
    /// and getting it the wrong way round would have the panel offer to upload
    /// a file to the machine it is already on.
    #[gpui::test]
    async fn a_local_source_says_it_is_local(executor: BackgroundExecutor) {
        assert!(LocalSource::new(executor).is_local());
    }

    /// The trait's default, kept in place on the platform that has one tree.
    ///
    /// Worth a test because it is what the panel reads as "there is nothing to
    /// choose between": overriding it here would silently change what pressing
    /// the root breadcrumb does on unix.
    #[cfg(unix)]
    #[gpui::test]
    async fn a_unix_source_has_the_one_posix_root(executor: BackgroundExecutor) {
        let roots = LocalSource::new(executor)
            .roots()
            .await
            .expect("the roots must be readable");
        assert_eq!(roots, ["/"]);
    }

    /// The mask, letter for letter, including the bits that are not letters.
    ///
    /// Written against a made-up mask rather than the machine's, so that the
    /// arithmetic is pinned whatever drives happen to be plugged in here.
    #[cfg(windows)]
    #[test]
    fn a_drive_mask_becomes_one_root_per_letter() {
        // Bits 2 and 3: the third and fourth letters.
        assert_eq!(drive_roots(0b1100), ["C:/", "D:/"]);
        assert_eq!(drive_roots(1), ["A:/"]);
        // Bit 25 is `Z:`, and everything above it is reserved.
        assert_eq!(drive_roots(1 << 25), ["Z:/"]);
        assert_eq!(drive_roots(1 << 26), [] as [String; 0]);
        assert_eq!(drive_roots(u32::MAX).len(), 26);
        assert_eq!(
            drive_roots(u32::MAX).last().map(String::as_str),
            Some("Z:/")
        );
        assert!(drive_roots(0).is_empty());
    }

    /// The roots this machine really reports, which is what the panel offers.
    ///
    /// Deliberately thin on specifics — which drives exist is the machine's
    /// business — but `C:` is on every Windows install, and every answer has to
    /// be a path the panel can list as it stands.
    #[cfg(windows)]
    #[gpui::test]
    async fn the_roots_of_this_machine_are_its_drive_letters(executor: BackgroundExecutor) {
        let roots = LocalSource::new(executor)
            .roots()
            .await
            .expect("the drive letters must be readable");

        assert!(roots.iter().any(|root| root == "C:/"), "saw {roots:?}");
        for root in &roots {
            assert_eq!(root.len(), 3, "{root} is not a drive root");
            let mut letters = root.chars();
            assert!(
                letters
                    .next()
                    .is_some_and(|letter| letter.is_ascii_uppercase())
            );
            assert_eq!(letters.next(), Some(':'));
            assert_eq!(letters.next(), Some('/'));
        }
    }

    /// The spelling the panel is handed, over the three shapes Windows produces:
    /// an ordinary path, the verbatim form `canonicalize` answers `realpath`
    /// with, and the verbatim form of a network share — whose leading
    /// separators are part of the name and must survive.
    #[cfg(windows)]
    #[test]
    fn a_windows_path_reaches_the_panel_with_no_prefix_and_forward_slashes() {
        assert_eq!(normalise(r"C:\Users\ada"), "C:/Users/ada");
        assert_eq!(normalise(r"\\?\C:\Users\ada"), "C:/Users/ada");
        assert_eq!(normalise(r"\\?\UNC\build\share\logs"), "//build/share/logs");
        // A drive root keeps its separator: `C:` alone names that drive's own
        // current directory, which is a different place.
        assert_eq!(normalise(r"\\?\C:\"), "C:/");
        // Already in the panel's spelling, and so left alone.
        assert_eq!(normalise("C:/Users/ada"), "C:/Users/ada");
    }
}
