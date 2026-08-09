//! Terminal emulation for logman.
//!
//! This crate owns everything that turns a raw byte stream coming from an SSH
//! channel into something a GUI can draw, and everything that turns user input
//! back into the bytes a remote shell expects. It is intentionally free of any
//! GUI or transport dependency:
//!
//! * [`TerminalModel`] wraps `alacritty_terminal` and consumes bytes with
//!   [`TerminalModel::feed`].
//! * [`TerminalSnapshot`] is an `alacritty`-free description of one frame,
//!   produced by [`TerminalModel::snapshot`].
//! * [`TerminalTheme`] resolves the abstract cell colors into RGB, and ships a
//!   registry of built-in schemes selectable by id ([`TerminalTheme::builtin`],
//!   [`TerminalTheme::by_name`]) that the embedder can extend with schemes read
//!   from Windows Terminal-compatible [`SchemeFile`]s.
//! * [`encode_key`] / [`encode_paste`] encode user input.
//! * [`CwdTracker`] watches the same byte stream for the `OSC 7` / `OSC 1337`
//!   sequences that tell us which directory the remote shell is in.
//! * [`Charset`] transcodes both directions for a host that does not speak
//!   UTF-8, with [`CharsetDecoder`] carrying the inbound state across the chunk
//!   boundaries a multi-byte character is routinely split over.
//!
//! ```
//! use logman_term::{TerminalModel, TerminalTheme};
//!
//! let mut term = TerminalModel::new(80, 24, 1000, TerminalTheme::dark());
//! term.feed(b"hello");
//! assert_eq!(term.snapshot().lines[0].text(), "hello");
//! ```

#![deny(missing_docs)]

pub mod charset;
pub mod cwd;
pub mod keys;
pub mod model;
pub mod snapshot;
pub mod theme;

pub use charset::{Charset, CharsetDecoder};
pub use cwd::CwdTracker;
pub use keys::{KeyCode, KeyInput, TermModes, encode_key, encode_paste};
pub use model::TerminalModel;
pub use snapshot::{
    CursorPos, RunFlags, ScrollPosition, StyledRun, TerminalLine, TerminalSnapshot,
};
pub use theme::{CustomScheme, Rgb, SchemeEntry, SchemeFile, SchemeInfo, TerminalTheme};
