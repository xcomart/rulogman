//! SSH transport for logman.
//!
//! This crate turns connection settings into a live remote shell without ever
//! blocking the caller. [`SshSession::connect`] hands back a handle plus a
//! stream of [`SshEvent`]s; the actual protocol work happens on a dedicated
//! thread with its own Tokio runtime, which makes the handle safe to hold from
//! a GUI thread.
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use logman_ssh::{AcceptAllVerifier, SshAuth, SshConfig, SshSession};
//!
//! let config = SshConfig::new("example.org", 22, "alice", SshAuth::Password("hunter2".into()));
//! let (session, mut events) = SshSession::connect(config, Arc::new(AcceptAllVerifier));
//!
//! session.send_input(b"uptime\n".to_vec());
//! while let Ok(Some(event)) = events.try_next() {
//!     println!("{event:?}");
//! }
//! ```
//!
//! The same transport also carries file transfers: [`SshSession::sftp`] hands
//! out an [`SftpClient`] that opens its own SFTP channel on first use, so
//! listing a directory or moving a file never interferes with the shell.
//!
//! It carries port forwardings too: every [`TunnelForward`] in the
//! configuration becomes a local listener once the shell is up, and each
//! connection it accepts is tunnelled over the session's own transport. A rule
//! that opens is reported as [`SshEvent::TunnelOpened`]; one that cannot be
//! opened is reported as [`SshEvent::TunnelFailed`] and leaves the session
//! running.
//!
//! Host key policy is deliberately left to the caller through the
//! [`HostKeyVerifier`] trait: this crate ships only [`AcceptAllVerifier`] and
//! [`RejectAllVerifier`], so that `known_hosts` storage lives in the
//! application layer.
//!
//! Secrets are contained by design — [`SshAuth`] and [`SshConfig`] implement
//! `Debug` by hand and render passwords, passphrases and key material as
//! `<redacted>`, and no error message or log line produced here includes them.

#![warn(missing_docs)]

mod config;
mod event;
mod session;
mod sftp;
mod tunnel;
mod verify;

pub use config::{
    DEFAULT_COLS, DEFAULT_CONNECT_TIMEOUT_SECS, DEFAULT_KEEPALIVE_SECS, DEFAULT_ROWS, DEFAULT_TERM,
    SshAuth, SshConfig, TunnelForward,
};
pub use event::{SshErrorKind, SshEvent};
pub use session::SshSession;
pub use sftp::{RemoteEntry, SftpClient, SftpError};
pub use verify::{
    AcceptAllVerifier, HostKeyVerifier, RejectAllVerifier, algorithm_name, fingerprint,
};
