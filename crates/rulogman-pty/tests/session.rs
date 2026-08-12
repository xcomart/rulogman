//! End-to-end tests for the local pty transport.
//!
//! Every test drives a real pty with a real child process, so each one waits
//! against a deadline rather than on the stream: a shell that never speaks
//! must fail a test, not hang the suite.

#![cfg(unix)]

use std::time::{Duration, Instant};

use futures::channel::mpsc::{TryRecvError, UnboundedReceiver};
use rulogman_pty::{PtyConfig, PtyEvent, PtySession};

/// How long any single wait may take before the test gives up and reports what
/// it saw. Generous enough to survive a loaded CI machine.
const TIMEOUT: Duration = Duration::from_secs(5);

/// How long to sleep between polls of an empty stream.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Drains events until `done` is satisfied, the stream closes, or the deadline
/// passes; returns everything seen along the way.
fn drain(
    events: &mut UnboundedReceiver<PtyEvent>,
    mut done: impl FnMut(&[PtyEvent]) -> bool,
) -> Vec<PtyEvent> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();

    while Instant::now() < deadline {
        match events.try_recv() {
            Ok(event) => {
                seen.push(event);
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

/// Everything the shell wrote, as text. The pty is in its default cooked mode,
/// so line endings are `\r\n` and callers should match on substrings.
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

/// Configuration that runs `command` instead of the login shell, so that the
/// test's expectations do not depend on whoever runs it.
fn config_running(command: &[&str]) -> PtyConfig {
    let mut config = PtyConfig::new(80, 24);
    config.command = Some(command.iter().map(|part| (*part).to_owned()).collect());
    config
}

#[test]
fn publishes_ready_then_output_then_exited() {
    let (session, mut events) =
        PtySession::spawn(config_running(&["/bin/sh", "-c", "printf ready"]));

    let seen = drain(&mut events, |seen| seen.iter().any(is_exited));

    assert!(
        matches!(seen.first(), Some(PtyEvent::Ready)),
        "the first event must be Ready, saw {seen:?}"
    );
    assert!(
        output(&seen).contains("ready"),
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
    let (session, mut events) = PtySession::spawn(config_running(&["/bin/cat"]));

    // Queued before the pty is even open: the command channel holds it until
    // the control thread is ready, which is the behaviour the UI relies on
    // when the user types into a tab that has just been created.
    session.send_input(b"hello\n".to_vec());

    let echoed = drain(&mut events, |seen| output(seen).contains("hello"));
    assert!(
        output(&echoed).contains("hello"),
        "input was not echoed back, saw {echoed:?}"
    );

    session.shutdown();

    let rest = drain(&mut events, |seen| seen.iter().any(is_exited));
    assert!(
        rest.iter().any(is_exited),
        "shutdown must end the stream with Exited, saw {rest:?}"
    );
}

#[test]
fn starts_the_shell_in_the_configured_directory() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut config = config_running(&["/bin/sh", "-c", "pwd"]);
    config.cwd = Some(directory.path().to_path_buf());

    let (session, mut events) = PtySession::spawn(config);
    let seen = drain(&mut events, |seen| seen.iter().any(is_exited));

    // Compared by leaf name rather than by full path: on macOS a temporary
    // directory is handed out as `/var/...` but reported by `pwd` as
    // `/private/var/...`.
    let name = directory
        .path()
        .file_name()
        .expect("temporary directory has a name")
        .to_string_lossy()
        .into_owned();
    assert!(
        output(&seen).contains(&name),
        "`pwd` did not report the configured directory {name}, saw {seen:?}"
    );

    drop(session);
}

#[test]
fn a_dropped_handle_ends_the_session() {
    let (session, mut events) = PtySession::spawn(config_running(&["/bin/cat"]));

    let ready = drain(&mut events, |seen| !seen.is_empty());
    assert!(matches!(ready.first(), Some(PtyEvent::Ready)), "{ready:?}");

    // `cat` would otherwise sit on the pty forever; dropping the handle has to
    // hang up on it, or a closed tab would leak a process.
    drop(session);

    let rest = drain(&mut events, |seen| seen.iter().any(is_exited));
    assert!(
        rest.iter().any(is_exited),
        "dropping the handle must end the stream with Exited, saw {rest:?}"
    );
}

#[test]
fn a_command_that_does_not_exist_is_reported_as_an_error() {
    let (session, mut events) =
        PtySession::spawn(config_running(&["/nonexistent/rulogman-pty-test-shell"]));

    let seen = drain(&mut events, |seen| !seen.is_empty());
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

#[test]
fn the_login_shell_has_a_bare_name() {
    let name = rulogman_pty::login_shell_name();
    assert!(!name.is_empty(), "the shell name must never be empty");
    assert!(
        !name.contains('/'),
        "the shell name must be a basename, got {name}"
    );
}
