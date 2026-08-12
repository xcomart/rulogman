//! Connection settings for an SSH session.
//!
//! Everything the transport needs in order to open a shell on a remote host
//! lives in [`SshConfig`]. Credentials are carried by [`SshAuth`]; both types
//! implement [`Debug`](std::fmt::Debug) by hand so that secrets are never
//! rendered into logs or panic messages.

use std::fmt;
use std::path::PathBuf;

/// Terminal type advertised to the remote host by default.
pub const DEFAULT_TERM: &str = "xterm-256color";

/// Default number of terminal columns.
pub const DEFAULT_COLS: u16 = 80;

/// Default number of terminal rows.
pub const DEFAULT_ROWS: u16 = 24;

/// Default keepalive interval, in seconds.
pub const DEFAULT_KEEPALIVE_SECS: u64 = 30;

/// Default TCP connect timeout, in seconds.
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;

/// Placeholder rendered in place of a secret by the manual `Debug` impls.
struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Maps `Some(secret)` to `Some(<redacted>)` so optional secrets keep their
/// shape in debug output without disclosing anything.
fn mask<T>(value: &Option<T>) -> Option<Redacted> {
    value.as_ref().map(|_| Redacted)
}

/// How to authenticate against the remote host.
///
/// Exactly one method is attempted per session — there is no fallback chain,
/// so a rejected password is reported as an authentication failure rather than
/// silently retried with a key.
#[derive(Clone)]
pub enum SshAuth {
    /// Keyboard-less password authentication (`ssh-userauth` `password`).
    Password(String),
    /// Public key authentication using a private key read from disk.
    PrivateKeyFile {
        /// Path of the private key file (OpenSSH or PKCS#8 PEM).
        path: PathBuf,
        /// Passphrase, when the key on disk is encrypted.
        passphrase: Option<String>,
    },
    /// Public key authentication using private key material held in memory.
    PrivateKeyData {
        /// The private key, in PEM form.
        pem: String,
        /// Passphrase, when the key material is encrypted.
        passphrase: Option<String>,
    },
}

impl fmt::Debug for SshAuth {
    /// Renders the authentication method without disclosing any secret.
    ///
    /// Passwords, passphrases and private key material are all replaced by
    /// `<redacted>`; only the key *path* — which is not sensitive and is
    /// useful when diagnosing a failure — is printed verbatim.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Password(_) => f.debug_tuple("Password").field(&Redacted).finish(),
            Self::PrivateKeyFile { path, passphrase } => f
                .debug_struct("PrivateKeyFile")
                .field("path", path)
                .field("passphrase", &mask(passphrase))
                .finish(),
            Self::PrivateKeyData { passphrase, .. } => f
                .debug_struct("PrivateKeyData")
                .field("pem", &Redacted)
                .field("passphrase", &mask(passphrase))
                .finish(),
        }
    }
}

/// One local port forwarding, the equivalent of OpenSSH's `-L`.
///
/// The listener is opened on the machine running rulogman once the session's
/// shell is up, and every connection it accepts is carried over the *same*
/// transport as the shell — no second connection is made, and no second
/// authentication happens. `remote_host` is resolved by the remote end, so it
/// may well be a name that exists only inside the remote network.
///
/// A forwarding that cannot bind does not fail the session: it is reported as
/// [`SshEvent::TunnelFailed`](crate::SshEvent::TunnelFailed) and the shell
/// carries on, the way `ssh -L` warns and still logs you in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelForward {
    /// Local address the listener binds, usually `127.0.0.1`.
    pub bind_address: String,
    /// Local TCP port to listen on.
    pub local_port: u16,
    /// Host to connect to from the remote end, as the remote end resolves it.
    pub remote_host: String,
    /// TCP port to connect to on `remote_host`.
    pub remote_port: u16,
}

/// Everything needed to open an interactive shell on a remote host.
#[derive(Clone)]
pub struct SshConfig {
    /// Hostname or IP address of the server.
    pub host: String,
    /// TCP port of the SSH service.
    pub port: u16,
    /// Remote account to log in as.
    pub username: String,
    /// The single authentication method to attempt.
    pub auth: SshAuth,
    /// Initial terminal width, in columns.
    pub cols: u16,
    /// Initial terminal height, in rows.
    pub rows: u16,
    /// `TERM` value requested for the remote pty. Defaults to
    /// [`DEFAULT_TERM`].
    pub term: String,
    /// Keepalive interval in seconds; `0` disables keepalives. Defaults to
    /// [`DEFAULT_KEEPALIVE_SECS`].
    pub keepalive_secs: u64,
    /// TCP connect timeout in seconds; `0` disables the timeout and defers to
    /// the operating system. Defaults to [`DEFAULT_CONNECT_TIMEOUT_SECS`].
    pub connect_timeout_secs: u64,
    /// Local port forwardings to open once the shell is running. Empty by
    /// default, which is a session that forwards nothing.
    pub tunnels: Vec<TunnelForward>,
}

impl SshConfig {
    /// Builds a configuration from the four mandatory settings, filling in the
    /// documented defaults for everything else.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        auth: SshAuth,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            auth,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            term: DEFAULT_TERM.to_owned(),
            keepalive_secs: DEFAULT_KEEPALIVE_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            tunnels: Vec::new(),
        }
    }
}

impl fmt::Debug for SshConfig {
    /// Written by hand rather than derived so that adding a secret-bearing
    /// field later cannot accidentally start leaking it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("auth", &self.auth)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("term", &self.term)
            .field("keepalive_secs", &self.keepalive_secs)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            // Addresses and ports only; a forwarding carries no secret.
            .field("tunnels", &self.tunnels)
            .finish()
    }
}
