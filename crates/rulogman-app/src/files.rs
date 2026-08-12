//! The filesystem the file panel browses, behind one transport neutral trait.
//!
//! The panel was written straight against [`SftpClient`]: every action reached
//! for a remote client, every failure was an [`SftpError`], every row was a
//! [`RemoteEntry`](rulogman_ssh::RemoteEntry). That is exactly right for as long
//! as "the panel" and "the remote host" mean the same thing — and it stops being
//! right the moment a session running a shell on *this* machine wants the same
//! listing, the same drag and drop, the same rename field.
//!
//! So the panel talks to a [`FileSource`] instead. The trait covers precisely
//! the operations the panel performs and not one more: there is no `stat`, no
//! `chmod`, no `symlink`, because the panel does not offer them and a wider
//! trait would be a promise every future backend has to keep.
//!
//! Two members are not operations at all, and they are here *because* of that
//! rule rather than in spite of it. [`FileSource::writable`] is the first: the
//! editor has to know before it opens a file whether saving it could ever land,
//! and there was no honest way to ask: a mode bit describes the file's owner and
//! not this account, this mount or this share, so a `stat` would have answered
//! the question wrongly rather than not at all — and answering it from
//! [`FileSource::is_local`] is forbidden outright.
//! [`FileSource::can_write_as_root`] is the question that follows a `no` from
//! it: whether this backend has a way of writing the file that the account
//! itself has not. A capability the panel needs is a method on this trait; that
//! is the whole shape of the rule, and this is what obeying it looks like —
//! including [`FileSource::copy_in_as_root`], the operation the second
//! capability leads to, which is a call of its own rather than a flag on
//! [`FileSource::copy_in`] so that a backend with no such way in inherits a
//! refusal instead of having to notice a parameter.
//!
//! Three implementations answer it, and they are shaped very differently on
//! purpose. [`SftpSource`] is a forwarding shim — every decision about how SFTP
//! behaves stays in [`rulogman_ssh`], and this module only renames things and
//! folds the error kinds. The local source in [`local`] is the real
//! implementation of its side: there is no service behind it to delegate to, so
//! it is where "what does listing a directory on this computer mean?" is
//! actually answered, and it answers it to match the SFTP shim call for call,
//! because the panel above branches on neither — including the spelling of the
//! paths it hands back, which it brings to the POSIX shape the panel does its
//! arithmetic in whatever the local platform writes. The third, in [`wsl_fs`],
//! is that same local source seen through a mirror: a WSL tab's shell lives in
//! a Linux filesystem, and Windows serves that filesystem as a share, so it
//! answers in the shell's own Linux paths and translates them to
//! `\\wsl.localhost\<distro>\…` at the edge of every call.
//!
//! Three shapes of this interface are worth explaining, because each of them is
//! a decision rather than an accident:
//!
//! * **`?Send` futures.** gpui's foreground executor drives futures on the UI
//!   thread and never moves them between threads, so [`Send`] would buy nothing
//!   here — and it would cost a local backend, which has every reason to hold
//!   thread-bound handles, the freedom to do so. The panel holds sources as
//!   `Arc<dyn FileSource>`, so the trait also has to stay object safe, which is
//!   what the boxing [`async_trait`](async_trait::async_trait) does for it.
//! * **Owned paths as plain [`str`].** A source names its own entries, and only
//!   the source knows how they are spelled: SFTP paths are POSIX on the wire
//!   whatever the server runs on, so a [`Path`](std::path::Path) would grow
//!   backslashes the moment rulogman itself ran on Windows. The one place real
//!   [`PathBuf`]s appear is where they belong — the two transfer calls, whose
//!   *other* end is always this machine.
//! * **`copy_in` / `copy_out` rather than `upload` / `download`.** Upload and
//!   download are the SFTP layer's own words and stay accurate there. Named on
//!   this trait they would be wrong for a backend that is already local: copying
//!   a file into a directory on the same disk is not an upload, and calling it
//!   one would leave every reader of the local implementation translating the
//!   name back. The neutral pair says what actually happens — a file crosses
//!   between this computer and the source — and leaves it to the implementation
//!   whether that crossing involves a network.

use std::path::PathBuf;

use futures::channel::mpsc::UnboundedSender;
use rulogman_ssh::{SftpClient, SftpError};

// Compiled everywhere rulogman is. It was unix only for as long as unix was the
// only platform with a local shell to hand it to; now that Windows starts one
// too, both have a session that browses this machine.
mod local;
// Windows-only because `\\wsl.localhost` is: it is how Windows reaches a
// distribution's filesystem, and on Linux itself a distribution is simply this
// machine, with nothing to reach across.
#[cfg(windows)]
mod wsl_fs;

pub use local::LocalSource;
#[cfg(windows)]
pub use wsl_fs::WslSource;

/// One entry of a directory listing the panel shows.
///
/// Deliberately an alias of the SFTP listing type rather than a type of its own.
/// [`RemoteEntry`](rulogman_ssh::RemoteEntry) is already flat and owned — a name,
/// two flags and a size, with no handle back into the session it came from — so
/// there is nothing in it a local backend could not fill in from
/// [`std::fs::Metadata`], and nothing a conversion pass would change. Aliasing
/// keeps the panel from copying every entry of a ten thousand file directory
/// into an identical struct on the way in, and keeps the *name* honest at the
/// call sites, which no longer care where the entry came from.
pub type FileEntry = rulogman_ssh::RemoteEntry;

/// Why a file operation could not be completed.
///
/// Transport neutral, and one variant coarser than [`SftpError`]: the panel's
/// only reader of these is [`Notice::from_error`](crate::file_panel), which
/// frames the sentence and shows it, so distinctions it cannot act on are
/// distinctions worth losing. What survives is the split a *reader* can act on —
/// the session is gone, the far side refused, this machine refused, the path was
/// unusable.
///
/// Every variant renders as a finished English sentence fragment, exactly as the
/// SFTP layer's did: the application wraps it in a localised sentence but never
/// rewrites it, so the wording has to explain itself. No variant carries
/// credentials, and none carries file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    /// The session had already ended, or ended while the request was in flight.
    ///
    /// Remote sources only — a filesystem on this machine has no session to
    /// lose — and retrying is pointless until a new one is connected.
    Disconnected,
    /// The backend refused the operation, or the exchange with it broke down.
    ///
    /// Both of SFTP's far-side failures land here: a subsystem that would not
    /// start and a server that said no are the same thing to the person reading
    /// the status line, which is that the other end did not do it.
    Backend(String),
    /// A file on *this* computer could not be opened, read, or written.
    ///
    /// Kept apart from [`FileError::Backend`] because the two point at different
    /// places to go and look: a full disk here is not a permission problem
    /// there. For a source that is itself local both ends are this machine, and
    /// the distinction collapses — which is fine, because then either answer
    /// sends the reader to the same place.
    Local(String),
    /// A path could not be used as given — a local path with no file name
    /// component, say, or one that is not valid UTF-8.
    Path(String),
}

impl std::fmt::Display for FileError {
    /// Renders the sentence fragment the status line frames.
    ///
    /// The carried strings are already complete explanations, built by whoever
    /// raised them, so all this adds is the wording for the one variant that
    /// carries nothing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => f.write_str("the SSH session is no longer connected"),
            Self::Backend(message) | Self::Local(message) | Self::Path(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for FileError {}

impl From<SftpError> for FileError {
    /// Folds an SFTP failure into the panel's vocabulary.
    ///
    /// [`SftpError::Subsystem`] and [`SftpError::Remote`] both become
    /// [`FileError::Backend`]: they differ in *which* part of the far side gave
    /// up, which their own sentence already says and which nothing in the panel
    /// branches on. The rest map across one for one.
    fn from(error: SftpError) -> Self {
        match error {
            SftpError::Disconnected => Self::Disconnected,
            SftpError::Subsystem(message) | SftpError::Remote(message) => Self::Backend(message),
            SftpError::Local(message) => Self::Local(message),
            SftpError::Path(message) => Self::Path(message),
        }
    }
}

/// A filesystem the file panel can browse.
///
/// Implementations are held as `Arc<dyn FileSource>` and handed to the futures
/// each panel action spawns, so one is expected to be cheap to clone the `Arc`
/// of and safe to keep past the action that started it. Nothing here is
/// cancellable: a caller that stops awaiting simply drops the future, and it is
/// the implementation's business whether the work behind it unwinds or runs to
/// its end.
///
/// Paths are absolute, in whatever spelling the source itself uses, and come
/// back from [`FileSource::home`] and [`FileSource::realpath`] rather than being
/// built by the caller from a separator it guessed.
#[async_trait::async_trait(?Send)]
pub trait FileSource {
    /// The absolute path of the directory a session starts in.
    ///
    /// Asked for once, when a session first appears in the panel and its shell
    /// has not reported a directory of its own.
    async fn home(&self) -> Result<String, FileError>;

    /// Canonicalises `path`, resolving `.`, `..` and symbolic links.
    ///
    /// This is how the panel walks upwards: asking for `<current>/..` yields the
    /// parent without any path arithmetic here, which would otherwise have to
    /// guess the source's own conventions.
    async fn realpath(&self, path: &str) -> Result<String, FileError>;

    /// Lists the directory at `path`, without `.` and `..`.
    ///
    /// The order is whatever the source produced; sorting belongs to the caller,
    /// because only the UI knows what order the user asked for.
    async fn read_dir(&self, path: &str) -> Result<Vec<FileEntry>, FileError>;

    /// The absolute paths every one of this source's trees starts at.
    ///
    /// Exists because a Windows filesystem is not one tree but several, one per
    /// drive letter, and nothing else on this trait can reach the others: the
    /// panel walks upwards with `<current>/..`, which stops at `C:/` and has
    /// nowhere further to go. A source that *does* have somewhere further to go
    /// says so here, and the panel's root breadcrumb becomes the way across.
    ///
    /// The default answers `/` — the single POSIX root — and is what SFTP and
    /// WSL both use, since a session on either browses one tree whatever the
    /// machine underneath it is partitioned into. That is a contract and not
    /// just a convenience: with one root there is nothing to choose between, so
    /// the panel leaves the root breadcrumb filling its dropdown with that
    /// root's subdirectories exactly as it always has, and only a source
    /// reporting two or more changes what pressing it does.
    ///
    /// The paths come back spelled as the source spells every other path, so
    /// they can be listed as they are.
    async fn roots(&self) -> Result<Vec<String>, FileError> {
        Ok(vec!["/".to_owned()])
    }

    /// Creates the directory `path`, and succeeds if it already exists.
    ///
    /// Existing-is-fine rather than an error, because a caller reproducing a
    /// tree creates every directory in it unconditionally: probing first would
    /// cost a round trip per directory to learn what the create is about to
    /// report anyway. The recursive copy depends on this.
    async fn mkdir(&self, path: &str) -> Result<(), FileError>;

    /// Deletes the file at `path`.
    ///
    /// A symbolic link is removed as itself, never followed — following one
    /// would delete the target's contents instead of the link. Directories are
    /// not handled here; pointing this at one fails.
    async fn remove_file(&self, path: &str) -> Result<(), FileError>;

    /// Deletes the directory at `path`, which must already be empty.
    ///
    /// Emptiness is a requirement rather than a convenience: there is no
    /// recursive delete on this trait, so a caller removing a tree walks it
    /// itself and removes the children first — which is also the only way its
    /// progress line can say how much is left.
    async fn remove_dir(&self, path: &str) -> Result<(), FileError>;

    /// Renames the entry `old` to `new`.
    ///
    /// Whether an existing `new` is overwritten or refused is left to the
    /// source: implementations disagree, and probing first would only widen the
    /// window in which the answer stops being true.
    async fn rename(&self, old: &str, new: &str) -> Result<(), FileError>;

    /// Copies one file from *this* computer into the source's directory `dir`,
    /// keeping its file name, and answers the path it was written to.
    ///
    /// Named for what it does rather than for how a remote source does it: this
    /// is an upload over SFTP, and a plain copy for a source that is already
    /// local. An existing file of that name is truncated and overwritten. Only
    /// regular files are handled — pointing this at a directory fails rather
    /// than recursing, which keeps the failure obvious instead of half-copying a
    /// tree. Callers that *want* a tree walk it themselves and call
    /// [`FileSource::mkdir`] along the way.
    ///
    /// `progress`, when given, receives this file's running byte count — one
    /// message per chunk, monotonically increasing, ending at the file's size.
    /// It is a hint for a status line, so a receiver that has gone away is not
    /// an error and does not stop the copy.
    async fn copy_in(
        &self,
        local: PathBuf,
        dir: &str,
        progress: Option<UnboundedSender<u64>>,
    ) -> Result<String, FileError>;

    /// Copies the source's file at `path` to `local` on *this* computer.
    ///
    /// The other direction of [`FileSource::copy_in`], and named the same way
    /// and for the same reason: a download over SFTP, a plain copy for a local
    /// source. An existing local file is truncated and overwritten, the parent
    /// directory must already exist, and `progress` behaves exactly as it does
    /// there.
    async fn copy_out(
        &self,
        path: &str,
        local: PathBuf,
        progress: Option<UnboundedSender<u64>>,
    ) -> Result<(), FileError>;

    /// Whether writing the file at `path` would be permitted right now.
    ///
    /// The probe is the first thing a save does and nothing else: open the file
    /// for writing, without creating it and without truncating it, then close
    /// it again. A file that is there is left byte for byte as it was, and one
    /// that is not there is not conjured into existence by being asked about.
    /// Nothing cheaper answers the question — a permission bit says what the
    /// *owner* may do, and the account, the mount, the share and the server all
    /// still have a veto after it.
    ///
    /// Only a definite refusal answers `false`. Every ambiguous outcome — the
    /// session went away, something else holds the file open, a server that
    /// cannot say — answers `true`, because the two mistakes are not the same
    /// size: a save that turns out to be impossible explains itself in a
    /// sentence under the editor, while a buffer wrongly locked is a file the
    /// user cannot edit and is never told why.
    ///
    /// The answer is a snapshot and not a promise. It is true of the moment it
    /// was taken, and the save that follows is what finds out whether it still
    /// is — which is why nothing here is worth a round trip to re-check.
    ///
    /// No default, deliberately: what makes a file writable is different on
    /// every backend, and an inherited `true` would be a source that had
    /// forgotten to answer wearing the face of one that had decided to.
    async fn writable(&self, path: &str) -> bool;

    /// Whether this source can write a file as root when the account cannot.
    ///
    /// Asked only once [`FileSource::writable`] has said no, and it asks about
    /// the *backend* rather than about a file: whether there is a second way in
    /// at all. A source that has one lets the editor offer the way out of a
    /// read-only pane; a source that has not leaves the pane as it opened.
    ///
    /// Not a permission check, and it could not be one. Whether the elevated
    /// write actually lands is [`FileSource::copy_in_as_root`]'s to find out —
    /// what is answered here is that the road exists, not that it is open.
    ///
    /// The default is `false`, which is the honest answer for a source holding
    /// nothing but the account's own credentials. It is also why this has a
    /// default where [`FileSource::writable`] refuses one: "there is no other
    /// account here" is a *complete* answer, and a source that never thought
    /// about the question is in exactly the same position as one that did.
    fn can_write_as_root(&self) -> bool {
        false
    }

    /// [`FileSource::copy_in`] again, performed as root.
    ///
    /// The same contract as that one — one regular file into `dir`, keeping its
    /// name, overwriting whatever is there, answering the path it was written
    /// to — with one deliberate omission: there is no `progress`. The editor's
    /// save is the only caller, its file is capped at
    /// [`MAX_EDIT_BYTES`](crate::editor_pane::MAX_EDIT_BYTES), and a progress
    /// line for a write that is over before it can be drawn would be plumbing
    /// for nobody to read.
    ///
    /// The default refuses, in a sentence rather than by panicking. Nothing
    /// reaches here except through a source whose
    /// [`FileSource::can_write_as_root`] said yes, so a caller that arrives has
    /// already gone wrong somewhere above — and the pane that started the save
    /// has a place to show the reason, which is more use than a crash.
    async fn copy_in_as_root(&self, local: PathBuf, dir: &str) -> Result<String, FileError> {
        let name = local.file_name().unwrap_or(local.as_os_str());
        Err(FileError::Backend(format!(
            "{} cannot be written into {dir} as root: there is no way to become root on this filesystem",
            name.to_string_lossy()
        )))
    }

    /// Whether this source is the computer rulogman itself is running on.
    ///
    /// Purely presentational: it is what lets a view say "Copy" where it would
    /// otherwise say "Upload", and pick a save dialog over a plain copy. Nothing
    /// about *behaviour* may branch on it — a caller that needs different work
    /// done wants a different method on this trait, not a flag.
    fn is_local(&self) -> bool;
}

/// A source riding on an SSH session's SFTP channel.
///
/// A forwarding shim and nothing more: the SFTP layer already decided how each
/// of these behaves, and repeating any of that here would be a second place for
/// it to be wrong. Cheap to construct — the wrapped client only holds a request
/// channel, and the SFTP channel itself is opened lazily on the first call.
pub struct SftpSource(SftpClient);

impl SftpSource {
    /// Wraps `client` as the file source of the session it belongs to.
    pub fn new(client: SftpClient) -> Self {
        Self(client)
    }
}

#[async_trait::async_trait(?Send)]
impl FileSource for SftpSource {
    async fn home(&self) -> Result<String, FileError> {
        self.0.home().await.map_err(FileError::from)
    }

    async fn realpath(&self, path: &str) -> Result<String, FileError> {
        self.0.realpath(path).await.map_err(FileError::from)
    }

    async fn read_dir(&self, path: &str) -> Result<Vec<FileEntry>, FileError> {
        self.0.read_dir(path).await.map_err(FileError::from)
    }

    async fn mkdir(&self, path: &str) -> Result<(), FileError> {
        self.0.mkdir(path).await.map_err(FileError::from)
    }

    async fn remove_file(&self, path: &str) -> Result<(), FileError> {
        self.0.remove_file(path).await.map_err(FileError::from)
    }

    async fn remove_dir(&self, path: &str) -> Result<(), FileError> {
        self.0.remove_dir(path).await.map_err(FileError::from)
    }

    async fn rename(&self, old: &str, new: &str) -> Result<(), FileError> {
        self.0.rename(old, new).await.map_err(FileError::from)
    }

    async fn copy_in(
        &self,
        local: PathBuf,
        dir: &str,
        progress: Option<UnboundedSender<u64>>,
    ) -> Result<String, FileError> {
        self.0
            .upload(local, dir, progress)
            .await
            .map_err(FileError::from)
    }

    async fn copy_out(
        &self,
        path: &str,
        local: PathBuf,
        progress: Option<UnboundedSender<u64>>,
    ) -> Result<(), FileError> {
        self.0
            .download(path, local, progress)
            .await
            .map_err(FileError::from)
    }

    /// Folds a failed probe into `true`, which is the only fold that keeps the
    /// trait's promise: an unreachable server has not said the file is
    /// unwritable, it has said nothing at all — and a disconnect that locks the
    /// buffer would still be locking it after the session came back.
    async fn writable(&self, path: &str) -> bool {
        self.0.writable(path).await.unwrap_or(true)
    }

    /// Always `false`: the bytes are on the server, however near it happens to
    /// be. A session pointed at `localhost` is still crossing SSH, so treating
    /// it as local would make the panel promise a copy it would still perform as
    /// a transfer.
    fn is_local(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sftp_failures_keep_their_wording() {
        // The sentence is what reaches the status line, so folding the kinds
        // must not touch it.
        assert_eq!(
            FileError::from(SftpError::Remote("could not list /etc: denied".to_owned()))
                .to_string(),
            "could not list /etc: denied"
        );
        assert_eq!(
            FileError::from(SftpError::Disconnected).to_string(),
            "the SSH session is no longer connected"
        );
    }

    #[test]
    fn both_far_side_failures_fold_into_one_kind() {
        assert_eq!(
            FileError::from(SftpError::Subsystem("no SFTP here".to_owned())),
            FileError::Backend("no SFTP here".to_owned())
        );
        assert_eq!(
            FileError::from(SftpError::Remote("no SFTP here".to_owned())),
            FileError::Backend("no SFTP here".to_owned())
        );
    }
}
