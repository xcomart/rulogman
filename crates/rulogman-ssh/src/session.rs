//! The SSH session driver.
//!
//! [`SshSession::connect`] moves every blocking or `async` operation onto a
//! dedicated OS thread that owns its own Tokio runtime, so a GUI thread can
//! hold an [`SshSession`] and never block on it. All communication happens
//! through channels: commands flow in, [`SshEvent`]s flow out.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use parking_lot::Mutex;
use russh::client::{self, Handle};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::{Channel, ChannelMsg, Disconnect};
use tokio::net::TcpStream;

use crate::config::{SshAuth, SshConfig};
use crate::event::{SshErrorKind, SshEvent};
use crate::sftp::{SftpClient, SftpRequest};
use crate::verify::{HostKeyVerifier, algorithm_name, fingerprint};

/// Host key has not been examined yet.
const KEY_UNCHECKED: u8 = 0;
/// Host key was accepted by the verifier.
const KEY_ACCEPTED: u8 = 1;
/// Host key was rejected by the verifier.
const KEY_REJECTED: u8 = 2;

/// How long the teardown handshake may take before the worker gives up and
/// lets the thread exit anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How often a running session checks that somebody is still reading its
/// events. Independent of the keepalive, which the configuration can disable.
const LIVENESS_INTERVAL: Duration = Duration::from_secs(5);

/// A request sent from the owning thread to the session worker.
enum Command {
    /// Bytes to write to the remote pty.
    Input(Vec<u8>),
    /// New terminal size, in columns and rows.
    Resize(u16, u16),
    /// Close the session.
    Disconnect,
}

impl fmt::Debug for Command {
    /// Never renders keystrokes: input bytes routinely contain passwords typed
    /// into remote prompts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(data) => write!(f, "Input({} bytes)", data.len()),
            Self::Resize(cols, rows) => write!(f, "Resize({cols}, {rows})"),
            Self::Disconnect => f.write_str("Disconnect"),
        }
    }
}

/// A live (or recently ended) SSH shell session running on its own thread.
///
/// The handle is cheap to clone-free share across threads: it is `Send` and
/// `Sync`, and every method is non-blocking. Dropping it disconnects the
/// session and lets the worker thread wind down.
pub struct SshSession {
    /// Command channel to the worker thread.
    commands: UnboundedSender<Command>,
    /// Request channel to the session's SFTP service.
    ///
    /// Separate from `commands` on purpose: file transfers must not queue
    /// behind — or hold up — the shell's keystrokes and resizes.
    sftp: UnboundedSender<SftpRequest>,
    /// `true` between [`SshEvent::Ready`] and the terminal event.
    alive: Arc<AtomicBool>,
}

impl SshSession {
    /// Starts connecting on a background thread and returns immediately.
    ///
    /// The returned receiver yields every [`SshEvent`] the session produces,
    /// in order, starting with [`SshEvent::Connecting`] and ending with either
    /// [`SshEvent::Disconnected`] or [`SshEvent::Error`].
    ///
    /// Dropping the receiver ends the session: a running session notices
    /// within [`LIVENESS_INTERVAL`] even when the remote shell is completely
    /// silent, and winds the connection down.
    pub fn connect(
        config: SshConfig,
        verifier: Arc<dyn HostKeyVerifier>,
    ) -> (SshSession, UnboundedReceiver<SshEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded();
        let (command_tx, command_rx) = mpsc::unbounded();
        let (sftp_tx, sftp_rx) = mpsc::unbounded();
        let alive = Arc::new(AtomicBool::new(false));

        let session = SshSession {
            commands: command_tx,
            sftp: sftp_tx,
            alive: Arc::clone(&alive),
        };

        let thread_name = format!("rulogman-ssh-{}-{}", config.host, config.port);
        let failure_tx = event_tx.clone();
        let spawned = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || worker(config, verifier, event_tx, command_rx, sftp_rx, &alive));

        if let Err(error) = spawned {
            emit(
                &failure_tx,
                SshEvent::Error(
                    SshErrorKind::Io,
                    format!("could not start the SSH worker thread: {error}"),
                ),
            );
        }

        (session, event_rx)
    }

    /// Queues `data` for the remote pty, e.g. keystrokes or pasted text.
    ///
    /// Bytes sent before the session is ready are buffered and flushed once
    /// the shell starts. Silently ignored once the session has ended.
    pub fn send_input(&self, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        let _ = self.commands.unbounded_send(Command::Input(data));
    }

    /// Tells the server that the terminal has been resized.
    ///
    /// Applied to the pty request itself when it arrives before the session is
    /// ready. Silently ignored once the session has ended.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.commands.unbounded_send(Command::Resize(cols, rows));
    }

    /// Ends the session in an orderly fashion.
    ///
    /// Returns without waiting for the worker to finish; the final
    /// [`SshEvent::Disconnected`] still arrives on the event receiver. Safe to
    /// call any number of times.
    pub fn disconnect(&self) {
        let _ = self.commands.unbounded_send(Command::Disconnect);
        self.commands.close_channel();
        // Closes the channel for every outstanding `SftpClient` too, so a file
        // transfer started just before the disconnect fails immediately instead
        // of waiting for the worker thread to notice.
        self.sftp.close_channel();
    }

    /// Returns a handle for SFTP operations on this session.
    ///
    /// The SFTP channel itself is opened lazily, on the first request, and is
    /// then shared by every handle taken from this session; calling this is
    /// free and puts nothing on the wire. Requests made before
    /// [`SshEvent::Ready`] are queued and served once the session is up.
    pub fn sftp(&self) -> SftpClient {
        SftpClient::new(self.sftp.clone())
    }

    /// Reports whether the shell is running, i.e. whether [`SshEvent::Ready`]
    /// has been emitted and no terminal event has followed it.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

impl Drop for SshSession {
    /// Signals the worker thread to stop. Closing the command channel also
    /// unblocks the worker if it is still connecting, so no thread outlives
    /// its session.
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// A fatal problem raised while establishing the session.
struct Failure {
    /// Classification handed to the UI.
    kind: SshErrorKind,
    /// Human-readable description; never contains credentials.
    message: String,
}

impl Failure {
    /// Builds a failure of the given kind.
    fn new(kind: SshErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// How the main loop ended.
enum Ending {
    /// The session finished; the payload explains why.
    Closed(String),
    /// The session broke down.
    Failed(SshErrorKind, String),
}

/// A channel request whose reply the session is waiting for.
enum PendingRequest {
    /// `pty-req`, sent before the shell is started.
    Pty,
    /// `shell`, sent once the pty exists.
    Shell,
}

impl PendingRequest {
    /// Name of the request, for log and error messages.
    fn name(&self) -> &'static str {
        match self {
            Self::Pty => "pty",
            Self::Shell => "shell",
        }
    }

    /// What to tell the user when the server answers with a failure.
    fn refusal(&self) -> &'static str {
        match self {
            Self::Pty => "the server refused a pty",
            Self::Shell => "the server refused to start a shell",
        }
    }
}

/// A fully established session: authenticated, with a confirmed pty and shell.
struct Established {
    /// The transport handle.
    handle: Handle<ClientHandler>,
    /// The shell channel.
    channel: Channel<client::Msg>,
    /// Output that arrived before the shell request was confirmed. Held back
    /// so that no event can precede [`SshEvent::Ready`].
    early_output: Vec<SshEvent>,
}

/// Credentials resolved into the form russh wants, with any private key
/// already parsed and decrypted.
enum Credentials {
    /// A password to send verbatim.
    Password(String),
    /// A parsed private key.
    Key(Arc<PrivateKey>),
}

/// Bridges russh's transport callbacks to the verifier and the event stream.
struct ClientHandler {
    /// Policy consulted for the server's host key.
    verifier: Arc<dyn HostKeyVerifier>,
    /// Where [`SshEvent::HostKey`] is published.
    events: UnboundedSender<SshEvent>,
    /// Host being connected to, for the verifier's benefit.
    host: String,
    /// Port being connected to, for the verifier's benefit.
    port: u16,
    /// Records the verdict so a handshake error can be attributed correctly.
    key_state: Arc<AtomicU8>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let algorithm = algorithm_name(server_public_key);
        let fingerprint = fingerprint(server_public_key);
        let accepted = self
            .verifier
            .verify(&self.host, self.port, server_public_key)
            .await;

        self.key_state.store(
            if accepted { KEY_ACCEPTED } else { KEY_REJECTED },
            Ordering::SeqCst,
        );
        log::debug!(
            "host key for {}:{} ({algorithm} {fingerprint}) accepted={accepted}",
            self.host,
            self.port
        );
        emit(
            &self.events,
            SshEvent::HostKey {
                algorithm,
                fingerprint,
                accepted,
            },
        );
        Ok(accepted)
    }
}

/// Publishes an event, reporting whether anyone is still listening.
pub(crate) fn emit(events: &UnboundedSender<SshEvent>, event: SshEvent) -> bool {
    match events.unbounded_send(event) {
        Ok(()) => true,
        Err(_) => {
            log::debug!("ssh event receiver is gone");
            false
        }
    }
}

/// Entry point of the worker thread: owns the runtime for one session.
fn worker(
    config: SshConfig,
    verifier: Arc<dyn HostKeyVerifier>,
    events: UnboundedSender<SshEvent>,
    commands: UnboundedReceiver<Command>,
    sftp_requests: UnboundedReceiver<SftpRequest>,
    alive: &AtomicBool,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            emit(
                &events,
                SshEvent::Error(
                    SshErrorKind::Io,
                    format!("could not start the SSH runtime: {error}"),
                ),
            );
            return;
        }
    };

    runtime.block_on(run(
        config,
        verifier,
        &events,
        commands,
        sftp_requests,
        alive,
    ));
    alive.store(false, Ordering::SeqCst);
    log::debug!("ssh worker thread finished");
}

/// Drives one session from first connect to final event.
async fn run(
    config: SshConfig,
    verifier: Arc<dyn HostKeyVerifier>,
    events: &UnboundedSender<SshEvent>,
    mut commands: UnboundedReceiver<Command>,
    sftp_requests: UnboundedReceiver<SftpRequest>,
    alive: &AtomicBool,
) {
    if !emit(events, SshEvent::Connecting) {
        return;
    }

    // Shared with the command drain below so that input typed and resizes
    // requested before the shell exists are not lost.
    let size = Arc::new(Mutex::new((config.cols, config.rows)));
    let pending_input = Arc::new(Mutex::new(Vec::<u8>::new()));

    // Racing setup against the command channel keeps a dropped `SshSession`
    // from leaving this thread stuck in a long connect.
    let outcome = tokio::select! {
        result = setup(&config, &verifier, events, &size) => Some(result),
        () = drain_until_shutdown(&mut commands, &pending_input, &size) => None,
    };

    let Established {
        handle,
        mut channel,
        early_output,
    } = match outcome {
        None => {
            emit(
                events,
                SshEvent::Disconnected {
                    reason: "closed before the session was ready".to_owned(),
                },
            );
            return;
        }
        Some(Err(failure)) => {
            log::warn!("ssh session failed: {} ({})", failure.message, failure.kind);
            emit(events, SshEvent::Error(failure.kind, failure.message));
            return;
        }
        Some(Ok(session)) => session,
    };

    // Shared with the SFTP service, which opens its own channel on the same
    // transport. Every `Handle` method takes `&self`, so the two never contend.
    let handle = Arc::new(handle);

    // Both the pty and the shell are confirmed by now, so `Ready` cannot be
    // contradicted by a refusal arriving afterwards.
    alive.store(true, Ordering::SeqCst);
    let mut listening = emit(events, SshEvent::Ready);
    for event in early_output {
        listening &= emit(events, event);
    }
    if !listening {
        alive.store(false, Ordering::SeqCst);
        shutdown(&handle, channel).await;
        return;
    }

    // Runs beside the shell loop rather than inside it: a large transfer must
    // not delay a keystroke, and a stalled shell must not delay a transfer.
    // The task ends with this runtime, so it cannot outlive the session.
    tokio::spawn(crate::sftp::serve(Arc::clone(&handle), sftp_requests));

    // After `Ready` on purpose: a forwarding is an addition to a session that
    // already works, and a rule that cannot be bound must not be able to keep
    // the shell from opening. Each rule that does bind runs as its own task on
    // this same runtime and ends with it.
    crate::tunnel::open(&handle, &config.tunnels, events).await;

    let buffered = std::mem::take(&mut *pending_input.lock());
    if !buffered.is_empty()
        && let Err(error) = channel.data_bytes(buffered).await
    {
        log::warn!("could not flush buffered input: {error}");
    }

    let ending = main_loop(&config, &handle, &mut channel, &mut commands, events).await;
    alive.store(false, Ordering::SeqCst);
    shutdown(&handle, channel).await;

    match ending {
        Ending::Closed(reason) => {
            log::debug!("ssh session closed: {reason}");
            emit(events, SshEvent::Disconnected { reason });
        }
        Ending::Failed(kind, message) => {
            log::warn!("ssh session failed: {message} ({kind})");
            emit(events, SshEvent::Error(kind, message));
        }
    };
}

/// Consumes commands while the session is still being established, returning
/// as soon as a disconnect is requested or the command channel closes.
async fn drain_until_shutdown(
    commands: &mut UnboundedReceiver<Command>,
    pending_input: &Mutex<Vec<u8>>,
    size: &Mutex<(u16, u16)>,
) {
    loop {
        match commands.next().await {
            Some(Command::Input(mut data)) => pending_input.lock().append(&mut data),
            Some(Command::Resize(cols, rows)) => *size.lock() = (cols, rows),
            Some(Command::Disconnect) | None => return,
        }
    }
}

/// Loads credentials, connects, authenticates and opens an interactive shell.
///
/// Both channel requests are made with `want_reply` set and their answers are
/// waited for, so a caller that sees this succeed knows the remote pty and
/// shell really exist. The wait is unbounded on purpose — a server that never
/// answers is escaped by disconnecting, which cancels this future.
async fn setup(
    config: &SshConfig,
    verifier: &Arc<dyn HostKeyVerifier>,
    events: &UnboundedSender<SshEvent>,
    size: &Mutex<(u16, u16)>,
) -> Result<Established, Failure> {
    // Done first so a bad key path fails fast, before touching the network.
    let credentials = load_credentials(&config.auth)?;

    let stream = tcp_connect(config).await?;
    let key_state = Arc::new(AtomicU8::new(KEY_UNCHECKED));
    let handler = ClientHandler {
        verifier: Arc::clone(verifier),
        events: events.clone(),
        host: config.host.clone(),
        port: config.port,
        key_state: Arc::clone(&key_state),
    };

    let client_config = client::Config {
        // Keepalives are driven by the session loop instead, so that russh and
        // this crate never both ping the server.
        inactivity_timeout: None,
        keepalive_interval: None,
        ..client::Config::default()
    };

    let mut handle = client::connect_stream(Arc::new(client_config), stream, handler)
        .await
        .map_err(|error| {
            if key_state.load(Ordering::SeqCst) == KEY_REJECTED {
                Failure::new(
                    SshErrorKind::HostKeyRejected,
                    format!(
                        "the host key presented by {}:{} was rejected",
                        config.host, config.port
                    ),
                )
            } else {
                Failure::new(
                    SshErrorKind::Connect,
                    format!("SSH handshake with {} failed: {error}", config.host),
                )
            }
        })?;

    authenticate(&mut handle, config, credentials).await?;

    let mut channel = handle.channel_open_session().await.map_err(|error| {
        Failure::new(
            SshErrorKind::Channel,
            format!("could not open a session channel: {error}"),
        )
    })?;

    let mut early_output = Vec::new();
    let requested = *size.lock();
    channel
        .request_pty(
            true,
            &config.term,
            u32::from(requested.0),
            u32::from(requested.1),
            0,
            0,
            &[],
        )
        .await
        .map_err(|error| {
            Failure::new(
                SshErrorKind::Channel,
                format!("could not request a pty: {error}"),
            )
        })?;
    await_reply(&mut channel, &PendingRequest::Pty, &mut early_output).await?;

    channel.request_shell(true).await.map_err(|error| {
        Failure::new(
            SshErrorKind::Channel,
            format!("could not request a shell: {error}"),
        )
    })?;
    await_reply(&mut channel, &PendingRequest::Shell, &mut early_output).await?;

    // A resize that arrived while the two requests were in flight missed the
    // pty request, so it has to be delivered as a window change instead.
    let current = *size.lock();
    if current != requested
        && let Err(error) = channel
            .window_change(u32::from(current.0), u32::from(current.1), 0, 0)
            .await
    {
        log::warn!("could not apply the terminal size requested during setup: {error}");
    }

    Ok(Established {
        handle,
        channel,
        early_output,
    })
}

/// Waits for the server's answer to a channel request.
///
/// Any output that arrives meanwhile is collected into `early_output` rather
/// than dropped, so a server that talks before confirming loses nothing.
async fn await_reply(
    channel: &mut Channel<client::Msg>,
    request: &PendingRequest,
    early_output: &mut Vec<SshEvent>,
) -> Result<(), Failure> {
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => return Ok(()),
            Some(ChannelMsg::Failure) => {
                return Err(Failure::new(SshErrorKind::Channel, request.refusal()));
            }
            Some(ChannelMsg::Data { data }) => early_output.push(SshEvent::Data(data.to_vec())),
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                early_output.push(SshEvent::ExtendedData(data.to_vec()));
            }
            Some(ChannelMsg::OpenFailure(reason)) => {
                return Err(Failure::new(
                    SshErrorKind::Channel,
                    format!(
                        "the server closed the channel while the {} request was pending: {reason:?}",
                        request.name()
                    ),
                ));
            }
            Some(ChannelMsg::Eof | ChannelMsg::Close) | None => {
                return Err(Failure::new(
                    SshErrorKind::Channel,
                    format!(
                        "the connection closed before the {} request was answered",
                        request.name()
                    ),
                ));
            }
            Some(other) => {
                log::trace!(
                    "ignoring {other:?} while waiting for the {} reply",
                    request.name()
                );
            }
        }
    }
}

/// Reads and decrypts the private key, or hands back the password unchanged.
fn load_credentials(auth: &SshAuth) -> Result<Credentials, Failure> {
    match auth {
        SshAuth::Password(password) => Ok(Credentials::Password(password.clone())),
        SshAuth::PrivateKeyFile { path, passphrase } => {
            // russh's error messages describe the failure only, never the
            // passphrase, so they are safe to surface and to log.
            russh::keys::load_secret_key(path, passphrase.as_deref())
                .map(|key| Credentials::Key(Arc::new(key)))
                .map_err(|error| {
                    Failure::new(
                        SshErrorKind::KeyLoad,
                        format!(
                            "could not load the private key at {}: {error}",
                            path.display()
                        ),
                    )
                })
        }
        SshAuth::PrivateKeyData { pem, passphrase } => {
            russh::keys::decode_secret_key(pem, passphrase.as_deref())
                .map(|key| Credentials::Key(Arc::new(key)))
                .map_err(|error| {
                    Failure::new(
                        SshErrorKind::KeyLoad,
                        format!("could not decode the supplied private key: {error}"),
                    )
                })
        }
    }
}

/// Opens the TCP connection, honouring the configured connect timeout.
async fn tcp_connect(config: &SshConfig) -> Result<TcpStream, Failure> {
    let address = (config.host.as_str(), config.port);
    let attempt = TcpStream::connect(address);

    let connected = if config.connect_timeout_secs == 0 {
        attempt.await.map_err(|error| {
            Failure::new(
                SshErrorKind::Connect,
                format!(
                    "could not connect to {}:{}: {error}",
                    config.host, config.port
                ),
            )
        })?
    } else {
        let limit = Duration::from_secs(config.connect_timeout_secs);
        match tokio::time::timeout(limit, attempt).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                return Err(Failure::new(
                    SshErrorKind::Connect,
                    format!(
                        "could not connect to {}:{}: {error}",
                        config.host, config.port
                    ),
                ));
            }
            Err(_) => {
                return Err(Failure::new(
                    SshErrorKind::Connect,
                    format!(
                        "connecting to {}:{} timed out after {}s",
                        config.host, config.port, config.connect_timeout_secs
                    ),
                ));
            }
        }
    };

    if let Err(error) = connected.set_nodelay(true) {
        log::debug!("could not disable Nagle's algorithm: {error}");
    }
    Ok(connected)
}

/// Runs exactly the one authentication method the configuration asks for.
async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    config: &SshConfig,
    credentials: Credentials,
) -> Result<(), Failure> {
    let result = match credentials {
        Credentials::Password(password) => {
            handle
                .authenticate_password(config.username.as_str(), password)
                .await
        }
        Credentials::Key(key) => {
            let hash_alg = handle.best_supported_rsa_hash().await.map_err(|error| {
                Failure::new(
                    SshErrorKind::Io,
                    format!("could not negotiate a signature algorithm: {error}"),
                )
            })?;
            handle
                .authenticate_publickey(
                    config.username.as_str(),
                    PrivateKeyWithHashAlg::new(key, hash_alg.flatten()),
                )
                .await
        }
    };

    match result {
        Ok(outcome) if outcome.success() => Ok(()),
        Ok(_) => Err(Failure::new(
            SshErrorKind::Auth,
            format!(
                "the server rejected the credentials for user {}",
                config.username
            ),
        )),
        // A transport-level break during authentication is not a rejection, so
        // it must not be reported as one.
        Err(error) => Err(Failure::new(
            SshErrorKind::Io,
            format!("the connection broke during authentication: {error}"),
        )),
    }
}

/// One iteration's worth of work picked up by the main loop.
enum Step {
    /// A message arrived on the shell channel.
    Message(Option<ChannelMsg>),
    /// A command arrived from the owning thread.
    Command(Option<Command>),
    /// The keepalive timer fired.
    Keepalive,
    /// The liveness timer fired.
    Liveness,
}

/// Pumps the shell until the session ends.
async fn main_loop(
    config: &SshConfig,
    handle: &Handle<ClientHandler>,
    channel: &mut Channel<client::Msg>,
    commands: &mut UnboundedReceiver<Command>,
    events: &UnboundedSender<SshEvent>,
) -> Ending {
    let mut keepalive = keepalive_timer(config.keepalive_secs);
    let mut liveness = repeating_timer(LIVENESS_INTERVAL);

    loop {
        // Every branch resolves to a value rather than acting in place: the
        // borrows taken by `select!` end with the statement, which leaves the
        // channel free to be written to below.
        let step = tokio::select! {
            message = channel.wait() => Step::Message(message),
            command = commands.next() => Step::Command(command),
            () = tick(&mut keepalive) => Step::Keepalive,
            _ = liveness.tick() => Step::Liveness,
        };

        match step {
            Step::Message(Some(message)) => match message {
                ChannelMsg::Data { data } => {
                    if !emit(events, SshEvent::Data(data.to_vec())) {
                        return Ending::Closed("nobody is reading the session".to_owned());
                    }
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    if !emit(events, SshEvent::ExtendedData(data.to_vec())) {
                        return Ending::Closed("nobody is reading the session".to_owned());
                    }
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    if !emit(events, SshEvent::ExitStatus(exit_status)) {
                        return Ending::Closed("nobody is reading the session".to_owned());
                    }
                }
                ChannelMsg::ExitSignal {
                    signal_name,
                    error_message,
                    ..
                } => {
                    log::debug!("remote shell killed by {signal_name:?}: {error_message}");
                }
                ChannelMsg::Eof => {
                    return Ending::Closed("the remote shell closed its output".to_owned());
                }
                ChannelMsg::Close => {
                    return Ending::Closed("the remote host closed the channel".to_owned());
                }
                // Both requests this client makes are confirmed during setup,
                // so replies seen here answer something the server volunteered
                // (many servers reply to `window-change`, for instance) and
                // must not be blamed on the shell.
                ChannelMsg::Success | ChannelMsg::Failure => {
                    log::trace!("ignoring an unsolicited channel reply");
                }
                ChannelMsg::OpenFailure(reason) => {
                    return Ending::Failed(
                        SshErrorKind::Channel,
                        format!("the server closed the channel: {reason:?}"),
                    );
                }
                other => log::trace!("ignoring channel message {other:?}"),
            },
            Step::Message(None) => {
                return Ending::Closed("the connection was closed".to_owned());
            }
            Step::Command(Some(Command::Input(data))) => {
                if let Err(error) = channel.data_bytes(data).await {
                    return Ending::Failed(
                        SshErrorKind::Io,
                        format!("could not write to the remote shell: {error}"),
                    );
                }
            }
            Step::Command(Some(Command::Resize(cols, rows))) => {
                if let Err(error) = channel
                    .window_change(u32::from(cols), u32::from(rows), 0, 0)
                    .await
                {
                    // Not fatal on its own: if the transport really is gone,
                    // the channel will report it on the next iteration.
                    log::warn!("could not resize the remote terminal: {error}");
                }
            }
            Step::Command(Some(Command::Disconnect)) => {
                return Ending::Closed("disconnected locally".to_owned());
            }
            Step::Command(None) => {
                return Ending::Closed("the session handle was dropped".to_owned());
            }
            Step::Keepalive => {
                if handle.is_closed() {
                    return Ending::Closed("the connection was closed".to_owned());
                }
                if let Err(error) = handle.send_keepalive(true).await {
                    return Ending::Closed(format!("the keepalive failed: {error}"));
                }
            }
            Step::Liveness => {
                // The only way to notice a dropped receiver while the remote
                // shell is silent, and therefore the only thing that makes
                // "dropping the receiver ends the session" true.
                if events.is_closed() {
                    return Ending::Closed("nobody is reading the session".to_owned());
                }
            }
        }
    }
}

/// Builds the keepalive timer, or `None` when keepalives are disabled.
fn keepalive_timer(keepalive_secs: u64) -> Option<tokio::time::Interval> {
    if keepalive_secs == 0 {
        return None;
    }
    Some(repeating_timer(Duration::from_secs(keepalive_secs)))
}

/// Builds a timer that fires every `period`.
///
/// The first tick is deliberately pushed out by one full period so that a
/// session does nothing at all the instant it becomes ready, and ticks missed
/// under load are delayed rather than fired back to back.
fn repeating_timer(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// Waits for the next keepalive tick, or forever when there is no timer.
async fn tick(timer: &mut Option<tokio::time::Interval>) {
    match timer {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// Closes the channel and the transport, giving up after [`SHUTDOWN_GRACE`] so
/// an unresponsive server cannot keep the worker thread alive.
///
/// Takes the handle by reference because the SFTP service shares it; the last
/// reference goes away with the runtime a moment later, which is what actually
/// stops russh's own session task.
async fn shutdown(handle: &Handle<ClientHandler>, channel: Channel<client::Msg>) {
    let teardown = async {
        let _ = channel.close().await;
        let _ = handle
            .disconnect(Disconnect::ByApplication, "", "en-US")
            .await;
    };
    if tokio::time::timeout(SHUTDOWN_GRACE, teardown)
        .await
        .is_err()
    {
        log::debug!("timed out while closing the ssh session");
    }
}
