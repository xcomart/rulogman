//! End-to-end tests for the local pty transport on Windows.
//!
//! Every test drives a real pseudoconsole with a real child process, so each
//! one waits against a deadline rather than on the stream: a shell that never
//! speaks must fail a test, not hang the suite.
//!
//! Two ConPTY facts shape what the assertions can say. Its output is not what
//! the child printed but a rendering of the console it printed into, escape
//! sequences and all, so expectations are substring matches and never whole
//! lines. And it opens every session by asking where the cursor is and stalls
//! the child until something answers — see [`drain`], which does here what the
//! terminal emulator does in the application.

#![cfg(windows)]

use std::time::{Duration, Instant};

use futures::channel::mpsc::{TryRecvError, UnboundedReceiver};
use rulogman_pty::{PtyConfig, PtyEvent, PtySession};

/// How long any single wait may take before the test gives up and reports what
/// it saw. Generous enough to survive a loaded CI machine.
const TIMEOUT: Duration = Duration::from_secs(15);

/// How long to sleep between polls of an empty stream.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The cursor-position report ConPTY asks for as it opens a session.
const CURSOR_QUERY: &str = "\u{1b}[6n";

/// The answer to it: the cursor is at row 1, column 1. ConPTY only cares that
/// an answer arrives, and every session here starts on a fresh console.
const CURSOR_REPLY: &[u8] = b"\x1b[1;1R";

/// Drains events until `done` is satisfied, the stream closes, or the deadline
/// passes; returns everything seen along the way.
///
/// `session` is what answers ConPTY's opening cursor query — without it the
/// shell never gets going, so pass `None` only once the handle is gone.
fn drain(
    session: Option<&PtySession>,
    events: &mut UnboundedReceiver<PtyEvent>,
    mut done: impl FnMut(&[PtyEvent]) -> bool,
) -> Vec<PtyEvent> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    let mut answered = 0;

    while Instant::now() < deadline {
        match events.try_recv() {
            Ok(event) => {
                seen.push(event);

                // In the application the terminal emulator replies to a device
                // status report as a matter of course; here the test stands in
                // for it, or nothing the shell prints would ever arrive.
                if let Some(session) = session {
                    let asked = output(&seen).matches(CURSOR_QUERY).count();
                    while answered < asked {
                        session.send_input(CURSOR_REPLY.to_vec());
                        answered += 1;
                    }
                }

                if done(&seen) {
                    break;
                }
            }
            // The session closed the stream; nothing more can arrive.
            Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Empty) => std::thread::sleep(POLL_INTERVAL),
        }
    }

    seen
}

/// Everything the shell wrote, as text — a console rendering, so callers must
/// match on substrings rather than on lines.
fn output(events: &[PtyEvent]) -> String {
    let bytes: Vec<u8> = events
        .iter()
        .filter_map(|event| match event {
            PtyEvent::Data(data) => Some(data.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn is_exited(event: &PtyEvent) -> bool {
    matches!(event, PtyEvent::Exited)
}

/// Configuration that runs `command` instead of the default shell, so that the
/// test's expectations do not depend on whoever runs it.
///
/// Wide on purpose: ConPTY renders into a console of exactly this width, and a
/// wrapped path is a path no substring match will find.
fn config_running(command: &[&str]) -> PtyConfig {
    let mut config = PtyConfig::new(200, 24);
    config.command = Some(command.iter().map(|part| (*part).to_owned()).collect());
    config
}

#[test]
fn publishes_ready_then_output_then_exited() {
    let (session, mut events) =
        PtySession::spawn(config_running(&["cmd.exe", "/c", "echo hello-rulogman"]));

    let seen = drain(Some(&session), &mut events, |seen| {
        seen.iter().any(is_exited)
    });

    assert!(
        matches!(seen.first(), Some(PtyEvent::Ready)),
        "the first event must be Ready, saw {seen:?}"
    );
    assert!(
        output(&seen).contains("hello-rulogman"),
        "the command's output is missing from {seen:?}"
    );
    assert!(
        matches!(seen.last(), Some(PtyEvent::Exited)),
        "the stream must end with Exited, saw {seen:?}"
    );

    drop(session);
}

#[test]
fn writes_input_to_the_shell_and_ends_on_shutdown() {
    let (session, mut events) = PtySession::spawn(config_running(&["cmd.exe"]));

    // Typed only once the opening cursor query has been answered: keystrokes
    // queued ahead of the answer would sit in the input pipe in front of it.
    let started = drain(Some(&session), &mut events, |seen| {
        output(seen).contains(CURSOR_QUERY)
    });
    assert!(
        output(&started).contains(CURSOR_QUERY),
        "the pty never asked for the cursor position, saw {started:?}"
    );

    session.send_input(b"echo typed-rulogman\r\n".to_vec());

    let echoed = drain(Some(&session), &mut events, |seen| {
        output(seen).contains("typed-rulogman")
    });
    assert!(
        output(&echoed).contains("typed-rulogman"),
        "input never reached the shell, saw {echoed:?}"
    );

    session.shutdown();

    let rest = drain(Some(&session), &mut events, |seen| {
        seen.iter().any(is_exited)
    });
    assert!(
        rest.iter().any(is_exited),
        "shutdown must end the stream with Exited, saw {rest:?}"
    );
}

#[test]
fn starts_the_shell_in_the_configured_directory() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut config = config_running(&["cmd.exe", "/c", "cd"]);
    config.cwd = Some(directory.path().to_path_buf());

    let (session, mut events) = PtySession::spawn(config);
    let seen = drain(Some(&session), &mut events, |seen| {
        seen.iter().any(is_exited)
    });

    // Compared by leaf name rather than by full path: a temporary directory
    // may be handed out under a path Windows reports back in another form, and
    // the generated name is unique enough on its own.
    let name = directory
        .path()
        .file_name()
        .expect("temporary directory has a name")
        .to_string_lossy()
        .into_owned();
    assert!(
        output(&seen).contains(&name),
        "`cd` did not report the configured directory {name}, saw {seen:?}"
    );

    drop(session);
}

#[test]
fn a_dropped_handle_ends_the_session() {
    let (session, mut events) = PtySession::spawn(config_running(&["cmd.exe"]));

    let ready = drain(Some(&session), &mut events, |seen| !seen.is_empty());
    assert!(matches!(ready.first(), Some(PtyEvent::Ready)), "{ready:?}");

    // `cmd.exe` would otherwise sit on the pseudoconsole forever; dropping the
    // handle has to hang up on it, or a closed tab would leak a process.
    drop(session);

    let rest = drain(None, &mut events, |seen| seen.iter().any(is_exited));
    assert!(
        rest.iter().any(is_exited),
        "dropping the handle must end the stream with Exited, saw {rest:?}"
    );
}

#[test]
fn a_command_that_does_not_exist_is_reported_as_an_error() {
    let (session, mut events) = PtySession::spawn(config_running(&[
        "rulogman-pty-test-shell-that-is-not-there",
    ]));

    let seen = drain(Some(&session), &mut events, |seen| !seen.is_empty());
    assert!(
        matches!(seen.first(), Some(PtyEvent::Error(_))),
        "a shell that cannot be started must fail the session, saw {seen:?}"
    );

    drop(session);
}

#[test]
fn the_handle_can_be_shared_across_threads() {
    // The application holds one from a GUI thread and hands references to it
    // to whatever else needs to write; both bounds have to hold.
    fn require<T: Send + Sync>() {}
    require::<PtySession>();
}
