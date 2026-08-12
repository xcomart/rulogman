//! The parts both platform drivers are built from.
//!
//! The unix and Windows sessions are separate modules because the pty
//! underneath them is genuinely different, but they are the same machine: a
//! *control* thread that owns the pty and a *reader* thread that does nothing
//! but blocking reads, talking over one command channel in and one event
//! channel out. The command vocabulary and the outbound sink are therefore
//! written once, here, so that the two backends cannot drift apart on the
//! guarantees the public API makes.

use std::fmt;

use futures::channel::mpsc::UnboundedSender;
use parking_lot::Mutex;

use crate::event::PtyEvent;

/// Size of a single blocking read from the pty master.
///
/// Generous on purpose: a program dumping a large file should be drained in as
/// few reads — and published as few events — as possible.
pub(crate) const READ_BUFFER: usize = 32 * 1024;

/// A request sent to a session's control thread.
pub(crate) enum Command {
    /// Bytes to write to the pty master.
    Input(Vec<u8>),
    /// New terminal size, in columns and rows.
    Resize(u16, u16),
    /// Hang up on the shell and wind the session down.
    Shutdown,
    /// Posted by the reader thread once the master reports end of stream.
    ///
    /// On unix that is also how the control thread learns the shell is gone; on
    /// Windows it means only that there is nothing left to read.
    ReaderDone,
    /// Posted by the waiter thread once the child process has exited.
    ///
    /// Windows only, because ConPTY only closes the output pipe when the
    /// pseudoconsole itself is closed — the child's exit is invisible on the
    /// stream and has to be waited for on the process instead.
    #[cfg(windows)]
    ChildExited,
}

impl fmt::Debug for Command {
    /// Never renders keystrokes: input bytes routinely contain passwords typed
    /// into local prompts such as `sudo`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(data) => write!(f, "Input({} bytes)", data.len()),
            Self::Resize(cols, rows) => write!(f, "Resize({cols}, {rows})"),
            Self::Shutdown => f.write_str("Shutdown"),
            Self::ReaderDone => f.write_str("ReaderDone"),
            #[cfg(windows)]
            Self::ChildExited => f.write_str("ChildExited"),
        }
    }
}

/// The outbound event channel, shared by a session's worker threads.
///
/// All of them publish, and the promise that nothing follows the terminal
/// event has to hold across them. Closing the sink under the same lock that
/// guards a send is what keeps that promise: once the control thread has
/// published [`PtyEvent::Exited`], a reader still parked in a `read` can no
/// longer slip a late [`PtyEvent::Data`] in behind it.
pub(crate) struct EventSink(Mutex<Option<UnboundedSender<PtyEvent>>>);

impl EventSink {
    pub(crate) fn new(sender: UnboundedSender<PtyEvent>) -> Self {
        Self(Mutex::new(Some(sender)))
    }

    /// Publishes an event, unless the session has already ended.
    pub(crate) fn emit(&self, event: PtyEvent) {
        if let Some(sender) = self.0.lock().as_ref() {
            let _ = sender.unbounded_send(event);
        }
    }

    /// Publishes the terminal event and closes the stream.
    pub(crate) fn close(&self, last: PtyEvent) {
        let mut sender = self.0.lock();
        if let Some(sender) = sender.as_ref() {
            let _ = sender.unbounded_send(last);
        }
        *sender = None;
    }
}
