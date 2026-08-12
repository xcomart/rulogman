//! Host key verification policy.
//!
//! The transport never decides on its own whether a server is trustworthy: it
//! delegates to a [`HostKeyVerifier`]. Persistent `known_hosts` handling is
//! deliberately *not* implemented here so that storage policy stays in the
//! application layer; this module only ships the two trivial policies that are
//! useful for tests and for bootstrapping.

use russh::keys::{HashAlg, PublicKey};

/// Decides whether the host key presented by a server should be trusted.
///
/// Implementations run on the SSH transport thread inside the key exchange, so
/// they must not block; ask the user asynchronously (e.g. through a channel)
/// rather than spinning.
#[async_trait::async_trait]
pub trait HostKeyVerifier: Send + Sync + 'static {
    /// Returns `true` to accept `key` for `host:port`, `false` to abort the
    /// connection.
    async fn verify(&self, host: &str, port: u16, key: &PublicKey) -> bool;
}

/// Trusts every host key unconditionally.
///
/// Convenient for tests and local development; using it in production defeats
/// the protection SSH host keys provide against machine-in-the-middle attacks.
pub struct AcceptAllVerifier;

#[async_trait::async_trait]
impl HostKeyVerifier for AcceptAllVerifier {
    async fn verify(&self, host: &str, port: u16, key: &PublicKey) -> bool {
        log::debug!(
            "accepting host key for {host}:{port} ({}) without verification",
            fingerprint(key)
        );
        true
    }
}

/// Rejects every host key.
///
/// Useful as a safe default and for testing the rejection path.
pub struct RejectAllVerifier;

#[async_trait::async_trait]
impl HostKeyVerifier for RejectAllVerifier {
    async fn verify(&self, host: &str, port: u16, key: &PublicKey) -> bool {
        log::debug!(
            "rejecting host key for {host}:{port} ({})",
            fingerprint(key)
        );
        false
    }
}

/// Formats the SHA-256 fingerprint of `key` the way OpenSSH does, e.g.
/// `SHA256:CCHPElk8HNQIXrhrTE8g8WpybVXvNVuP8YlkUi6gFXY`.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Returns the SSH algorithm name of `key`, e.g. `ssh-ed25519`.
pub fn algorithm_name(key: &PublicKey) -> String {
    key.algorithm().as_str().to_owned()
}
