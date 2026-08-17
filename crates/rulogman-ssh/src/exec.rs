//! One-shot remote command execution on an established
//! [`SshSession`](crate::SshSession).
//!
//! Some things the application needs from the remote host are not files and
//! not keystrokes. Saving a root-owned file, for instance, is `sudo -S tee`
//! reading the new contents from its standard input — a program with an exit
//! status, an error stream, and nothing to do with either the terminal the user
//! is looking at or the SFTP channel the file panel uses. This module is that
//! third rider on the session's transport.
//!
//! The pieces fit together exactly as [`crate::sftp`]'s do:
//!
//! * [`ExecClient`] is the caller-facing handle. It is cheap to clone, `Send`
//!   and `Sync`, and its one method is `async`: the request is queued on the
//!   session's worker thread and the returned future resolves when the command
//!   has finished. Nothing here touches the network directly.
//! * [`serve`] runs on the session worker's runtime, owns the transport handle,
//!   and drives each request on its own task so a slow command does not hold up
//!   one queued behind it.
//!
//! The one structural difference from SFTP is that nothing is cached. SFTP
//! keeps its channel because the subsystem is a long conversation; `exec` is
//! not — the protocol allows exactly one `exec` request per channel, and the
//! channel dies with the process. So every request opens a channel, runs one
//! command on it, and closes it again. That is also what makes two commands
//! issued at once genuinely independent rather than serialised.
//!
//! Errors are plain English sentences carried in [`ExecError`], which is
//! deliberately *not* [`SftpError`](crate::SftpError): the two services fail
//! for unrelated reasons, and one shared vocabulary would end up describing
//! neither well. Once the session is gone, every call fails with
//! [`ExecError::Disconnected`] rather than hanging or panicking.

use std::sync::Arc;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use russh::client::{self, Handle};
use russh::{ChannelMsg, ChannelReadHalf, ChannelWriteHalf};

/// Ceiling on the standard output and standard error kept from one command,
/// counted together.
///
/// The commands this crate exists to run answer in sentences, not archives, so
/// there is no reason to let a mistake — `cat` on a log file, a program stuck
/// in a loop — grow a buffer without bound on the session's worker thread.
/// Output past the cap is dropped, but the channel is still drained to its
/// close, so the exit status arrives either way.
const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

/// Why a remote command could not be run.
///
/// Both variants render as a finished English sentence fragment suitable for
/// display; the application wraps it in a localised sentence but never rewrites
/// it. Neither carries credentials, and neither repeats the command line back —
/// see [`ExecClient::run`] for why that restraint is worth keeping even though
/// the command line is not supposed to hold a secret in the first place.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    /// The SSH session had already ended, or ended while the command was in
    /// flight. Retrying is pointless until a new session is connected.
    #[error("the SSH session is no longer connected")]
    Disconnected,
    /// The command could not be started, or the exchange with the server broke
    /// down before it finished. Says nothing about what the command itself
    /// would have done — a command that ran and failed reports that through
    /// [`ExecOutput::exit_status`] instead.
    #[error("{0}")]
    Failed(String),
}

/// What one remote command produced.
///
/// Bytes rather than strings throughout: the remote side's encoding is the
/// caller's business, and this crate has no way to know whether a given host
/// answers in UTF-8, EUC-KR or a mixture of the two.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput {
    /// The status the remote process exited with, when the server reported one.
    ///
    /// `None` means the channel closed without an `exit-status` message, which
    /// servers are permitted to skip — a process killed by a signal is the
    /// usual reason, but a terse server can simply omit it. There is no honest
    /// default to substitute here, so the caller has to decide by its own
    /// lights whether a command that did not say how it ended counts as having
    /// succeeded.
    pub exit_status: Option<u32>,
    /// Everything the command wrote to its standard output, in order, up to
    /// [`OUTPUT_LIMIT`] shared with `stderr`.
    pub stdout: Vec<u8>,
    /// Everything the command wrote to its standard error, in order, up to
    /// [`OUTPUT_LIMIT`] shared with `stdout`.
    ///
    /// Kept apart from `stdout` rather than interleaved, because the whole
    /// point of running a command this way is to be able to tell a diagnostic
    /// from an answer.
    pub stderr: Vec<u8>,
}

/// Handle for running commands on a live session.
///
/// Obtained from [`SshSession::exec`](crate::SshSession::exec). Cloning is
/// cheap; unlike [`SftpClient`](crate::SftpClient) the clones share no channel,
/// only the queue, because every command gets a channel of its own anyway.
///
/// Requests made before the session becomes ready are queued, not rejected;
/// requests made after it ends fail with [`ExecError::Disconnected`].
#[derive(Clone)]
pub struct ExecClient {
    /// Request channel to the exec service running on the session worker.
    requests: UnboundedSender<ExecRequest>,
}

impl ExecClient {
    /// Wraps the sending half of the session's exec request channel.
    pub(crate) fn new(requests: UnboundedSender<ExecRequest>) -> Self {
        Self { requests }
    }

    /// Runs `command` on the remote host, feeding it `stdin`, and waits for it
    /// to finish.
    ///
    /// The command is handed to the server as one string and interpreted by the
    /// user's login shell there, exactly as `ssh host '…'` would — so quoting is
    /// the caller's responsibility and so is every shell metacharacter in it.
    ///
    /// **Never put a secret in `command`.** The command line of a remote
    /// process is visible to `ps` for every account on that machine, and to the
    /// server's own logs besides. That is precisely why `stdin` is a parameter:
    /// a password for `sudo -S`, or a file's contents for `tee`, goes there,
    /// where nothing but the process itself can read it.
    ///
    /// `stdin` is written in full and then the input is closed, so a command
    /// that reads until end of file — `cat`, `tee`, `sudo -S` — terminates
    /// instead of waiting forever. Pass an empty vector for a command that
    /// reads nothing; the end-of-file is still sent.
    ///
    /// Returns `Ok` for every command that *ran*, however badly it went: a
    /// non-zero exit status and a page of standard error are an
    /// [`ExecOutput`], not an [`ExecError`]. The error type is reserved for the
    /// cases where there is no answer to report at all.
    pub async fn run(&self, command: String, stdin: Vec<u8>) -> Result<ExecOutput, ExecError> {
        // A closed request channel and a dropped reply sender mean the same
        // thing from here — the session worker is gone — so both collapse into
        // `Disconnected`.
        let (reply, answer) = oneshot::channel();
        self.requests
            .unbounded_send(ExecRequest::Run {
                command,
                stdin,
                reply,
            })
            .map_err(|_| ExecError::Disconnected)?;
        answer.await.map_err(|_| ExecError::Disconnected)?
    }
}

impl std::fmt::Debug for ExecClient {
    /// Reports only whether the client can still reach its session; there is
    /// no useful state to show and the channel itself is not printable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecClient")
            .field("connected", &!self.requests.is_closed())
            .finish()
    }
}

/// One unit of work handed to the exec service, with the channel to answer on.
///
/// An enum with a single variant, matching [`SftpRequest`](crate::sftp) in
/// shape so that a second kind of request — a command whose output is streamed
/// rather than collected, say — is an addition here rather than a rewrite.
/// Every field is owned: the request crosses a thread boundary and outlives the
/// call that made it.
pub(crate) enum ExecRequest {
    /// Run one command to completion and report everything it produced.
    Run {
        /// Command line, interpreted by the remote login shell.
        command: String,
        /// Bytes to write to the command's standard input before closing it.
        stdin: Vec<u8>,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<ExecOutput, ExecError>>,
    },
}

impl std::fmt::Debug for ExecRequest {
    /// Never renders `stdin`: it exists precisely so that secrets — a `sudo`
    /// password, a file's contents — have somewhere to go that logs cannot see.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Run { command, stdin, .. } => f
                .debug_struct("Run")
                .field("command", command)
                .field("stdin", &format_args!("{} bytes", stdin.len()))
                .finish_non_exhaustive(),
        }
    }
}

/// Serves exec requests for one session until the request channel closes.
///
/// Runs as a task on the session worker's runtime, alongside — never inside —
/// the shell's message loop and the SFTP service. Each request is spawned
/// separately so a command that takes a minute does not hold up one queued
/// behind it; there is no shared state between them to protect, because each
/// opens its own channel.
pub(crate) async fn serve<H>(handle: Arc<Handle<H>>, mut requests: UnboundedReceiver<ExecRequest>)
where
    H: client::Handler + 'static,
{
    while let Some(request) = requests.next().await {
        let handle = Arc::clone(&handle);
        tokio::spawn(async move {
            match request {
                ExecRequest::Run {
                    command,
                    stdin,
                    reply,
                } => {
                    // A dropped receiver is not an error: the caller simply lost
                    // interest, and the work is already done by the time the
                    // send fails.
                    let _ = reply.send(execute(&handle, command, stdin).await);
                }
            }
        });
    }
    log::debug!("exec service finished");
}

/// Opens a channel, runs one command on it, and collects everything it said.
async fn execute<H>(
    handle: &Handle<H>,
    command: String,
    stdin: Vec<u8>,
) -> Result<ExecOutput, ExecError>
where
    H: client::Handler + 'static,
{
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|error| broken(handle, "open a channel for the command", &error))?;

    // Split rather than kept whole, because feeding the command and reading it
    // have to happen at the same time. russh buffers only a hundred messages
    // per channel and blocks its session task once that fills, so a client that
    // writes a large standard input without reading meanwhile can wedge the
    // whole transport as soon as the command answers while still being fed.
    let (mut reader, writer) = channel.split();

    // `want_reply`, and the reply is waited for. Skipping the wait would be
    // legal but unhelpful: a server that will not run commands answers
    // `failure`, and the caller would otherwise sit there until the transport
    // gave out rather than being told what happened.
    writer
        .exec(true, command)
        .await
        .map_err(|error| broken(handle, "send the command to the server", &error))?;

    let mut output = ExecOutput::default();
    let mut collected = 0usize;
    await_reply(&mut reader, &mut output, &mut collected).await?;
    collect(&mut reader, &writer, stdin, &mut output, &mut collected).await;

    // Best effort, and a no-op when the server already closed: russh drops a
    // close for a channel it no longer knows. Sent so that a server which ends
    // with a bare `eof` still has the channel reclaimed, rather than leaving one
    // behind per command for the life of the session.
    let _ = writer.close().await;
    Ok(output)
}

/// Waits for the server's answer to the `exec` request.
///
/// Anything the command manages to say before the reply lands is collected
/// rather than dropped — a server is free to send output first — so a talkative
/// start loses nothing.
///
/// An `eof` *here* really is a refusal, unlike the one [`collect`] has to read
/// past: the reply is owed the moment the request is parsed, before the command
/// has been started and so before it can have anything to say, and one channel
/// carries its messages in order. A server that ends the output stream without
/// having answered is therefore a server that is never going to answer, and
/// waiting on for a `Success` that cannot come would turn a refusal into a
/// hang.
async fn await_reply(
    reader: &mut ChannelReadHalf,
    output: &mut ExecOutput,
    collected: &mut usize,
) -> Result<(), ExecError> {
    loop {
        match reader.wait().await {
            Some(ChannelMsg::Success) => return Ok(()),
            Some(ChannelMsg::Failure) => {
                return Err(ExecError::Failed(
                    "the server refused to run the command".to_owned(),
                ));
            }
            Some(ChannelMsg::Eof | ChannelMsg::Close) | None => {
                return Err(ExecError::Failed(
                    "the server closed the channel before accepting the command".to_owned(),
                ));
            }
            Some(other) => absorb(other, output, collected),
        }
    }
}

/// Feeds the command its standard input while reading everything it answers,
/// and returns once the channel is done.
///
/// "Done" is the subtle part. RFC 4254 orders the `exit-status` request against
/// nothing at all: it may arrive before the end of output, after it, or after a
/// second helping of output. OpenSSH — and so nearly every host this program
/// will ever talk to — sends `eof`, *then* `exit-status`, *then* `close`, which
/// means a reader that stops at the first `eof` never learns how the command
/// ended. So the loop treats `eof` as one more message and reads on. Only
/// `close`, or the channel dying under it, ends the loop outright.
///
/// The one shortcut taken is a safety net rather than an optimisation: once
/// `eof` has been seen *and* a status recorded, nothing more can usefully
/// arrive — the `eof` forbids further output and the status was the only thing
/// still owed — so a server that neglects to send its `close` cannot leave the
/// caller waiting for it.
///
/// The two run under one `select!` rather than one after the other, and the
/// feeding future is pinned across iterations so that a partly written input is
/// resumed rather than restarted. Ordering them would deadlock in both
/// directions: writing first stalls once the remote's answer fills russh's
/// per-channel buffer, and reading first never sends the end-of-file that a
/// command like `tee` is waiting for. Dropping the half-written feed when the
/// channel closes is deliberate too — a command that exits without reading its
/// input (`sudo` refusing a password, say) would otherwise leave this waiting
/// on a send window that will never open again.
async fn collect(
    reader: &mut ChannelReadHalf,
    writer: &ChannelWriteHalf<client::Msg>,
    stdin: Vec<u8>,
    output: &mut ExecOutput,
    collected: &mut usize,
) {
    let feed = async {
        if !stdin.is_empty() {
            writer.data_bytes(stdin).await?;
        }
        writer.eof().await
    };
    tokio::pin!(feed);
    let mut feeding = true;
    let mut output_ended = false;

    loop {
        let message = tokio::select! {
            result = &mut feed, if feeding => {
                feeding = false;
                // Logged rather than returned: the command has already started,
                // so whatever it makes of a truncated input is a real answer and
                // belongs in the output the caller gets.
                if let Err(error) = result {
                    log::debug!("could not feed the remote command its input: {error}");
                }
                continue;
            }
            message = reader.wait() => message,
        };

        match message {
            Some(ChannelMsg::Close) | None => return,
            Some(ChannelMsg::Eof) => output_ended = true,
            Some(other) => absorb(other, output, collected),
        }

        if output_ended && output.exit_status.is_some() {
            return;
        }
    }
}

/// Files one channel message into the output being built.
///
/// Only extended data on stream 1 is standard error; the protocol reserves the
/// other stream numbers and nothing in the wild sends them, so anything else is
/// traced and dropped rather than silently mixed into a stream it is not.
fn absorb(message: ChannelMsg, output: &mut ExecOutput, collected: &mut usize) {
    match message {
        ChannelMsg::Data { data } => append(&mut output.stdout, &data, collected),
        ChannelMsg::ExtendedData { data, ext: 1 } => append(&mut output.stderr, &data, collected),
        ChannelMsg::ExitStatus { exit_status } => output.exit_status = Some(exit_status),
        other => log::trace!("ignoring {other:?} from a remote command"),
    }
}

/// Appends as much of `data` to `sink` as [`OUTPUT_LIMIT`] still allows.
///
/// `collected` counts both streams together, so a command that floods one of
/// them cannot starve the budget of the other beyond the shared total.
fn append(sink: &mut Vec<u8>, data: &[u8], collected: &mut usize) {
    let room = OUTPUT_LIMIT.saturating_sub(*collected);
    let taken = room.min(data.len());
    if taken == 0 {
        return;
    }
    if let Some(slice) = data.get(..taken) {
        sink.extend_from_slice(slice);
        *collected = collected.saturating_add(taken);
    }
}

/// Turns a transport failure into the error that describes it.
///
/// A handle that has already closed means the session went away underneath the
/// request, which is a `Disconnected` and not a fault of the command; anything
/// else is reported as itself. `attempt` completes the sentence "could not …".
/// The command line is deliberately left out of it: it is not supposed to carry
/// a secret, but an error message is the last place worth relying on that.
fn broken<H>(handle: &Handle<H>, attempt: &str, error: &russh::Error) -> ExecError
where
    H: client::Handler,
{
    if handle.is_closed() {
        ExecError::Disconnected
    } else {
        ExecError::Failed(format!("could not {attempt}: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_without_a_session_reports_a_disconnect() {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        let client = ExecClient::new(sender);
        drop(receiver);

        let error = futures::executor::block_on(client.run("id".to_owned(), Vec::new()));
        assert_eq!(error, Err(ExecError::Disconnected));
    }

    #[test]
    fn errors_render_as_whole_sentences() {
        assert_eq!(
            ExecError::Disconnected.to_string(),
            "the SSH session is no longer connected"
        );
        assert_eq!(
            ExecError::Failed("the server refused to run the command".to_owned()).to_string(),
            "the server refused to run the command"
        );
    }

    #[test]
    fn output_is_capped_and_the_two_streams_share_the_budget() {
        let mut collected = 0usize;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        append(&mut stdout, &vec![b'o'; OUTPUT_LIMIT - 4], &mut collected);
        append(&mut stderr, b"12345678", &mut collected);
        // Four bytes of room were left, and the second stream got exactly them.
        assert_eq!(stdout.len(), OUTPUT_LIMIT - 4);
        assert_eq!(stderr, b"1234");
        assert_eq!(collected, OUTPUT_LIMIT);

        append(&mut stdout, b"more", &mut collected);
        assert_eq!(
            stdout.len(),
            OUTPUT_LIMIT - 4,
            "a full budget must take nothing further"
        );
    }

    #[test]
    fn a_debug_rendering_never_shows_the_input_bytes() {
        let request = ExecRequest::Run {
            command: "sudo -S tee /etc/hosts".to_owned(),
            stdin: b"hunter2\n127.0.0.1 localhost\n".to_vec(),
            reply: oneshot::channel().0,
        };

        let rendered = format!("{request:?}");
        assert!(
            rendered.contains("sudo -S tee /etc/hosts"),
            "the command is not a secret and belongs in the log: {rendered}"
        );
        assert!(
            !rendered.contains("hunter2"),
            "the standard input leaked into a debug rendering: {rendered}"
        );
    }
}
