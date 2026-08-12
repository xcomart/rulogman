//! SFTP client riding on an established [`SshSession`](crate::SshSession).
//!
//! The remote file panel needs directory listings and file transfers while the
//! terminal keeps running. Both live on the same SSH transport but on
//! *different* channels: the shell owns one, SFTP gets its own. That separation
//! is the whole point of this module — a directory listing must never stall the
//! shell, and a multi-gigabyte download must never swallow a keystroke.
//!
//! The pieces fit together like this:
//!
//! * [`SftpClient`] is the caller-facing handle. It is cheap to clone, `Send`
//!   and `Sync`, and every method is `async`: the request is queued on the
//!   session's worker thread and the returned future resolves when the answer
//!   comes back. Nothing here touches the network directly, so a GUI thread can
//!   hold one safely.
//! * [`serve`] runs on the session worker's runtime. It owns the transport
//!   handle, opens the SFTP channel *lazily* on the first request, keeps it for
//!   every request afterwards, and drives each request on its own task so slow
//!   transfers do not serialise behind one another.
//!
//! Errors are plain English sentences carried in [`SftpError`]. The application
//! layer shows them verbatim and only localises the sentence that frames them,
//! so every message built here has to read as a complete explanation on its
//! own. Once the session is gone, every call fails with
//! [`SftpError::Disconnected`] rather than hanging or panicking.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use russh::client::{self, Handle};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as ProtocolError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

/// Bytes moved between the local file and the remote one per iteration.
///
/// Transfers are streamed rather than buffered whole, so file size is bounded
/// by the remote disk and not by this process's memory. The loop is also where
/// progress is reported from: it already sees every chunk, so the running byte
/// count costs nothing beyond the send.
const TRANSFER_CHUNK: usize = 64 * 1024;

/// Why an SFTP operation could not be completed.
///
/// Every variant renders as a finished English sentence fragment suitable for
/// display; the application wraps it in a localised sentence but never rewrites
/// it. No variant carries credentials, and none carries file contents.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SftpError {
    /// The SSH session had already ended, or ended while the request was in
    /// flight. Retrying is pointless until a new session is connected.
    #[error("the SSH session is no longer connected")]
    Disconnected,
    /// The `sftp` subsystem could not be started on this server. Usually means
    /// the server has SFTP disabled rather than that anything went wrong here.
    #[error("{0}")]
    Subsystem(String),
    /// The server refused the operation, or the exchange with it broke down.
    #[error("{0}")]
    Remote(String),
    /// A local file could not be opened, read, or written.
    #[error("{0}")]
    Local(String),
    /// A path could not be used as given — for instance a local path with no
    /// file name component, or one that is not valid UTF-8.
    #[error("{0}")]
    Path(String),
}

/// One entry of a remote directory listing.
///
/// Deliberately flat and owned: the file panel keeps thousands of these and
/// sorts them itself, so this carries no handle back into the session and no
/// borrow of the listing it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    /// File name only, without any directory part. Never `.` or `..`.
    pub name: String,
    /// Whether the entry can be descended into.
    ///
    /// For a symlink this describes its *target*, so that a link to a directory
    /// behaves like a directory in the UI. When the target cannot be stat'ed —
    /// a broken link, or one pointing somewhere unreadable — the link's own
    /// type is used instead and this is `false`.
    pub is_dir: bool,
    /// Whether the entry is itself a symbolic link, regardless of what it
    /// points at. Purely presentational; `is_dir` already decided navigation.
    pub is_symlink: bool,
    /// Size in bytes, taken from the target for a resolvable symlink. Zero when
    /// the server reported no size.
    pub size: u64,
}

/// Handle for issuing SFTP requests against a live session.
///
/// Obtained from [`SshSession::sftp`](crate::SshSession::sftp). Cloning is
/// cheap and every clone talks to the same SFTP channel, so a UI can hand one
/// to each pane without multiplying channels on the wire.
///
/// Requests made before the session becomes ready are queued, not rejected;
/// requests made after it ends fail with [`SftpError::Disconnected`].
#[derive(Clone)]
pub struct SftpClient {
    /// Request channel to the SFTP service running on the session worker.
    requests: UnboundedSender<SftpRequest>,
}

impl SftpClient {
    /// Wraps the sending half of the session's SFTP request channel.
    pub(crate) fn new(requests: UnboundedSender<SftpRequest>) -> Self {
        Self { requests }
    }

    /// Returns the absolute path of the login directory.
    ///
    /// Resolved by asking the server to canonicalise `"."`, which is what every
    /// SFTP client does: the protocol has no separate "home" request, and the
    /// server's notion of the starting directory is exactly what `realpath .`
    /// answers.
    pub async fn home(&self) -> Result<String, SftpError> {
        self.request(SftpRequest::Home).await
    }

    /// Canonicalises `path` on the server.
    ///
    /// Resolves `.`, `..` and symbolic links, and makes relative paths
    /// absolute. This is how the file panel walks upwards: asking for
    /// `<current>/..` yields the parent without any local path arithmetic,
    /// which would otherwise have to guess the remote separator conventions.
    pub async fn realpath(&self, path: &str) -> Result<String, SftpError> {
        let path = path.to_owned();
        self.request(move |reply| SftpRequest::RealPath { path, reply })
            .await
    }

    /// Lists the directory at `path`.
    ///
    /// `.` and `..` are filtered out. The order is whatever the server sent —
    /// sorting is the caller's business, because only the UI knows whether the
    /// user asked for name, size or type order.
    ///
    /// Symbolic links cost one extra round trip each, spent resolving the
    /// target so that [`RemoteEntry::is_dir`] is meaningful.
    pub async fn read_dir(&self, path: &str) -> Result<Vec<RemoteEntry>, SftpError> {
        let path = path.to_owned();
        self.request(move |reply| SftpRequest::ReadDir { path, reply })
            .await
    }

    /// Creates the remote directory `path`, and succeeds if it already exists.
    ///
    /// Existing-is-fine rather than an error, because the caller creating a
    /// tree creates every directory in it unconditionally: probing first would
    /// cost a round trip per directory to learn what the create is about to
    /// report anyway, and re-sending a folder into a place it was sent before
    /// is a merge, not a mistake.
    pub async fn mkdir(&self, path: &str) -> Result<(), SftpError> {
        let path = path.to_owned();
        self.request(move |reply| SftpRequest::MkDir { path, reply })
            .await
    }

    /// Deletes the remote file at `path`.
    ///
    /// A symbolic link is removed as itself, never followed: this is what the
    /// file panel calls for a link to a directory, and following it would
    /// delete the target's contents instead of the link.
    ///
    /// Directories are not handled — the protocol has a separate request for
    /// them — so pointing this at one fails with whatever the server answers.
    pub async fn remove_file(&self, path: &str) -> Result<(), SftpError> {
        let path = path.to_owned();
        self.request(move |reply| SftpRequest::RemoveFile { path, reply })
            .await
    }

    /// Deletes the remote directory at `path`, which must already be empty.
    ///
    /// Emptiness is the server's rule, not one added here: SFTP has no
    /// recursive delete, so a caller removing a tree walks it itself and
    /// removes the children first — which is also the only way the progress
    /// line can say how much is left.
    pub async fn remove_dir(&self, path: &str) -> Result<(), SftpError> {
        let path = path.to_owned();
        self.request(move |reply| SftpRequest::RemoveDir { path, reply })
            .await
    }

    /// Renames the remote entry `old` to `new`.
    ///
    /// Whether an existing `new` is overwritten or refused is left entirely to
    /// the server: SFTP 3 does not say, implementations disagree, and probing
    /// first would only widen the window in which the answer stops being true.
    /// A refusal surfaces as the server's own sentence.
    pub async fn rename(&self, old: &str, new: &str) -> Result<(), SftpError> {
        let old = old.to_owned();
        let new = new.to_owned();
        self.request(move |reply| SftpRequest::Rename { old, new, reply })
            .await
    }

    /// Uploads the local file `local` into the remote directory `remote_dir`,
    /// keeping its file name, and returns the remote path it was written to.
    ///
    /// An existing remote file of the same name is truncated and overwritten.
    /// Only regular files are handled; pointing this at a directory fails
    /// rather than recursing, which keeps the failure obvious instead of
    /// half-copying a tree. Callers that *want* a tree walk it themselves and
    /// call [`SftpClient::mkdir`] along the way.
    ///
    /// `progress`, when given, receives this file's running byte count — one
    /// message per chunk, monotonically increasing, ending at the file's size.
    /// It is a hint for a status line, so a receiver that has gone away is not
    /// an error and does not stop the transfer.
    pub async fn upload(
        &self,
        local: PathBuf,
        remote_dir: &str,
        progress: Option<UnboundedSender<u64>>,
    ) -> Result<String, SftpError> {
        let remote_dir = remote_dir.to_owned();
        self.request(move |reply| SftpRequest::Upload {
            local,
            remote_dir,
            progress,
            reply,
        })
        .await
    }

    /// Downloads the remote file at `remote_path` to the local path `local`.
    ///
    /// An existing local file is truncated and overwritten. The parent
    /// directory must already exist.
    ///
    /// `progress` behaves exactly as it does for [`SftpClient::upload`].
    pub async fn download(
        &self,
        remote_path: &str,
        local: PathBuf,
        progress: Option<UnboundedSender<u64>>,
    ) -> Result<(), SftpError> {
        let remote_path = remote_path.to_owned();
        self.request(move |reply| SftpRequest::Download {
            remote_path,
            local,
            progress,
            reply,
        })
        .await
    }

    /// Queues one request and waits for its answer.
    ///
    /// A closed request channel and a dropped reply sender mean the same thing
    /// from here — the session worker is gone — so both collapse into
    /// [`SftpError::Disconnected`].
    async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, SftpError>>) -> SftpRequest,
    ) -> Result<T, SftpError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .unbounded_send(build(reply))
            .map_err(|_| SftpError::Disconnected)?;
        answer.await.map_err(|_| SftpError::Disconnected)?
    }
}

impl std::fmt::Debug for SftpClient {
    /// Reports only whether the client can still reach its session; there is
    /// no useful state to show and the channel itself is not printable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpClient")
            .field("connected", &!self.requests.is_closed())
            .finish()
    }
}

/// One unit of work handed to the SFTP service, with the channel to answer on.
///
/// Every variant owns its arguments: the request crosses a thread boundary and
/// outlives the call that made it.
pub(crate) enum SftpRequest {
    /// Resolve the login directory.
    Home(oneshot::Sender<Result<String, SftpError>>),
    /// Canonicalise a path.
    RealPath {
        /// Path to canonicalise.
        path: String,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<String, SftpError>>,
    },
    /// List a directory.
    ReadDir {
        /// Directory to list.
        path: String,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<Vec<RemoteEntry>, SftpError>>,
    },
    /// Create a remote directory.
    MkDir {
        /// Directory to create.
        path: String,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<(), SftpError>>,
    },
    /// Delete a remote file, or a symbolic link of any kind.
    RemoveFile {
        /// File to delete.
        path: String,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<(), SftpError>>,
    },
    /// Delete an empty remote directory.
    RemoveDir {
        /// Directory to delete.
        path: String,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<(), SftpError>>,
    },
    /// Rename a remote entry.
    Rename {
        /// Path as it is now.
        old: String,
        /// Path it should have.
        new: String,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<(), SftpError>>,
    },
    /// Copy a local file to the remote host.
    Upload {
        /// Local file to read.
        local: PathBuf,
        /// Remote directory to write into.
        remote_dir: String,
        /// Running byte count of this file, for a status line.
        progress: Option<UnboundedSender<u64>>,
        /// Where the resulting remote path goes.
        reply: oneshot::Sender<Result<String, SftpError>>,
    },
    /// Copy a remote file to the local host.
    Download {
        /// Remote file to read.
        remote_path: String,
        /// Local path to write.
        local: PathBuf,
        /// Running byte count of this file, for a status line.
        progress: Option<UnboundedSender<u64>>,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<(), SftpError>>,
    },
}

/// Serves SFTP requests for one session until the request channel closes.
///
/// Runs as a task on the session worker's runtime, alongside — never inside —
/// the shell's message loop. Each request is spawned separately so a long
/// transfer does not hold up a directory listing queued behind it; the shared
/// [`Service`] makes them all reuse the single SFTP channel.
pub(crate) async fn serve<H>(handle: Arc<Handle<H>>, mut requests: UnboundedReceiver<SftpRequest>)
where
    H: client::Handler + 'static,
{
    let service = Arc::new(Service {
        handle,
        session: Mutex::new(None),
    });

    while let Some(request) = requests.next().await {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.dispatch(request).await });
    }
    log::debug!("sftp service finished");
}

/// The lazily opened SFTP channel plus the transport it is opened on.
struct Service<H: client::Handler> {
    /// Transport handle, shared with the session's shell loop.
    handle: Arc<Handle<H>>,
    /// The SFTP channel, opened on first use and reused afterwards.
    ///
    /// An async mutex rather than a spin lock: opening the channel is a network
    /// round trip, and holding a blocking lock across it would stall the whole
    /// single-threaded worker runtime. Cleared whenever the channel is found to
    /// be unusable, so the next request opens a fresh one.
    session: Mutex<Option<Arc<SftpSession>>>,
}

impl<H: client::Handler> Service<H> {
    /// Answers one request on the channel it carries.
    ///
    /// A dropped receiver is not an error: the caller simply lost interest, and
    /// the work is already done by the time the send fails.
    async fn dispatch(&self, request: SftpRequest) {
        match request {
            SftpRequest::Home(reply) => {
                let _ = reply.send(self.realpath(".").await);
            }
            SftpRequest::RealPath { path, reply } => {
                let _ = reply.send(self.realpath(&path).await);
            }
            SftpRequest::ReadDir { path, reply } => {
                let _ = reply.send(self.read_dir(&path).await);
            }
            SftpRequest::MkDir { path, reply } => {
                let _ = reply.send(self.mkdir(&path).await);
            }
            SftpRequest::RemoveFile { path, reply } => {
                let _ = reply.send(self.remove_file(&path).await);
            }
            SftpRequest::RemoveDir { path, reply } => {
                let _ = reply.send(self.remove_dir(&path).await);
            }
            SftpRequest::Rename { old, new, reply } => {
                let _ = reply.send(self.rename(&old, &new).await);
            }
            SftpRequest::Upload {
                local,
                remote_dir,
                progress,
                reply,
            } => {
                let outcome = self.upload(&local, &remote_dir, progress.as_ref()).await;
                // Dropped before the answer goes out, so a caller watching both
                // sees the progress stream end first and never has to decide
                // which of the two arriving in the other order means "done".
                drop(progress);
                let _ = reply.send(outcome);
            }
            SftpRequest::Download {
                remote_path,
                local,
                progress,
                reply,
            } => {
                let outcome = self.download(&remote_path, &local, progress.as_ref()).await;
                drop(progress);
                let _ = reply.send(outcome);
            }
        }
    }

    /// Returns the SFTP channel, opening it on first use.
    ///
    /// The lock is held across the open so two concurrent first requests share
    /// one channel instead of racing two into existence.
    async fn session(&self) -> Result<Arc<SftpSession>, SftpError> {
        let mut slot = self.session.lock().await;
        if let Some(existing) = slot.as_ref() {
            return Ok(Arc::clone(existing));
        }
        if self.handle.is_closed() {
            return Err(SftpError::Disconnected);
        }

        let session = Arc::new(self.open().await?);
        *slot = Some(Arc::clone(&session));
        log::debug!("sftp channel opened");
        Ok(session)
    }

    /// Opens a channel, starts the `sftp` subsystem on it and shakes hands.
    ///
    /// The subsystem request is made with `want_reply` and its answer is waited
    /// for. Skipping that wait would be legal but unhelpful: a server without
    /// SFTP answers `failure`, the stream reader silently discards it, and the
    /// caller would be left waiting out the protocol timeout instead of being
    /// told what happened.
    async fn open(&self) -> Result<SftpSession, SftpError> {
        let mut channel = self.handle.channel_open_session().await.map_err(|error| {
            SftpError::Subsystem(format!("could not open an SFTP channel: {error}"))
        })?;

        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|error| {
                SftpError::Subsystem(format!("could not request the SFTP subsystem: {error}"))
            })?;

        loop {
            match channel.wait().await {
                Some(russh::ChannelMsg::Success) => break,
                Some(russh::ChannelMsg::Failure) => {
                    return Err(SftpError::Subsystem(
                        "the server does not offer the SFTP subsystem".to_owned(),
                    ));
                }
                Some(russh::ChannelMsg::Eof | russh::ChannelMsg::Close) | None => {
                    return Err(SftpError::Subsystem(
                        "the server closed the channel before starting the SFTP subsystem"
                            .to_owned(),
                    ));
                }
                Some(other) => log::trace!("ignoring {other:?} while starting the SFTP subsystem"),
            }
        }

        SftpSession::new(channel.into_stream())
            .await
            .map_err(|error| SftpError::Subsystem(format!("the SFTP handshake failed: {error}")))
    }

    /// Forgets the cached channel so the next request opens a new one.
    ///
    /// Called after any failure that is not a plain refusal by the server: once
    /// the framing is out of step there is no way to resynchronise a channel,
    /// and every later request on it would fail the same way.
    async fn invalidate(&self) {
        *self.session.lock().await = None;
    }

    /// Turns a protocol result into an [`SftpError`], prefixed with `context`.
    ///
    /// `context` describes the attempt ("could not list /etc"), the protocol
    /// error explains the refusal, and the two are joined into the sentence the
    /// UI shows.
    async fn remote<T>(
        &self,
        result: Result<T, ProtocolError>,
        context: &str,
    ) -> Result<T, SftpError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                // A status packet is the server answering "no"; anything else
                // means the conversation itself broke down.
                if !matches!(error, ProtocolError::Status(_)) {
                    self.invalidate().await;
                }
                Err(SftpError::Remote(format!("{context}: {error}")))
            }
        }
    }

    /// Turns a stream I/O result on a remote file into an [`SftpError`].
    ///
    /// Reads and writes on an open remote file surface as `io::Error` rather
    /// than a protocol error, but they break the channel just as thoroughly.
    async fn transfer<T>(
        &self,
        result: Result<T, std::io::Error>,
        context: &str,
    ) -> Result<T, SftpError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                self.invalidate().await;
                Err(SftpError::Remote(format!("{context}: {error}")))
            }
        }
    }

    /// Canonicalises `path` on the server.
    async fn realpath(&self, path: &str) -> Result<String, SftpError> {
        let session = self.session().await?;
        self.remote(
            session.canonicalize(path).await,
            &format!("could not resolve the remote path {path}"),
        )
        .await
    }

    /// Lists `path`, resolving symlink targets where the server allows it.
    async fn read_dir(&self, path: &str) -> Result<Vec<RemoteEntry>, SftpError> {
        let session = self.session().await?;
        let listing = self
            .remote(
                session.read_dir(path).await,
                &format!("could not list the remote directory {path}"),
            )
            .await?;

        let mut entries = Vec::new();
        for entry in listing {
            let metadata = entry.metadata();
            let is_symlink = metadata.is_symlink();
            let mut is_dir = metadata.is_dir();
            let mut size = metadata.size.unwrap_or(0);

            // Listings report the link itself, which would make every symlink
            // look like an unopenable file. A broken or unreadable target is
            // not worth failing the whole listing over, so the link's own
            // attributes stand in when the follow-up stat fails.
            if is_symlink {
                match session.metadata(entry.path()).await {
                    Ok(target) => {
                        is_dir = target.is_dir();
                        size = target.size.unwrap_or(size);
                    }
                    Err(error) => {
                        log::debug!("could not resolve the symlink {}: {error}", entry.path());
                    }
                }
            }

            entries.push(RemoteEntry {
                name: entry.file_name(),
                is_dir,
                is_symlink,
                size,
            });
        }
        Ok(entries)
    }

    /// Creates `path`, treating "it is already a directory" as success.
    ///
    /// Servers disagree on how they refuse an existing name — `Failure`,
    /// `PermissionDenied` and `NoSuchFile` have all been seen — so the status
    /// code is not worth inspecting. One stat settles it, and only on the
    /// failure path, so the common case still costs a single round trip.
    async fn mkdir(&self, path: &str) -> Result<(), SftpError> {
        let session = self.session().await?;
        let outcome = session.create_dir(path).await;
        if outcome.is_ok() {
            return Ok(());
        }
        if session
            .metadata(path)
            .await
            .is_ok_and(|metadata| metadata.is_dir())
        {
            return Ok(());
        }
        self.remote(
            outcome,
            &format!("could not create the remote directory {path}"),
        )
        .await
    }

    /// Deletes the file — or the link — at `path`.
    async fn remove_file(&self, path: &str) -> Result<(), SftpError> {
        let session = self.session().await?;
        self.remote(
            session.remove_file(path).await,
            &format!("could not delete the remote file {path}"),
        )
        .await
    }

    /// Deletes the empty directory at `path`.
    async fn remove_dir(&self, path: &str) -> Result<(), SftpError> {
        let session = self.session().await?;
        self.remote(
            session.remove_dir(path).await,
            &format!("could not delete the remote directory {path}"),
        )
        .await
    }

    /// Renames `old` to `new`, passing the server's verdict straight through.
    async fn rename(&self, old: &str, new: &str) -> Result<(), SftpError> {
        let session = self.session().await?;
        self.remote(
            session.rename(old, new).await,
            &format!("could not rename {old} to {new}"),
        )
        .await
    }

    /// Streams `local` into `remote_dir`, returning the remote path written.
    async fn upload(
        &self,
        local: &Path,
        remote_dir: &str,
        progress: Option<&UnboundedSender<u64>>,
    ) -> Result<String, SftpError> {
        let name = local
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                SftpError::Path(format!(
                    "{} has no file name that can be used on the remote host",
                    local.display()
                ))
            })?;
        let remote_path = join(remote_dir, name);

        // Opened before the remote file is created so that an unreadable local
        // file leaves no empty file behind on the server.
        let mut source = tokio::fs::File::open(local).await.map_err(|error| {
            SftpError::Local(format!("could not open {}: {error}", local.display()))
        })?;

        let session = self.session().await?;
        let mut target = self
            .remote(
                session.create(remote_path.as_str()).await,
                &format!("could not create the remote file {remote_path}"),
            )
            .await?;

        let mut buffer = vec![0u8; TRANSFER_CHUNK];
        let mut moved = 0u64;
        loop {
            let read = source.read(&mut buffer).await.map_err(|error| {
                SftpError::Local(format!("could not read {}: {error}", local.display()))
            })?;
            if read == 0 {
                break;
            }
            #[allow(clippy::indexing_slicing)]
            self.transfer(
                target.write_all(&buffer[..read]).await,
                &format!("could not write to the remote file {remote_path}"),
            )
            .await?;
            moved = moved.saturating_add(read as u64);
            report(progress, moved);
        }

        // Writes are pipelined, so only the shutdown proves they all landed —
        // and it is also what closes the remote handle.
        self.transfer(
            target.shutdown().await,
            &format!("could not finish writing the remote file {remote_path}"),
        )
        .await?;

        Ok(remote_path)
    }

    /// Streams the remote file at `remote_path` into the local path `local`.
    async fn download(
        &self,
        remote_path: &str,
        local: &Path,
        progress: Option<&UnboundedSender<u64>>,
    ) -> Result<(), SftpError> {
        let session = self.session().await?;
        let mut source = self
            .remote(
                session.open(remote_path).await,
                &format!("could not open the remote file {remote_path}"),
            )
            .await?;

        let mut target = tokio::fs::File::create(local).await.map_err(|error| {
            SftpError::Local(format!("could not create {}: {error}", local.display()))
        })?;

        let mut buffer = vec![0u8; TRANSFER_CHUNK];
        let mut moved = 0u64;
        loop {
            let read = self
                .transfer(
                    source.read(&mut buffer).await,
                    &format!("could not read the remote file {remote_path}"),
                )
                .await?;
            if read == 0 {
                break;
            }
            #[allow(clippy::indexing_slicing)]
            target.write_all(&buffer[..read]).await.map_err(|error| {
                SftpError::Local(format!("could not write {}: {error}", local.display()))
            })?;
            moved = moved.saturating_add(read as u64);
            report(progress, moved);
        }

        target.flush().await.map_err(|error| {
            SftpError::Local(format!(
                "could not finish writing {}: {error}",
                local.display()
            ))
        })?;
        // The remote handle is closed explicitly; dropping it would leak it
        // until the channel goes away, and a file panel opens many of them.
        let _ = source.shutdown().await;
        Ok(())
    }
}

/// Announces `moved` bytes on `progress`, if anyone is still listening.
///
/// A closed channel means the UI stopped watching — the panel was closed, the
/// session went away — and never that the transfer should stop, so the send is
/// deliberately unchecked.
fn report(progress: Option<&UnboundedSender<u64>>, moved: u64) {
    if let Some(progress) = progress {
        let _ = progress.unbounded_send(moved);
    }
}

/// Joins a remote directory and a file name with the protocol's separator.
///
/// SFTP paths are always POSIX-style on the wire, whatever the server's native
/// convention is, so this never consults [`std::path`] — doing so would produce
/// backslashes when rulogman itself runs on Windows.
fn join(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else if directory.ends_with('/') {
        format!("{directory}{name}")
    } else {
        format!("{directory}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joining_adds_exactly_one_separator() {
        assert_eq!(join("/home/alice", "notes.txt"), "/home/alice/notes.txt");
        assert_eq!(join("/home/alice/", "notes.txt"), "/home/alice/notes.txt");
        assert_eq!(join("/", "notes.txt"), "/notes.txt");
        assert_eq!(join("", "notes.txt"), "notes.txt");
    }

    #[test]
    fn a_client_without_a_session_reports_a_disconnect() {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        let client = SftpClient::new(sender);
        drop(receiver);

        let error = futures::executor::block_on(client.home());
        assert_eq!(error, Err(SftpError::Disconnected));
    }

    #[test]
    fn errors_render_as_whole_sentences() {
        assert_eq!(
            SftpError::Disconnected.to_string(),
            "the SSH session is no longer connected"
        );
        assert_eq!(
            SftpError::Remote("could not list /etc: permission denied".to_owned()).to_string(),
            "could not list /etc: permission denied"
        );
    }
}
