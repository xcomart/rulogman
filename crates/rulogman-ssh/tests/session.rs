//! Hermetic tests for the SSH transport.
//!
//! Nothing here needs a server: the failure paths that are exercised all end
//! before, or instead of, a successful handshake.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::executor::block_on;
use rulogman_ssh::{
    AcceptAllVerifier, DEFAULT_CONNECT_TIMEOUT_SECS, HopSpec, HostKeyVerifier, RejectAllVerifier,
    SshAuth, SshConfig, SshErrorKind, SshEvent, SshSession, algorithm_name, fingerprint,
};

/// A throwaway ed25519 public key, together with the fingerprint OpenSSH
/// prints for it (`ssh-keygen -lf`).
const TEST_PUBLIC_KEY: &str =
    "AAAAC3NzaC1lZDI1NTE5AAAAICwcrJDrM1CScr55jykgFg/NV6C1q2zpz7EXpIsVNOlL";
const TEST_FINGERPRINT: &str = "SHA256:CCHPElk8HNQIXrhrTE8g8WpybVXvNVuP8YlkUi6gFXY";

/// A port that is closed on every sane machine, so connecting to it fails fast.
const CLOSED_PORT: u16 = 9;

/// Runs a session to completion and returns every event it produced.
///
/// A watchdog disconnects the session once `limit` elapses, so the test can
/// never hang: the worker thread honours a disconnect even while it is still
/// connecting.
fn run_session(config: SshConfig, limit: Duration) -> Vec<SshEvent> {
    let (session, events) = SshSession::connect(config, Arc::new(AcceptAllVerifier));
    let session = Arc::new(session);
    let done = Arc::new(AtomicBool::new(false));

    let watchdog = {
        let session = Arc::clone(&session);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let deadline = Instant::now() + limit;
            while !done.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    session.disconnect();
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };

    // The stream ends when the worker thread drops its event sender.
    let collected = block_on(events.collect::<Vec<_>>());
    done.store(true, Ordering::SeqCst);
    let _ = watchdog.join();
    collected
}

/// Returns the kind of the first `Error` event, if any.
fn first_error(events: &[SshEvent]) -> Option<SshErrorKind> {
    events.iter().find_map(|event| match event {
        SshEvent::Error(kind, _) => Some(*kind),
        _ => None,
    })
}

#[test]
fn new_config_fills_in_the_documented_defaults() {
    let config = SshConfig::new("example.org", 2222, "alice", SshAuth::Password("pw".into()));

    assert_eq!(config.host, "example.org");
    assert_eq!(config.port, 2222);
    assert_eq!(config.username, "alice");
    assert_eq!(config.term, "xterm-256color");
    assert_eq!(config.cols, 80);
    assert_eq!(config.rows, 24);
    assert_eq!(config.keepalive_secs, 30);
    assert_eq!(config.connect_timeout_secs, 15);
    assert_eq!(config.connect_timeout_secs, DEFAULT_CONNECT_TIMEOUT_SECS);
    // The two defaults that mean "behave exactly as this crate always has":
    // no jump hosts, and a login shell rather than a command.
    assert!(config.hops.is_empty());
    assert_eq!(config.command, None);
}

#[test]
fn debug_output_never_contains_a_password() {
    let auth = SshAuth::Password("hunter2".into());
    let rendered = format!("{auth:?}");

    assert!(!rendered.contains("hunter2"), "password leaked: {rendered}");
    assert_eq!(rendered, "Password(<redacted>)");

    let config = SshConfig::new("example.org", 22, "alice", auth);
    let rendered = format!("{config:?}");
    assert!(!rendered.contains("hunter2"), "password leaked: {rendered}");
    assert!(rendered.contains("<redacted>"));
    // Non-secret fields stay visible so the output is still useful.
    assert!(rendered.contains("example.org"));
    assert!(rendered.contains("alice"));
}

#[test]
fn debug_output_never_contains_a_passphrase_or_key_material() {
    let from_file = SshAuth::PrivateKeyFile {
        path: "/home/alice/.ssh/id_ed25519".into(),
        passphrase: Some("correct horse".into()),
    };
    let rendered = format!("{from_file:?}");
    assert!(
        !rendered.contains("correct horse"),
        "passphrase leaked: {rendered}"
    );
    assert!(rendered.contains("Some(<redacted>)"));
    // The path is not a secret and is needed to diagnose failures.
    assert!(rendered.contains("id_ed25519"));

    let from_memory = SshAuth::PrivateKeyData {
        pem: "-----BEGIN OPENSSH PRIVATE KEY-----\nsecretkeybytes\n".into(),
        passphrase: None,
    };
    let rendered = format!("{from_memory:?}");
    assert!(
        !rendered.contains("secretkeybytes"),
        "key material leaked: {rendered}"
    );
    assert!(rendered.contains("pem: <redacted>"));
    assert!(rendered.contains("passphrase: None"));
}

#[test]
fn debug_output_never_contains_a_hops_credentials() {
    let mut config = SshConfig::new("web-01", 22, "alice", SshAuth::Password("hunter2".into()));
    config.hops = vec![HopSpec {
        host: "bastion".into(),
        port: 2222,
        username: "jumper".into(),
        auth: SshAuth::Password("let-me-through".into()),
    }];
    config.command = Some("tail -f /var/log/syslog".into());

    let rendered = format!("{config:?}");
    assert!(
        !rendered.contains("let-me-through"),
        "a hop's password leaked: {rendered}"
    );
    // Everything a chain needs to be diagnosed by stays visible — including the
    // command, which is not a secret and is the first thing anyone looks for.
    assert!(rendered.contains("bastion"));
    assert!(rendered.contains("2222"));
    assert!(rendered.contains("jumper"));
    assert!(rendered.contains("tail -f /var/log/syslog"));
}

#[test]
fn session_handle_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SshSession>();
    assert_send_sync::<SshEvent>();
    assert_send_sync::<SshConfig>();
}

#[test]
fn connecting_to_a_closed_port_reports_a_connect_error() {
    let config = SshConfig::new(
        "127.0.0.1",
        CLOSED_PORT,
        "nobody",
        SshAuth::Password("pw".into()),
    );
    let events = run_session(config, Duration::from_secs(10));

    assert!(
        matches!(events.first(), Some(SshEvent::Connecting)),
        "expected the stream to open with Connecting, got {events:?}"
    );
    let terminal = events.last().expect("the session produced no events");
    assert!(
        matches!(
            terminal,
            SshEvent::Error(SshErrorKind::Connect, _) | SshEvent::Disconnected { .. }
        ),
        "expected a connect failure, got {terminal:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(event, SshEvent::Ready)),
        "the session must not become ready: {events:?}"
    );
}

#[test]
fn a_missing_private_key_reports_a_key_load_error() {
    let config = SshConfig::new(
        "127.0.0.1",
        CLOSED_PORT,
        "nobody",
        SshAuth::PrivateKeyFile {
            path: "this-key-does-not-exist.pem".into(),
            passphrase: Some("hunter2".into()),
        },
    );
    let events = run_session(config, Duration::from_secs(10));

    assert_eq!(
        first_error(&events),
        Some(SshErrorKind::KeyLoad),
        "expected a key load failure, got {events:?}"
    );
    // The failure must not disclose the passphrase, not even in its message.
    let rendered = format!("{events:?}");
    assert!(
        !rendered.contains("hunter2"),
        "passphrase leaked: {rendered}"
    );
}

#[test]
fn a_session_can_be_disconnected_before_it_is_ready() {
    let config = SshConfig::new(
        "127.0.0.1",
        CLOSED_PORT,
        "nobody",
        SshAuth::Password("pw".into()),
    );
    let (session, events) = SshSession::connect(config, Arc::new(AcceptAllVerifier));
    session.disconnect();
    // Calling it twice must be harmless.
    session.disconnect();

    let collected = block_on(events.collect::<Vec<_>>());
    assert!(!session.is_alive());
    assert!(
        !collected
            .iter()
            .any(|event| matches!(event, SshEvent::Ready)),
        "the session must not become ready: {collected:?}"
    );
}

#[test]
fn fingerprints_match_openssh() {
    let key = russh::keys::parse_public_key_base64(TEST_PUBLIC_KEY).expect("test key must parse");

    assert_eq!(fingerprint(&key), TEST_FINGERPRINT);
    assert_eq!(algorithm_name(&key), "ssh-ed25519");
}

#[test]
fn the_bundled_verifiers_apply_their_policy() {
    let key = russh::keys::parse_public_key_base64(TEST_PUBLIC_KEY).expect("test key must parse");

    assert!(block_on(AcceptAllVerifier.verify("example.org", 22, &key)));
    assert!(!block_on(RejectAllVerifier.verify("example.org", 22, &key)));
}
