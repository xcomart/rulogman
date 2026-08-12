//! Events published by a running [`PtySession`](crate::PtySession).
//!
//! A session never blocks its caller: everything it learns about the shell
//! arrives as a [`PtyEvent`] on the receiver handed out by
//! [`PtySession::spawn`](crate::PtySession::spawn).

use std::fmt;

/// A single observation about the life of a local shell.
///
/// The stream always ends with either [`PtyEvent::Exited`] or
/// [`PtyEvent::Error`], and the channel closes with it; no further events
/// follow those.
#[derive(Clone)]
pub enum PtyEvent {
    /// The shell is running.
    Ready,
    /// Bytes read from the pty master.
    Data(Vec<u8>),
    /// The shell ended; the stream carries nothing after this.
    Exited,
    /// The shell could not be started, or the pty failed. Terminal.
    Error(String),
}

impl fmt::Debug for PtyEvent {
    /// Summarises [`PtyEvent::Data`] by length instead of dumping the bytes:
    /// terminal traffic is both noisy and potentially sensitive.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready => f.write_str("Ready"),
            Self::Data(bytes) => write!(f, "Data({} bytes)", bytes.len()),
            Self::Exited => f.write_str("Exited"),
            Self::Error(message) => f.debug_tuple("Error").field(message).finish(),
        }
    }
}
