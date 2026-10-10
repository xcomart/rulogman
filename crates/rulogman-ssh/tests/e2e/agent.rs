//! Real agent protocol tests, isolated from the developer's SSH agent.

use std::process::Command;

use russh::keys::signature::Signer;
use russh::keys::ssh_key::{HashAlg, Signature, private::KeypairData};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use super::*;

/// Gives each test its own process environment before any worker threads start.
/// No test changes the parent's SSH_AUTH_SOCK or contacts its agent.
fn isolated(name: &str, test: impl FnOnce()) {
    const CHILD: &str = "RULOGMAN_AGENT_TEST_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        test();
        return;
    }
    let directory = tempfile::tempdir().expect("creating the agent directory must succeed");
    let output = Command::new(std::env::current_exe().expect("the test executable has a path"))
        .args(["--exact", &format!("agent::{name}"), "--nocapture"])
        .env(CHILD, name)
        .env("SSH_AUTH_SOCK", directory.path().join("agent.sock"))
        .output()
        .expect("starting the isolated agent test must succeed");
    assert!(
        output.status.success(),
        "agent test {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// An in-process agent with a separate key store and a real Unix socket.
struct TestAgent {
    _runtime: Runtime,
    task: JoinHandle<()>,
    /// The order the production client will receive, including rejected keys.
    identities: Vec<PublicKey>,
}

impl TestAgent {
    fn start(keys: &[PrivateKey]) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the agent runtime must start");
        let keys = Arc::new(keys.to_vec());
        let identities = keys.iter().map(|key| key.public_key().clone()).collect();
        let listener = {
            let _entered = runtime.enter();
            let path =
                std::env::var_os("SSH_AUTH_SOCK").expect("the child has a private socket path");
            UnixListener::bind(path).expect("the agent socket must bind")
        };
        let task = runtime.spawn(async move {
            loop {
                let (stream, _) = listener
                    .accept()
                    .await
                    .expect("the agent must accept connections");
                tokio::spawn(serve(stream, Arc::clone(&keys)));
            }
        });
        Self {
            _runtime: runtime,
            task,
            identities,
        }
    }
}

/// Only the two agent protocol operations authentication needs. In particular,
/// RSA signing honors the request's hash flags; russh's bundled agent server
/// currently ignores those flags and always produces legacy SHA-1 signatures.
async fn serve(mut stream: UnixStream, keys: Arc<Vec<PrivateKey>>) {
    while let Ok(length) = stream.read_u32().await {
        assert!(length <= 256 * 1024, "unbounded agent request");
        let mut request = vec![0; length as usize];
        stream
            .read_exact(&mut request)
            .await
            .expect("the agent request must be complete");
        let reply = match request.first() {
            Some(11) => {
                let mut reply = vec![12]; // SSH_AGENT_IDENTITIES_ANSWER
                reply.extend_from_slice(&(keys.len() as u32).to_be_bytes());
                for key in keys.iter() {
                    put_string(&mut reply, &key.public_key().to_bytes().unwrap());
                    put_string(&mut reply, b"test identity");
                }
                reply
            }
            Some(13) => sign(&request[1..], &keys), // SSH_AGENTC_SIGN_REQUEST
            _ => vec![5],                           // SSH_AGENT_FAILURE
        };
        if stream.write_u32(reply.len() as u32).await.is_err()
            || stream.write_all(&reply).await.is_err()
        {
            break;
        }
    }
}

fn sign(mut request: &[u8], keys: &[PrivateKey]) -> Vec<u8> {
    let public = take_string(&mut request);
    let data = take_string(&mut request);
    let flags = u32::from_be_bytes(request.try_into().expect("signing flags are a u32"));
    let Some(key) = keys
        .iter()
        .find(|key| key.public_key().to_bytes().unwrap() == public)
    else {
        return vec![5];
    };
    let hash = match flags {
        2 => Some(HashAlg::Sha256),
        4 => Some(HashAlg::Sha512),
        0 => None,
        _ => panic!("unknown agent signing flags: {flags}"),
    };
    let signature: Signature = match key.key_data() {
        KeypairData::Rsa(rsa) => (rsa, hash).try_sign(data),
        other => {
            assert_eq!(flags, 0, "non-RSA keys must not receive RSA signing flags");
            other.try_sign(data)
        }
    }
    .expect("the agent must sign the authentication request");
    let mut encoded = Vec::new();
    put_string(&mut encoded, signature.algorithm().as_str().as_bytes());
    put_string(&mut encoded, signature.as_bytes());
    let mut reply = vec![14]; // SSH_AGENT_SIGN_RESPONSE
    put_string(&mut reply, &encoded);
    reply
}

fn put_string(packet: &mut Vec<u8>, value: &[u8]) {
    packet.extend_from_slice(&(value.len() as u32).to_be_bytes());
    packet.extend_from_slice(value);
}

fn take_string<'a>(packet: &mut &'a [u8]) -> &'a [u8] {
    let (length, rest) = packet.split_at(4);
    let (value, rest) = rest.split_at(u32::from_be_bytes(length.try_into().unwrap()) as usize);
    *packet = rest;
    value
}

impl Drop for TestAgent {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn key() -> PrivateKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("a test key must generate")
}

#[test]
fn agent_keys_are_tried_until_one_authenticates() {
    isolated("agent_keys_are_tried_until_one_authenticates", || {
        let agent = TestAgent::start(&[key(), key()]);
        let server = TestServer::with_public_key("alice", agent.identities[1].clone());
        let (session, mut events) = server.connect(
            server.config("alice", SshAuth::Agent),
            Arc::new(AcceptAllVerifier),
        );
        events.wait_ready();
        session.send_input(b"signed by the agent\n".to_vec());
        assert_eq!(
            events.read_line(b"signed by the agent\n"),
            b"signed by the agent\n"
        );
        assert!(session.is_alive());
    });
}

#[test]
fn an_agent_rsa_key_uses_a_supported_signature_hash() {
    isolated("an_agent_rsa_key_uses_a_supported_signature_hash", || {
        let rsa = PrivateKey::random(&mut rand::rng(), Algorithm::Rsa { hash: None })
            .expect("an RSA test key must generate");
        let agent = TestAgent::start(&[rsa]);
        let server = TestServer::with_public_key("alice", agent.identities[0].clone());
        let (session, mut events) = server.connect(
            server.config("alice", SshAuth::Agent),
            Arc::new(AcceptAllVerifier),
        );
        events.wait_ready();
        assert!(session.is_alive());
    });
}

#[test]
fn a_missing_agent_reports_the_socket_and_target() {
    isolated("a_missing_agent_reports_the_socket_and_target", || {
        let server = TestServer::with_public_key("alice", key().public_key().clone());
        let (_session, mut events) = server.connect(
            server.config("alice", SshAuth::Agent),
            Arc::new(AcceptAllVerifier),
        );
        let SshEvent::Error(SshErrorKind::KeyLoad, message) = events.wait_terminal() else {
            panic!(
                "missing agent must be a key-load failure: {:?}",
                events.seen()
            );
        };
        assert!(message.contains("SSH_AUTH_SOCK"), "{message}");
        assert!(message.contains("127.0.0.1"), "{message}");
        assert!(
            !events
                .seen()
                .iter()
                .any(|event| matches!(event, SshEvent::Ready))
        );
    });
}

#[test]
fn an_empty_agent_reports_that_it_has_no_identities() {
    isolated("an_empty_agent_reports_that_it_has_no_identities", || {
        let _agent = TestAgent::start(&[]);
        let server = TestServer::with_public_key("alice", key().public_key().clone());
        let (_session, mut events) = server.connect(
            server.config("alice", SshAuth::Agent),
            Arc::new(AcceptAllVerifier),
        );
        let SshEvent::Error(SshErrorKind::KeyLoad, message) = events.wait_terminal() else {
            panic!(
                "empty agent must be a key-load failure: {:?}",
                events.seen()
            );
        };
        assert!(message.contains("no identities"), "{message}");
    });
}

#[test]
fn rejected_agent_keys_report_an_authentication_failure() {
    isolated(
        "rejected_agent_keys_report_an_authentication_failure",
        || {
            let _agent = TestAgent::start(&[key(), key()]);
            let server = TestServer::with_public_key("alice", key().public_key().clone());
            let (_session, mut events) = server.connect(
                server.config("alice", SshAuth::Agent),
                Arc::new(AcceptAllVerifier),
            );
            let SshEvent::Error(SshErrorKind::Auth, message) = events.wait_terminal() else {
                panic!(
                    "rejected agent keys must be an auth failure: {:?}",
                    events.seen()
                );
            };
            assert!(message.contains("alice"), "{message}");
            assert!(message.contains("127.0.0.1"), "{message}");
        },
    );
}

#[test]
fn the_agent_authenticates_both_a_jump_host_and_its_target() {
    isolated(
        "the_agent_authenticates_both_a_jump_host_and_its_target",
        || {
            let agent = TestAgent::start(&[key(), key()]);
            let bastion = TestServer::with_public_key("jumper", agent.identities[0].clone());
            let target = TestServer::with_public_key("alice", agent.identities[1].clone());
            let verifier = RecordingVerifier::new();
            let mut config = target.config("alice", SshAuth::Agent);
            config.hops.push(HopSpec {
                host: "localhost".to_owned(),
                port: bastion.port,
                username: "jumper".to_owned(),
                auth: SshAuth::Agent,
            });
            let (session, mut events) =
                target.connect(config, Arc::clone(&verifier) as Arc<dyn HostKeyVerifier>);
            events.wait_ready();
            session.send_input(b"agent through the bastion\n".to_vec());
            assert_eq!(
                events.read_line(b"agent through the bastion\n"),
                b"agent through the bastion\n"
            );
            assert_eq!(bastion.forwarded_channels(), 1);
            assert_eq!(bastion.shell_requests(), 0);
            assert_eq!(target.shell_requests(), 1);
            assert_eq!(verifier.hosts_seen(), 2);
        },
    );
}
