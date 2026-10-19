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
use futures::channel::oneshot;
use parking_lot::Mutex;
use russh::client::{self, DisconnectReason, Handle};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::{
    Channel, ChannelMsg, ChannelOpenFailure, ChannelStream, Disconnect, Error as RusshError,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::config::{SshAuth, SshConfig};
use crate::event::{SshErrorKind, SshEvent};
use crate::exec::{ExecClient, ExecRequest};
use crate::sftp::{SftpClient, SftpRequest};
use crate::verify::{HostKeyVerifier, algorithm_name, fingerprint};

mod agent;

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

/// How long a transport that was *seen* closed may take to say why.
///
/// russh closes the handle's channel a moment before it tells the handler what
/// ended the session, so the shell loop can notice the loss first; waiting
/// this long for the reason is what keeps an unanswered keepalive from being
/// reported as a plain "connection closed".
const LOSS_GRACE: Duration = Duration::from_millis(500);

/// How long a shell that closed its output has to say how it ended.
///
/// OpenSSH's `sshd` sends `SSH_MSG_CHANNEL_EOF` *before* the `exit-status`
/// request and the close, so a session that ended on the end of output would
/// never see the exit status at all. After the EOF the loop goes on reading
/// for this long, for the status, a signal and the close; a server that sends
/// none of them still ends the session, only this much later. The answer
/// normally follows within milliseconds — the wait is for a loaded host.
const EOF_GRACE: Duration = Duration::from_secs(2);

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
    /// Request channel to the session's command-execution service.
    ///
    /// Separate from both of the others for the same reason they are separate
    /// from each other: a command that takes a minute must not delay a
    /// keystroke, and a file transfer must not delay a command.
    exec: UnboundedSender<ExecRequest>,
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
    ) -> (SshSession, rulogman_stream::Receiver<SshEvent>) {
        let (event_tx, event_rx) = rulogman_stream::channel(rulogman_stream::OUTPUT_BUDGET);
        let (command_tx, command_rx) = mpsc::unbounded();
        let (sftp_tx, sftp_rx) = mpsc::unbounded();
        let (exec_tx, exec_rx) = mpsc::unbounded();
        let alive = Arc::new(AtomicBool::new(false));

        let session = SshSession {
            commands: command_tx,
            sftp: sftp_tx,
            exec: exec_tx,
            alive: Arc::clone(&alive),
        };

        let thread_name = format!("rulogman-ssh-{}-{}", config.host, config.port);
        let failure_tx = event_tx.clone();
        let spawned = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                worker(
                    config, verifier, event_tx, command_rx, sftp_rx, exec_rx, &alive,
                );
            });

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
        // of waiting for the worker thread to notice. The exec channel is
        // closed for exactly the same reason.
        self.sftp.close_channel();
        self.exec.close_channel();
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

    /// Returns a handle for running commands on this session.
    ///
    /// Each command opens a channel of its own when it runs — the protocol
    /// allows one `exec` per channel — so calling this is free and puts nothing
    /// on the wire. Commands issued before [`SshEvent::Ready`] are queued and
    /// run once the session is up.
    pub fn exec(&self) -> ExecClient {
        ExecClient::new(self.exec.clone())
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

/// What ended an established transport, as the target's handler heard it
/// from russh — the one ending the shell loop cannot see for itself.
type Loss = Ending;

/// What the target leg's handler is given that a jump host's is not: the
/// keepalive settings it runs under, and where to say why the transport ended.
struct TargetWatch {
    /// Seconds of silence before a keepalive goes out; `0` sends none.
    keepalive_secs: u64,
    /// Keepalives that may go unanswered before the transport is given up.
    keepalive_max: usize,
    /// Where the reason the transport ended is sent, once.
    lost: oneshot::Sender<Loss>,
}

/// A channel request whose reply the session is waiting for.
enum PendingRequest {
    /// `pty-req`, sent before the shell or the command is started.
    Pty,
    /// `shell`, sent once the pty exists.
    Shell,
    /// `exec`, sent once the pty exists, in place of `shell` — the session's
    /// own channel runs the configured command instead of a login shell.
    Exec,
}

impl PendingRequest {
    /// Name of the request, for log and error messages.
    fn name(&self) -> &'static str {
        match self {
            Self::Pty => "pty",
            Self::Shell => "shell",
            Self::Exec => "command",
        }
    }

    /// What to tell the user when the server answers with a failure.
    fn refusal(&self) -> &'static str {
        match self {
            Self::Pty => "the server refused a pty",
            Self::Shell => "the server refused to start a shell",
            // A refusal here is the server declining to run *anything*, which
            // is a policy decision (a forced command, or a restricted shell);
            // it says nothing about whether the command itself exists.
            Self::Exec => "the server refused to run the command",
        }
    }
}

/// A fully established session: authenticated, with a confirmed pty and a
/// confirmed shell or command.
struct Established {
    /// The target's transport handle.
    handle: Handle<ClientHandler>,
    /// The jump hosts' transports, in the order they were traversed.
    ///
    /// Never used again, and that is the point: each one carries the
    /// connection made through it, so dropping any of them tears down
    /// everything beyond it. They are held for exactly as long as the session
    /// is.
    jumps: Vec<Handle<ClientHandler>>,
    /// The session channel the shell — or the command — runs on.
    channel: Channel<client::Msg>,
    /// Output that arrived before the shell or command request was confirmed.
    /// Held back so that no event can precede [`SshEvent::Ready`].
    early_output: Vec<SshEvent>,
}

/// Credentials resolved into the form russh wants, with any private key
/// already parsed and decrypted.
enum Credentials {
    /// A password to send verbatim.
    Password(String),
    /// A parsed private key.
    Key(Arc<PrivateKey>),
    /// Signing is delegated to the platform's SSH agent.
    Agent,
}

/// Bridges russh's transport callbacks to the verifier and the event stream.
struct ClientHandler {
    /// Policy consulted for the server's host key.
    verifier: Arc<dyn HostKeyVerifier>,
    /// Where [`SshEvent::HostKey`] is published.
    events: rulogman_stream::Sender<SshEvent>,
    /// Host being connected to, for the verifier's benefit.
    host: String,
    /// Port being connected to, for the verifier's benefit.
    port: u16,
    /// Records the verdict so a handshake error can be attributed correctly.
    key_state: Arc<AtomicU8>,
    /// Where the reason the transport ended is sent, once — only on the
    /// target's leg. The shell loop waits on the other end, which is what lets
    /// it report an unanswered keepalive as such, and at once.
    lost: Option<oneshot::Sender<Loss>>,
    /// The keepalive settings, for the message when they run out.
    keepalive: (u64, usize),
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
                host: self.host.clone(),
                port: self.port,
                algorithm,
                fingerprint,
                accepted,
            },
        );
        Ok(accepted)
    }

    /// Records why the transport ended, then hands the reason back to russh
    /// exactly as the default implementation would.
    ///
    /// Only called once the handshake is over; a failure before that comes
    /// back from `connect_stream` instead. Of the three endings only the
    /// keepalive timeout is a *failure* for the session: a server that says
    /// goodbye, or a socket that breaks, ends it the way a closed connection
    /// always has, now with the reason in its words. A link that went silent —
    /// a NAT mapping dropped, a laptop resumed on another network — is the one
    /// that would otherwise sit at *connected* until TCP gave up, minutes
    /// later, and is the one the user has to be told about.
    async fn disconnected(
        &mut self,
        reason: DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        let (loss, result) = match reason {
            DisconnectReason::ReceivedDisconnect(info) => {
                let message = if info.message.is_empty() {
                    format!("{}:{} closed the connection", self.host, self.port)
                } else {
                    format!(
                        "{}:{} closed the connection: {}",
                        self.host, self.port, info.message
                    )
                };
                (Ending::Closed(message), Ok(()))
            }
            DisconnectReason::Error(RusshError::KeepaliveTimeout) => {
                let (secs, max) = self.keepalive;
                let message = format!(
                    "{}:{} stopped answering: {max} keepalives sent {secs}s apart went \
                     unanswered, so the connection was given up",
                    self.host, self.port
                );
                (
                    Ending::Failed(SshErrorKind::Io, message),
                    Err(RusshError::KeepaliveTimeout),
                )
            }
            DisconnectReason::Error(error) => {
                let message = format!(
                    "the connection to {}:{} was lost: {error}",
                    self.host, self.port
                );
                (Ending::Closed(message), Err(error))
            }
        };
        if let Some(lost) = self.lost.take() {
            let _ = lost.send(loss);
        }
        result
    }
}

/// Publishes an event, reporting whether anyone is still listening.
pub(crate) fn emit(events: &rulogman_stream::Sender<SshEvent>, event: SshEvent) -> bool {
    if matches!(event, SshEvent::Disconnected { .. } | SshEvent::Error(..)) {
        events.finish(event)
    } else {
        debug_assert!(!matches!(
            event,
            SshEvent::Data(_) | SshEvent::ExtendedData(_)
        ));
        events.control(event)
    }
}

fn output_size(event: &SshEvent) -> usize {
    match event {
        SshEvent::Data(bytes) | SshEvent::ExtendedData(bytes) => bytes.len(),
        _ => 0,
    }
}

fn queue_output(event: SshEvent, pending: &mut std::collections::VecDeque<SshEvent>) {
    let (bytes, extended) = match event {
        SshEvent::Data(bytes) => (bytes, false),
        SshEvent::ExtendedData(bytes) => (bytes, true),
        other => {
            pending.push_back(other);
            return;
        }
    };
    for chunk in bytes.chunks(rulogman_stream::OUTPUT_CHUNK) {
        pending.push_back(if extended {
            SshEvent::ExtendedData(chunk.to_vec())
        } else {
            SshEvent::Data(chunk.to_vec())
        });
    }
}

/// Entry point of the worker thread: owns the runtime for one session.
fn worker(
    config: SshConfig,
    verifier: Arc<dyn HostKeyVerifier>,
    events: rulogman_stream::Sender<SshEvent>,
    commands: UnboundedReceiver<Command>,
    sftp_requests: UnboundedReceiver<SftpRequest>,
    exec_requests: UnboundedReceiver<ExecRequest>,
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
        exec_requests,
        alive,
    ));
    alive.store(false, Ordering::SeqCst);
    log::debug!("ssh worker thread finished");
}

/// Drives one session from first connect to final event.
async fn run(
    config: SshConfig,
    verifier: Arc<dyn HostKeyVerifier>,
    events: &rulogman_stream::Sender<SshEvent>,
    mut commands: UnboundedReceiver<Command>,
    sftp_requests: UnboundedReceiver<SftpRequest>,
    exec_requests: UnboundedReceiver<ExecRequest>,
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
    let (lost_tx, mut lost) = oneshot::channel();
    let outcome = tokio::select! {
        result = setup(&config, &verifier, events, &size, lost_tx) => Some(result),
        () = drain_until_shutdown(&mut commands, &pending_input, &size) => None,
    };

    let Established {
        handle,
        jumps,
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

    // Shared with the SFTP and exec services, which open their own channels on
    // the same transport. Every `Handle` method takes `&self`, so they never
    // contend.
    let handle = Arc::new(handle);

    // Both the pty and the shell are confirmed by now, so `Ready` cannot be
    // contradicted by a refusal arriving afterwards.
    alive.store(true, Ordering::SeqCst);
    if !emit(events, SshEvent::Ready) {
        alive.store(false, Ordering::SeqCst);
        shutdown(&handle, channel).await;
        return;
    }

    // Runs beside the shell loop rather than inside it: a large transfer must
    // not delay a keystroke, and a stalled shell must not delay a transfer.
    // The task ends with this runtime, so it cannot outlive the session.
    tokio::spawn(crate::sftp::serve(Arc::clone(&handle), sftp_requests));

    // Beside it rather than inside it, and for the same reasons: a command that
    // takes a minute must not stall a directory listing, and neither may touch
    // the shell's channel.
    tokio::spawn(crate::exec::serve(Arc::clone(&handle), exec_requests));

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

    let ending = main_loop(
        &config,
        &handle,
        &mut channel,
        &mut commands,
        &mut lost,
        events,
        early_output,
    )
    .await;
    alive.store(false, Ordering::SeqCst);
    shutdown(&handle, channel).await;

    // Last, and explicitly: every jump host carries the connection made
    // through it, so letting these go before the target's own teardown would
    // cut the wire out from under it.
    drop(jumps);

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

/// Loads credentials, walks the connection chain and opens the session's own
/// channel, with either a shell or the configured command running on it.
///
/// Both channel requests are made with `want_reply` set and their answers are
/// waited for, so a caller that sees this succeed knows the remote pty and the
/// shell (or the command) really exist. The wait is unbounded on purpose — a
/// server that never answers is escaped by disconnecting, which cancels this
/// future.
async fn setup(
    config: &SshConfig,
    verifier: &Arc<dyn HostKeyVerifier>,
    events: &rulogman_stream::Sender<SshEvent>,
    size: &Mutex<(u16, u16)>,
    lost: oneshot::Sender<Loss>,
) -> Result<Established, Failure> {
    let Chain { handle, jumps } = connect_chain(config, verifier, events, lost).await?;

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

    // The one place the two modes differ. Everything downstream — the data
    // pump, `window-change`, the exit status, the teardown — is written
    // against a channel, not against a shell, so neither of them knows which
    // request started it.
    match &config.command {
        Some(command) => {
            channel
                .exec(true, command.as_bytes())
                .await
                .map_err(|error| {
                    Failure::new(
                        SshErrorKind::Channel,
                        format!("could not ask the server to run the command: {error}"),
                    )
                })?;
            await_reply(&mut channel, &PendingRequest::Exec, &mut early_output).await?;
        }
        None => {
            channel.request_shell(true).await.map_err(|error| {
                Failure::new(
                    SshErrorKind::Channel,
                    format!("could not request a shell: {error}"),
                )
            })?;
            await_reply(&mut channel, &PendingRequest::Shell, &mut early_output).await?;
        }
    }

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
        jumps,
        channel,
        early_output,
    })
}

/// One leg of the connection: a host to reach, and the account to reach it
/// with.
///
/// The target is a leg like any other. The only thing that distinguishes it is
/// that it is the last one, and the only place that shows is [`Leg::label`] —
/// which is what makes every message below name the host it is about.
struct Leg<'a> {
    /// Host name or address, as the *previous* leg resolves it.
    host: &'a str,
    /// TCP port of this leg's SSH service.
    port: u16,
    /// Account to log in as on this leg.
    username: &'a str,
    /// The single method to authenticate with on this leg.
    auth: &'a SshAuth,
    /// `true` while this leg is a jump host rather than the target.
    jump: bool,
}

impl Leg<'_> {
    /// How this leg is named in an error message.
    ///
    /// A jump host says so, because "connection refused" means something quite
    /// different depending on which machine refused it, and the user's fix is
    /// on a different machine too.
    fn label(&self) -> String {
        if self.jump {
            format!("jump host {}:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// The authenticated transports of one connection chain.
struct Chain {
    /// The target's transport: the one the shell, SFTP and exec all ride on.
    handle: Handle<ClientHandler>,
    /// The jump hosts' transports, in the order they were traversed.
    jumps: Vec<Handle<ClientHandler>>,
}

/// Connects to and authenticates against every host on the way to the target.
///
/// The first leg is dialled over a socket of its own; each later one is dialled
/// *through* the leg before it, as a `direct-tcpip` channel carrying a whole
/// second SSH connection — which is exactly what `ssh -J` does, and why the
/// target's host key is still the target's own rather than the bastion's.
async fn connect_chain(
    config: &SshConfig,
    verifier: &Arc<dyn HostKeyVerifier>,
    events: &rulogman_stream::Sender<SshEvent>,
    lost: oneshot::Sender<Loss>,
) -> Result<Chain, Failure> {
    let legs: Vec<Leg<'_>> = config
        .hops
        .iter()
        .map(|hop| Leg {
            host: &hop.host,
            port: hop.port,
            username: &hop.username,
            auth: &hop.auth,
            jump: true,
        })
        .chain(std::iter::once(Leg {
            host: &config.host,
            port: config.port,
            username: &config.username,
            auth: &config.auth,
            jump: false,
        }))
        .collect();

    // Every credential is resolved before anything is dialled, so that a bad
    // key path — on the last hop as much as on the first — fails fast, before
    // a single packet leaves this machine.
    let credentials = legs
        .iter()
        .map(|leg| load_credentials(leg.auth))
        .collect::<Result<Vec<_>, _>>()?;

    let mut jumps: Vec<Handle<ClientHandler>> = Vec::new();
    let mut established: Option<Handle<ClientHandler>> = None;
    // Handed to the target's leg alone, which is always the last: see
    // [`handshake`] for why no jump host keeps a watch of its own.
    let mut watch = Some(TargetWatch {
        keepalive_secs: config.keepalive_secs,
        keepalive_max: config.keepalive_max,
        lost,
    });

    for (index, (leg, credentials)) in legs.iter().zip(credentials).enumerate() {
        let target = if leg.jump { None } else { watch.take() };
        let mut handle = match established.take() {
            // The first leg is the only one with a socket of its own.
            None => {
                let stream = tcp_connect(leg, config.connect_timeout_secs).await?;
                handshake(stream, leg, verifier, events, target).await?
            }
            // Every later one rides inside the leg before it. `previous` is
            // moved in here on purpose: should this leg fail, dropping it is
            // what tears the half-built chain down.
            Some(previous) => {
                let through = &legs[index - 1];
                let stream =
                    hop_stream(&previous, through, leg, config.connect_timeout_secs).await?;
                let carried = handshake(stream, leg, verifier, events, target).await?;
                jumps.push(previous);
                carried
            }
        };
        authenticate(&mut handle, leg, credentials).await?;
        established = Some(handle);
    }

    // `legs` always ends with the target, so the loop always leaves a handle
    // behind. Reported rather than unwrapped all the same: a panic here would
    // take down a worker thread and leave the session with no final event.
    let handle = established.ok_or_else(|| {
        Failure::new(
            SshErrorKind::Connect,
            format!("there is no route to {}:{}", config.host, config.port),
        )
    })?;

    Ok(Chain { handle, jumps })
}

/// Runs the SSH handshake for one leg over an already-open stream.
///
/// Generic over the stream because that is the whole trick: the first leg
/// hands in a [`TcpStream`], every later one hands in a `direct-tcpip` channel
/// borrowed from the leg before it, and russh cannot tell the difference.
async fn handshake<S>(
    stream: S,
    leg: &Leg<'_>,
    verifier: &Arc<dyn HostKeyVerifier>,
    events: &rulogman_stream::Sender<SshEvent>,
    target: Option<TargetWatch>,
) -> Result<Handle<ClientHandler>, Failure>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // One per leg: the verdict recorded here is what tells a handshake error
    // on *this* leg apart from a rejected key on it.
    let key_state = Arc::new(AtomicU8::new(KEY_UNCHECKED));
    let mut handler = ClientHandler {
        verifier: Arc::clone(verifier),
        events: events.clone(),
        // The host and port this leg was *asked* for, not whatever it resolved
        // to, so the verifier is consulted under the name the user wrote — and
        // for a hop, under the hop's own name rather than the target's.
        host: leg.host.to_owned(),
        port: leg.port,
        key_state: Arc::clone(&key_state),
        keepalive: target
            .as_ref()
            .map_or((0, 0), |watch| (watch.keepalive_secs, watch.keepalive_max)),
        lost: None,
    };
    let (keepalive_interval, keepalive_max) = match target {
        Some(TargetWatch {
            keepalive_secs,
            keepalive_max,
            lost,
        }) => {
            handler.lost = Some(lost);
            (
                (keepalive_secs > 0).then(|| Duration::from_secs(keepalive_secs)),
                keepalive_max,
            )
        }
        None => (None, 0),
    };

    let client_config = client::Config {
        // russh's own keepalive rather than one sent from the shell loop: it
        // only fires after `keepalive_secs` with nothing heard, it counts the
        // ones that go unanswered — any packet from the server counts as an
        // answer — and it ends the transport after `keepalive_max` of them,
        // which `disconnected` above reports. A keepalive that is merely
        // *sent* proves nothing: on a half-dead TCP connection the write
        // succeeds into the kernel's buffer for minutes.
        //
        // The target's leg only. A jump host needs none of its own: every byte
        // the target exchanges — its keepalives included — crosses it, so a
        // dead bastion is a silent target.
        inactivity_timeout: None,
        keepalive_interval,
        keepalive_max,
        ..client::Config::default()
    };

    client::connect_stream(Arc::new(client_config), stream, handler)
        .await
        .map_err(|error| {
            if key_state.load(Ordering::SeqCst) == KEY_REJECTED {
                Failure::new(
                    SshErrorKind::HostKeyRejected,
                    format!("the host key presented by {} was rejected", leg.label()),
                )
            } else {
                Failure::new(
                    SshErrorKind::Connect,
                    format!("SSH handshake with {} failed: {error}", leg.label()),
                )
            }
        })
}

/// Opens the next leg's transport inside the current one's connection.
///
/// The result is an ordinary byte stream as far as the caller is concerned;
/// what it actually is, is a `direct-tcpip` channel on `through`'s transport,
/// which is the same primitive a local port forwarding uses.
async fn hop_stream(
    through_handle: &Handle<ClientHandler>,
    through: &Leg<'_>,
    next: &Leg<'_>,
    connect_timeout_secs: u64,
) -> Result<ChannelStream<client::Msg>, Failure> {
    // The originator is what OpenSSH sends for a jump: the loopback address of
    // the machine asking, and a port it does not have to invent. Servers log
    // it; none of them route on it.
    let open = through_handle.channel_open_direct_tcpip(
        next.host.to_owned(),
        u32::from(next.port),
        "127.0.0.1".to_owned(),
        0,
    );

    // The connect timeout is per hop, and this is a hop's connect: `through`
    // is the one dialling, and it is the one that may never answer.
    let opened = if connect_timeout_secs == 0 {
        open.await
    } else {
        let limit = Duration::from_secs(connect_timeout_secs);
        match tokio::time::timeout(limit, open).await {
            Ok(result) => result,
            Err(_) => {
                return Err(Failure::new(
                    SshErrorKind::Connect,
                    format!(
                        "{} did not answer the request to connect to {}:{} within \
                         {connect_timeout_secs}s",
                        through.host, next.host, next.port
                    ),
                ));
            }
        }
    };

    let channel = opened.map_err(|error| {
        Failure::new(
            SshErrorKind::Connect,
            describe_hop_refusal(&error, through.host, next),
        )
    })?;

    // `into_stream` is what makes a channel look like a socket; dropping the
    // stream closes the channel, so this leg's transport owns the hop it rides
    // on for exactly as long as it lives.
    Ok(channel.into_stream())
}

/// Explains why a jump host would not open the next leg's connection.
///
/// The distinction that matters is *whose* configuration is at fault, because
/// each answer sends the user to a different machine: the jump host's
/// forwarding policy, the target's own reachability, or the name and port the
/// hop was asked to dial. Kept apart from the tunnel module's near-twin on
/// purpose — a forwarding that is refused names a rule the user wrote, and a
/// hop that is refused names the machine that refused it.
fn describe_hop_refusal(error: &RusshError, gateway: &str, next: &Leg<'_>) -> String {
    let destination = format!("{}:{}", next.host, next.port);
    match error {
        RusshError::ChannelOpenFailure(ChannelOpenFailure::AdministrativelyProhibited) => format!(
            "{gateway} refused the connection to {destination} — most likely AllowTcpForwarding \
             is disabled in its sshd_config, or it restricts the destinations it will open"
        ),
        RusshError::ChannelOpenFailure(ChannelOpenFailure::ConnectFailed) => format!(
            "{gateway} could not reach {destination} — check the host name as {gateway} resolves \
             it, the port, and the target's own firewall"
        ),
        RusshError::ChannelOpenFailure(ChannelOpenFailure::ResourceShortage) => format!(
            "{gateway} is out of resources and would not open a connection to {destination}"
        ),
        RusshError::ChannelOpenFailure(ChannelOpenFailure::UnknownChannelType) => format!(
            "{gateway} does not implement direct-tcpip forwarding, so it cannot be used as a jump \
             host to reach {destination}"
        ),
        RusshError::ChannelOpenFailure(ChannelOpenFailure::Other { code, reason }) => {
            format!("{gateway} refused the connection to {destination} with code {code}: {reason}")
        }
        other => format!("{gateway} could not connect to {destination}: {other}"),
    }
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
    let mut buffered: usize = early_output.iter().map(output_size).sum();
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => return Ok(()),
            Some(ChannelMsg::Failure) => {
                return Err(Failure::new(SshErrorKind::Channel, request.refusal()));
            }
            Some(ChannelMsg::Data { data }) => {
                buffered += data.len();
                if buffered > rulogman_stream::OUTPUT_BUDGET {
                    return Err(Failure::new(
                        SshErrorKind::Channel,
                        "too much output before the shell was ready",
                    ));
                }
                early_output.push(SshEvent::Data(data.to_vec()));
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                buffered += data.len();
                if buffered > rulogman_stream::OUTPUT_BUDGET {
                    return Err(Failure::new(
                        SshErrorKind::Channel,
                        "too much output before the shell was ready",
                    ));
                }
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
        SshAuth::Agent => Ok(Credentials::Agent),
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

/// Opens the TCP connection to the chain's first leg, honouring the configured
/// connect timeout.
///
/// Only ever the first leg: every later one is reached through the leg before
/// it and never touches a socket of its own. See [`hop_stream`], which applies
/// the same timeout to the same step one hop further along.
async fn tcp_connect(leg: &Leg<'_>, connect_timeout_secs: u64) -> Result<TcpStream, Failure> {
    let address = (leg.host, leg.port);
    let attempt = TcpStream::connect(address);

    let connected = if connect_timeout_secs == 0 {
        attempt.await.map_err(|error| {
            Failure::new(
                SshErrorKind::Connect,
                format!("could not connect to {}: {error}", leg.label()),
            )
        })?
    } else {
        let limit = Duration::from_secs(connect_timeout_secs);
        match tokio::time::timeout(limit, attempt).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                return Err(Failure::new(
                    SshErrorKind::Connect,
                    format!("could not connect to {}: {error}", leg.label()),
                ));
            }
            Err(_) => {
                return Err(Failure::new(
                    SshErrorKind::Connect,
                    format!(
                        "connecting to {} timed out after {connect_timeout_secs}s",
                        leg.label()
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

/// Runs exactly the one authentication method this leg asks for.
///
/// Every message names the host, because a chain has several of them and
/// "the credentials were rejected" is unactionable until the user knows which
/// account on which machine to fix.
async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    leg: &Leg<'_>,
    credentials: Credentials,
) -> Result<(), Failure> {
    let result = match credentials {
        Credentials::Agent => return agent::authenticate(handle, leg).await,
        Credentials::Password(password) => {
            handle.authenticate_password(leg.username, password).await
        }
        Credentials::Key(key) => {
            let hash_alg = handle.best_supported_rsa_hash().await.map_err(|error| {
                Failure::new(
                    SshErrorKind::Io,
                    format!(
                        "could not negotiate a signature algorithm with {}: {error}",
                        leg.label()
                    ),
                )
            })?;
            handle
                .authenticate_publickey(
                    leg.username,
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
                "{} rejected the credentials for user {}",
                leg.label(),
                leg.username
            ),
        )),
        // A transport-level break during authentication is not a rejection, so
        // it must not be reported as one.
        Err(error) => Err(Failure::new(
            SshErrorKind::Io,
            format!(
                "the connection to {} broke during authentication: {error}",
                leg.label()
            ),
        )),
    }
}

/// One iteration's worth of work picked up by the main loop.
enum Step {
    Output(bool),
    /// The input writer stopped, which it only does when a write failed.
    InputFailed(std::io::Error),
    /// A message arrived on the shell channel.
    Message(Option<ChannelMsg>),
    /// A command arrived from the owning thread.
    Command(Option<Command>),
    /// The target's handler reported why the transport ended, or went away
    /// without saying.
    Lost(Option<Loss>),
    /// The keepalive timer fired.
    Keepalive,
    /// The liveness timer fired.
    Liveness,
    /// The remote shell closed its output [`EOF_GRACE`] ago and said nothing
    /// more since.
    EofGrace,
}

/// How the remote shell said it ended, as far as it has said so yet.
///
/// Folded from the channel messages that follow the end of output — `eof`,
/// `exit-status`, `exit-signal` — which arrive in either order, so the reason
/// the session ends with is worded from whatever was heard by the time the
/// channel closed or [`EOF_GRACE`] ran out.
#[derive(Debug, Default)]
struct ShellEnd {
    /// When the shell closed its output, or `None` while it has not.
    eof_at: Option<tokio::time::Instant>,
    /// The exit status it reported.
    status: Option<u32>,
    /// The signal it was killed by, as the SSH signal name (`KILL`, `TERM`).
    signal: Option<String>,
}

impl ShellEnd {
    /// The deadline by which the shell has to have finished, once it closed
    /// its output.
    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.eof_at.map(|eof| eof + EOF_GRACE)
    }

    /// Why the session ended, worded from what the shell reported; `fallback`
    /// is the plain fact of how the channel went away, used when the shell
    /// reported nothing about itself.
    fn reason(&self, fallback: &str) -> String {
        match (&self.signal, self.status) {
            (Some(signal), _) => format!("the remote shell was killed by signal {signal}"),
            (None, Some(status)) => format!("the remote shell exited with status {status}"),
            (None, None) => fallback.to_owned(),
        }
    }
}

/// Pumps the shell until the session ends.
async fn main_loop(
    config: &SshConfig,
    handle: &Handle<ClientHandler>,
    channel: &mut Channel<client::Msg>,
    commands: &mut UnboundedReceiver<Command>,
    lost: &mut oneshot::Receiver<Loss>,
    events: &rulogman_stream::Sender<SshEvent>,
    early_output: Vec<SshEvent>,
) -> Ending {
    let mut keepalive = keepalive_timer(config.keepalive_secs);
    let mut liveness = repeating_timer(LIVENESS_INTERVAL);

    let mut pending = std::collections::VecDeque::new();
    for event in early_output {
        queue_output(event, &mut pending);
    }

    // Keystrokes and pastes go out through their own writer, polled as one
    // more branch below, rather than being awaited in the loop body: a write
    // waits for the server's window, and a loop stuck in one stops reading
    // the channel, which is what lets the server — busy echoing a big paste
    // back — open the window again. It would also hold up `Disconnect` and
    // the keepalives for as long as the paste took.
    let (input, input_queue) = mpsc::unbounded::<Vec<u8>>();
    let mut writer = std::pin::pin!(write_input(channel.make_writer(), input_queue));
    let mut ended = ShellEnd::default();

    loop {
        // Every branch resolves to a value rather than acting in place: the
        // borrows taken by `select!` end with the statement, which leaves the
        // channel free to be written to below.
        let output = pending.front().cloned();
        let step = tokio::select! {
            sent = async {
                match output {
                    Some(event) => { let size = output_size(&event); events.send(event, size).await }
                    None => futures::future::pending().await,
                }
            } => Step::Output(sent),
            message = channel.wait(), if pending.is_empty() => Step::Message(message),
            command = commands.next() => Step::Command(command),
            loss = &mut *lost => Step::Lost(loss.ok()),
            error = &mut writer => Step::InputFailed(error),
            () = tick(&mut keepalive) => Step::Keepalive,
            _ = liveness.tick() => Step::Liveness,
            () = until(ended.deadline()) => Step::EofGrace,
        };

        match step {
            Step::Output(true) => {
                pending.pop_front();
            }
            Step::Output(false) => {
                return Ending::Closed("nobody is reading the session".to_owned());
            }
            Step::Message(Some(message)) => match message {
                ChannelMsg::Data { data } => {
                    queue_output(SshEvent::Data(data.to_vec()), &mut pending);
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    queue_output(SshEvent::ExtendedData(data.to_vec()), &mut pending);
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    ended.status = Some(exit_status);
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
                    ended.signal = Some(signal_name_text(&signal_name));
                }
                // Not the end yet: the status and the close are still to come,
                // and from OpenSSH they come *after* this. The output is over,
                // so nothing more will be queued, but the channel is read on
                // until it closes or [`EOF_GRACE`] runs out.
                ChannelMsg::Eof => {
                    ended.eof_at.get_or_insert_with(tokio::time::Instant::now);
                }
                ChannelMsg::Close => {
                    return Ending::Closed(ended.reason("the remote host closed the channel"));
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
            // The channel goes when the transport does, and it may say so a
            // moment before the handler says why.
            Step::Message(None) => return lost_ending(lost).await,
            Step::Command(Some(Command::Input(data))) => {
                // Cannot fail while `writer` is alive, and it has not stopped:
                // that would have ended the loop.
                let _ = input.unbounded_send(data);
            }
            Step::InputFailed(error) => {
                return Ending::Failed(
                    SshErrorKind::Io,
                    format!("could not write to the remote shell: {error}"),
                );
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
            Step::Lost(Some(loss)) => return loss,
            // The handler is dropped with russh's session task, so this is the
            // transport ending by a path that never reached `disconnected`.
            Step::Lost(None) => return connection_closed(),
            Step::Keepalive => {
                // russh sends the keepalives and counts the unanswered ones;
                // this tick only makes sure a loss is noticed within one
                // keepalive interval even if nothing reported it.
                if handle.is_closed() {
                    return lost_ending(lost).await;
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
            Step::EofGrace => {
                return Ending::Closed(ended.reason("the remote shell closed its output"));
            }
        }
    }
}

/// The ending for a transport found closed: the handler's reason if it
/// arrives within [`LOSS_GRACE`], a plain closed connection otherwise.
async fn lost_ending(lost: &mut oneshot::Receiver<Loss>) -> Ending {
    match tokio::time::timeout(LOSS_GRACE, lost).await {
        Ok(Ok(loss)) => loss,
        _ => connection_closed(),
    }
}

/// The ending for a transport that ended without saying why.
fn connection_closed() -> Ending {
    Ending::Closed("the connection was closed".to_owned())
}

/// Waits until `deadline`, or forever when there is none.
async fn until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// An `exit-signal`'s signal as a word: the SSH name, without the `SIG`
/// prefix the protocol leaves off, or what a non-standard one called itself.
fn signal_name_text(signal: &russh::Sig) -> String {
    match signal {
        russh::Sig::Custom(name) => name.clone(),
        other => format!("{other:?}"),
    }
}

/// Writes everything queued on `input` to the shell, in order, until a write
/// fails; answers that failure.
///
/// Never finishes otherwise: the main loop holds the sending half for as long
/// as it runs. `writer` is the channel's own [`AsyncWrite`], which waits for
/// the server's window by itself and borrows nothing from the channel, so it
/// can be polled alongside [`Channel::wait`].
async fn write_input(
    mut writer: impl AsyncWrite + Unpin,
    mut input: UnboundedReceiver<Vec<u8>>,
) -> std::io::Error {
    use tokio::io::AsyncWriteExt as _;
    while let Some(data) = input.next().await {
        if let Err(error) = writer.write_all(&data).await {
            return error;
        }
    }
    futures::future::pending().await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_end_is_worded_from_what_the_shell_reported() {
        let mut ended = ShellEnd::default();
        assert_eq!(
            ended.deadline(),
            None,
            "no grace runs before the end of output"
        );
        assert_eq!(ended.reason("the channel closed"), "the channel closed");

        ended.status = Some(3);
        assert_eq!(
            ended.reason("the channel closed"),
            "the remote shell exited with status 3"
        );
        // A signal says more than the status a shell killed by one reports.
        ended.signal = Some(signal_name_text(&russh::Sig::KILL));
        assert_eq!(
            ended.reason("the channel closed"),
            "the remote shell was killed by signal KILL"
        );
        ended.signal = Some(signal_name_text(&russh::Sig::Custom("XCPU".to_owned())));
        assert_eq!(
            ended.reason("the channel closed"),
            "the remote shell was killed by signal XCPU"
        );
    }
}
