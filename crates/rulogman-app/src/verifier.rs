//! Host key policy for rulogman.
//!
//! rulogman follows the *trust on first use* convention that OpenSSH popularised:
//! the first key a host presents is recorded, and from then on any other key for
//! that host and port aborts the connection — another fingerprint of the same
//! algorithm, or a key of an algorithm the host was never seen with.
//!
//! The database lives in [`rulogman_core::KnownHosts`]; this module only supplies
//! the policy that sits between it and [`rulogman_ssh`].

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use rulogman_core::{HostKeyStatus, KnownHosts, known_hosts_file};
use rulogman_ssh::{HostKeyVerifier, algorithm_name, fingerprint};
use russh::keys::PublicKey;

/// Trust-on-first-use policy backed by [`rulogman_core::KnownHosts`].
///
/// One verifier is shared by every session of the process (see
/// [`host_key_verifier`]), and its mutex is held for the whole
/// read–decide–write cycle. Each decision re-reads the file first and a new key
/// is added to that fresh copy before it is written back, so concurrent
/// connects — restoring a workspace of tabs, say — cannot drop each other's new
/// records, and records written by another rulogman process or by hand since
/// the last connect are honoured. Verification is synchronous file I/O over a
/// handful of lines, which is short enough to run inline on the SSH transport
/// thread.
///
/// Decisions:
///
/// * [`HostKeyStatus::Trusted`] — accepted silently.
/// * [`HostKeyStatus::Unknown`] — recorded, persisted, and accepted. Only a
///   host and port with no key of any algorithm on record gets here. A failure
///   to persist is logged but does not block the connection; the host simply
///   has to be trusted again next time. A file that exists but cannot be read
///   is never written over: the key is trusted for this run only.
/// * [`HostKeyStatus::Mismatch`] — **rejected**, and both fingerprints are
///   logged at error level, because a changed host key can mean a
///   machine-in-the-middle attack.
///
/// A rejection is not silent: the transport reports it to the UI as
/// [`SshEvent::HostKey`](rulogman_ssh::SshEvent::HostKey) with `accepted: false`,
/// immediately followed by
/// [`SshEvent::Error`](rulogman_ssh::SshEvent::Error) carrying
/// [`SshErrorKind::HostKeyRejected`](rulogman_ssh::SshErrorKind::HostKeyRejected).
pub struct TofuVerifier {
    /// Location of the database, or `None` when no configuration directory
    /// could be determined; then trust lasts for this run only.
    path: Option<PathBuf>,
    /// The last copy read from `path`, plus keys trusted for this run only.
    /// Used as is when the file cannot be read.
    known_hosts: Mutex<KnownHosts>,
}

impl TofuVerifier {
    /// Builds a verifier around the default `known_hosts` location.
    ///
    /// A database that cannot be read is not fatal: the verifier starts empty,
    /// which downgrades the policy to "trust the next key we see" rather than
    /// locking the user out of every host.
    pub fn new() -> Self {
        let path = known_hosts_file()
            .inspect_err(|err| {
                log::warn!("host keys will be trusted for this run only: {err:#}");
            })
            .ok();
        Self::with_path(path)
    }

    /// Builds a verifier around an explicit database location.
    fn with_path(path: Option<PathBuf>) -> Self {
        let known_hosts = path
            .as_deref()
            .map(|path| {
                KnownHosts::load_from(path).unwrap_or_else(|err| {
                    log::warn!("starting with an empty known_hosts database: {err:#}");
                    KnownHosts::default()
                })
            })
            .unwrap_or_default();

        Self {
            path,
            known_hosts: Mutex::new(known_hosts),
        }
    }

    /// Applies the policy to one presented key.
    ///
    /// Kept synchronous and separate from [`HostKeyVerifier::verify`] so that
    /// the mutex guard provably cannot be held across an `await` point.
    fn decide(&self, host: &str, port: u16, key: &PublicKey) -> bool {
        let algorithm = algorithm_name(key);
        let presented = fingerprint(key);

        // A poisoned mutex only means some earlier verification panicked; the
        // database itself is a plain list and stays usable.
        let mut known_hosts = self.known_hosts.lock().unwrap_or_else(|poisoned| {
            log::warn!("recovering the known_hosts database after a poisoned lock");
            poisoned.into_inner()
        });
        // Only a file read just now may be written back: saving the cached copy
        // would drop whatever was recorded since it was read.
        let writable = self
            .path
            .as_deref()
            .filter(|path| match KnownHosts::load_from(path) {
                Ok(fresh) => {
                    *known_hosts = fresh;
                    true
                }
                Err(err) => {
                    log::warn!("using the cached known_hosts database: {err:#}");
                    false
                }
            });

        match known_hosts.status(host, port, &algorithm, &presented) {
            HostKeyStatus::Trusted => {
                log::debug!("host key for {host}:{port} ({algorithm}) is already trusted");
                true
            }
            HostKeyStatus::Unknown => {
                known_hosts.trust(host, port, &algorithm, &presented);
                match writable.map(|path| known_hosts.save_to(path)) {
                    Some(Ok(())) => log::info!(
                        "trusting new host key for {host}:{port} ({algorithm}): {presented}"
                    ),
                    Some(Err(err)) => log::warn!(
                        "trusted {host}:{port} ({algorithm}) for this run only, \
                         the known_hosts file could not be written: {err:#}"
                    ),
                    None => log::warn!(
                        "trusted {host}:{port} ({algorithm}) for this run only, \
                         the known_hosts file could not be read"
                    ),
                }
                true
            }
            HostKeyStatus::Mismatch {
                stored_algorithm,
                stored_fingerprint,
            } => {
                log::error!(
                    "host key mismatch for {host}:{port}: expected {stored_fingerprint} \
                     ({stored_algorithm}), server offered {presented} ({algorithm}). \
                     Refusing the connection; this may be a machine-in-the-middle attack."
                );
                false
            }
        }
    }
}

impl Default for TofuVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl HostKeyVerifier for TofuVerifier {
    async fn verify(&self, host: &str, port: u16, key: &PublicKey) -> bool {
        self.decide(host, port, key)
    }
}

/// The verifier every outgoing session is opened with.
///
/// Returns the one [`TofuVerifier`] of the process, built on first use, so
/// that its lock serialises every update of the `known_hosts` file.
pub fn host_key_verifier() -> Arc<dyn HostKeyVerifier> {
    static VERIFIER: OnceLock<Arc<TofuVerifier>> = OnceLock::new();
    VERIFIER
        .get_or_init(|| Arc::new(TofuVerifier::new()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    use russh::keys::parse_public_key_base64;

    const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIEe3NDNdHe2UVXTE3ABF72/jYBVKaMWcr0mUQgEjy1eF";
    const KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAICxi6DsQkVIz3YmOA+LdkvToQJI1PBBz6O2Hh+JSGcJt";

    fn key(base64: &str) -> PublicKey {
        parse_public_key_base64(base64).expect("test key must parse")
    }

    #[test]
    fn verifiers_with_stale_copies_keep_each_others_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("known_hosts");
        // Both load the empty file before either connects, as two windows or
        // two restored tabs used to with a verifier per session.
        let first = TofuVerifier::with_path(Some(path.clone()));
        let second = TofuVerifier::with_path(Some(path.clone()));

        assert!(first.decide("one.example", 22, &key(KEY_A)));
        assert!(second.decide("two.example", 22, &key(KEY_B)));

        let saved = KnownHosts::load_from(&path).expect("load");
        let a = fingerprint(&key(KEY_A));
        let b = fingerprint(&key(KEY_B));
        assert_eq!(
            saved.status("one.example", 22, "ssh-ed25519", &a),
            HostKeyStatus::Trusted,
            "the second save must not drop the first record"
        );
        assert_eq!(
            saved.status("two.example", 22, "ssh-ed25519", &b),
            HostKeyStatus::Trusted
        );
        // The first verifier sees what the second recorded.
        assert!(!first.decide("two.example", 22, &key(KEY_A)));
    }

    #[test]
    fn concurrent_connects_through_one_verifier_all_persist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("known_hosts");
        let verifier = Arc::new(TofuVerifier::with_path(Some(path.clone())));

        let threads: Vec<_> = (0..8)
            .map(|n| {
                let verifier = Arc::clone(&verifier);
                std::thread::spawn(move || {
                    verifier.decide(&format!("h{n}.example"), 22, &key(KEY_A))
                })
            })
            .collect();
        for thread in threads {
            assert!(thread.join().expect("thread"));
        }

        let saved = KnownHosts::load_from(&path).expect("load");
        let a = fingerprint(&key(KEY_A));
        for n in 0..8 {
            assert_eq!(
                saved.status(&format!("h{n}.example"), 22, "ssh-ed25519", &a),
                HostKeyStatus::Trusted,
                "h{n} lost"
            );
        }
    }

    #[test]
    fn an_unreadable_file_is_not_written_over() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("known_hosts");
        let garbage = b"\xff\xfe not utf-8\n";
        std::fs::write(&path, garbage).expect("write");

        let verifier = TofuVerifier::with_path(Some(path.clone()));
        assert!(verifier.decide("one.example", 22, &key(KEY_A)));
        assert!(
            verifier.decide("one.example", 22, &key(KEY_A)),
            "trusted for the rest of the run"
        );
        assert_eq!(std::fs::read(&path).expect("read"), garbage);
    }
}
