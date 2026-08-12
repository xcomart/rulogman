//! Local pty transport for rulogman.
//!
//! A shell on this machine is reached the same way a remote one is:
//! [`PtySession::spawn`] hands back a handle plus a stream of [`PtyEvent`]s,
//! and every blocking operation lives on threads this crate owns, so a GUI
//! thread can hold the handle and never wait on it.
//!
//! ```no_run
//! # #[cfg(any(unix, windows))]
//! # fn demo() {
//! use rulogman_pty::{PtyConfig, PtyEvent, PtySession};
//!
//! let (session, mut events) = PtySession::spawn(PtyConfig::new(80, 24));
//!
//! session.send_input(b"uptime\n".to_vec());
//! while let Ok(event) = events.try_recv() {
//!     if let PtyEvent::Data(bytes) = event {
//!         print!("{}", String::from_utf8_lossy(&bytes));
//!     }
//! }
//! session.shutdown();
//! # }
//! ```
//!
//! Two backends sit behind that one API, because a pty and a pseudoconsole are
//! not the same object, and neither are the rules for winding one down. The
//! public surface is identical on both, save for [`login_shell_name`], which
//! answers a question Windows does not ask.
//!
//! On unix the pty comes from `alacritty_terminal::tty` rather than from a
//! second implementation of the same thing: that module already handles
//! `openpty`, `setsid`, handing the slave over as the controlling terminal, and
//! the macOS detour through `/usr/bin/login` that makes the shell a genuine
//! login session. It ships unconditionally with the terminal emulator this
//! workspace already uses, so reusing it costs nothing.
//!
//! On Windows the pty is a ConPTY pseudoconsole, from `portable-pty`.
//! Alacritty's own Windows backend is built around its IOCP poller and cannot
//! be driven by the blocking reader thread this crate is shaped around, while
//! `portable-pty` presents the pseudoconsole as an ordinary `Read` and `Write`.
//!
//! Anywhere else — a target that is neither — the crate compiles to its
//! configuration and event types alone, so a build stays green while the
//! application gates its local-shell feature on the platforms that have one.

#![warn(missing_docs)]

mod config;
mod event;

#[cfg(any(unix, windows))]
mod driver;
#[cfg(unix)]
mod session;
#[cfg(unix)]
mod shell;
#[cfg(windows)]
mod windows;

pub use config::{DEFAULT_TERM, PtyConfig};
pub use event::PtyEvent;

#[cfg(unix)]
pub use session::PtySession;
#[cfg(unix)]
pub use shell::login_shell_name;
#[cfg(windows)]
pub use windows::PtySession;
