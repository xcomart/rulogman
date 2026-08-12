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
//! [`FileSource::root_access`] is the question that follows a `no` from it:
//! whether this backend has a way of writing the file that the account itself
//! has not, and what that way costs — see [`RootAccess`], which has three
//! answers because the two backends that have such a way disagree about the
//! price. A capability the panel needs is a method on this trait; that is the
//! whole shape of the rule, and this is what obeying it looks like — including
//! [`FileSource::unlock_root`] and [`FileSource::copy_in_as_root`], the two
//! operations that capability leads to, which are calls of their own rather
//! than flags on [`FileSource::copy_in`] so that a backend with no such way in
//! inherits a refusal instead of having to notice a parameter.
//!
//! Three implementations answer it, and they are shaped very differently on
//! purpose. [`SftpSource`] is a forwarding shim for everything the *account*
//! does — every decision about how SFTP behaves stays in [`rulogman_ssh`], and
//! this module only renames things and folds the error kinds — with one
//! deliberate exception: writing as root is not an SFTP operation at all. It is
//! `sudo` run over the session's exec channel, decided here, because the SFTP
//! layer knows about files and this is a question about accounts. The local
//! source in [`local`] is the real
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

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use futures::channel::mpsc::UnboundedSender;
use rulogman_ssh::{ExecClient, ExecError, ExecOutput, SftpClient, SftpError};

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

/// What a source would have to be given before it could write a file as root.
///
/// The answer to [`FileSource::root_access`], and three-valued because the two
/// backends that have a root to offer disagree about what it costs. A WSL
/// distribution hands one over for nothing; a remote host may want the
/// account's own password for `sudo`, or may want nothing because the account
/// is configured `NOPASSWD` — and the editor has to know which *before* it
/// draws a button, since one of the two answers leads to a dialog.
///
/// Not a permission check in any of its arms. What is answered here is that a
/// road exists and what the toll is, never that the road is open: only the
/// write itself finds that out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootAccess {
    /// No way through, and so no button: this source can write a file only as
    /// the account that opened it.
    None,
    /// Root writes need nothing further — no password, no dialog, no delay.
    Granted,
    /// Root writes need the account's `sudo` password, which the editor has to
    /// ask for before it can promise anything.
    NeedsPassword,
}

impl From<ExecError> for FileError {
    /// Folds a failed remote command into the panel's vocabulary, exactly as
    /// [`SftpError`] is folded above and to the same rule: the sentence the
    /// error carries is already finished, and only the *kind* is translated.
    ///
    /// [`ExecError::Failed`] becomes [`FileError::Backend`] because that is
    /// what it is — the far side did not do it — and because nothing above
    /// branches on whether the refusal came from the server or from the
    /// command it would not start.
    fn from(error: ExecError) -> Self {
        match error {
            ExecError::Disconnected => Self::Disconnected,
            ExecError::Failed(message) => Self::Backend(message),
        }
    }
}

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

    /// Whether this source can write a file as root when the account cannot,
    /// and what doing so would cost.
    ///
    /// Asked only once [`FileSource::writable`] has said no, and it asks about
    /// the *backend* rather than about a file: whether there is a second way in
    /// at all. A source that has one lets the editor offer the way out of a
    /// read-only pane; a source that has not leaves the pane as it opened. See
    /// [`RootAccess`] for what the three answers mean.
    ///
    /// Not a permission check, and it could not be one. Whether the elevated
    /// write actually lands is [`FileSource::copy_in_as_root`]'s to find out —
    /// what is answered here is that the road exists, not that it is open.
    ///
    /// `async` where the stage before it was not, because the one backend that
    /// has to *ask* the far side cannot answer from memory: a remote host is
    /// interrogated over the session, and an implementation may well cache what
    /// it learns. The panel calls this in the same spawned future as
    /// [`FileSource::writable`] and for the same reason — the answer travels to
    /// the pane on the event, so no frame ever waits for it.
    ///
    /// The default is [`RootAccess::None`], which is the honest answer for a
    /// source holding nothing but the account's own credentials. It is also why
    /// this has a default where [`FileSource::writable`] refuses one: "there is
    /// no other account here" is a *complete* answer, and a source that never
    /// thought about the question is in exactly the same position as one that
    /// did.
    async fn root_access(&self) -> RootAccess {
        RootAccess::None
    }

    /// Makes ready whatever [`FileSource::copy_in_as_root`] will need, and
    /// reports whether it worked.
    ///
    /// The editor calls this once, on the press that unlocks the pane, and
    /// `password` carries what the user typed when
    /// [`FileSource::root_access`] answered [`RootAccess::NeedsPassword`] and
    /// nothing when it answered [`RootAccess::Granted`]. Two things come of it,
    /// and the first is the reason it exists: a wrong password is found out
    /// *here*, while a dialog is still on screen to say so, rather than at the
    /// end of a save the user has by then typed a file's worth of changes into.
    /// The second is `remember` — whether the implementation keeps the password
    /// for the rest of the session, so that later saves need no dialog at all.
    /// A `false` there is not a promise that nothing is kept; it is an
    /// instruction not to, and the implementations obey it.
    ///
    /// Nothing is written and no file is named: this is about the *account*.
    /// Succeeding says the elevated road was open a moment ago, which is
    /// exactly as much as [`FileSource::writable`] says about the ordinary one.
    ///
    /// The default refuses, for the reason [`FileSource::copy_in_as_root`]'s
    /// default does: nothing reaches here except through a source that said it
    /// had a root to offer.
    async fn unlock_root(&self, password: Option<&str>, remember: bool) -> Result<(), FileError> {
        let _ = (password, remember);
        Err(FileError::Backend(
            "there is no way to become root on this filesystem".to_owned(),
        ))
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
    /// `password` is the account's own, for a backend whose root asks for one,
    /// and `None` for every other case: a backend that needs no password, and a
    /// backend that was given one to keep by [`FileSource::unlock_root`] and is
    /// expected to use it. Which of those two a `None` means is the
    /// implementation's business and not the caller's — the editor knows only
    /// that it has a password to hand this time or has not.
    ///
    /// The default refuses, in a sentence rather than by panicking. Nothing
    /// reaches here except through a source whose [`FileSource::root_access`]
    /// answered something other than [`RootAccess::None`], so a caller that
    /// arrives has already gone wrong somewhere above — and the pane that
    /// started the save has a place to show the reason, which is more use than
    /// a crash.
    async fn copy_in_as_root(
        &self,
        local: PathBuf,
        dir: &str,
        password: Option<&str>,
    ) -> Result<String, FileError> {
        let _ = password;
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
/// A forwarding shim for everything an ordinary account does: the SFTP layer
/// already decided how each of those behaves, and repeating any of it here
/// would be a second place for it to be wrong. Writing as root is the one part
/// that is decided here instead, because it is not an SFTP operation — it is
/// `sudo` run over the session's exec channel, and the SFTP layer has no
/// business knowing what an administrative group is called.
///
/// Cheap to construct, which it has to be:
/// [`Session::files`](crate::session::Session::files) builds one on every
/// terminal notification. Both clients only clone a request channel, the SFTP
/// channel itself is opened lazily on the first call, and an exec channel is
/// opened per command and closed with it.
///
/// **Neither of the two cells survives a rebuild**, and that is a consequence
/// worth stating rather than a subtlety to trip over: a source built afresh has
/// probed nothing and remembers no password. What makes it work anyway is that
/// the editor holds the `Arc` it was opened with for as long as the pane is
/// open — see [`EditorPane`](crate::editor_pane::EditorPane) — so the cells
/// live exactly as long as the file does.
pub struct SftpSource {
    /// The SFTP client every file operation is forwarded to.
    files: SftpClient,
    /// The exec client the elevated write and its probes run over.
    ///
    /// A second rider on the same session rather than a second session: the
    /// commands below have to run as the account whose files these are, and
    /// they have to run on the host the panel is browsing.
    commands: ExecClient,
    /// What the last probe of the remote account's `sudo` found, once one has
    /// answered.
    ///
    /// [`RefCell`] rather than a lock because this trait's futures are `?Send`
    /// and are polled on the UI thread, so there is no second thread to race
    /// with — but no borrow of it may be held across an `.await`, which is why
    /// every reader below copies the value out and drops the borrow first.
    ///
    /// Cached because the answer is about the *account*, which does not change
    /// under an open session, and because finding it out costs up to three
    /// round trips.
    access: RefCell<Option<RootAccess>>,
    /// The account's `sudo` password, once the user has asked for it to be
    /// remembered.
    ///
    /// A plain [`String`], held in memory for the life of this source and
    /// nowhere else: not written to disk, not put in a log line, not passed on
    /// a command line — every use of it goes on a command's standard input,
    /// where the remote host's `ps` cannot read it. That is the same standard
    /// [`SshAuth`](rulogman_ssh::SshAuth) keeps for the login password, and
    /// this type has no [`Debug`](std::fmt::Debug) implementation to leak it
    /// through.
    password: RefCell<Option<String>>,
}

impl SftpSource {
    /// Wraps a session's two clients as the file source it browses through.
    ///
    /// `commands` is taken here rather than reached for later because a source
    /// with no way to run a command could not offer the elevated write at all,
    /// and a capability that appears halfway through a session's life would be
    /// a button that grows under the pointer.
    pub fn new(files: SftpClient, commands: ExecClient) -> Self {
        Self {
            files,
            commands,
            access: RefCell::new(None),
            password: RefCell::new(None),
        }
    }

    /// Runs one command on the remote host, feeding it `stdin`.
    ///
    /// Every command below goes through here so that the rule about secrets is
    /// kept in one place: what a remote `ps` can read is the command line, so
    /// nothing that must stay private is ever formatted into one — the password
    /// and the file's bytes both travel in `stdin`.
    async fn run(&self, command: String, stdin: Vec<u8>) -> Result<ExecOutput, FileError> {
        self.commands
            .run(command, stdin)
            .await
            .map_err(FileError::from)
    }

    /// Asks the remote account's `sudo` what it would want, in up to three
    /// commands.
    ///
    /// Each gate is skipped once an earlier one has settled the answer, which
    /// is why the unreached ones are folded in as `None`: an exit status that
    /// was never asked for and one the server never sent are the same absence,
    /// and [`sudo_verdict`] consults neither once an earlier gate has decided.
    ///
    /// The `Err` here is the transport's, never the account's. A command that
    /// ran and said no is an answer and comes back as one; only a session that
    /// could not carry the question at all fails, and
    /// [`FileSource::root_access`] is careful not to cache that.
    async fn probe_root_access(&self) -> Result<RootAccess, FileError> {
        let sudo = self.run(SUDO_PRESENT.to_owned(), Vec::new()).await?;
        let sudo = sudo.exit_status;

        let free = if sudo == Some(0) {
            self.run(SUDO_WITHOUT_PASSWORD.to_owned(), Vec::new())
                .await?
                .exit_status
        } else {
            None
        };

        let groups = if sudo == Some(0) && free != Some(0) {
            Some(self.run(GROUP_NAMES.to_owned(), Vec::new()).await?)
        } else {
            None
        };
        // Lossy on purpose: a group name is ASCII in every practical case, and
        // a host that answers in something else has still answered — mangling
        // one name is better than refusing the whole list over it.
        let names = groups
            .as_ref()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();

        Ok(sudo_verdict(
            sudo,
            free,
            groups.and_then(|output| output.exit_status),
            &names,
        ))
    }
}

/// Whether the account has a `sudo` to run at all.
///
/// `command -v` rather than a path, because `sudo` is in `/usr/bin` on one
/// distribution and `/usr/local/bin` on the next, and the login shell's own
/// `PATH` is the only authority on which. The output is thrown away — this is
/// asked for its exit status alone.
const SUDO_PRESENT: &str = "command -v sudo >/dev/null 2>&1";

/// Whether `sudo` would run something *now* without asking anything.
///
/// `-n` forbids it to prompt, so this either succeeds or fails at once instead
/// of hanging on a password prompt nobody is watching. `-p ''` empties the
/// prompt string, so a refusal writes only its own diagnostic to standard error
/// and nothing that has to be told apart from one.
///
/// True for a `NOPASSWD` rule and true for an account whose timestamp is still
/// live from a `sudo` in the terminal beside this one. Both are the same answer
/// to the only question being asked, which is whether a password is needed *at
/// this moment*.
const SUDO_WITHOUT_PASSWORD: &str = "sudo -n -p '' true";

/// The names of every group the account belongs to, whitespace separated.
///
/// `-n` for names rather than ids, because the answer is compared against
/// [`ADMIN_GROUPS`] and group ids are not portable between hosts.
const GROUP_NAMES: &str = "id -Gn";

/// Validates a password and, on a host that allows it, starts the timestamp.
///
/// `-S` reads the password from standard input, which is where a password
/// belongs; `-p ''` keeps the prompt out of the standard error a failure is
/// explained from; `-k` ignores any live timestamp, so this really does test
/// the password rather than riding on a `sudo` the user ran a minute ago.
const SUDO_VALIDATE: &str = "sudo -S -p '' -k true";

/// Group names that conventionally carry `sudo` rights.
///
/// A convention and not a rule, which is the whole limitation of this probe:
/// membership of one of these is strong evidence that `sudo` will accept the
/// account's password, and an account granted rights by name in `sudoers`
/// without any of these groups is invisible to it. The failure mode is a button
/// that is not offered rather than one that does not work, which is the right
/// way round.
///
/// `root` is in the list because a `sudo` that is configured for it is a `sudo`
/// that works; `admin` and `wheel` are what the BSDs and Red Hat call the group
/// Debian calls `sudo`.
const ADMIN_GROUPS: [&str; 4] = ["sudo", "wheel", "admin", "root"];

/// Turns the three gates' answers into the verdict the editor acts on.
///
/// Pure, and separated from the commands that feed it, because this is the
/// whole of the decision and none of the plumbing — it can be read, and tested,
/// without a host to run anything on.
///
/// **Every gate is an exit status, never a message.** A remote `sudo` writes
/// its diagnostics in the remote host's own locale, and a probe that matched
/// English text would silently decide that a German host had no `sudo` at all.
/// Exit statuses are the one part of the answer that is the same everywhere.
///
/// A missing status — the server skipped `exit-status`, or the process was
/// killed by a signal — counts as failure at every gate. There is no honest
/// default to substitute: a command that did not say how it ended has not said
/// it succeeded, and the cost of guessing wrongly is a button that promises a
/// save it cannot make.
fn sudo_verdict(
    sudo: Option<u32>,
    without_password: Option<u32>,
    groups: Option<u32>,
    group_names: &str,
) -> RootAccess {
    if sudo != Some(0) {
        return RootAccess::None;
    }
    if without_password == Some(0) {
        return RootAccess::Granted;
    }
    if groups != Some(0) {
        return RootAccess::None;
    }
    if group_names
        .split_whitespace()
        .any(|name| ADMIN_GROUPS.contains(&name))
    {
        RootAccess::NeedsPassword
    } else {
        RootAccess::None
    }
}

/// `word`, quoted so that a POSIX shell reads it as exactly one argument.
///
/// Single quotes, because inside them a shell interprets nothing at all — no
/// `$`, no backtick, no backslash, no newline — and the one character that
/// cannot appear between them is the quote itself, which is spelled by closing,
/// escaping it, and opening again: `it's` becomes `'it'\''s'`.
///
/// Only paths go through here. The password never does, and never needs to,
/// because it is never on a command line to be quoted: it goes in on the
/// command's standard input, where the remote host's `ps` cannot read it.
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The name `local` will be given on the far side of a copy.
///
/// The same derivation [`SftpClient::upload`] makes, and it has to be: an
/// elevated save writes the file the ordinary save would have written, and a
/// second spelling of that name would be a second file.
fn file_name(local: &Path) -> Result<String, FileError> {
    local
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            FileError::Path(format!(
                "{} has no file name that can be used on the remote host",
                local.display()
            ))
        })
}

/// Appends `name` to the directory `dir`, POSIX style.
///
/// Spelled here rather than borrowed from the SFTP layer, which keeps its own
/// copy private, and to the same rule: exactly one separator, and none added to
/// a directory that already ends in one — `//etc` is not a path POSIX promises
/// means `/etc`.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The sentence a refused command explains itself with.
///
/// `reason` is the remote host's own standard error, which is not translated —
/// it is written in that host's locale by a program this application does not
/// control, and rewriting it would mean pretending to have understood it. With
/// `-p ''` on every `sudo` here there is no prompt mixed into it, so whatever
/// arrives is diagnostics. A refusal with nothing to say falls back to
/// `attempt` alone, which is the only other thing known about it.
fn refusal(attempt: &str, output: &ExecOutput) -> FileError {
    let reason = String::from_utf8_lossy(&output.stderr);
    let reason = reason.trim();
    FileError::Backend(if reason.is_empty() {
        attempt.to_owned()
    } else {
        format!("{attempt}: {reason}")
    })
}

#[async_trait::async_trait(?Send)]
impl FileSource for SftpSource {
    async fn home(&self) -> Result<String, FileError> {
        self.files.home().await.map_err(FileError::from)
    }

    async fn realpath(&self, path: &str) -> Result<String, FileError> {
        self.files.realpath(path).await.map_err(FileError::from)
    }

    async fn read_dir(&self, path: &str) -> Result<Vec<FileEntry>, FileError> {
        self.files.read_dir(path).await.map_err(FileError::from)
    }

    async fn mkdir(&self, path: &str) -> Result<(), FileError> {
        self.files.mkdir(path).await.map_err(FileError::from)
    }

    async fn remove_file(&self, path: &str) -> Result<(), FileError> {
        self.files.remove_file(path).await.map_err(FileError::from)
    }

    async fn remove_dir(&self, path: &str) -> Result<(), FileError> {
        self.files.remove_dir(path).await.map_err(FileError::from)
    }

    async fn rename(&self, old: &str, new: &str) -> Result<(), FileError> {
        self.files.rename(old, new).await.map_err(FileError::from)
    }

    async fn copy_in(
        &self,
        local: PathBuf,
        dir: &str,
        progress: Option<UnboundedSender<u64>>,
    ) -> Result<String, FileError> {
        self.files
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
        self.files
            .download(path, local, progress)
            .await
            .map_err(FileError::from)
    }

    /// Folds a failed probe into `true`, which is the only fold that keeps the
    /// trait's promise: an unreachable server has not said the file is
    /// unwritable, it has said nothing at all — and a disconnect that locks the
    /// buffer would still be locking it after the session came back.
    async fn writable(&self, path: &str) -> bool {
        self.files.writable(path).await.unwrap_or(true)
    }

    /// Asks the remote account's `sudo` what it would want, once per session.
    ///
    /// The answer is cached because it is about the account rather than about a
    /// file, and because finding it out costs up to three round trips — a price
    /// worth paying on the first unwritable file and not on the fiftieth.
    ///
    /// A failed *probe* is not cached, and the asymmetry is deliberate. An
    /// answer of [`RootAccess::None`] taken from a session that had just
    /// dropped would follow this source for the rest of its life, and the way
    /// out of a read-only pane would be missing on a reconnected session for no
    /// reason the user could see. An offer not made can be made again on the
    /// next file; an offer wrongly withdrawn stays withdrawn.
    async fn root_access(&self) -> RootAccess {
        // Copied out, and the borrow dropped on this line: a `RefCell` borrow
        // held across the `.await` below would be a panic waiting for the first
        // caller who asked twice.
        let cached = *self.access.borrow();
        if let Some(access) = cached {
            return access;
        }

        match self.probe_root_access().await {
            Ok(access) => {
                *self.access.borrow_mut() = Some(access);
                access
            }
            Err(error) => {
                log::debug!("could not ask the remote host about sudo: {error}");
                RootAccess::None
            }
        }
    }

    /// Puts the account's `sudo` to the test before anything is written.
    ///
    /// With a password, that is [`SUDO_VALIDATE`]: the bytes go in on standard
    /// input with a newline after them, because that is how `sudo -S` reads a
    /// password and how a password stays off the command line. An exit status
    /// of zero is the password being accepted, and only then is it kept —
    /// `remember` is asked *after* the verdict, so nothing wrong is ever stored
    /// and a rejected attempt leaves whatever was already remembered alone.
    ///
    /// Without one, the [`RootAccess::Granted`] gate is simply run again. That
    /// is the point of doing it here rather than assuming it: the probe that
    /// answered `Granted` may have been riding a `sudo` timestamp that has
    /// since expired, and finding that out on the press — with a sentence in
    /// the pane — is better than unlocking a buffer whose every save will fail.
    async fn unlock_root(&self, password: Option<&str>, remember: bool) -> Result<(), FileError> {
        let Some(password) = password else {
            let output = self
                .run(SUDO_WITHOUT_PASSWORD.to_owned(), Vec::new())
                .await?;
            return if output.exit_status == Some(0) {
                Ok(())
            } else {
                Err(refusal("sudo would not run without a password", &output))
            };
        };

        let mut stdin = password.as_bytes().to_vec();
        stdin.push(b'\n');
        let output = self.run(SUDO_VALIDATE.to_owned(), stdin).await?;
        if output.exit_status != Some(0) {
            return Err(refusal("sudo did not accept the password", &output));
        }

        if remember {
            *self.password.borrow_mut() = Some(password.to_owned());
        }
        Ok(())
    }

    /// Writes `local` into the remote directory `dir` as root, and answers the
    /// path it landed at.
    ///
    /// Not over SFTP, which is the whole of the point: the SFTP subsystem runs
    /// as the account that logged in, and that account is precisely the one
    /// [`FileSource::writable`] has already said may not write this file. So
    /// the bytes go in through a command instead — `sudo … tee`, with the file
    /// on its standard input.
    ///
    /// `tee` rather than a redirection into the target, and for the reason the
    /// WSL source picks it: `tee` *truncates* the file that is there rather
    /// than replacing it, so the inode survives the write and with it the
    /// file's owner, group and mode. Editing `/etc/hosts` as root does not hand
    /// `/etc/hosts` to root. `>/dev/null` matters as much: this command line
    /// goes through the remote login shell, and without the redirection `tee`
    /// would echo every byte it wrote back across the session for nobody to
    /// read. The `--` before the path stops a file called `-x` from being read
    /// as an option, and [`shell_quote`] stops everything else in it from being
    /// read as syntax.
    ///
    /// The password — the one passed in, or the one
    /// [`FileSource::unlock_root`] was asked to keep — goes in ahead of the
    /// file's bytes on the same standard input, because that is where `sudo -S`
    /// reads it and because a command line is world-readable on the far side.
    /// With no password to hand at all the `-n` form runs instead, which is the
    /// [`RootAccess::Granted`] case: it refuses rather than prompting, so a
    /// host that has changed its mind fails in a sentence instead of hanging on
    /// a prompt nobody can answer.
    async fn copy_in_as_root(
        &self,
        local: PathBuf,
        dir: &str,
        password: Option<&str>,
    ) -> Result<String, FileError> {
        let written = join(dir, &file_name(&local)?);
        let bytes = std::fs::read(&local).map_err(|error| {
            FileError::Local(format!("{} could not be read: {error}", local.display()))
        })?;

        // Copied out and the borrow dropped before anything is awaited; the
        // clone is a password's worth of bytes and buys a `RefCell` that is
        // never borrowed across a suspension point.
        let remembered = self.password.borrow().clone();
        let quoted = shell_quote(&written);
        let (command, stdin) = match password.or(remembered.as_deref()) {
            Some(password) => {
                let mut stdin = password.as_bytes().to_vec();
                stdin.push(b'\n');
                stdin.extend_from_slice(&bytes);
                (
                    format!("sudo -S -p '' -k -- tee -- {quoted} >/dev/null"),
                    stdin,
                )
            }
            None => (
                format!("sudo -n -p '' -- tee -- {quoted} >/dev/null"),
                bytes,
            ),
        };

        let output = self.run(command, stdin).await?;
        if output.exit_status == Some(0) {
            Ok(written)
        } else {
            Err(refusal(
                &format!("{written} could not be written as root"),
                &output,
            ))
        }
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

    #[test]
    fn a_command_that_could_not_run_folds_the_way_a_transfer_does() {
        // The two services fail for unrelated reasons and keep their own error
        // types; what the panel sees of either is the same four kinds.
        assert_eq!(
            FileError::from(ExecError::Disconnected),
            FileError::Disconnected
        );
        assert_eq!(
            FileError::from(ExecError::Failed("the server refused it".to_owned())),
            FileError::Backend("the server refused it".to_owned())
        );
    }

    #[test]
    fn an_account_without_sudo_is_offered_nothing() {
        // The first gate settles it, and the later two are never even run — so
        // they arrive here as the absence they are.
        assert_eq!(sudo_verdict(Some(1), None, None, ""), RootAccess::None);
        assert_eq!(sudo_verdict(Some(127), None, None, ""), RootAccess::None);
    }

    #[test]
    fn sudo_that_runs_without_asking_costs_nothing_further() {
        assert_eq!(
            sudo_verdict(Some(0), Some(0), None, ""),
            RootAccess::Granted
        );
    }

    #[test]
    fn membership_of_an_administrative_group_is_worth_asking_for_a_password() {
        // The four names the conventions use, each on its own, and each in a
        // list with the account's ordinary groups around it.
        for group in ["sudo", "wheel", "admin", "root"] {
            assert_eq!(
                sudo_verdict(
                    Some(0),
                    Some(1),
                    Some(0),
                    &format!("ada dialout {group} plugdev")
                ),
                RootAccess::NeedsPassword,
                "{group} was not recognised"
            );
        }
    }

    #[test]
    fn an_account_in_no_administrative_group_is_offered_nothing() {
        assert_eq!(
            sudo_verdict(Some(0), Some(1), Some(0), "ada dialout plugdev"),
            RootAccess::None
        );
        // A prefix of one of the names is not one of the names: the list is
        // split on whitespace and compared whole, so `sudoers` is not `sudo`.
        assert_eq!(
            sudo_verdict(Some(0), Some(1), Some(0), "sudoers wheelie"),
            RootAccess::None
        );
    }

    #[test]
    fn a_command_that_never_said_how_it_ended_counts_as_a_refusal_at_every_gate() {
        // A server may skip `exit-status` altogether, and a process killed by a
        // signal leaves none. There is nothing to read into that but failure.
        assert_eq!(
            sudo_verdict(None, Some(0), Some(0), "sudo"),
            RootAccess::None
        );
        // The second gate: no status is not a passwordless sudo, so the third
        // gate decides — and it says the account could still be asked.
        assert_eq!(
            sudo_verdict(Some(0), None, Some(0), "sudo"),
            RootAccess::NeedsPassword
        );
        // The third: a group list that did not arrive is not a group list.
        assert_eq!(
            sudo_verdict(Some(0), Some(1), None, "sudo"),
            RootAccess::None
        );
    }

    #[test]
    fn a_path_is_quoted_so_that_the_remote_shell_reads_it_as_one_word() {
        assert_eq!(shell_quote("/etc/hosts"), "'/etc/hosts'");
        assert_eq!(shell_quote("/tmp/my notes"), "'/tmp/my notes'");
        // Everything a shell would otherwise act on, inert inside the quotes.
        assert_eq!(shell_quote("/tmp/$HOME"), "'/tmp/$HOME'");
        assert_eq!(shell_quote("/tmp/`id`"), "'/tmp/`id`'");
        assert_eq!(shell_quote("/tmp/a;rm -rf /"), "'/tmp/a;rm -rf /'");
        assert_eq!(shell_quote("/tmp/a\\b"), "'/tmp/a\\b'");
        // A newline in a file name survives, because a single-quoted string
        // spans lines.
        assert_eq!(shell_quote("/tmp/two\nlines"), "'/tmp/two\nlines'");
    }

    #[test]
    fn a_quote_in_a_name_closes_the_string_escapes_itself_and_opens_it_again() {
        // The one character single quotes cannot hold. `ada's notes` has to
        // come out as four concatenated pieces, and a shell joins them back
        // into one word.
        assert_eq!(shell_quote("ada's notes"), r"'ada'\''s notes'");
        assert_eq!(shell_quote("'"), r"''\'''");
    }

    #[test]
    fn an_elevated_write_names_the_same_file_the_ordinary_one_would() {
        assert_eq!(join("/etc", "hosts"), "/etc/hosts");
        assert_eq!(join("/", "hosts"), "/hosts");
        assert_eq!(join("", "hosts"), "hosts");
        assert_eq!(
            file_name(Path::new("/tmp/staging/hosts")).expect("a staged file has a name"),
            "hosts"
        );
    }

    #[test]
    fn a_refusal_carries_the_remote_hosts_own_words_or_says_only_what_it_knows() {
        let mut output = ExecOutput {
            exit_status: Some(1),
            ..ExecOutput::default()
        };
        assert_eq!(
            refusal("sudo did not accept the password", &output),
            FileError::Backend("sudo did not accept the password".to_owned())
        );

        // Whitespace and the trailing newline come off; nothing else is
        // touched, because the sentence is the host's and not ours to edit.
        output.stderr = b"sudo: 3 incorrect password attempts\n".to_vec();
        assert_eq!(
            refusal("sudo did not accept the password", &output),
            FileError::Backend(
                "sudo did not accept the password: sudo: 3 incorrect password attempts".to_owned()
            )
        );
    }

    /// Nothing that has to stay private may be formatted into a command line:
    /// a remote `ps` shows one to every account on the host. The three
    /// constants are checked as a set, because the rule is about all of them
    /// and a fourth added later has to obey it too.
    #[test]
    fn no_command_this_module_sends_has_anywhere_to_put_a_password() {
        for command in [
            SUDO_PRESENT,
            SUDO_WITHOUT_PASSWORD,
            GROUP_NAMES,
            SUDO_VALIDATE,
        ] {
            assert!(
                !command.contains("%s") && !command.contains('{'),
                "{command} looks like it interpolates something"
            );
        }
        // And every `sudo` among them silences the prompt, so the standard
        // error a failure is explained from holds diagnostics and nothing else.
        for command in [SUDO_WITHOUT_PASSWORD, SUDO_VALIDATE] {
            assert!(command.contains("-p ''"), "{command} would print a prompt");
        }
    }
}
