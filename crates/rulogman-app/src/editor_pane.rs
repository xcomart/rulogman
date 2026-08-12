//! One file, open for editing, in a tab of its own.
//!
//! [`crate::editor`] is a text widget and nothing more: it holds a rope and
//! knows how to draw and edit it, and it deliberately has no idea where the
//! bytes came from. This module is the other half — the part that reads a file
//! off a [`FileSource`], decides how it is to be spelled back, tracks whether
//! it still matches what is on disk, and writes it out again.
//!
//! # Reading and writing through the panel's own trait
//!
//! [`FileSource`] has no "give me the contents" call, on purpose: the file panel
//! never needed one, and a trait method is a promise all three backends have to
//! keep. So a load is a [`FileSource::copy_out`] into a temporary directory
//! followed by a plain [`std::fs::read`], and a save is the mirror of that. The
//! detour costs one extra copy of a file that is capped at [`MAX_EDIT_BYTES`]
//! anyway, and it means SFTP, the local filesystem and WSL all arrive here
//! already working.
//!
//! # What is refused, and why it is refused early
//!
//! Two things: a file larger than [`MAX_EDIT_BYTES`], and one whose bytes are
//! not valid text in the charset in force. The size is checked by the *panel*,
//! off the listing it already has, so nothing is transferred before the refusal;
//! the encoding can only be checked once the bytes are here. The size is not a
//! limitation of the buffer — the rope would hold a gigabyte — but of the
//! transfer.
//!
//! # The charset the bytes are read in
//!
//! A file opens in its session's charset, the same one the terminal decodes that
//! host with, and the status bar names it beside the file type. Only UTF-8 is
//! ever refused: it is the one encoding whose bytes can be *wrong*, and a file
//! it cannot decode is one the editor would silently corrupt on save. Every
//! legacy charset accepts any byte sequence, so switching the picker to one of
//! them always shows something — and switching back to UTF-8 may well refuse.
//!
//! Picking another charset re-reads the file rather than re-decoding what is in
//! hand, because the bytes are not kept: the buffer holds text, and the rope
//! behind it is the only copy. That is also why a switch is refused while there
//! are unsaved changes — the reload replaces the buffer wholesale, undo history
//! and all.
//!
//! # What is preserved across a round trip
//!
//! A byte order mark and the line ending style. Both are stripped on the way in
//! — the buffer holds `\n` and no BOM, which is what every command in the editor
//! assumes — and put back on the way out, so opening a CRLF file with a BOM and
//! saving it unchanged writes the same bytes it read.

use std::sync::Arc;

use gpui::{
    Action, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding,
    MouseButton, MouseDownEvent, Pixels, Point, SharedString, Subscription, Window, actions, div,
    prelude::*, px,
};
use rulogman_term::{Charset, TerminalTheme};

use crate::SHORTCUT_MODIFIER;
use crate::editor::{EditorEvent, EditorView, Language, palette_for};
// The editor's own commands, reached through the module rather than imported
// one at a time: one of them is called `Copy`, which is also the name of the
// trait this file derives on two of its types.
use crate::editor::view as editor_actions;
use crate::files::{FileError, FileSource};
use crate::i18n::ts;
use crate::session::Session;
use crate::terminal_view::resolve_font;
use crate::ui::{ContextMenu, MenuEntry, theme, tooltip_label};

actions!(
    rulogman_editor_pane,
    [
        /// Write the buffer back to the file it was opened from.
        SaveFile,
    ]
);

/// Key context of the pane, which wraps the editor's own.
///
/// Separate from the editor's `Editor` context because the two answer different
/// questions: that one is "the keyboard is in a text buffer", this one is "that
/// buffer belongs to a file". Saving needs the second, and binding it in the
/// first would offer it to a text widget with nowhere to save to.
const KEY_CONTEXT: &str = "EditorPane";

/// Largest file the editor will open, in bytes.
///
/// Not the rope's limit — that is measured in gigabytes — but the transfer's:
/// every load copies the whole file across the session and back out of a
/// temporary directory, with no progress bar and no way to cancel it, so the cap
/// is set where that round trip stays imperceptible on a slow link.
pub const MAX_EDIT_BYTES: u64 = 10 * 1024 * 1024;

/// Height of the pane's header strip, in pixels.
const HEADER_HEIGHT: f32 = 26.;

/// The dirty marker: a filled dot beside the file name.
const DIRTY_MARK: &str = "\u{25cf}";

/// The close button's glyph, the same multiplication sign the tab strip uses.
const CLOSE_MARK: &str = "\u{00d7}";

/// How a file spells the end of a line.
///
/// Two cases and no "mixed", because a file with both is still written back one
/// way: whichever dominated when it was read. Guessing per line would mean
/// storing the endings in the buffer, and the buffer holds text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Newlines {
    /// `\n`, as everything but Windows writes.
    Lf,
    /// `\r\n`.
    Crlf,
}

/// A file's contents, decoded, plus what has to be put back to write it out
/// unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFile {
    /// The text, with `\n` line endings and no byte order mark.
    pub text: String,
    /// How the file spelled its line endings.
    pub newlines: Newlines,
    /// Whether the file started with a UTF-8 byte order mark.
    ///
    /// Only ever true under UTF-8: a byte order mark is a Unicode device, and
    /// the legacy charsets have nothing to put back.
    pub bom: bool,
    /// The charset the bytes were read in, and the one they will be written in.
    ///
    /// One field for both directions on purpose. A file read as EUC-KR that
    /// saved as UTF-8 would be a silent conversion nobody asked for, and the
    /// only honest way to change the answer is the picker — which re-reads.
    pub charset: Charset,
}

/// The UTF-8 byte order mark, as it appears at the head of a file.
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

impl TextFile {
    /// Decodes `bytes` as the editor's buffer in `charset`, or says why it
    /// cannot.
    ///
    /// Only the UTF-8 arm can fail, and it is strict: a byte order mark comes
    /// off the front and the rest has to be valid, because half-decoded UTF-8 is
    /// how a file gets corrupted on the way back out. Every other charset here
    /// is total — each of them reads *some* character out of every byte — so its
    /// arm is lossy and infallible, and a wrong guess shows as mojibake the user
    /// can see and correct with the picker rather than as a refusal they cannot.
    ///
    /// The line ending style is decided by *dominance* rather than by the first
    /// one seen: a file of ten thousand CRLF lines with one stray `\n` in it is
    /// a CRLF file, and writing it back as LF would rewrite every line of a diff.
    /// It is decided on the *decoded* text, since only the text says where the
    /// line breaks are — in a stateful charset a `\n` byte need not be one.
    pub fn decode(bytes: &[u8], charset: Charset) -> Result<Self, LoadError> {
        let (text, bom) = if charset.is_utf8() {
            let bom = bytes.starts_with(&BOM);
            let body = if bom { &bytes[BOM.len()..] } else { bytes };
            let text = std::str::from_utf8(body).map_err(|_| LoadError::NotUtf8)?;
            (text.to_owned(), bom)
        } else {
            // The malformed flag is dropped rather than reported: mojibake is
            // legible on screen in a way a count of bad bytes is not, and the
            // fix — pick another charset — is one click away either way.
            (charset.decode_lossy(bytes).0, false)
        };

        // Every `\r\n` is also an `\n`, so the LF count is the total and the
        // difference is the lone ones.
        let total = text.matches('\n').count();
        let crlf = text.matches("\r\n").count();
        let newlines = if crlf * 2 > total {
            Newlines::Crlf
        } else {
            Newlines::Lf
        };

        // Unconditional, not only for a file that is mostly CRLF: the minority
        // endings of a mixed file have to go too, or the buffer would carry a
        // carriage return the editor draws as a character and the caret can be
        // put beside. Which style *dominated* decides how it is written back,
        // not what the buffer holds.
        Ok(Self {
            text: text.replace("\r\n", "\n"),
            newlines,
            bom,
            charset,
        })
    }

    /// The bytes to write for `text`, in this file's own spelling, and whether
    /// anything had to be substituted to get them.
    ///
    /// Takes the text rather than reading [`TextFile::text`] because the copy in
    /// here is the one that was *loaded*; what gets written is whatever the
    /// buffer holds now.
    ///
    /// The flag is only ever set by a legacy charset, which can hold a few
    /// thousand characters where the buffer can hold every one: a `가` pasted
    /// into a windows-1252 file has no byte, and goes out as `?`. It is returned
    /// rather than swallowed because that is a loss the user is the only one who
    /// can decide is acceptable — see [`Charset::encode_lossy`].
    pub fn encode(&self, text: &str) -> (Vec<u8>, bool) {
        // Both arms normalise first, because the buffer can have acquired a
        // carriage return since it was loaded — pasted out of a Windows editor,
        // say. Without it a CRLF file would come back out as `\r\r\n` and an LF
        // file would keep an ending it never had.
        let normalised = text.replace("\r\n", "\n");
        let body = match self.newlines {
            Newlines::Crlf => normalised.replace('\n', "\r\n"),
            Newlines::Lf => normalised,
        };
        if !self.charset.is_utf8() {
            // No byte order mark: `bom` is only ever set on a UTF-8 file, and
            // the legacy charsets have no such mark to write anyway.
            return self.charset.encode_lossy(&body);
        }
        let mut bytes = Vec::with_capacity(body.len() + BOM.len());
        if self.bom {
            bytes.extend_from_slice(&BOM);
        }
        bytes.extend_from_slice(body.as_bytes());
        (bytes, false)
    }
}

/// Why a file could not be opened for editing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// Larger than [`MAX_EDIT_BYTES`].
    TooLarge,
    /// The bytes are not valid UTF-8, and so cannot be shown without corrupting
    /// them on the way back out.
    ///
    /// Reachable only while the charset in force *is* UTF-8 — every other one
    /// the picker offers decodes any byte sequence — which makes this the
    /// prompt to pick one of the others.
    NotUtf8,
    /// The file could not be fetched at all.
    Transport(FileError),
}

impl From<FileError> for LoadError {
    fn from(error: FileError) -> Self {
        Self::Transport(error)
    }
}

/// Fetches `dir/name` from `source` and hands back its bytes.
///
/// The temporary directory is a directory rather than a bare temporary file
/// because [`FileSource::copy_in`] keeps the local file's *name*, and the save
/// that follows has to hand it a file called exactly what the remote one is
/// called. Both ends of the round trip therefore work in a scratch directory of
/// their own, which is removed when the `TempDir` drops — including on the
/// error paths, which is why it is bound rather than passed through.
pub async fn read_file(
    source: &Arc<dyn FileSource>,
    dir: &str,
    name: &str,
) -> Result<Vec<u8>, FileError> {
    let scratch = scratch_dir()?;
    let local = scratch.path().join(name);
    source
        .copy_out(&file_path(dir, name), local.clone(), None)
        .await?;
    std::fs::read(&local).map_err(|error| local_error(&local, &error))
}

/// Writes `bytes` back to `dir/name` on `source`.
///
/// **Not atomic**, deliberately. The usual shape — write a sibling temporary
/// file and rename it over the target — depends on the rename replacing an
/// existing file, and over SFTP that is exactly what is not portable: the
/// version 3 protocol most servers still speak leaves the behaviour of
/// `SSH_FXP_RENAME` over an existing path unspecified, so OpenSSH refuses it
/// while others silently replace, and the `posix-rename@openssh.com` extension
/// that fixes it is not something every server offers. A save that worked
/// against one host and failed against the next would be worse than the window
/// this leaves open, and the file panel has no way to recover a half-renamed
/// target either. So the file is overwritten in place, and a save that fails
/// part way says so on the pane rather than being silently repaired.
///
/// `as_root` chooses which of the source's two write calls carries the staging
/// file the last step, and nothing else about the write: the same bytes are
/// staged under the same name in the same private directory either way, because
/// the difference between the two saves is only in who does the writing. A flag
/// rather than a second function for exactly that reason — two copies of the
/// staging would be two places for the staged name to stop matching the
/// target's, and the name is the whole reason the staging directory exists.
pub async fn write_file(
    source: &Arc<dyn FileSource>,
    dir: &str,
    name: &str,
    bytes: &[u8],
    as_root: bool,
) -> Result<(), FileError> {
    let scratch = scratch_dir()?;
    let local = scratch.path().join(name);
    std::fs::write(&local, bytes).map_err(|error| local_error(&local, &error))?;
    if as_root {
        source.copy_in_as_root(local, dir).await?;
    } else {
        source.copy_in(local, dir, None).await?;
    }
    Ok(())
}

/// A private directory on this machine for one transfer's staging file.
fn scratch_dir() -> Result<tempfile::TempDir, FileError> {
    tempfile::TempDir::new().map_err(|error| {
        FileError::Local(format!(
            "a temporary directory could not be created: {error}"
        ))
    })
}

/// The sentence a failed local read or write reports.
fn local_error(path: &std::path::Path, error: &std::io::Error) -> FileError {
    FileError::Local(format!("{} could not be used: {error}", path.display()))
}

/// Joins a source directory and an entry name the way the file panel does.
///
/// Sources spell their paths POSIX style whatever this machine writes — see
/// [`crate::files`] — so this is string arithmetic and not `PathBuf` work.
///
/// Public because the workspace asks "is this file already open?" before it
/// builds a pane, and the answer has to be spelled the way an open pane spells
/// its own path.
pub fn file_path(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else if directory.ends_with('/') {
        format!("{directory}{name}")
    } else {
        format!("{directory}/{name}")
    }
}

/// What the pane tells the workspace about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorPaneEvent {
    /// A press landed inside the pane, which makes it the active one.
    ///
    /// Reported rather than left to gpui's focus listeners for the same reason
    /// [`crate::terminal_view::PaneFocused`] is: a focus listener runs after the
    /// frame that carried the click was drawn, so the accent frame would trail
    /// the press by one input event.
    Focused,
    /// The close button was pressed. The workspace decides what closing means.
    CloseRequested,
    /// The save the pane was asked to make *before* closing has landed, and
    /// every edit in the buffer is on disk. The pane may now be taken down.
    ///
    /// Only [`EditorPane::save_and_close`] can lead here; an ordinary save —
    /// the header button, <kbd>Ctrl</kbd>+<kbd>S</kbd> — is silent however well
    /// it goes, or saving a file would be a way of closing it.
    SavedForClose,
}

/// Whether a save that has just landed may take the pane down with it.
///
/// Three conditions, and the last is the interesting one: `saved` is the
/// revision the written bytes were encoded from and `current` is where the
/// buffer stands now, so a mismatch means something was typed while the write
/// was in flight. That text is not on disk, and closing on it would lose
/// exactly what the question the user answered was asked about.
///
/// A free function rather than a branch inside [`EditorPane::finish_save`]
/// because it is the whole of the rule and none of the plumbing: a pane needs a
/// live session and a file source before it can exist, and the rule needs
/// neither to be read or checked.
fn save_closes_pane(closing: bool, saved_ok: bool, saved: u64, current: u64) -> bool {
    closing && saved_ok && saved == current
}

/// Which commands of the right-click menu may be run, given where the buffer
/// stands.
///
/// Kept apart from the menu that reads it for the same reason
/// [`save_closes_pane`] is kept apart from the save: this is the whole of the
/// rule and none of the plumbing. Whether a row is greyed is a claim about a
/// selection, a history and a read-only flag, and none of the three needs a
/// pane, a session or a window in order to be checked — or tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MenuState {
    /// Whether there is a selection, as opposed to only a caret.
    selected: bool,
    /// Whether the buffer accepts edits at all.
    writable: bool,
    /// Whether there is a change to take back.
    can_undo: bool,
    /// Whether there is a change to put back.
    can_redo: bool,
    /// Whether the file's format has a line comment to toggle.
    can_comment: bool,
}

impl MenuState {
    /// Reads the five predicates off the editor as it stands.
    fn of(editor: &EditorView) -> Self {
        Self {
            selected: editor.has_selection(),
            writable: !editor.is_read_only(),
            can_undo: editor.can_undo(),
            can_redo: editor.can_redo(),
            can_comment: editor.language().line_comment().is_some(),
        }
    }

    /// Cut takes the selection *out*, so it wants both something to take and a
    /// buffer that may lose it.
    const fn cut(self) -> bool {
        self.selected && self.writable
    }

    /// Copy only reads, so a read-only buffer still offers it.
    const fn copy(self) -> bool {
        self.selected
    }

    /// Paste needs nowhere to paste *from* — an empty clipboard is the
    /// platform's answer and not something to ask about here — only somewhere
    /// to paste to.
    const fn paste(self) -> bool {
        self.writable
    }

    /// Undo and redo follow the history and nothing else: a read-only buffer
    /// has no history to begin with.
    const fn undo(self) -> bool {
        self.can_undo
    }

    /// See [`MenuState::undo`].
    const fn redo(self) -> bool {
        self.can_redo
    }

    /// The comment toggle writes a prefix at the head of the selected lines, so
    /// it is an edit like any other — and it needs a prefix to write. JSON has
    /// none, and a row that would do nothing is greyed rather than offered.
    const fn toggle_comment(self) -> bool {
        self.writable && self.can_comment
    }

    /// Find is offered on any buffer; replace is an edit, and is not.
    const fn replace(self) -> bool {
        self.writable
    }

    /// Saving needs a file that will take the bytes. Greyed rather than left
    /// out on a read-only pane, because this is the one row that says what a
    /// pane over a file is *for*, and a menu that quietly stopped mentioning it
    /// would read as a menu over some other kind of buffer.
    const fn save(self) -> bool {
        self.writable
    }
}

/// The message strip under the editor, when there is something to say.
struct Message {
    /// The sentence.
    text: SharedString,
    /// Whether it is a failure, and so drawn in the danger colour and left up.
    error: bool,
}

/// One open file: the editor, the path it came from, and the state of the save.
pub struct EditorPane {
    /// The text surface.
    editor: Entity<EditorView>,
    /// The session the file was opened out of.
    ///
    /// Kept for two things and neither of them is the transfer: the colour
    /// scheme the text is drawn in, and the repaint that follows a change to it.
    /// The [`FileSource`] below is what the file is actually read and written
    /// through, and it outlives a disconnect — a save attempted after the
    /// session ends fails with a sentence rather than with a missing pane.
    session: Entity<Session>,
    /// The filesystem the file lives on.
    source: Arc<dyn FileSource>,
    /// The directory holding the file, in the source's own spelling.
    dir: String,
    /// The file's name within [`EditorPane::dir`].
    name: SharedString,
    /// What was stripped on the way in and has to be put back on the way out.
    file: TextFile,
    /// Bumped by every buffer change.
    ///
    /// A save writes the text as it stood when it started, so an edit made while
    /// the bytes are in flight must not be cleared by the save that did not
    /// include it. The saved revision is compared against this when the write
    /// lands, and only a match marks the pane clean.
    revision: u64,
    /// Whether a save is in flight, which is also the lock keeping a second one
    /// from starting.
    saving: bool,
    /// Whether saves go out through the source's root rather than the account's.
    ///
    /// Set once, by [`EditorPane::edit_as_root`], and never cleared: a pane the
    /// user deliberately unlocked stays unlocked for as long as it is open, and
    /// there is no state it could sensibly fall back to — the account still
    /// cannot write the file, which is what put the pane here.
    ///
    /// It is not a second read-only flag. The buffer's own is what refuses
    /// edits, and this answers only two questions: which write call a save
    /// makes, and whether the header says so.
    as_root: bool,
    /// Whether a re-read for a change of charset is in flight, which is also the
    /// lock keeping a second one from starting.
    ///
    /// Its own flag rather than [`EditorPane::saving`]: the two say different
    /// things to the user — the header prints "Saving…" for one of them — and a
    /// reload that borrowed the save's lock would make <kbd>Ctrl</kbd>+<kbd>S</kbd>
    /// silently do nothing while it ran. Saving stays available for that reason,
    /// and is safe to: a switch only starts on a clean buffer, and the reload
    /// drops its own result rather than overwrite a buffer that stopped being
    /// clean while the bytes were on their way.
    reloading: bool,
    /// Whether the save in flight is one the pane is being closed for.
    ///
    /// Set only by [`EditorPane::save_and_close`], and cleared by the very next
    /// save that finishes however it finishes — so a failure, or a save the
    /// buffer moved on from, leaves an ordinary pane behind rather than one
    /// that will close itself the next time <kbd>Ctrl</kbd>+<kbd>S</kbd> works.
    close_after_save: bool,
    /// The line under the editor, if anything has been said.
    message: Option<Message>,
    /// Where the right-click that asked for the context menu landed, in window
    /// coordinates, while that menu is open.
    ///
    /// The pane holds it rather than the editor because the editor holds none of
    /// the strings such a menu needs — it reports the press as
    /// [`EditorEvent::ContextMenu`] and leaves the drawing here.
    context: Option<Point<Pixels>>,
    /// Focus target of the pane as a whole; the editor inside has its own.
    focus_handle: FocusHandle,
    /// Keeps the buffer-change subscription alive.
    _editor_events: Subscription,
    /// Repaints the pane when the session's colour scheme changes under it.
    _session: Subscription,
}

/// Registers the key bindings an [`EditorPane`] relies on.
///
/// Call once during application start-up, after [`crate::editor::init`].
pub fn init(cx: &mut App) {
    let modifier = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    cx.bind_keys([KeyBinding::new(
        &format!("{modifier}-s"),
        SaveFile,
        Some(KEY_CONTEXT),
    )]);
}

/// The save button's tooltip: the command, and the key that already is it.
///
/// Spelled with the platform's own modifier, the same choice [`init`] makes
/// when it binds the key and the same one the menu's shortcut hints make.
fn save_shortcut_label() -> String {
    format!("{} ({SHORTCUT_MODIFIER}+S)", ts!("common.save"))
}

impl EditorPane {
    /// A pane showing `file`, read from `name` in `dir` on `source`.
    ///
    /// `writable` is [`FileSource::writable`]'s verdict, taken by the panel
    /// beside the read. `false` opens the pane read-only: the buffer refuses
    /// every edit, the header shows the state where the Save button would be,
    /// and the write rows of the context menu are greyed. It is passed in rather
    /// than probed here because the probe is a round trip and this runs on the
    /// frame that draws the pane.
    ///
    /// Nothing puts a read-only pane back into writing *by itself*. A file's
    /// permissions can of course change under an open pane, but the only honest
    /// way to notice would be to keep asking, and the reward for asking would be
    /// an editor that unlocks itself while nobody is looking at it. Closing the
    /// pane and opening the file again is the way, and it is one keystroke. The
    /// one thing that does unlock a pane is [`EditorPane::edit_as_root`], which
    /// is not the pane noticing anything: it is the user, having read the badge,
    /// asking for a different account.
    pub fn new(
        session: Entity<Session>,
        source: Arc<dyn FileSource>,
        dir: String,
        name: SharedString,
        file: TextFile,
        writable: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        // The file's name is what says how to colour it, with its first line as
        // the fallback for the scripts that carry a `#!` and no extension. The
        // pane is the only place that knows both, which is why the widget is
        // told rather than asked.
        let language = Language::detect(&name, file.text.lines().next().unwrap_or_default());

        // Filled in the constructor rather than after it, so the pane never
        // exists holding an empty buffer. Whether the `Changed` this emits is
        // seen by the subscription below does not matter either way: the dirty
        // flag is read off the editor, and a load leaves the editor clean.
        let editor = cx.new(|cx| {
            let mut editor = EditorView::new(cx);
            editor.set_text(&file.text, cx);
            editor.set_language(language, cx);
            // Last, and after the text rather than before it. Every editing
            // path in the widget is guarded by this flag, and a buffer that was
            // locked before it was filled would be the one thing a read-only
            // pane must not be: empty.
            editor.set_read_only(!writable, cx);
            editor
        });
        let editor_events = cx.subscribe(&editor, |pane, _editor, event: &EditorEvent, cx| {
            match event {
                EditorEvent::Changed => pane.on_changed(cx),
                EditorEvent::ContextMenu { position } => pane.open_context(*position, cx),
                // The pane draws nothing that follows the caret — the header
                // shows the file, not the place in it — but the status bar does,
                // and it reads the position off this pane. The notification is
                // what carries a caret move up to the workspace observing it;
                // without it an arrow key would leave a stale line number on
                // screen until something else asked for a frame.
                EditorEvent::SelectionChanged => cx.notify(),
            }
        });
        let session_changed = cx.observe(&session, |_pane, _session, cx| cx.notify());

        Self {
            editor,
            session,
            source,
            dir,
            name,
            file,
            revision: 0,
            saving: false,
            as_root: false,
            reloading: false,
            close_after_save: false,
            message: None,
            context: None,
            focus_handle: cx.focus_handle(),
            _editor_events: editor_events,
            _session: session_changed,
        }
    }

    /// The session the file was opened out of.
    pub fn session(&self) -> &Entity<Session> {
        &self.session
    }

    /// The file's name, which is what the header and the tab strip show.
    pub fn name(&self) -> &SharedString {
        &self.name
    }

    /// The absolute path of the open file, in the source's own spelling.
    ///
    /// What tells "this file is already open" from "this is another file of the
    /// same name somewhere else".
    pub fn path(&self) -> String {
        file_path(&self.dir, &self.name)
    }

    /// Where the caret is and how much file there is around it: the one-based
    /// line and column, and the number of lines.
    ///
    /// Read straight off the editor for the same reason [`EditorPane::is_dirty`]
    /// is: the widget already knows, and a copy kept here would be a second
    /// answer that could disagree with the caret on screen. The three come back
    /// together because the status bar prints them together and would otherwise
    /// take three borrows of the same widget to build one string.
    pub fn caret_summary(&self, cx: &App) -> (usize, usize, usize) {
        let editor = self.editor.read(cx);
        let (line, column) = editor.caret_position();
        (line, editor.line_count(), column)
    }

    /// The language the file is being coloured as.
    pub fn language(&self, cx: &App) -> Language {
        self.editor.read(cx).language()
    }

    /// Colours the file as `language` from here on.
    ///
    /// What the status bar's picker calls. The choice sticks: nothing detects
    /// the language again once the file is open — [`EditorPane::new`] does it
    /// once, from the name — so a file the detector placed wrongly stays where
    /// the user put it for as long as it is open.
    pub fn set_language(&mut self, language: Language, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.set_language(language, cx));
    }

    /// The charset the file was read in, and will be written in.
    pub fn charset(&self) -> Charset {
        self.file.charset
    }

    /// Re-reads the file in `charset` and shows it decoded that way.
    ///
    /// What the status bar's charset picker calls. Unlike
    /// [`EditorPane::set_language`] this is not a relabelling: the bytes are not
    /// kept anywhere — the rope holds text — so the only way to decode them
    /// again is to fetch them again, and what comes back replaces the buffer,
    /// the undo history and the caret with it.
    ///
    /// Which is why a dirty buffer refuses instead. There is no honest thing to
    /// do with unsaved edits here: keeping them would show text decoded two
    /// ways at once, and dropping them would lose work to what reads as a
    /// display setting. The user is asked to save first, in the strip under the
    /// editor.
    ///
    /// Nothing is written. A switch changes how the file is *read*; the file on
    /// disk is converted only if and when the user saves.
    pub fn set_charset(&mut self, charset: Charset, cx: &mut Context<Self>) {
        if charset == self.file.charset || self.reloading {
            return;
        }
        if self.is_dirty(cx) || self.saving {
            self.message = Some(Message {
                text: ts!("editor.charset_dirty"),
                error: true,
            });
            cx.notify();
            return;
        }

        let source = self.source.clone();
        let dir = self.dir.clone();
        let name = self.name.to_string();

        self.reloading = true;
        self.message = None;
        cx.notify();

        cx.spawn(async move |pane, cx| {
            let loaded = match read_file(&source, &dir, &name).await {
                Ok(bytes) => TextFile::decode(&bytes, charset),
                Err(error) => Err(LoadError::Transport(error)),
            };
            pane.update(cx, |pane, cx| pane.finish_reload(charset, loaded, cx))
                .ok();
        })
        .detach();
    }

    /// Puts the re-read file into the buffer, or says why it could not.
    fn finish_reload(
        &mut self,
        charset: Charset,
        loaded: Result<TextFile, LoadError>,
        cx: &mut Context<Self>,
    ) {
        self.reloading = false;
        let file = match loaded {
            Ok(file) => file,
            Err(LoadError::NotUtf8) => {
                // The one charset that can refuse bytes. The file is left
                // exactly as it was being shown, which is still something the
                // user can read and save.
                self.message = Some(Message {
                    text: ts!("editor.charset_not_text", charset = charset.name()),
                    error: true,
                });
                cx.notify();
                return;
            }
            Err(error) => {
                // `TooLarge` is unreachable — the panel checks the size before
                // the file is ever opened, and this is the same file — so the
                // sentence is the transport's own where there is one.
                let reason = match &error {
                    LoadError::Transport(error) => error.to_string(),
                    other => format!("{other:?}"),
                };
                log::warn!("could not re-read {}: {reason}", self.path());
                self.message = Some(Message {
                    text: ts!("editor.charset_failed", error = reason),
                    error: true,
                });
                cx.notify();
                return;
            }
        };

        // Typed into while the bytes were coming back. Replacing the buffer now
        // would throw that away for what the user asked to be a change of
        // *encoding*, so the bytes are dropped instead and the refusal the
        // switch would have started with is given late.
        if self.is_dirty(cx) || self.saving {
            self.message = Some(Message {
                text: ts!("editor.charset_dirty"),
                error: true,
            });
            cx.notify();
            return;
        }

        self.editor
            .update(cx, |editor, cx| editor.set_text(&file.text, cx));
        self.file = file;
        self.message = None;
        cx.notify();
    }

    /// Whether the buffer has unsaved changes.
    ///
    /// Read off the editor rather than mirrored into a field of its own, so
    /// there is one answer and not two that can disagree: the widget already
    /// tracks this, and a load or a successful save clears it there.
    pub fn is_dirty(&self, cx: &App) -> bool {
        self.editor.read(cx).is_dirty()
    }

    /// Records a buffer change.
    fn on_changed(&mut self, cx: &mut Context<Self>) {
        self.revision = self.revision.wrapping_add(1);
        // A message about the last save stops being true the moment the buffer
        // moves on from it.
        self.message = None;
        cx.notify();
    }

    /// Raises the right-click menu at `position`, in window coordinates.
    ///
    /// The press is also reported as a focus, the way the left button's is: the
    /// editor has already taken the keyboard by the time it reports the click,
    /// so a pane whose menu is open but whose accent frame sits on a sibling
    /// would be showing the wrong pane as active. The terminal's right-click
    /// says the same thing for the same reason.
    fn open_context(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.context = Some(position);
        cx.emit(EditorPaneEvent::Focused);
        cx.notify();
    }

    /// Puts the right-click menu away.
    fn close_context(&mut self, cx: &mut Context<Self>) {
        self.context = None;
        cx.notify();
    }

    /// The rows of the right-click menu, in the order the keyboard offers them.
    ///
    /// Every row *dispatches* an action rather than calling a method: the editor
    /// exposes no method for any of these and should not have to, so the menu
    /// and the chord are one command reaching one handler. The editor's own
    /// commands go to the editor's focus handle, which is what puts them in the
    /// `Editor` key context; "Save" goes to the pane's, because saving is the
    /// pane's — the widget has nowhere to save to.
    ///
    /// The handles are dispatched to whether or not they hold the focus, but in
    /// practice the editor has just taken it: the right-click that opened this
    /// menu focused the buffer before it reported the press.
    fn menu_entries(&self, cx: &mut Context<Self>) -> Vec<MenuEntry> {
        let editor = self.editor.read(cx);
        let state = MenuState::of(editor);
        let editor_handle = editor.focus_handle(cx);
        let pane_handle = self.focus_handle.clone();

        let row = |label: SharedString, key: &str, enabled: bool, action: Box<dyn Action>| {
            let handle = editor_handle.clone();
            MenuEntry::new(label)
                .shortcut(format!("{SHORTCUT_MODIFIER}+{key}"))
                .enabled(enabled)
                .on_activate(move |window, cx| handle.dispatch_action(&*action, window, cx))
        };

        vec![
            row(
                ts!("editor.menu_cut"),
                "X",
                state.cut(),
                Box::new(editor_actions::Cut),
            ),
            row(
                ts!("editor.menu_copy"),
                "C",
                state.copy(),
                Box::new(editor_actions::Copy),
            ),
            row(
                ts!("editor.menu_paste"),
                "V",
                state.paste(),
                Box::new(editor_actions::Paste),
            ),
            MenuEntry::separator(),
            row(
                ts!("editor.menu_select_all"),
                "A",
                true,
                Box::new(editor_actions::SelectAll),
            ),
            MenuEntry::separator(),
            row(
                ts!("editor.menu_undo"),
                "Z",
                state.undo(),
                Box::new(editor_actions::Undo),
            ),
            row(
                ts!("editor.menu_redo"),
                "Shift+Z",
                state.redo(),
                Box::new(editor_actions::Redo),
            ),
            MenuEntry::separator(),
            row(
                ts!("editor.menu_toggle_comment"),
                "/",
                state.toggle_comment(),
                Box::new(editor_actions::ToggleComment),
            ),
            MenuEntry::separator(),
            row(
                ts!("editor.menu_find"),
                "F",
                true,
                Box::new(editor_actions::Find),
            ),
            row(
                ts!("editor.menu_replace"),
                "H",
                state.replace(),
                Box::new(editor_actions::Replace),
            ),
            MenuEntry::separator(),
            MenuEntry::new(ts!("common.save"))
                .shortcut(format!("{SHORTCUT_MODIFIER}+S"))
                .enabled(state.save())
                .on_activate(move |window, cx| {
                    pane_handle.dispatch_action(&SaveFile, window, cx);
                }),
        ]
    }

    /// Builds the menu a right-click in the buffer opens, if one is open.
    fn render_context(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let position = self.context?;
        let this = cx.entity();
        Some(
            ContextMenu::new("editor-pane-context")
                .position(position)
                .entries(self.menu_entries(cx))
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |pane, cx| pane.close_context(cx));
                }),
        )
    }

    /// Handles <kbd>Ctrl</kbd>/<kbd>Cmd</kbd> + <kbd>S</kbd>.
    fn save_action(&mut self, _: &SaveFile, _window: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
    }

    /// Unlocks a read-only pane, and points every save it makes from here on at
    /// the source's root.
    ///
    /// The header's "Edit as root" button, which is offered only where
    /// [`FileSource::can_write_as_root`] said there was such a thing to be. It
    /// undoes precisely what the constructor did — the buffer takes edits again
    /// — and adds one thing to it: the save that follows is
    /// [`FileSource::copy_in_as_root`] instead of [`FileSource::copy_in`].
    ///
    /// Why this is not the constructor's business, given that it knew the same
    /// two facts: because it is a decision rather than a fact. `writable` is
    /// what the filesystem said, and this is what the user said after reading
    /// it. A pane that opened as root because it could would be an editor that
    /// quietly chose the most powerful account available every time, which is
    /// the opposite of what a warning in the header is for.
    ///
    /// A source with no root to write as is refused outright rather than
    /// unlocked and disappointed later. Nothing draws the button on such a pane,
    /// so this is not reachable by pressing anything — but a buffer that takes
    /// edits and then fails every save would be worse than the locked one it
    /// replaced, and the check that rules it out is one line.
    pub fn edit_as_root(&mut self, cx: &mut Context<Self>) {
        if !self.source.can_write_as_root() {
            return;
        }
        self.as_root = true;
        self.editor
            .update(cx, |editor, cx| editor.set_read_only(false, cx));
        cx.notify();
    }

    /// Writes the buffer back to the file it came from.
    ///
    /// A clean buffer is still written: "save" that silently does nothing is
    /// indistinguishable from "save" that failed, and a file whose contents
    /// match may still have been changed underneath by something else.
    ///
    /// A read-only pane is the one exception, and it is not the same silence:
    /// there is no button to press and no menu row to reach, so the only way
    /// here is <kbd>Ctrl</kbd>+<kbd>S</kbd> on a buffer whose header already
    /// says why it cannot be saved. Writing the file anyway would send bytes the
    /// user was told would not be sent; reporting a failure would put a red line
    /// under a pane that is doing exactly what it says. A pane unlocked by
    /// [`EditorPane::edit_as_root`] is no longer read-only and so no longer that
    /// exception; the only trace of it here is which write call the bytes leave
    /// through.
    fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving || self.editor.read(cx).is_read_only() {
            return;
        }
        // Encoded here rather than in the task, because it is the buffer as it
        // stands *now* that is being written — and whether anything had to be
        // substituted to write it is a fact about these bytes, so it travels
        // with them to the report.
        let (bytes, substituted) = self.file.encode(&self.editor.read(cx).text());
        let charset = self.file.charset;
        let revision = self.revision;
        let source = self.source.clone();
        let dir = self.dir.clone();
        let name = self.name.to_string();
        let as_root = self.as_root;

        self.saving = true;
        self.message = None;
        cx.notify();

        cx.spawn(async move |pane, cx| {
            let result = write_file(&source, &dir, &name, &bytes, as_root).await;
            pane.update(cx, |pane, cx| {
                pane.finish_save(revision, result, (charset, substituted), cx);
            })
            .ok();
        })
        .detach();
    }

    /// Saves the buffer, and closes the pane if — and only if — the write
    /// lands.
    ///
    /// The "Save" button of the workspace's close question. It returns at once:
    /// the question goes down on the press, and the pane reports the transfer
    /// where it already reports every other one, in its own header. Whether the
    /// close follows is [`EditorPane::finish_save`]'s to decide, and it says no
    /// to a failure — the pane stays open with the reason under it, which is the
    /// only place the reason can be read.
    ///
    /// A save already in flight is not restarted; [`EditorPane::save`]'s lock
    /// sees to that, and the pane rides on that write's result instead. Honest,
    /// because those are the bytes going to the file — but if the buffer has
    /// moved on since they left, the revision check keeps the pane open.
    ///
    /// A read-only pane never reaches here: the question this answers is only
    /// asked of a pane with unsaved changes, and a buffer that refuses every
    /// edit has none to have.
    pub fn save_and_close(&mut self, cx: &mut Context<Self>) {
        self.close_after_save = true;
        self.save(cx);
    }

    /// Records how the save went, and closes the pane if it was saving to close.
    ///
    /// `encoded` is the charset the bytes went out in and whether anything in
    /// them had to be substituted to get there — both taken from the save that
    /// is landing rather than read off the file now, since the picker could have
    /// moved in between.
    fn finish_save(
        &mut self,
        revision: u64,
        result: Result<(), FileError>,
        encoded: (Charset, bool),
        cx: &mut Context<Self>,
    ) {
        self.saving = false;
        // Taken rather than read: whatever this save decides, it decides once.
        // A failure — or a write the buffer moved on from — answers the close
        // question with "no", and the save the user starts next, from the header
        // or the keyboard, must not inherit an answer given about this one.
        let closing = std::mem::take(&mut self.close_after_save);
        let saved_ok = result.is_ok();
        match result {
            Ok(()) => {
                // Only the revision that was actually written may clear the
                // flag; anything typed while the bytes were in flight is still
                // unsaved, and saying otherwise would lose it at the next close.
                if revision == self.revision {
                    self.editor.update(cx, |editor, cx| editor.mark_clean(cx));
                }
                // A save that could not spell part of what the buffer held is
                // still a save — the file is written and the pane is clean —
                // but the user has just lost characters, so it is reported in
                // the danger colour the failures use rather than in the muted
                // one. There is no third flavour to draw it in: `Message` has
                // the two the theme has, and a warning shown as an aside would
                // be the one sentence here nobody reads.
                let (text, error) = if encoded.1 {
                    (ts!("editor.saved_lossy", charset = encoded.0.name()), true)
                } else {
                    (ts!("editor.saved", name = self.name.to_string()), false)
                };
                self.message = Some(Message { text, error });
            }
            Err(error) => {
                log::warn!("could not save {}: {error}", self.path());
                self.message = Some(Message {
                    text: ts!("editor.save_failed", error = error.to_string()),
                    error: true,
                });
            }
        }
        if save_closes_pane(closing, saved_ok, revision, self.revision) {
            cx.emit(EditorPaneEvent::SavedForClose);
        }
        cx.notify();
    }

    /// The colours and the font the text surface is drawn in, from this
    /// session's effective terminal settings.
    ///
    /// Recomputed every frame rather than stored, because all three — the
    /// scheme, the font family and the font size — can change under an open
    /// pane, from the settings or from a per-session override, and there is no
    /// one event that says so. [`EditorView::set_palette`] and
    /// [`EditorView::set_font`] repaint nothing when the answer has not moved,
    /// which is every frame but the one after such a change.
    ///
    /// Both are pushed from here rather than read by the widget, because the
    /// widget knows nothing about sessions; the one snapshot is taken once and
    /// used for both, since resolving it clones a handful of strings.
    fn sync_appearance(&mut self, cx: &mut Context<Self>) {
        let effective = self.session.read(cx).effective(cx);
        let palette = palette_for(&TerminalTheme::by_name_or_default(&effective.scheme));
        let font = resolve_font(&effective, cx);
        let font_size = px(effective.font_size);
        self.editor.update(cx, |editor, cx| {
            editor.set_palette(palette, cx);
            editor.set_font(font, font_size, cx);
        });
    }
}

impl EventEmitter<EditorPaneEvent> for EditorPane {}

impl Focusable for EditorPane {
    /// The editor's handle, not the pane's: focusing a pane means putting the
    /// caret in the file, and the header holds nothing the keyboard can reach.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl Render for EditorPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_appearance(cx);
        // The chrome is application furniture and takes the application theme;
        // only the text surface below follows the terminal scheme. Drawing the
        // header in the scheme too would make the pane a window of its own
        // rather than a pane of this one.
        let theme = theme(cx);
        let dirty = self.is_dirty(cx);
        let saving = self.saving;
        // Read off the widget rather than kept beside it, for the reason
        // `is_dirty` is: the editor is the thing that actually refuses the
        // edits, so a copy here would be a second answer free to disagree with
        // the buffer the user is looking at.
        let read_only = self.editor.read(cx).is_read_only();
        // Asked of the source rather than remembered from the constructor: it
        // is a property of the backend, it cannot change under an open pane,
        // and reading it here keeps the button and the method that answers it
        // deciding off the same answer.
        let rooted = self.source.can_write_as_root();

        let header = div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .h(px(HEADER_HEIGHT))
            .px(px(8.))
            .bg(theme.surface)
            .border_b_1()
            .border_color(theme.border)
            .text_size(px(11.))
            .text_color(theme.text_muted)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.text)
                    .child(self.name.clone()),
            )
            .when(dirty, |header| {
                header.child(
                    div()
                        .flex_none()
                        .text_size(px(9.))
                        .text_color(theme.accent)
                        .child(DIRTY_MARK),
                )
            })
            .when(saving, |header| {
                header.child(div().flex_none().child(ts!("editor.saving")))
            })
            // The same slot, holding one of two things. A worded Save button —
            // rather than an icon, and there whether or not the buffer is dirty:
            // the keyboard already saves, so the button's job is to *say* that
            // saving is a thing this pane does, to the user who has never
            // pressed Ctrl+S in it, and the tooltip teaches the key.
            //
            // Or, when the file cannot be written, a badge saying so. Replacing
            // the button rather than greying it, because a greyed button is a
            // thing that might work later and this one never will; and putting
            // the badge where the button was rather than beside the file name,
            // so that the header answers "can I save this?" in one place. It
            // takes no click, but it is still a stateful element, because a
            // tooltip needs an id to hang the hover state on — and the tooltip
            // is the whole point: the badge says *what*, and only it says why.
            .child(if read_only {
                div()
                    .id("editor-pane-read-only")
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(18.))
                    .px(px(6.))
                    .text_color(theme.text_muted)
                    .tooltip(tooltip_label(ts!("editor.read_only_tip")))
                    .child(ts!("editor.read_only"))
            } else {
                div()
                    .id("editor-pane-save")
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(18.))
                    .px(px(6.))
                    .rounded_sm()
                    .text_color(theme.text_muted)
                    .hover(|style| style.bg(theme.surface_hover).text_color(theme.text))
                    .tooltip(tooltip_label(save_shortcut_label()))
                    .on_click(cx.listener(|pane, _: &ClickEvent, _window, cx| {
                        pane.save(cx);
                    }))
                    .child(ts!("common.save"))
            })
            // Beside that slot, in the two states that have something to add to
            // it — and it is the same slot's two states seen from the other
            // side, which is why they share a chain rather than being drawn
            // wherever each happened to fit.
            //
            // A locked pane over a source that has a root to write as gets the
            // way out, worded and styled like the Save button because that is
            // what it turns into. A pane that has taken it gets a marker naming
            // the account the next save will use: no click, no command, and in
            // the danger colour, because "root" here is not an ornament but the
            // one fact about this pane worth interrupting a reader for — it is
            // the difference between a save that cannot happen and one that
            // cannot be taken back. The id it carries is for the tooltip, which
            // is where the sentence is.
            .children(if read_only {
                rooted.then(|| {
                    div()
                        .id("editor-pane-edit-as-root")
                        .flex()
                        .flex_none()
                        .items_center()
                        .h(px(18.))
                        .px(px(6.))
                        .rounded_sm()
                        .text_color(theme.text_muted)
                        .hover(|style| style.bg(theme.surface_hover).text_color(theme.text))
                        .tooltip(tooltip_label(ts!("editor.edit_as_root_tip")))
                        .on_click(cx.listener(|pane, _: &ClickEvent, _window, cx| {
                            pane.edit_as_root(cx);
                        }))
                        .child(ts!("editor.edit_as_root"))
                })
            } else {
                self.as_root.then(|| {
                    div()
                        .id("editor-pane-as-root")
                        .flex()
                        .flex_none()
                        .items_center()
                        .h(px(18.))
                        .px(px(6.))
                        .text_color(theme.danger)
                        .tooltip(tooltip_label(ts!("editor.as_root_tip")))
                        .child(ts!("editor.as_root"))
                })
            })
            .child(
                div()
                    .id("editor-pane-close")
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(16.))
                    .rounded_sm()
                    .text_size(px(12.))
                    .text_color(theme.icon)
                    .hover(|style| style.bg(theme.surface_hover).text_color(theme.text))
                    .on_click(cx.listener(|_pane, _: &ClickEvent, _window, cx| {
                        cx.emit(EditorPaneEvent::CloseRequested);
                    }))
                    .child(CLOSE_MARK),
            );

        let message = self.message.as_ref().map(|message| {
            div()
                .flex()
                .flex_none()
                .items_center()
                .h(px(HEADER_HEIGHT))
                .px(px(8.))
                .bg(theme.surface)
                .border_t_1()
                .border_color(theme.border)
                .text_size(px(11.))
                .text_color(if message.error {
                    theme.danger
                } else {
                    theme.text_muted
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(message.text.clone()),
                )
        });

        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .min_h_0()
            // Before the focus listeners gpui runs after the frame, so the
            // active-pane frame moves on the press rather than on the next
            // input event. Propagation is left alone: the editor underneath
            // still has to take the focus and start the selection.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_pane, _: &MouseDownEvent, _window, cx| {
                    cx.emit(EditorPaneEvent::Focused);
                }),
            )
            .on_action(cx.listener(Self::save_action))
            .child(header)
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
            .children(message)
            // Last, and positioned in window coordinates, so the panel paints
            // over the buffer and the message strip alike rather than being
            // clipped to the box it is declared in.
            .children(self.render_context(cx))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures::channel::mpsc::UnboundedSender;
    use gpui::TestAppContext;

    use super::*;
    use crate::files::{FileEntry, LocalSource};

    /// Every UTF-8 test below reads the same way it did before there was a
    /// charset to pass, which is the point: the default path is unchanged.
    fn decode(bytes: &[u8]) -> Result<TextFile, LoadError> {
        TextFile::decode(bytes, Charset::UTF8)
    }

    /// The charset the legacy cases are written against.
    fn euc_kr() -> Charset {
        Charset::for_label("euc-kr").expect("euc-kr is a registry label")
    }

    /// `안녕` in EUC-KR: two KS X 1001 characters, two bytes each.
    const HELLO_EUC_KR: [u8; 4] = [0xbe, 0xc8, 0xb3, 0xe7];

    #[test]
    fn a_plain_lf_file_round_trips_unchanged() {
        let file = decode(b"one\ntwo\n").expect("valid UTF-8");
        assert_eq!(file.newlines, Newlines::Lf);
        assert!(!file.bom);
        assert_eq!(file.text, "one\ntwo\n");
        assert_eq!(file.encode(&file.text), (b"one\ntwo\n".to_vec(), false));
    }

    #[test]
    fn crlf_is_normalised_in_and_restored_out() {
        let file = decode(b"one\r\ntwo\r\n").expect("valid UTF-8");
        assert_eq!(file.newlines, Newlines::Crlf);
        // The buffer never sees a carriage return, which is what lets every
        // caret command count `\n` alone.
        assert_eq!(file.text, "one\ntwo\n");
        assert_eq!(file.encode(&file.text).0, b"one\r\ntwo\r\n");
    }

    #[test]
    fn the_dominant_ending_decides_a_mixed_file() {
        // Three CRLF lines and one lone LF: still a CRLF file, so saving it
        // does not rewrite the three that were already right.
        let file = decode(b"a\r\nb\r\nc\r\nd\n").expect("valid UTF-8");
        assert_eq!(file.newlines, Newlines::Crlf);
        assert_eq!(file.encode(&file.text).0, b"a\r\nb\r\nc\r\nd\r\n");

        // The other way round, with the lone LF in the majority. The minority
        // CRLF is still stripped on the way in — no carriage return reaches the
        // buffer, whichever style won — so the file is written back all LF.
        let file = decode(b"a\nb\nc\nd\r\n").expect("valid UTF-8");
        assert_eq!(file.newlines, Newlines::Lf);
        assert_eq!(file.text, "a\nb\nc\nd\n");
        assert_eq!(file.encode(&file.text).0, b"a\nb\nc\nd\n");
    }

    #[test]
    fn a_file_with_no_line_break_at_all_is_lf() {
        let file = decode(b"no newline here").expect("valid UTF-8");
        assert_eq!(file.newlines, Newlines::Lf);
        assert_eq!(file.encode(&file.text).0, b"no newline here");
    }

    #[test]
    fn a_byte_order_mark_is_kept_out_of_the_buffer_and_put_back() {
        let mut bytes = BOM.to_vec();
        bytes.extend_from_slice("hello".as_bytes());
        let file = decode(&bytes).expect("valid UTF-8");
        assert!(file.bom);
        // The mark must not reach the buffer: it would show as a zero width
        // space the caret can be put inside of.
        assert_eq!(file.text, "hello");
        assert_eq!(file.encode(&file.text).0, bytes);
    }

    #[test]
    fn a_bom_on_a_crlf_file_survives_both_transformations() {
        let mut bytes = BOM.to_vec();
        bytes.extend_from_slice(b"one\r\ntwo\r\n");
        let file = decode(&bytes).expect("valid UTF-8");
        assert!(file.bom);
        assert_eq!(file.newlines, Newlines::Crlf);
        assert_eq!(file.text, "one\ntwo\n");
        assert_eq!(file.encode(&file.text).0, bytes);
    }

    #[test]
    fn bytes_that_are_not_utf8_are_refused_rather_than_mangled() {
        // A lone 0x80 continuation byte: valid Latin-1, not valid UTF-8.
        assert_eq!(decode(&[b'a', 0x80, b'b']), Err(LoadError::NotUtf8));
    }

    #[test]
    fn a_bom_in_front_of_invalid_bytes_is_still_a_refusal() {
        let mut bytes = BOM.to_vec();
        bytes.push(0xFF);
        assert_eq!(decode(&bytes), Err(LoadError::NotUtf8));
    }

    #[test]
    fn a_carriage_return_typed_into_a_crlf_buffer_is_not_doubled() {
        let file = decode(b"one\r\n").expect("valid UTF-8");
        // As though the user had pasted a Windows line ending into the buffer.
        assert_eq!(file.encode("one\r\ntwo\n").0, b"one\r\ntwo\r\n");
    }

    #[test]
    fn multibyte_text_survives_a_round_trip() {
        let source = "한 줄\r\n두 줄\r\n";
        let file = decode(source.as_bytes()).expect("valid UTF-8");
        assert_eq!(file.text, "한 줄\n두 줄\n");
        assert_eq!(file.encode(&file.text).0, source.as_bytes());
    }

    #[test]
    fn a_legacy_file_round_trips_byte_for_byte() {
        let file = TextFile::decode(&HELLO_EUC_KR, euc_kr()).expect("no legacy charset refuses");
        assert_eq!(file.text, "안녕");
        assert_eq!(file.charset, euc_kr());
        // The whole promise of the picker: what was read is what is written.
        assert_eq!(file.encode(&file.text), (HELLO_EUC_KR.to_vec(), false));
    }

    #[test]
    fn the_same_bytes_read_two_ways_give_two_files() {
        // What a charset switch is: the bytes on disk do not move, the text
        // does. As UTF-8 these are simply not text at all.
        assert_eq!(decode(&HELLO_EUC_KR), Err(LoadError::NotUtf8));
        let file = TextFile::decode(&HELLO_EUC_KR, euc_kr()).expect("EUC-KR reads any byte");
        assert_eq!(file.text, "안녕");
        // And UTF-8 bytes read as EUC-KR are mojibake rather than a refusal,
        // which is why the switch back is the one that can fail.
        let wrong = TextFile::decode("안녕".as_bytes(), euc_kr()).expect("EUC-KR reads any byte");
        assert_ne!(wrong.text, "안녕");
    }

    #[test]
    fn line_endings_are_detected_through_a_legacy_charset() {
        // The ASCII transparency the charsets are chosen for: the CRLF pairs are
        // still visible between the two-byte characters.
        let mut bytes = Vec::new();
        for _ in 0..3 {
            bytes.extend_from_slice(&HELLO_EUC_KR);
            bytes.extend_from_slice(b"\r\n");
        }
        let file = TextFile::decode(&bytes, euc_kr()).expect("no legacy charset refuses");
        assert_eq!(file.newlines, Newlines::Crlf);
        assert_eq!(file.text, "안녕\n안녕\n안녕\n");
        assert_eq!(file.encode(&file.text).0, bytes);
    }

    #[test]
    fn a_legacy_file_never_carries_a_byte_order_mark() {
        // The mark is a Unicode device; read as EUC-KR those three bytes are
        // characters like any others, and putting a mark back on the way out
        // would write three bytes the file never had.
        let mut bytes = BOM.to_vec();
        bytes.extend_from_slice(&HELLO_EUC_KR);
        let file = TextFile::decode(&bytes, euc_kr()).expect("no legacy charset refuses");
        assert!(!file.bom);
        assert!(
            !file.text.starts_with('\u{feff}'),
            "the bytes were decoded, not sniffed away"
        );
        // Nothing is prepended on the way out either: writing other text into
        // this file gives that text's bytes and no mark in front of them.
        assert_eq!(file.encode("안녕"), (HELLO_EUC_KR.to_vec(), false));
    }

    #[test]
    fn a_character_the_charset_cannot_spell_is_reported_as_well_as_replaced() {
        let file = TextFile::decode(&HELLO_EUC_KR, euc_kr()).expect("no legacy charset refuses");
        // As though the user had typed an emoji into a Korean file: it is
        // written as `?`, and the flag is what puts a sentence on the pane.
        let (bytes, substituted) = file.encode("안녕\u{1f600}");
        assert_eq!(bytes.last(), Some(&b'?'));
        assert!(substituted);
        // The same text in UTF-8 loses nothing and says so.
        let utf8 = decode(b"").expect("valid UTF-8");
        assert_eq!(
            utf8.encode("안녕\u{1f600}"),
            ("안녕\u{1f600}".into(), false)
        );
    }

    #[test]
    fn an_ordinary_save_never_closes_the_pane() {
        // The header button and Ctrl+S land here too, with the flag unset. A
        // save that closed the file it saved would be a trap.
        assert!(!save_closes_pane(false, true, 7, 7));
    }

    #[test]
    fn a_save_asked_for_by_the_close_question_closes_on_success() {
        assert!(save_closes_pane(true, true, 7, 7));
    }

    #[test]
    fn a_failed_save_leaves_the_pane_where_the_reason_can_be_read() {
        assert!(!save_closes_pane(true, false, 7, 7));
    }

    #[test]
    fn an_edit_made_while_the_bytes_were_in_flight_keeps_the_pane_open() {
        // The write carried revision 7 and the buffer is at 8: what was typed
        // in between is unsaved, so the close question is unanswered still.
        assert!(!save_closes_pane(true, true, 7, 8));
    }

    /// A buffer with a caret and nothing else: the state a right-click on an
    /// untouched file finds.
    const IDLE: MenuState = MenuState {
        selected: false,
        writable: true,
        can_undo: false,
        can_redo: false,
        can_comment: true,
    };

    #[test]
    fn with_nothing_selected_there_is_nothing_to_cut_or_copy() {
        assert!(!IDLE.cut());
        assert!(!IDLE.copy());
        // Paste is about where the text is going, not about what is highlighted.
        assert!(IDLE.paste());
    }

    #[test]
    fn a_selection_opens_the_clipboard_rows() {
        let state = MenuState {
            selected: true,
            ..IDLE
        };
        assert!(state.cut());
        assert!(state.copy());
    }

    #[test]
    fn a_read_only_buffer_offers_only_the_commands_that_read_it() {
        let state = MenuState {
            selected: true,
            writable: false,
            ..IDLE
        };
        // Copy takes a copy and changes nothing, so it stands; the three that
        // would write do not.
        assert!(state.copy());
        assert!(!state.cut());
        assert!(!state.paste());
        assert!(!state.toggle_comment());
        assert!(!state.replace());
    }

    #[test]
    fn undo_and_redo_follow_the_history_and_nothing_else() {
        assert!(!IDLE.undo());
        assert!(!IDLE.redo());

        let undone = MenuState {
            can_undo: true,
            can_redo: true,
            // Neither one asks about the selection.
            selected: false,
            ..IDLE
        };
        assert!(undone.undo());
        assert!(undone.redo());
    }

    #[test]
    fn a_format_with_no_comment_syntax_greys_the_toggle() {
        // JSON, and only JSON. The row would otherwise write a `#` into a file
        // whose own reader rejects it.
        let json = MenuState {
            can_comment: false,
            ..IDLE
        };
        assert!(!json.toggle_comment());
        assert!(IDLE.toggle_comment());
        // The two predicates are independent: a read-only YAML has a comment
        // syntax and still cannot be edited.
        assert_eq!(Language::Json.line_comment(), None);
        assert_eq!(Language::Yaml.line_comment(), Some("#"));
    }

    #[test]
    fn saving_is_the_one_menu_row_that_belongs_to_the_pane_and_it_follows_the_same_rule() {
        assert!(IDLE.save());
        let locked = MenuState {
            writable: false,
            ..IDLE
        };
        assert!(!locked.save());
    }

    /// A pane over `file`, on a session attached to nothing and the filesystem
    /// this test is already running on.
    ///
    /// Both are real rather than stubbed, and neither is touched: the source is
    /// only reached through a save, and the assertions below are about a save
    /// that never starts.
    fn pane(cx: &mut TestAppContext, writable: bool) -> Entity<EditorPane> {
        let session = cx.new(Session::dormant);
        let source: Arc<dyn FileSource> =
            Arc::new(LocalSource::new(cx.background_executor.clone()));
        let file = decode(b"one\ntwo\n").expect("valid UTF-8");
        cx.new(|cx| {
            EditorPane::new(
                session,
                source,
                "/etc".to_owned(),
                SharedString::from("hosts"),
                file,
                writable,
                cx,
            )
        })
    }

    /// The whole of what `writable: false` buys, from the constructor's side:
    /// the widget is locked, and it is locked *around* the file rather than
    /// instead of it — a buffer that took the flag before it took the text
    /// would be empty, which is the one thing a read-only pane must not be.
    #[gpui::test]
    fn a_pane_over_a_file_that_cannot_be_written_opens_locked_and_full(cx: &mut TestAppContext) {
        let locked = pane(cx, false);
        locked.read_with(cx, |pane, cx| {
            let editor = pane.editor.read(cx);
            assert!(editor.is_read_only());
            assert_eq!(editor.text(), "one\ntwo\n");
        });

        // And the ordinary case is untouched by any of it.
        let open = pane(cx, true);
        open.read_with(cx, |pane, cx| {
            assert!(!pane.editor.read(cx).is_read_only());
        });
    }

    /// Ctrl+S on a read-only pane. The header offers no button and the menu
    /// greys its row, so the keyboard is the only way in — and it has to stop
    /// here rather than send bytes the header just promised would not be sent.
    #[gpui::test]
    fn a_read_only_pane_starts_no_save(cx: &mut TestAppContext) {
        let pane = pane(cx, false);
        pane.update(cx, |pane, cx| pane.save(cx));
        pane.read_with(cx, |pane, _cx| {
            assert!(!pane.saving, "a read-only pane began writing the file");
            assert!(
                pane.message.is_none(),
                "a save that was never attempted reported something"
            );
        });
    }

    /// A source that answers only what a save asks of one, and remembers which
    /// of the two write calls the save chose.
    ///
    /// Stubbed rather than real because the thing under test is a *branch*, and
    /// the only backend that has both sides of it is WSL — which needs a
    /// distribution on the machine running the tests, and which is where the
    /// ignored integration tests in [`crate::files`] test the write itself.
    /// Everything not on the way to a save fails loudly, so a test that grows a
    /// dependency on one says so rather than quietly passing.
    struct RootSource {
        /// What [`FileSource::can_write_as_root`] answers.
        rooted: bool,
        /// Set by [`FileSource::copy_in_as_root`] and by nothing else, which is
        /// what tells the two save paths apart from outside the pane.
        as_root: Arc<AtomicBool>,
        /// The same, for the ordinary [`FileSource::copy_in`].
        plain: Arc<AtomicBool>,
    }

    /// The failure every call this stub does not implement answers with.
    fn unused(call: &str) -> FileError {
        FileError::Backend(format!("a save does not call {call}"))
    }

    #[async_trait::async_trait(?Send)]
    impl FileSource for RootSource {
        async fn home(&self) -> Result<String, FileError> {
            Err(unused("home"))
        }

        async fn realpath(&self, _path: &str) -> Result<String, FileError> {
            Err(unused("realpath"))
        }

        async fn read_dir(&self, _path: &str) -> Result<Vec<FileEntry>, FileError> {
            Err(unused("read_dir"))
        }

        async fn mkdir(&self, _path: &str) -> Result<(), FileError> {
            Err(unused("mkdir"))
        }

        async fn remove_file(&self, _path: &str) -> Result<(), FileError> {
            Err(unused("remove_file"))
        }

        async fn remove_dir(&self, _path: &str) -> Result<(), FileError> {
            Err(unused("remove_dir"))
        }

        async fn rename(&self, _old: &str, _new: &str) -> Result<(), FileError> {
            Err(unused("rename"))
        }

        async fn copy_in(
            &self,
            local: PathBuf,
            dir: &str,
            _progress: Option<UnboundedSender<u64>>,
        ) -> Result<String, FileError> {
            self.plain.store(true, Ordering::SeqCst);
            Ok(file_path(
                dir,
                &local.file_name().unwrap_or_default().to_string_lossy(),
            ))
        }

        async fn copy_out(
            &self,
            _path: &str,
            _local: PathBuf,
            _progress: Option<UnboundedSender<u64>>,
        ) -> Result<(), FileError> {
            Err(unused("copy_out"))
        }

        async fn writable(&self, _path: &str) -> bool {
            false
        }

        fn can_write_as_root(&self) -> bool {
            self.rooted
        }

        async fn copy_in_as_root(&self, local: PathBuf, dir: &str) -> Result<String, FileError> {
            self.as_root.store(true, Ordering::SeqCst);
            Ok(file_path(
                dir,
                &local.file_name().unwrap_or_default().to_string_lossy(),
            ))
        }

        fn is_local(&self) -> bool {
            true
        }
    }

    /// A pane over a [`RootSource`], with the two flags its writes set.
    ///
    /// `rooted` is whether the source claims a root to write as, and `writable`
    /// is the verdict the panel took before the pane was built — the same two
    /// facts the header branches on.
    fn root_pane(
        cx: &mut TestAppContext,
        rooted: bool,
        writable: bool,
    ) -> (Entity<EditorPane>, Arc<AtomicBool>, Arc<AtomicBool>) {
        let session = cx.new(Session::dormant);
        let as_root = Arc::new(AtomicBool::new(false));
        let plain = Arc::new(AtomicBool::new(false));
        let source: Arc<dyn FileSource> = Arc::new(RootSource {
            rooted,
            as_root: as_root.clone(),
            plain: plain.clone(),
        });
        let file = decode(b"one\ntwo\n").expect("valid UTF-8");
        let pane = cx.new(|cx| {
            EditorPane::new(
                session,
                source,
                "/etc".to_owned(),
                SharedString::from("hosts"),
                file,
                writable,
                cx,
            )
        });
        (pane, as_root, plain)
    }

    /// What pressing "Edit as root" buys, from the pane's side: the buffer that
    /// refused every edit takes them again, and the menu rows that follow that
    /// flag come back with it. The menu is asserted here rather than left to
    /// [`MenuState`]'s own tests because the interesting claim is not that
    /// `writable` enables the row — that is pinned above — but that the unlock
    /// moves the thing the row reads.
    #[gpui::test]
    fn editing_as_root_unlocks_the_buffer_and_the_rows_that_write(cx: &mut TestAppContext) {
        let (pane, ..) = root_pane(cx, true, false);
        pane.update(cx, |pane, cx| pane.edit_as_root(cx));
        pane.read_with(cx, |pane, cx| {
            let editor = pane.editor.read(cx);
            assert!(!editor.is_read_only(), "the buffer is still locked");
            assert!(pane.as_root, "the pane did not take the flag");
            let state = MenuState::of(editor);
            assert!(state.save(), "the menu still greys the row saving is on");
            assert!(state.paste());
        });
    }

    /// And what it buys from the source's side, which is the half a locked
    /// buffer cannot show: the bytes leave through the elevated call. Nothing
    /// else about the save moves — the staging, the name, the encoding are the
    /// ordinary ones — so this is the only assertion that can tell the two
    /// apart.
    #[gpui::test]
    fn a_save_from_a_pane_unlocked_as_root_goes_out_through_the_elevated_call(
        cx: &mut TestAppContext,
    ) {
        let (pane, as_root, plain) = root_pane(cx, true, false);
        pane.update(cx, |pane, cx| pane.edit_as_root(cx));
        pane.update(cx, |pane, cx| pane.save(cx));
        cx.run_until_parked();

        assert!(
            as_root.load(Ordering::SeqCst),
            "the save was not made as root"
        );
        assert!(
            !plain.load(Ordering::SeqCst),
            "the save went out as the account that may not write the file"
        );
        pane.read_with(cx, |pane, _cx| assert!(!pane.saving));
    }

    /// The other side of that branch. A pane that opened writable saves the
    /// ordinary way whether or not the source *could* have written as root —
    /// the capability is not a preference, and an editor that quietly used the
    /// most powerful account available would be one nothing on screen warned
    /// about.
    #[gpui::test]
    fn a_writable_pane_saves_as_itself_even_where_a_root_was_available(cx: &mut TestAppContext) {
        let (pane, as_root, plain) = root_pane(cx, true, true);
        pane.update(cx, |pane, cx| pane.save(cx));
        cx.run_until_parked();

        assert!(
            plain.load(Ordering::SeqCst),
            "the ordinary save did not happen"
        );
        assert!(!as_root.load(Ordering::SeqCst), "an unasked-for elevation");
    }

    /// Defence in depth: nothing draws the button on a source with no root to
    /// write as, so this is reachable only by a caller that has stopped asking
    /// first. It refuses outright rather than unlocking the buffer, because a
    /// buffer that takes edits and then fails every save is worse than the
    /// locked one it replaced — the user would find out at the save, having
    /// typed.
    #[gpui::test]
    fn a_source_with_no_root_to_write_as_stays_locked(cx: &mut TestAppContext) {
        let (pane, as_root, _plain) = root_pane(cx, false, false);
        pane.update(cx, |pane, cx| pane.edit_as_root(cx));
        pane.read_with(cx, |pane, cx| {
            assert!(
                pane.editor.read(cx).is_read_only(),
                "the buffer was unlocked with nowhere to save to"
            );
            assert!(!pane.as_root);
        });

        // And the save that a caller might make next is still the one a
        // read-only pane makes, which is none.
        pane.update(cx, |pane, cx| pane.save(cx));
        cx.run_until_parked();
        assert!(!as_root.load(Ordering::SeqCst));
    }

    #[test]
    fn paths_are_joined_the_way_the_panel_joins_them() {
        assert_eq!(file_path("/etc", "hosts"), "/etc/hosts");
        // A root already ends in the separator; a second one would name a
        // different path on some servers and none on others.
        assert_eq!(file_path("/", "hosts"), "/hosts");
        assert_eq!(file_path("", "hosts"), "hosts");
    }
}
