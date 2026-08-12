//! End-to-end tests for the SSH transport.
//!
//! Unlike `session.rs`, which only exercises paths that fail before a
//! handshake can happen, every test here stands up a real SSH server *inside
//! the test process* using russh's server side, and drives the production
//! [`SshSession`] client against it. That makes the success paths —
//! authentication, pty allocation, shell start-up, data round-trips, window
//! resizing and teardown — observable for the first time.
//!
//! Design notes:
//!
//! * Each test owns its own [`TestServer`]: a fresh host key, a fresh ephemeral
//!   port (bound as `127.0.0.1:0`) and a fresh Tokio runtime. Nothing is shared
//!   between tests, so the default parallel `cargo test` run is safe.
//! * Nothing here sleeps to synchronise. Progress is observed by waiting for
//!   events, and every wait is bounded by [`EVENT_TIMEOUT`]; on expiry the
//!   panic message lists every event seen so far.
//! * The client runs on its own thread with its own runtime (see
//!   `SshSession::connect`), so the test thread only ever blocks on the
//!   *server's* runtime. The two never nest.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use parking_lot::Mutex;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Handler as ServerHandler, Msg, Session};
use russh::{Channel, ChannelId, Pty};
use russh_sftp::protocol::{
    Attrs, Data, File as SftpFile, FileAttributes, Handle as SftpFileHandle, Name, OpenFlags,
    Status, StatusCode, Version,
};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use rulogman_ssh::{
    AcceptAllVerifier, HostKeyVerifier, RejectAllVerifier, SftpError, SshAuth, SshConfig,
    SshErrorKind, SshEvent, SshSession, fingerprint,
};

/// Upper bound on any single wait for an expected event or server observation.
///
/// Generous enough to survive a loaded CI machine, small enough that a genuine
/// hang fails the run instead of wedging it.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Line the shell answers with the current window size instead of echoing.
const SIZE_COMMAND: &[u8] = b"SIZE\n";

/// Line the shell answers with the pty's `TERM` instead of echoing.
const TERM_COMMAND: &[u8] = b"TERM\n";

/// Line the shell answers on the extended (stderr) stream instead of echoing.
const STDERR_COMMAND: &[u8] = b"STDERR\n";

/// Line that makes the shell report [`EXIT_STATUS`] and shut itself down.
const EXIT_COMMAND: &[u8] = b"EXIT\n";

/// Exit status reported in answer to [`EXIT_COMMAND`].
const EXIT_STATUS: u32 = 7;

// ---------------------------------------------------------------------------
// Test server
// ---------------------------------------------------------------------------

/// Which credentials the test server accepts.
enum AuthPolicy {
    /// Accept exactly this user/password pair, reject everything else.
    Password {
        /// The only user name that may log in.
        user: String,
        /// The only password that is accepted for `user`.
        password: String,
    },
    /// Accept exactly this user/public key pair, reject everything else.
    PublicKey {
        /// The only user name that may log in.
        user: String,
        /// The only public key that is accepted for `user`.
        key: PublicKey,
    },
}

/// What the fake shell does with one complete line of input.
enum Reply {
    /// Write these bytes back on the standard output stream.
    Stdout(Vec<u8>),
    /// Write these bytes back on the extended (stderr) stream.
    Stderr(Vec<u8>),
    /// Report this exit status, then end the shell.
    Exit(u32),
}

/// Everything one test server records or needs while serving a connection.
struct ServerState {
    /// The credential policy this server enforces.
    auth: AuthPolicy,
    /// When set, `pty-req` requests are answered with a failure.
    refuse_pty: bool,
    /// When set, `shell` requests are answered with a failure.
    refuse_shell: bool,
    /// Directory served over the `sftp` subsystem, or `None` on a server that
    /// offers no subsystems at all.
    sftp_root: Option<PathBuf>,
    /// `TERM` from the most recent `pty-req`, if one arrived.
    term: Mutex<Option<String>>,
    /// Window size, seeded by `pty-req` and updated by `window-change`.
    size: Mutex<Option<(u32, u32)>>,
    /// Number of `shell` requests seen.
    shell_requests: AtomicUsize,
    /// Number of connections accepted so far.
    accepted: AtomicUsize,
    /// Number of connections whose session task has finished. Published through
    /// a watch channel so a test can await a close without polling.
    closed: watch::Sender<usize>,
}

impl ServerState {
    /// Produces the shell's answer to one complete input line.
    ///
    /// `SIZE` and `TERM` are answered from the recorded pty state, `STDERR` and
    /// `EXIT` exercise the remaining event variants, and everything else is
    /// echoed back verbatim, which is what the round-trip tests rely on.
    fn respond(&self, line: &[u8]) -> Reply {
        if line == SIZE_COMMAND {
            let (cols, rows) = self.size.lock().unwrap_or((0, 0));
            return Reply::Stdout(format!("{cols}x{rows}\n").into_bytes());
        }
        if line == TERM_COMMAND {
            let term = self.term.lock().clone().unwrap_or_default();
            return Reply::Stdout(format!("{term}\n").into_bytes());
        }
        if line == STDERR_COMMAND {
            return Reply::Stderr(b"on stderr\n".to_vec());
        }
        if line == EXIT_COMMAND {
            return Reply::Exit(EXIT_STATUS);
        }
        Reply::Stdout(line.to_vec())
    }
}

/// One connection's server-side handler.
struct TestHandler {
    /// Shared recording/policy state of the owning [`TestServer`].
    state: Arc<ServerState>,
    /// Bytes received on the shell channel that do not yet form a whole line.
    pending: Vec<u8>,
    /// Open channels kept until it is clear what they are for.
    ///
    /// Only populated on a server that offers SFTP: russh delivers every
    /// channel message to a retained [`Channel`] *and* to this handler, so a
    /// channel nobody reads would eventually block the session. A `shell`
    /// request drops the entry again; a `subsystem` request takes it over.
    channels: HashMap<ChannelId, Channel<Msg>>,
    /// Channels handed to the SFTP subsystem. Their traffic belongs to
    /// [`russh_sftp`] and must not reach the fake shell.
    sftp: HashSet<ChannelId>,
}

impl ServerHandler for TestHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        match &self.state.auth {
            AuthPolicy::Password {
                user: expected_user,
                password: expected_password,
            } if user == expected_user && password == expected_password => Ok(Auth::Accept),
            _ => Ok(Auth::reject()),
        }
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        match &self.state.auth {
            AuthPolicy::PublicKey {
                user: expected_user,
                key: expected_key,
            } if user == expected_user && key.key_data() == expected_key.key_data() => {
                Ok(Auth::Accept)
            }
            _ => Ok(Auth::reject()),
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.state.sftp_root.is_some() {
            self.channels.insert(channel.id(), channel);
        }
        reply.accept().await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.state.refuse_pty {
            session.channel_failure(channel)?;
            return Ok(());
        }
        *self.state.term.lock() = Some(term.to_owned());
        *self.state.size.lock() = Some((col_width, row_height));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // The fake shell answers out of `data` below, so the retained channel
        // is no longer needed — and keeping an unread one would stall.
        self.channels.remove(&channel);
        self.state.shell_requests.fetch_add(1, Ordering::SeqCst);
        if self.state.refuse_shell {
            session.channel_failure(channel)?;
        } else {
            session.channel_success(channel)?;
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        *self.state.size.lock() = Some((col_width, row_height));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let accepted = self.state.sftp_root.clone().filter(|_| name == "sftp");
        let Some(root) = accepted else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        let Some(stream) = self.channels.remove(&channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };

        session.channel_success(channel)?;
        self.sftp.insert(channel);
        // Returns as soon as the serving task is spawned, so the session loop
        // stays free to carry the shell channel alongside it.
        russh_sftp::server::run(stream.into_stream(), SftpTestHandler::new(root)).await;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // SFTP traffic is framed binary, not shell lines; it is already being
        // read by the subsystem task through the channel itself.
        if self.sftp.contains(&channel) {
            return Ok(());
        }
        self.pending.extend_from_slice(data);

        // Only whole lines are answered, which keeps the wire protocol
        // deterministic no matter how the client's writes are packetised.
        let mut replies = Vec::new();
        while let Some(index) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=index).collect();
            replies.push(self.state.respond(&line));
        }
        for reply in replies {
            match reply {
                Reply::Stdout(bytes) => session.data(channel, bytes)?,
                Reply::Stderr(bytes) => session.extended_data(channel, 1, bytes)?,
                Reply::Exit(status) => {
                    session.exit_status_request(channel, status)?;
                    session.eof(channel)?;
                    session.close(channel)?;
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SFTP subsystem
// ---------------------------------------------------------------------------

/// A minimal SFTP server backed by a real directory on disk.
///
/// Only the requests the client under test actually makes are implemented;
/// everything else falls through to `unimplemented` and is answered with
/// `OP_UNSUPPORTED`, which is exactly how a restricted real server behaves.
///
/// Paths on the wire are POSIX and rooted at `/`, which maps to [`root`]. They
/// are normalised lexically — `..` pops a component and never escapes the root
/// — so a test can assert on the canonical form the client gets back.
///
/// [`root`]: SftpTestHandler::root
struct SftpTestHandler {
    /// Directory that stands in for the remote file system's root.
    root: PathBuf,
    /// Snapshots taken by `opendir`, drained by the first `readdir`.
    dirs: HashMap<String, Vec<SftpFile>>,
    /// Files opened by `open`, keyed by the handle handed to the client.
    files: HashMap<String, std::fs::File>,
    /// Source of unique handle strings.
    next_handle: u64,
}

impl SftpTestHandler {
    /// Serves `root` as the remote file system.
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            dirs: HashMap::new(),
            files: HashMap::new(),
            next_handle: 0,
        }
    }

    /// Mints a handle that cannot collide with an earlier one.
    fn handle(&mut self, prefix: &str) -> String {
        self.next_handle += 1;
        format!("{prefix}{}", self.next_handle)
    }

    /// Maps a wire path onto the served directory.
    fn local(&self, path: &str) -> PathBuf {
        let normalized = normalize(path);
        let relative = normalized.trim_start_matches('/');
        if relative.is_empty() {
            self.root.clone()
        } else {
            self.root.join(relative)
        }
    }
}

/// Reduces a POSIX path to its canonical absolute form, without touching disk.
///
/// `.` and empty components are dropped, `..` pops — including at the root,
/// where it is simply a no-op, so no client can walk out of the served tree.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The success answer to a request that reports only a status.
fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_owned(),
        language_tag: "en-US".to_owned(),
    }
}

/// Translates a local I/O failure into the status code SFTP defines for it.
fn to_status(error: std::io::Error) -> StatusCode {
    match error.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

impl russh_sftp::server::Handler for SftpTestHandler {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        // No extensions: the client must work against a plain SFTP 3 server,
        // which is the lowest common denominator it will meet in the wild.
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![SftpFile::dummy(normalize(&path))],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<SftpFileHandle, Self::Error> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(self.local(&path)).map_err(to_status)? {
            let entry = entry.map_err(to_status)?;
            // `symlink_metadata`, not `metadata`: a listing describes the link
            // itself, which is precisely what makes the client resolve targets.
            let metadata = std::fs::symlink_metadata(entry.path()).map_err(to_status)?;
            files.push(SftpFile::new(
                entry.file_name().to_string_lossy().into_owned(),
                FileAttributes::from(&metadata),
            ));
        }

        let handle = self.handle("dir-");
        self.dirs.insert(handle.clone(), files);
        Ok(SftpFileHandle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.dirs.get_mut(&handle) {
            // The whole snapshot goes out in one packet; the second call ends
            // the listing, which is the sequence the client expects.
            Some(files) if !files.is_empty() => Ok(Name {
                id,
                files: std::mem::take(files),
            }),
            Some(_) => Err(StatusCode::Eof),
            None => Err(StatusCode::Failure),
        }
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<SftpFileHandle, Self::Error> {
        let options: std::fs::OpenOptions = pflags.into();
        let file = options.open(self.local(&filename)).map_err(to_status)?;

        let handle = self.handle("file-");
        self.files.insert(handle.clone(), file);
        Ok(SftpFileHandle { id, handle })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let file = self.files.get_mut(&handle).ok_or(StatusCode::Failure)?;
        file.seek(SeekFrom::Start(offset)).map_err(to_status)?;

        let mut data = vec![0u8; len as usize];
        let mut filled = 0;
        while filled < data.len() {
            let read = file.read(&mut data[filled..]).map_err(to_status)?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(filled);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let file = self.files.get_mut(&handle).ok_or(StatusCode::Failure)?;
        file.seek(SeekFrom::Start(offset)).map_err(to_status)?;
        file.write_all(&data).map_err(to_status)?;
        Ok(ok_status(id))
    }

    /// Creates a directory, and refuses an existing one exactly as a real
    /// server does — which is what makes the client's own tolerance testable.
    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        std::fs::create_dir(self.local(&path)).map_err(to_status)?;
        Ok(ok_status(id))
    }

    /// Deletes a file or a symbolic link, never what a link points at —
    /// `remove_file` is the local call that describes the link itself.
    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        std::fs::remove_file(self.local(&filename)).map_err(to_status)?;
        Ok(ok_status(id))
    }

    /// Deletes a directory, and refuses a non-empty one exactly as a real
    /// server does — which is what makes the client's bottom-up walk testable.
    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        std::fs::remove_dir(self.local(&path)).map_err(to_status)?;
        Ok(ok_status(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        std::fs::rename(self.local(&oldpath), self.local(&newpath)).map_err(to_status)?;
        Ok(ok_status(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.dirs.remove(&handle);
        self.files.remove(&handle);
        Ok(ok_status(id))
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let metadata = std::fs::metadata(self.local(&path)).map_err(to_status)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let metadata = std::fs::symlink_metadata(self.local(&path)).map_err(to_status)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let file = self.files.get(&handle).ok_or(StatusCode::Failure)?;
        let metadata = file.metadata().map_err(to_status)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }
}

/// An SSH server running inside the test process, plus the runtime driving it.
struct TestServer {
    /// Runtime the listener, the sessions and the test's own waits all use.
    runtime: Arc<Runtime>,
    /// Accept loop, aborted on drop so no listener outlives its test.
    accept_task: JoinHandle<()>,
    /// Ephemeral port the listener actually got.
    port: u16,
    /// Public half of this server's freshly generated host key.
    host_key: PublicKey,
    /// Recording/policy state shared with every connection handler.
    state: Arc<ServerState>,
}

impl TestServer {
    /// Starts a server on a fresh ephemeral port with a fresh host key.
    ///
    /// `refuse_pty` and `refuse_shell` answer the corresponding channel
    /// request with a failure, which is how the two "the session must not
    /// become ready" tests get a request they can watch be turned down.
    /// `sftp_root`, when given, makes the server offer the `sftp` subsystem
    /// over that directory; without it no subsystem is accepted at all.
    fn start(
        auth: AuthPolicy,
        refuse_pty: bool,
        refuse_shell: bool,
        sftp_root: Option<PathBuf>,
    ) -> Self {
        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .expect("generating a host key must succeed");
        let public_host_key = host_key.public_key().clone();

        let config = russh::server::Config {
            // Rejections are not rate-limited here: the tests assert on the
            // rejection itself, and a constant-time delay would only make the
            // suite slower.
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![host_key],
            inactivity_timeout: Some(Duration::from_secs(60)),
            nodelay: true,
            ..russh::server::Config::default()
        };

        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("building the server runtime must succeed"),
        );

        let (closed, _) = watch::channel(0usize);
        let state = Arc::new(ServerState {
            auth,
            refuse_pty,
            refuse_shell,
            sftp_root,
            term: Mutex::new(None),
            size: Mutex::new(None),
            shell_requests: AtomicUsize::new(0),
            accepted: AtomicUsize::new(0),
            closed,
        });

        // Bound before the accept loop starts so the port is known by the time
        // `start` returns; no test ever races a not-yet-listening socket.
        let listener = runtime
            .block_on(TcpListener::bind(("127.0.0.1", 0)))
            .expect("binding an ephemeral port must succeed");
        let port = listener
            .local_addr()
            .expect("the listener must have an address")
            .port();

        let accept_task =
            runtime.spawn(accept_loop(listener, Arc::new(config), Arc::clone(&state)));

        Self {
            runtime,
            accept_task,
            port,
            host_key: public_host_key,
            state,
        }
    }

    /// A server that accepts exactly `user`/`password`.
    fn with_password(user: &str, password: &str) -> Self {
        Self::start(
            AuthPolicy::Password {
                user: user.to_owned(),
                password: password.to_owned(),
            },
            false,
            false,
            None,
        )
    }

    /// As [`TestServer::with_password`], but every `shell` request is refused.
    fn refusing_shells(user: &str, password: &str) -> Self {
        Self::start(
            AuthPolicy::Password {
                user: user.to_owned(),
                password: password.to_owned(),
            },
            false,
            true,
            None,
        )
    }

    /// As [`TestServer::with_password`], but every `pty-req` is refused.
    fn refusing_ptys(user: &str, password: &str) -> Self {
        Self::start(
            AuthPolicy::Password {
                user: user.to_owned(),
                password: password.to_owned(),
            },
            true,
            false,
            None,
        )
    }

    /// A server that accepts exactly `user` presenting `key`.
    fn with_public_key(user: &str, key: PublicKey) -> Self {
        Self::start(
            AuthPolicy::PublicKey {
                user: user.to_owned(),
                key,
            },
            false,
            false,
            None,
        )
    }

    /// As [`TestServer::with_password`], and additionally serves `root` over
    /// the `sftp` subsystem.
    fn with_sftp(user: &str, password: &str, root: &Path) -> Self {
        Self::start(
            AuthPolicy::Password {
                user: user.to_owned(),
                password: password.to_owned(),
            },
            false,
            false,
            Some(root.to_path_buf()),
        )
    }

    /// Client settings pointing at this server, with timeouts tightened so a
    /// mistake surfaces as a failure rather than a stall.
    fn config(&self, username: &str, auth: SshAuth) -> SshConfig {
        let mut config = SshConfig::new("127.0.0.1", self.port, username, auth);
        config.connect_timeout_secs = 5;
        config
    }

    /// Connects a real [`SshSession`] to this server.
    fn connect(
        &self,
        config: SshConfig,
        verifier: Arc<dyn HostKeyVerifier>,
    ) -> (SshSession, Events) {
        let (session, receiver) = SshSession::connect(config, verifier);
        (session, self.events(receiver))
    }

    /// Wraps an event receiver so it can be awaited with a deadline.
    fn events(&self, receiver: UnboundedReceiver<SshEvent>) -> Events {
        Events {
            receiver,
            seen: Vec::new(),
            runtime: Arc::clone(&self.runtime),
        }
    }

    /// The public host key this server presents.
    fn host_key(&self) -> &PublicKey {
        &self.host_key
    }

    /// `TERM` recorded from the last `pty-req`, if any.
    fn recorded_term(&self) -> Option<String> {
        self.state.term.lock().clone()
    }

    /// Window size recorded from the last `pty-req` or `window-change`.
    fn recorded_size(&self) -> Option<(u32, u32)> {
        *self.state.size.lock()
    }

    /// How many `shell` requests this server has served.
    fn shell_requests(&self) -> usize {
        self.state.shell_requests.load(Ordering::SeqCst)
    }

    /// How many connections this server has accepted.
    fn accepted_connections(&self) -> usize {
        self.state.accepted.load(Ordering::SeqCst)
    }

    /// Drives `future` to completion on the server's runtime, bounded by
    /// [`EVENT_TIMEOUT`].
    ///
    /// Used for SFTP calls, whose futures resolve on the *client's* worker
    /// thread; this side only ever waits for the answer to come back, so a
    /// hung request fails the test instead of wedging it.
    fn run<F: Future>(&self, future: F) -> F::Output {
        self.runtime
            .block_on(async move { tokio::time::timeout(EVENT_TIMEOUT, future).await })
            .unwrap_or_else(|_| panic!("the request did not finish within {EVENT_TIMEOUT:?}"))
    }

    /// Blocks the test thread for `duration` on the server's runtime.
    ///
    /// Only used where the behaviour under test *is* a timer; everything else
    /// synchronises on events.
    fn wait(&self, duration: Duration) {
        // The timer has to be *created* inside the runtime, not just awaited
        // there, or it finds no reactor to register with.
        self.runtime
            .block_on(async move { tokio::time::sleep(duration).await });
    }

    /// Blocks until at least `count` connections have been torn down
    /// server-side, or panics once [`EVENT_TIMEOUT`] elapses.
    fn wait_for_closed_connections(&self, count: usize) {
        let mut closed = self.state.closed.subscribe();
        let waited = self.runtime.block_on(async {
            tokio::time::timeout(EVENT_TIMEOUT, async {
                loop {
                    if *closed.borrow_and_update() >= count {
                        return;
                    }
                    if closed.changed().await.is_err() {
                        return;
                    }
                }
            })
            .await
        });
        assert!(
            waited.is_ok(),
            "the server never saw {count} connection(s) close (saw {})",
            *self.state.closed.borrow()
        );
    }
}

impl Drop for TestServer {
    /// Stops accepting immediately; dropping the runtime afterwards discards
    /// any session task still in flight, so no thread or port leaks into the
    /// next test.
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// Serves connections until the task is aborted.
async fn accept_loop(
    listener: TcpListener,
    config: Arc<russh::server::Config>,
    state: Arc<ServerState>,
) {
    loop {
        let Ok((stream, _peer)) = listener.accept().await else {
            return;
        };
        state.accepted.fetch_add(1, Ordering::SeqCst);

        let config = Arc::clone(&config);
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let handler = TestHandler {
                state: Arc::clone(&state),
                pending: Vec::new(),
                channels: HashMap::new(),
                sftp: HashSet::new(),
            };
            if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
                let _ = session.await;
            }
            // Reached once the transport is gone, which is exactly what the
            // teardown tests want to observe.
            state.closed.send_modify(|count| *count += 1);
        });
    }
}

// ---------------------------------------------------------------------------
// Event stream helpers
// ---------------------------------------------------------------------------

/// A session's event receiver plus a log of everything already taken from it.
///
/// Every wait is bounded, and every failure message includes the events seen so
/// far so a broken expectation can be diagnosed from the test output alone.
struct Events {
    /// The receiver handed out by `SshSession::connect`.
    receiver: UnboundedReceiver<SshEvent>,
    /// Every event pulled off `receiver`, in arrival order.
    seen: Vec<SshEvent>,
    /// Runtime used purely to drive the timeout; the client has its own.
    runtime: Arc<Runtime>,
}

impl Events {
    /// Takes the next event, or panics on timeout or end of stream.
    fn next_before(&mut self, deadline: Instant) -> SshEvent {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let receiver = &mut self.receiver;
        let received = self
            .runtime
            .block_on(async { tokio::time::timeout(remaining, receiver.next()).await });

        match received {
            Ok(Some(event)) => {
                self.seen.push(event.clone());
                event
            }
            Ok(None) => panic!(
                "the event stream ended while more events were expected; events so far: {:?}",
                self.seen
            ),
            Err(_) => panic!(
                "timed out after {EVENT_TIMEOUT:?} waiting for an event; events so far: {:?}",
                self.seen
            ),
        }
    }

    /// Takes the next event with a fresh [`EVENT_TIMEOUT`] budget.
    fn next(&mut self) -> SshEvent {
        self.next_before(Instant::now() + EVENT_TIMEOUT)
    }

    /// Pulls events until `accept` matches one, and returns it.
    ///
    /// `expectation` names what was being waited for so the timeout message is
    /// self-explanatory.
    fn wait_for(&mut self, expectation: &str, accept: impl Fn(&SshEvent) -> bool) -> SshEvent {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            let event = self.next_before(deadline);
            if accept(&event) {
                return event;
            }
            assert!(
                !is_terminal(&event),
                "the session ended before {expectation} arrived; events so far: {:?}",
                self.seen
            );
        }
    }

    /// Waits for the session to become ready.
    fn wait_ready(&mut self) {
        self.wait_for("Ready", |event| matches!(event, SshEvent::Ready));
    }

    /// Waits for the session's final event, whatever it is.
    fn wait_terminal(&mut self) -> SshEvent {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            let event = self.next_before(deadline);
            if is_terminal(&event) {
                return event;
            }
        }
    }

    /// Accumulates shell output until `accept` is happy with everything read so
    /// far, then returns the accumulated bytes.
    ///
    /// Output legitimately arrives split across several [`SshEvent::Data`]
    /// events, so comparing a single event against the expected bytes would be
    /// flaky; this accumulates instead.
    fn read_until(&mut self, expectation: &str, accept: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut buffer = Vec::new();
        loop {
            if accept(&buffer) {
                return buffer;
            }
            let event = self.next_before(deadline);
            match event {
                SshEvent::Data(chunk) => buffer.extend_from_slice(&chunk),
                other => assert!(
                    !is_terminal(&other),
                    "the session ended before {expectation} was read (read {:?} so far); \
                     events so far: {:?}",
                    String::from_utf8_lossy(&buffer),
                    self.seen
                ),
            }
        }
    }

    /// Reads until the accumulated output ends with `suffix`.
    fn read_line(&mut self, suffix: &[u8]) -> Vec<u8> {
        let expectation = format!("{:?}", String::from_utf8_lossy(suffix));
        self.read_until(&expectation, |buffer| buffer.ends_with(suffix))
    }

    /// Every event taken from the stream so far.
    fn seen(&self) -> &[SshEvent] {
        &self.seen
    }
}

/// Whether `event` is one of the two events that end a session's stream.
fn is_terminal(event: &SshEvent) -> bool {
    matches!(event, SshEvent::Disconnected { .. } | SshEvent::Error(_, _))
}

/// Writes `key` to `path` in OpenSSH format, the way `ssh-keygen` would.
fn write_openssh_key(key: &PrivateKey, path: &Path) {
    let pem = key
        .to_openssh(LineEnding::LF)
        .expect("encoding a private key must succeed");
    std::fs::write(path, pem.as_bytes()).expect("writing the private key must succeed");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A correct password must take the session all the way to `Ready`, and the
/// events must arrive in the documented order.
#[test]
fn password_authentication_reaches_ready() {
    let server = TestServer::with_password("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    assert!(
        matches!(events.next(), SshEvent::Connecting),
        "the stream must open with Connecting; events: {:?}",
        events.seen()
    );

    let host_key = events.next();
    let SshEvent::HostKey { accepted, .. } = host_key else {
        panic!(
            "expected HostKey second, got {host_key:?}; events: {:?}",
            events.seen()
        );
    };
    assert!(accepted, "AcceptAllVerifier must accept the key");

    assert!(
        matches!(events.next(), SshEvent::Ready),
        "expected Ready third; events: {:?}",
        events.seen()
    );
    assert!(session.is_alive(), "the session must report itself alive");
    assert_eq!(server.accepted_connections(), 1);
}

/// A wrong password must surface as an authentication error — and the error
/// text must not carry the password that was tried.
#[test]
fn a_wrong_password_reports_an_auth_error_without_leaking_it() {
    let server = TestServer::with_password("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("swordfish-42".into()));
    let (_session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    let terminal = events.wait_terminal();
    assert!(
        matches!(terminal, SshEvent::Error(SshErrorKind::Auth, _)),
        "expected an Auth error, got {terminal:?}; events: {:?}",
        events.seen()
    );
    assert!(
        !events
            .seen()
            .iter()
            .any(|event| matches!(event, SshEvent::Ready)),
        "the session must never become ready; events: {:?}",
        events.seen()
    );

    let rendered = format!("{:?}", events.seen());
    assert!(
        !rendered.contains("swordfish-42"),
        "the password leaked into the event stream: {rendered}"
    );
}

/// A user name the server does not know must also be an authentication error.
#[test]
fn an_unknown_user_reports_an_auth_error() {
    let server = TestServer::with_password("alice", "hunter2");
    let config = server.config("mallory", SshAuth::Password("hunter2".into()));
    let (_session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    let terminal = events.wait_terminal();
    assert!(
        matches!(terminal, SshEvent::Error(SshErrorKind::Auth, _)),
        "expected an Auth error, got {terminal:?}; events: {:?}",
        events.seen()
    );
}

/// A key generated in-test, written to a temporary file and registered with the
/// server must authenticate through `SshAuth::PrivateKeyFile`.
#[test]
fn private_key_file_authentication_reaches_ready() {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .expect("generating a client key must succeed");
    let server = TestServer::with_public_key("alice", key.public_key().clone());

    let directory = tempfile::tempdir().expect("creating a temporary directory must succeed");
    let path = directory.path().join("id_ed25519");
    write_openssh_key(&key, &path);

    let config = server.config(
        "alice",
        SshAuth::PrivateKeyFile {
            path,
            passphrase: None,
        },
    );
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    events.wait_ready();
    assert!(session.is_alive());
}

/// The same key supplied as in-memory PEM must work exactly as well as one
/// read from disk — nothing should ever have to touch the filesystem.
#[test]
fn in_memory_private_key_authentication_reaches_ready() {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .expect("generating a client key must succeed");
    let server = TestServer::with_public_key("alice", key.public_key().clone());

    let pem = key
        .to_openssh(LineEnding::LF)
        .expect("encoding a private key must succeed");

    let config = server.config(
        "alice",
        SshAuth::PrivateKeyData {
            pem: pem.to_string(),
            passphrase: None,
        },
    );
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    events.wait_ready();
    assert!(session.is_alive());
}

/// The same key, encrypted at rest, must authenticate when the right
/// passphrase is supplied — and the passphrase must never reach the events.
#[test]
fn an_encrypted_private_key_authenticates_with_the_right_passphrase() {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .expect("generating a client key must succeed");
    let server = TestServer::with_public_key("alice", key.public_key().clone());

    let encrypted = key
        .encrypt(&mut rand::rng(), "correct-horse-battery")
        .expect("encrypting the client key must succeed");
    assert!(
        encrypted.is_encrypted(),
        "the key on disk must be encrypted"
    );

    let directory = tempfile::tempdir().expect("creating a temporary directory must succeed");
    let path = directory.path().join("id_ed25519");
    write_openssh_key(&encrypted, &path);

    let config = server.config(
        "alice",
        SshAuth::PrivateKeyFile {
            path,
            passphrase: Some("correct-horse-battery".into()),
        },
    );
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    events.wait_ready();
    assert!(session.is_alive());
    let rendered = format!("{:?}", events.seen());
    assert!(
        !rendered.contains("correct-horse-battery"),
        "the passphrase leaked into the event stream: {rendered}"
    );
}

/// The wrong passphrase must fail while the key is still being loaded, i.e.
/// before anything touches the network.
#[test]
fn an_encrypted_private_key_rejects_the_wrong_passphrase() {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .expect("generating a client key must succeed");
    let server = TestServer::with_public_key("alice", key.public_key().clone());

    let encrypted = key
        .encrypt(&mut rand::rng(), "correct-horse-battery")
        .expect("encrypting the client key must succeed");

    let directory = tempfile::tempdir().expect("creating a temporary directory must succeed");
    let path = directory.path().join("id_ed25519");
    write_openssh_key(&encrypted, &path);

    let config = server.config(
        "alice",
        SshAuth::PrivateKeyFile {
            path,
            passphrase: Some("not-the-passphrase".into()),
        },
    );
    let (_session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    let terminal = events.wait_terminal();
    assert!(
        matches!(terminal, SshEvent::Error(SshErrorKind::KeyLoad, _)),
        "expected a KeyLoad error, got {terminal:?}; events: {:?}",
        events.seen()
    );
    assert_eq!(
        server.accepted_connections(),
        0,
        "a key that cannot be loaded must fail before the socket is opened"
    );
    let rendered = format!("{:?}", events.seen());
    assert!(
        !rendered.contains("not-the-passphrase"),
        "the passphrase leaked into the event stream: {rendered}"
    );
}

/// A key the server does not know must be rejected as an authentication
/// failure rather than, say, a channel or I/O error.
#[test]
fn an_unregistered_private_key_reports_an_auth_error() {
    let registered = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .expect("generating the registered key must succeed");
    let stranger = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .expect("generating the stranger key must succeed");
    let server = TestServer::with_public_key("alice", registered.public_key().clone());

    let directory = tempfile::tempdir().expect("creating a temporary directory must succeed");
    let path = directory.path().join("id_ed25519");
    write_openssh_key(&stranger, &path);

    let config = server.config(
        "alice",
        SshAuth::PrivateKeyFile {
            path,
            passphrase: None,
        },
    );
    let (_session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    let terminal = events.wait_terminal();
    assert!(
        matches!(terminal, SshEvent::Error(SshErrorKind::Auth, _)),
        "expected an Auth error, got {terminal:?}; events: {:?}",
        events.seen()
    );
}

/// Bytes written with `send_input` must reach the remote shell and come back
/// on the event stream unchanged.
#[test]
fn input_round_trips_through_the_remote_shell() {
    let server = TestServer::with_password("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    events.wait_ready();
    session.send_input(b"hello\n".to_vec());
    let echoed = events.read_line(b"hello\n");
    assert_eq!(String::from_utf8_lossy(&echoed), "hello\n");

    // A second round trip proves the channel stays usable, and that output
    // split across events is handled.
    session.send_input(b"second line\n".to_vec());
    let echoed = events.read_line(b"second line\n");
    assert_eq!(String::from_utf8_lossy(&echoed), "second line\n");
}

/// Input queued before `Ready` must be buffered and flushed, not dropped.
#[test]
fn input_sent_before_ready_is_flushed_once_the_shell_starts() {
    let server = TestServer::with_password("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    // Queued immediately, i.e. long before the handshake can have finished.
    session.send_input(b"early\n".to_vec());

    events.wait_ready();
    let echoed = events.read_line(b"early\n");
    assert_eq!(String::from_utf8_lossy(&echoed), "early\n");
}

/// Output on the remote shell's stderr must arrive as `ExtendedData`, kept
/// separate from ordinary output.
#[test]
fn stderr_output_arrives_as_extended_data() {
    let server = TestServer::with_password("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    events.wait_ready();
    session.send_input(STDERR_COMMAND.to_vec());

    let event = events.wait_for("ExtendedData", |event| {
        matches!(event, SshEvent::ExtendedData(_))
    });
    let SshEvent::ExtendedData(bytes) = event else {
        unreachable!("wait_for only returns an ExtendedData event here");
    };
    assert_eq!(String::from_utf8_lossy(&bytes), "on stderr\n");
    assert!(
        !events
            .seen()
            .iter()
            .any(|event| matches!(event, SshEvent::Data(_))),
        "stderr must not be reported as ordinary output; events: {:?}",
        events.seen()
    );
}

/// A remote shell that exits must report its status and then end the session.
#[test]
fn a_remote_exit_reports_its_status_and_ends_the_session() {
    let server = TestServer::with_password("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    events.wait_ready();
    session.send_input(EXIT_COMMAND.to_vec());

    let event = events.wait_for("ExitStatus", |event| {
        matches!(event, SshEvent::ExitStatus(_))
    });
    assert!(
        matches!(event, SshEvent::ExitStatus(EXIT_STATUS)),
        "expected exit status {EXIT_STATUS}, got {event:?}"
    );

    let terminal = events.wait_terminal();
    assert!(
        matches!(terminal, SshEvent::Disconnected { .. }),
        "a clean remote exit must not be reported as an error, got {terminal:?}; events: {:?}",
        events.seen()
    );
    assert!(!session.is_alive());
}

/// A server that refuses the shell must end the session with a channel error,
/// and must never have been announced as ready.
///
/// The client waits for the `shell` reply before publishing `Ready`, so a
/// refusal arrives *instead of* `Ready`, not after it. Anything else would let
/// the UI flash "connected" for a session that never had a shell.
#[test]
fn a_refused_shell_ends_the_session_with_a_channel_error() {
    let server = TestServer::refusing_shells("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    let terminal = events.wait_terminal();
    let SshEvent::Error(SshErrorKind::Channel, message) = &terminal else {
        panic!(
            "expected a Channel error, got {terminal:?}; events: {:?}",
            events.seen()
        );
    };
    assert!(
        message.contains("shell"),
        "the error must name the shell, got {message:?}"
    );
    assert!(
        !events
            .seen()
            .iter()
            .any(|event| matches!(event, SshEvent::Ready)),
        "the session must never become ready; events: {:?}",
        events.seen()
    );
    assert!(
        !session.is_alive(),
        "a session without a shell must never report itself alive"
    );
    assert_eq!(server.shell_requests(), 1);
}

/// A server that grants the pty but is asked for one it refuses must fail the
/// same way — before `Ready`, and blaming the pty rather than the shell.
///
/// The `pty-req` is sent with `want_reply`, so the refusal is observable at
/// all; without it the client would silently settle for a session with no pty
/// and `resize` would quietly do nothing for the rest of its life.
#[test]
fn a_refused_pty_ends_the_session_before_ready() {
    let server = TestServer::refusing_ptys("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    let terminal = events.wait_terminal();
    let SshEvent::Error(SshErrorKind::Channel, message) = &terminal else {
        panic!(
            "expected a Channel error, got {terminal:?}; events: {:?}",
            events.seen()
        );
    };
    assert!(
        message.contains("pty"),
        "the error must name the pty, got {message:?}"
    );
    assert!(
        !message.contains("shell"),
        "a refused pty must not be reported as a refused shell, got {message:?}"
    );
    assert!(
        !events
            .seen()
            .iter()
            .any(|event| matches!(event, SshEvent::Ready)),
        "the session must never become ready; events: {:?}",
        events.seen()
    );
    assert!(!session.is_alive());
    assert_eq!(
        server.shell_requests(),
        0,
        "the client must not ask for a shell once the pty has been refused"
    );
}

/// The pty request must carry exactly the `TERM`, columns and rows the
/// configuration asked for.
#[test]
fn the_pty_request_carries_the_configured_term_and_size() {
    let server = TestServer::with_password("alice", "hunter2");
    let mut config = server.config("alice", SshAuth::Password("hunter2".into()));
    config.cols = 100;
    config.rows = 37;
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    events.wait_ready();

    // `pty-req`, `shell` and channel data travel over one channel in order, so
    // an answer to this command proves both requests were already handled.
    session.send_input(SIZE_COMMAND.to_vec());
    let reported = events.read_line(b"\n");
    assert_eq!(String::from_utf8_lossy(&reported), "100x37\n");

    session.send_input(TERM_COMMAND.to_vec());
    let reported = events.read_line(b"\n");
    assert_eq!(String::from_utf8_lossy(&reported), "xterm-256color\n");

    assert_eq!(server.recorded_term().as_deref(), Some("xterm-256color"));
    assert_eq!(server.recorded_size(), Some((100, 37)));
    assert_eq!(
        server.shell_requests(),
        1,
        "the client must request exactly one shell"
    );
}

/// `resize` must reach the server as a `window-change` request and update the
/// size the remote side sees.
#[test]
fn resize_is_delivered_as_a_window_change() {
    let server = TestServer::with_password("alice", "hunter2");
    let mut config = server.config("alice", SshAuth::Password("hunter2".into()));
    config.cols = 80;
    config.rows = 24;
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    events.wait_ready();
    session.send_input(SIZE_COMMAND.to_vec());
    let before = events.read_line(b"\n");
    assert_eq!(String::from_utf8_lossy(&before), "80x24\n");

    session.resize(120, 40);
    session.send_input(SIZE_COMMAND.to_vec());
    let after = events.read_line(b"\n");
    assert_eq!(String::from_utf8_lossy(&after), "120x40\n");
    assert_eq!(server.recorded_size(), Some((120, 40)));
}

/// A resize requested before the session is ready must be folded into the
/// original `pty-req` rather than lost.
#[test]
fn a_resize_before_ready_is_applied_to_the_pty_request() {
    let server = TestServer::with_password("alice", "hunter2");
    let mut config = server.config("alice", SshAuth::Password("hunter2".into()));
    config.cols = 80;
    config.rows = 24;
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    session.resize(132, 50);

    events.wait_ready();
    session.send_input(SIZE_COMMAND.to_vec());
    let reported = events.read_line(b"\n");
    assert_eq!(String::from_utf8_lossy(&reported), "132x50\n");
}

/// Keepalives must keep an idle session alive rather than quietly kill it.
///
/// This is the one test that genuinely has to wait, because the behaviour under
/// test *is* a timer: `main_loop` pings the server every `keepalive_secs` and
/// ends the session if the ping fails. The interval is turned down to one
/// second so a couple of them fire quickly, and the assertion is that the
/// session is still usable afterwards.
#[test]
fn keepalives_do_not_disturb_an_idle_session() {
    let server = TestServer::with_password("alice", "hunter2");
    let mut config = server.config("alice", SshAuth::Password("hunter2".into()));
    config.keepalive_secs = 1;
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    events.wait_ready();
    server.wait(Duration::from_millis(2_500));

    assert!(
        session.is_alive(),
        "keepalives must not end an idle session; events: {:?}",
        events.seen()
    );
    session.send_input(b"still here\n".to_vec());
    assert_eq!(
        String::from_utf8_lossy(&events.read_line(b"still here\n")),
        "still here\n"
    );
}

/// `AcceptAllVerifier` must see — and report — the server's real host key.
#[test]
fn the_reported_host_key_matches_the_server() {
    let server = TestServer::with_password("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (_session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    let event = events.wait_for("HostKey", |event| matches!(event, SshEvent::HostKey { .. }));
    let SshEvent::HostKey {
        algorithm,
        fingerprint: reported,
        accepted,
    } = event
    else {
        unreachable!("wait_for only returns a HostKey event here");
    };

    assert!(accepted);
    assert_eq!(algorithm, "ssh-ed25519");
    assert_eq!(reported, fingerprint(server.host_key()));
}

/// `RejectAllVerifier` must abort the connection and report it as a host key
/// rejection, not as a generic connect failure.
#[test]
fn a_rejected_host_key_ends_the_session() {
    let server = TestServer::with_password("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (_session, mut events) = server.connect(config, Arc::new(RejectAllVerifier));

    let host_key = events.wait_for("HostKey", |event| matches!(event, SshEvent::HostKey { .. }));
    assert!(
        matches!(
            host_key,
            SshEvent::HostKey {
                accepted: false,
                ..
            }
        ),
        "the key must be reported as rejected, got {host_key:?}"
    );

    let terminal = events.wait_terminal();
    assert!(
        matches!(terminal, SshEvent::Error(SshErrorKind::HostKeyRejected, _)),
        "expected a HostKeyRejected error, got {terminal:?}; events: {:?}",
        events.seen()
    );
    assert!(
        !events
            .seen()
            .iter()
            .any(|event| matches!(event, SshEvent::Ready)),
        "the session must never become ready; events: {:?}",
        events.seen()
    );
}

/// `disconnect` must end the session cleanly: a `Disconnected` event, an
/// `is_alive` of `false`, and a connection the server sees go away.
#[test]
fn disconnect_ends_the_session_and_the_connection() {
    let server = TestServer::with_password("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    events.wait_ready();
    assert!(session.is_alive());

    session.disconnect();
    // Calling it twice must stay harmless.
    session.disconnect();

    let terminal = events.wait_terminal();
    assert!(
        matches!(terminal, SshEvent::Disconnected { .. }),
        "expected a Disconnected event, got {terminal:?}; events: {:?}",
        events.seen()
    );
    assert!(
        !session.is_alive(),
        "the session must not report itself alive after disconnecting"
    );
    server.wait_for_closed_connections(1);
}

/// Dropping the handle must tear the connection down too, so a forgotten
/// session cannot leave a zombie connection on the server.
#[test]
fn dropping_the_session_closes_the_connection() {
    let server = TestServer::with_password("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    events.wait_ready();
    assert_eq!(server.accepted_connections(), 1);

    drop(session);

    let terminal = events.wait_terminal();
    assert!(
        matches!(terminal, SshEvent::Disconnected { .. }),
        "expected a Disconnected event, got {terminal:?}; events: {:?}",
        events.seen()
    );
    server.wait_for_closed_connections(1);
}

/// Dropping the *receiver* also winds the session down: nobody is left to read
/// the output, so the worker has no reason to keep the connection open.
///
/// Deliberately written against a **silent** shell. The worker notices a
/// publish failure only when it has something to publish, so this holds up
/// only because the session also polls for readers on a timer; without that,
/// an idle connection would survive here indefinitely.
#[test]
fn dropping_the_event_receiver_closes_the_connection() {
    let server = TestServer::with_password("alice", "hunter2");
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));

    events.wait_ready();
    drop(events);

    // Nothing is sent to the shell: the teardown has to come from the timer.
    server.wait_for_closed_connections(1);
    // Keeping the handle alive until here proves the teardown came from the
    // dropped receiver rather than from a dropped handle.
    assert!(
        !session.is_alive(),
        "the session must not report itself alive once its events go unread"
    );
    drop(session);
}

/// Two sessions against the same server must not interfere with each other.
#[test]
fn concurrent_sessions_are_independent() {
    let server = TestServer::with_password("alice", "hunter2");

    let first_config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (first, mut first_events) = server.connect(first_config, Arc::new(AcceptAllVerifier));
    let second_config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (second, mut second_events) = server.connect(second_config, Arc::new(AcceptAllVerifier));

    first_events.wait_ready();
    second_events.wait_ready();

    first.send_input(b"first\n".to_vec());
    second.send_input(b"second\n".to_vec());

    assert_eq!(
        String::from_utf8_lossy(&first_events.read_line(b"first\n")),
        "first\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&second_events.read_line(b"second\n")),
        "second\n"
    );
    assert_eq!(server.accepted_connections(), 2);
    assert_eq!(server.shell_requests(), 2);

    drop(first);
    drop(second);
    server.wait_for_closed_connections(2);
}

/// A stand-in for the future `known_hosts` verifier: the callback must be
/// handed the host, the port and the key the server actually presented.
#[test]
fn the_verifier_receives_the_host_port_and_key() {
    struct Recording {
        /// What the verifier saw, keyed by nothing in particular — one entry is
        /// enough, but a map keeps the assertion messages readable.
        seen: Mutex<HashMap<String, String>>,
    }

    #[async_trait::async_trait]
    impl HostKeyVerifier for Recording {
        async fn verify(&self, host: &str, port: u16, key: &PublicKey) -> bool {
            self.seen
                .lock()
                .insert(format!("{host}:{port}"), fingerprint(key));
            true
        }
    }

    let server = TestServer::with_password("alice", "hunter2");
    let verifier = Arc::new(Recording {
        seen: Mutex::new(HashMap::new()),
    });
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (_session, mut events) =
        server.connect(config, Arc::clone(&verifier) as Arc<dyn HostKeyVerifier>);

    events.wait_ready();

    let seen = verifier.seen.lock();
    let key = format!("127.0.0.1:{}", server.port);
    assert_eq!(
        seen.get(&key).map(String::as_str),
        Some(fingerprint(server.host_key()).as_str()),
        "the verifier must be told the real host, port and key; saw {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// SFTP
// ---------------------------------------------------------------------------

/// Builds the directory the SFTP tests serve as the remote file system.
///
/// The layout is deliberately mixed — a file, a directory, and (where the
/// platform has them) a symlink pointing at that directory — because the three
/// take different paths through the listing code.
fn remote_tree() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("creating the remote tree must succeed");
    std::fs::write(root.path().join("notes.txt"), b"remote notes\n")
        .expect("writing the remote file must succeed");
    std::fs::create_dir(root.path().join("logs")).expect("creating the remote dir must succeed");
    std::fs::write(root.path().join("logs").join("app.log"), b"line\n")
        .expect("writing the nested file must succeed");
    #[cfg(unix)]
    std::os::unix::fs::symlink("logs", root.path().join("shortcut"))
        .expect("creating the remote symlink must succeed");
    root
}

/// Connects a ready session against a server offering SFTP over `root`.
fn sftp_session(server: &TestServer) -> (SshSession, Events) {
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (session, mut events) = server.connect(config, Arc::new(AcceptAllVerifier));
    events.wait_ready();
    (session, events)
}

/// The login directory is what the server canonicalises `"."` to — and asking
/// for it must work even when the request is made before the session is ready,
/// because the file panel opens at the same moment the terminal does.
#[test]
fn sftp_reports_the_login_directory() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let config = server.config("alice", SshAuth::Password("hunter2".into()));
    let (session, _events) = server.connect(config, Arc::new(AcceptAllVerifier));

    let home = server.run(session.sftp().home());
    assert_eq!(home, Ok("/".to_owned()));
}

/// A listing must name every entry, size the files, and — the point of the
/// extra round trip — describe a symlink by what it points at.
#[test]
fn sftp_lists_a_remote_directory() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);

    let entries = server
        .run(session.sftp().read_dir("/"))
        .expect("listing the remote root must succeed");
    let by_name: HashMap<&str, &rulogman_ssh::RemoteEntry> = entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();

    let notes = by_name
        .get("notes.txt")
        .unwrap_or_else(|| panic!("notes.txt must be listed; saw {entries:?}"));
    assert!(
        !notes.is_dir,
        "a regular file must not look like a directory"
    );
    assert!(!notes.is_symlink);
    assert_eq!(notes.size, b"remote notes\n".len() as u64);

    let logs = by_name
        .get("logs")
        .unwrap_or_else(|| panic!("logs must be listed; saw {entries:?}"));
    assert!(logs.is_dir, "a directory must be reported as one");

    // `.` and `..` are the server's business, never the caller's.
    assert!(!by_name.contains_key("."), "listings must not include .");
    assert!(!by_name.contains_key(".."), "listings must not include ..");

    #[cfg(unix)]
    {
        let shortcut = by_name
            .get("shortcut")
            .unwrap_or_else(|| panic!("shortcut must be listed; saw {entries:?}"));
        assert!(
            shortcut.is_symlink,
            "the link itself must still be recognisable"
        );
        assert!(
            shortcut.is_dir,
            "a symlink to a directory must be navigable; saw {shortcut:?}"
        );
    }
}

/// Walking upwards is done by canonicalising `<current>/..` rather than by
/// slicing the path locally, so the server has to answer that form.
#[test]
fn sftp_resolves_a_parent_path() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);
    let sftp = session.sftp();

    assert_eq!(server.run(sftp.realpath("/logs/..")), Ok("/".to_owned()));
    assert_eq!(server.run(sftp.realpath("/logs/.")), Ok("/logs".to_owned()));
}

/// A download must reproduce the remote bytes exactly, including across the
/// chunk boundaries a payload larger than one transfer buffer forces.
#[test]
fn sftp_downloads_a_remote_file() {
    let root = remote_tree();
    let payload: Vec<u8> = (0..300_000u32).map(|index| (index % 251) as u8).collect();
    std::fs::write(root.path().join("payload.bin"), &payload)
        .expect("writing the payload must succeed");

    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);

    let local = tempfile::tempdir().expect("creating the local dir must succeed");
    let target = local.path().join("payload.bin");
    let outcome = server.run(
        session
            .sftp()
            .download("/payload.bin", target.clone(), None),
    );

    assert_eq!(outcome, Ok(()));
    let written = std::fs::read(&target).expect("the downloaded file must exist");
    assert_eq!(written.len(), payload.len());
    assert!(
        written == payload,
        "the downloaded bytes must match exactly"
    );
}

/// An upload keeps the local file name, lands in the requested directory, and
/// overwrites whatever was there before.
#[test]
fn sftp_uploads_a_local_file() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);

    let local = tempfile::tempdir().expect("creating the local dir must succeed");
    let source = local.path().join("report.txt");
    let payload: Vec<u8> = (0..200_000u32).map(|index| (index % 97) as u8).collect();
    std::fs::write(&source, &payload).expect("writing the local file must succeed");

    let remote = server
        .run(session.sftp().upload(source.clone(), "/logs", None))
        .expect("uploading must succeed");
    assert_eq!(remote, "/logs/report.txt");

    let landed = std::fs::read(root.path().join("logs").join("report.txt"))
        .expect("the uploaded file must exist on the server");
    assert!(landed == payload, "the uploaded bytes must match exactly");

    // A second, shorter upload must truncate rather than leave a tail behind.
    std::fs::write(&source, b"short").expect("rewriting the local file must succeed");
    let remote = server
        .run(session.sftp().upload(source, "/logs", None))
        .expect("overwriting must succeed");
    assert_eq!(remote, "/logs/report.txt");
    assert_eq!(
        std::fs::read(root.path().join("logs").join("report.txt"))
            .expect("the overwritten file must exist"),
        b"short"
    );
}

/// Creating a directory must work, and creating it *again* must not fail: the
/// recursive upload creates every directory of a tree unconditionally, so a
/// folder sent twice would otherwise break on its own root.
#[test]
fn sftp_creates_a_remote_directory_and_tolerates_one_that_exists() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);
    let sftp = session.sftp();

    assert_eq!(server.run(sftp.mkdir("/archive")), Ok(()));
    assert!(
        root.path().join("archive").is_dir(),
        "the directory must exist on the server"
    );

    assert_eq!(
        server.run(sftp.mkdir("/archive")),
        Ok(()),
        "creating an existing directory must be a no-op, not a failure"
    );
    // A *file* of that name is a genuine collision and must still be reported.
    let outcome = server.run(sftp.mkdir("/notes.txt"));
    assert!(
        matches!(outcome, Err(SftpError::Remote(_))),
        "expected a remote error over an existing file, got {outcome:?}"
    );
}

/// The probe the editor opens a file with, over the three answers it has to
/// tell apart: the server let the file open for writing, the server refused on
/// permission, and the server refused for anything else at all.
///
/// The last is the fail-open rule and the one worth a real server to prove: a
/// missing file answers `NoSuchFile`, which is not a statement about permission
/// and so must not lock the buffer. Only `PermissionDenied` may.
///
/// The refusal case is skipped when the read-only attribute does not stick,
/// which is what running as a superuser looks like from here: root opens the
/// file regardless, and the assertion would then be measuring the account the
/// tests run under rather than the code.
#[test]
fn sftp_reports_whether_a_remote_file_can_be_written() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);
    let sftp = session.sftp();

    assert_eq!(server.run(sftp.writable("/notes.txt")), Ok(true));
    // Asking must not have written anything: no `CREATE`, no `TRUNCATE`, so the
    // file the editor is about to show is byte for byte the file it was.
    assert_eq!(
        std::fs::read(root.path().join("notes.txt")).expect("the file must still be readable"),
        b"remote notes\n"
    );

    // A path that names nothing. The server says `NoSuchFile`, which is a
    // refusal and not a verdict, so the answer is still "yes".
    assert_eq!(server.run(sftp.writable("/gone.txt")), Ok(true));
    assert!(
        !root.path().join("gone.txt").exists(),
        "the probe created the file it asked about"
    );

    let locked = root.path().join("locked.txt");
    std::fs::write(&locked, b"locked\n").expect("writing the remote file must succeed");
    set_writable(&locked, false);

    if std::fs::OpenOptions::new()
        .write(true)
        .open(&locked)
        .is_err()
    {
        assert_eq!(server.run(sftp.writable("/locked.txt")), Ok(false));
    }

    // Left writable again, or the temporary tree cannot be taken down on
    // Windows, where a read-only file refuses to be deleted.
    set_writable(&locked, true);
}

/// Turns the write permission of `path` on or off, in whichever of the two ways
/// this platform has one: a single read-only attribute on Windows, the mode bits
/// on unix. [`std::fs::Permissions::set_readonly`] is the one call that spells
/// both, and taking the answer as an argument is also what keeps clippy from
/// reading the restoring direction as a deliberate world-writable chmod — the
/// mode it restores is the one the file was created with a moment earlier.
fn set_writable(path: &Path, writable: bool) {
    let mut permissions = std::fs::metadata(path)
        .expect("the file must be there to have its permissions changed")
        .permissions();
    permissions.set_readonly(!writable);
    std::fs::set_permissions(path, permissions).expect("the permissions must be settable");
}

/// The panel's delete acts on one entry at a time, so a plain file has to go
/// away on its own without touching anything beside it.
#[test]
fn sftp_deletes_a_remote_file() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);
    let sftp = session.sftp();

    assert_eq!(server.run(sftp.remove_file("/notes.txt")), Ok(()));
    assert!(
        !root.path().join("notes.txt").exists(),
        "the file must be gone from the server"
    );
    assert!(
        root.path().join("logs").is_dir(),
        "deleting a file must leave its siblings alone"
    );
}

/// The bottom-up half of a recursive delete: once the children are gone, the
/// directory itself is removed by this call.
#[test]
fn sftp_deletes_an_empty_remote_directory() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);
    let sftp = session.sftp();

    assert_eq!(server.run(sftp.remove_file("/logs/app.log")), Ok(()));
    assert_eq!(server.run(sftp.remove_dir("/logs")), Ok(()));
    assert!(
        !root.path().join("logs").exists(),
        "the directory must be gone from the server"
    );
}

/// SFTP has no recursive delete, so a directory that still holds something
/// must be refused — that refusal is what forces the panel to walk the tree
/// itself instead of hoping the server will.
#[test]
fn sftp_refuses_to_delete_a_directory_that_is_not_empty() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);
    let sftp = session.sftp();

    let outcome = server.run(sftp.remove_dir("/logs"));
    assert!(
        matches!(outcome, Err(SftpError::Remote(_))),
        "expected a remote error over a populated directory, got {outcome:?}"
    );
    assert!(
        root.path().join("logs").join("app.log").is_file(),
        "a refused delete must leave the contents in place"
    );
}

/// Renaming is how the panel's inline rename lands, and it moves the entry
/// rather than copying it: the old name has to disappear.
#[test]
fn sftp_renames_a_remote_entry() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);
    let sftp = session.sftp();

    assert_eq!(
        server.run(sftp.rename("/notes.txt", "/journal.txt")),
        Ok(())
    );
    assert!(
        !root.path().join("notes.txt").exists(),
        "the old name must be gone"
    );
    assert_eq!(
        std::fs::read(root.path().join("journal.txt")).expect("the renamed file must exist"),
        b"remote notes\n"
    );
}

/// The server's verdict is passed straight through, so a rename of something
/// that is not there has to reach the user as a failure rather than a silent
/// no-op that leaves the listing looking wrong.
#[test]
fn sftp_reports_a_rename_of_a_missing_entry() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);
    let sftp = session.sftp();

    let outcome = server.run(sftp.rename("/absent.txt", "/journal.txt"));
    assert!(
        matches!(outcome, Err(SftpError::Remote(_))),
        "expected a remote error over a missing entry, got {outcome:?}"
    );
    assert!(
        !root.path().join("journal.txt").exists(),
        "a failed rename must not create the target"
    );
}

/// Checks a byte count reported by a transfer: at least one update, never
/// going backwards, and ending exactly on the file's size.
fn assert_progress(updates: &[u64], size: u64) {
    assert!(
        !updates.is_empty(),
        "a transfer larger than one chunk must report progress"
    );
    assert!(
        updates.windows(2).all(|pair| match pair {
            [before, after] => after > before,
            _ => true,
        }),
        "progress must increase strictly; saw {updates:?}"
    );
    assert_eq!(
        updates.last().copied(),
        Some(size),
        "the last update must account for the whole file; saw {updates:?}"
    );
}

/// The status line is driven by the byte counts the transfer loop emits, so an
/// upload has to report them — one per chunk, rising, finishing on the size.
#[test]
fn sftp_reports_upload_progress() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);

    let local = tempfile::tempdir().expect("creating the local dir must succeed");
    let source = local.path().join("payload.bin");
    let payload: Vec<u8> = (0..300_000u32).map(|index| (index % 89) as u8).collect();
    std::fs::write(&source, &payload).expect("writing the local file must succeed");

    let (sender, receiver) = futures::channel::mpsc::unbounded();
    server
        .run(session.sftp().upload(source, "/logs", Some(sender)))
        .expect("uploading must succeed");

    // The service drops its sender before answering, so the stream is already
    // finished by the time the upload resolves and this cannot block.
    let updates: Vec<u64> = server.run(receiver.collect());
    assert_progress(&updates, payload.len() as u64);
}

/// The same contract in the other direction.
#[test]
fn sftp_reports_download_progress() {
    let root = remote_tree();
    let payload: Vec<u8> = (0..300_000u32).map(|index| (index % 83) as u8).collect();
    std::fs::write(root.path().join("payload.bin"), &payload)
        .expect("writing the payload must succeed");

    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);

    let local = tempfile::tempdir().expect("creating the local dir must succeed");
    let target = local.path().join("payload.bin");
    let (sender, receiver) = futures::channel::mpsc::unbounded();
    assert_eq!(
        server.run(
            session
                .sftp()
                .download("/payload.bin", target, Some(sender))
        ),
        Ok(())
    );

    let updates: Vec<u64> = server.run(receiver.collect());
    assert_progress(&updates, payload.len() as u64);
}

/// A caller that stops listening must not stop the transfer: the progress
/// stream is a hint for a status line, not a back channel.
#[test]
fn sftp_finishes_a_transfer_whose_progress_is_ignored() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, _events) = sftp_session(&server);

    let local = tempfile::tempdir().expect("creating the local dir must succeed");
    let source = local.path().join("payload.bin");
    let payload: Vec<u8> = (0..200_000u32).map(|index| (index % 71) as u8).collect();
    std::fs::write(&source, &payload).expect("writing the local file must succeed");

    let (sender, receiver) = futures::channel::mpsc::unbounded();
    drop(receiver);
    server
        .run(session.sftp().upload(source, "/logs", Some(sender)))
        .expect("uploading must succeed even with nobody watching");

    let landed = std::fs::read(root.path().join("logs").join("payload.bin"))
        .expect("the uploaded file must exist on the server");
    assert!(landed == payload, "the uploaded bytes must match exactly");
}

/// The whole point of a separate channel: SFTP traffic must leave the shell
/// untouched, and the shell must keep answering while the file panel works.
#[test]
fn sftp_does_not_disturb_the_shell() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, mut events) = sftp_session(&server);

    session.send_input(b"before\n".to_vec());
    assert_eq!(events.read_line(b"before\n"), b"before\n");

    let entries = server
        .run(session.sftp().read_dir("/logs"))
        .expect("listing must succeed while the shell is live");
    assert_eq!(entries.len(), 1, "saw {entries:?}");

    session.send_input(b"after\n".to_vec());
    assert!(
        events.read_line(b"after\n").ends_with(b"after\n"),
        "the shell must still answer after an SFTP request; events: {:?}",
        events.seen()
    );
    assert_eq!(
        server.shell_requests(),
        1,
        "SFTP must not open a second shell"
    );
}

/// A server that does not offer the subsystem must produce an explanation, not
/// a hang: the client asks with `want_reply` precisely so the refusal is seen.
#[test]
fn sftp_reports_a_server_without_the_subsystem() {
    let server = TestServer::with_password("alice", "hunter2");
    let (session, _events) = sftp_session(&server);

    let outcome = server.run(session.sftp().home());
    assert!(
        matches!(outcome, Err(SftpError::Subsystem(_))),
        "expected a subsystem error, got {outcome:?}"
    );
}

/// Once the session is gone every request must fail immediately and by name.
/// Silence or a panic here would strand the file panel.
#[test]
fn sftp_after_a_disconnect_reports_it() {
    let root = remote_tree();
    let server = TestServer::with_sftp("alice", "hunter2", root.path());
    let (session, mut events) = sftp_session(&server);

    // Proves the channel worked before the disconnect, so the failure below
    // cannot be blamed on the subsystem never having started.
    assert!(server.run(session.sftp().home()).is_ok());

    let sftp = session.sftp();
    session.disconnect();
    events.wait_terminal();

    assert_eq!(server.run(sftp.read_dir("/")), Err(SftpError::Disconnected));
    assert_eq!(server.run(sftp.home()), Err(SftpError::Disconnected));
}
