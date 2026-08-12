//! Host key policy for rulogman.
//!
//! rulogman follows the *trust on first use* convention that OpenSSH popularised:
//! the first key a host presents is recorded, and from then on any other key for
//! that host, port and algorithm aborts the connection.
//!
//! The database lives in [`rulogman_core::KnownHosts`]; this module only supplies
//! the policy that sits between it and [`rulogman_ssh`].

use std::sync::{Arc, Mutex};

use rulogman_core::{HostKeyStatus, KnownHosts};
use rulogman_ssh::{HostKeyVerifier, algorithm_name, fingerprint};
use russh::keys::PublicKey;

/// Trust-on-first-use policy backed by [`rulogman_core::KnownHosts`].
///
/// The verifier is created once per process and shared by every session, so the
/// in-memory database is guarded by a mutex. Verification itself is synchronous
/// file I/O over a handful of lines, which is short enough to run inline on the
/// SSH transport thread.
///
/// Decisions:
///
/// * [`HostKeyStatus::Trusted`] — accepted silently.
/// * [`HostKeyStatus::Unknown`] — recorded, persisted, and accepted. A failure
///   to persist is logged but does not block the connection; the host simply
///   has to be trusted again next time.
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
    /// Trusted fingerprints, loaded once at construction time.
    known_hosts: Mutex<KnownHosts>,
}

impl TofuVerifier {
    /// Loads the trusted host database and builds a verifier around it.
    ///
    /// A database that cannot be read is not fatal: the verifier starts empty,
    /// which downgrades the policy to "trust the next key we see" rather than
    /// locking the user out of every host.
    pub fn new() -> Self {
        let known_hosts = KnownHosts::load().unwrap_or_else(|err| {
            log::warn!("starting with an empty known_hosts database: {err:#}");
            KnownHosts::default()
        });

        Self {
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

        match known_hosts.status(host, port, &algorithm, &presented) {
            HostKeyStatus::Trusted => {
                log::debug!("host key for {host}:{port} ({algorithm}) is already trusted");
                true
            }
            HostKeyStatus::Unknown => {
                known_hosts.trust(host, port, &algorithm, &presented);
                if let Err(err) = known_hosts.save() {
                    log::warn!(
                        "trusted {host}:{port} ({algorithm}) for this run only, \
                         the known_hosts file could not be written: {err:#}"
                    );
                } else {
                    log::info!(
                        "trusting new host key for {host}:{port} ({algorithm}): {presented}"
                    );
                }
                true
            }
            HostKeyStatus::Mismatch { stored_fingerprint } => {
                log::error!(
                    "host key mismatch for {host}:{port} ({algorithm}): \
                     expected {stored_fingerprint}, server offered {presented}. \
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
/// Returns a fresh [`TofuVerifier`], so each call re-reads the trusted host
/// database from disk. Callers that open many sessions should build it once and
/// clone the [`Arc`].
pub fn host_key_verifier() -> Arc<dyn HostKeyVerifier> {
    Arc::new(TofuVerifier::new())
}
