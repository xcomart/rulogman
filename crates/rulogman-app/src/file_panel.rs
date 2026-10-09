//! The file panel: a browser for the filesystem of the session in the active
//! pane.
//!
//! What that filesystem *is* reaches the panel as a [`FileSource`] and is never
//! named here: an SSH session hands over the server's, seen through its SFTP
//! channel, and a session running a shell on this machine hands over this
//! computer's. Every command below is written against the trait and works
//! unchanged over either.
//!
//! The one thing that does differ is what the panel *says*. "Upload" is the
//! wrong word for putting a file into a directory on the disk it is already on,
//! so the sentences come in pairs — `files.*` and `files.local.*` — and
//! [`FileSource::is_local`] picks between them. It is remembered per session in
//! [`SessionState::is_local`] rather than kept as one flag on the panel,
//! because two tabs can be looking at two different kinds of filesystem and the
//! panel switches between them on every tab change. Nothing else branches on
//! it: the same handler runs either way, and only the wording changes.
//!
//! The panel is a single entity owned by the workspace, not one per session.
//! What *is* per session is the browsing state — the directory being listed,
//! its entries, the selection, the scroll position — so switching tabs or panes
//! restores what that session was showing instead of asking the server again.
//! [`FilePanel::set_session`] is the only way in: the workspace calls it while
//! rendering, and a call naming the session already on screen is a no-op.
//!
//! Two things drive the panel, and they are deliberately allowed to disagree:
//!
//! * the **shell**, through [`Session::cwd`] — a prompt configured to emit
//!   `OSC 7` reports every `cd`, and the panel follows it;
//! * the **user**, through clicks in the list.
//!
//! Manual navigation wins until the shell moves again, at which point the panel
//! follows once more. That is the whole tracking rule; there is no "locked"
//! mode, because the next `cd` re-synchronises the two anyway.
//!
//! Every request is a [`cx.spawn`](gpui::Context::spawn) away, and a source's
//! futures are runtime-agnostic, so a transfer runs on gpui's own executor
//! without blocking a repaint. Replies are matched against a per-session
//! generation counter: clicking through three directories quickly leaves two
//! listings in flight whose answers must not overwrite the third.

mod navigation;
mod operations;
use operations::is_plain_name;
#[cfg(test)]
use operations::{needs_walking, plan_upload};
mod paths;
#[cfg(test)]
use paths::{crumb_width, drive_prefix};
use paths::{
    crumbs, file_name, fold_budget, format_size, is_root, name_is_clipped, needs_separator,
    root_targets, suggested_directory,
};
mod render;
use std::any::Any;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedReceiver};
use gpui::{
    AnyElement, App, AsyncApp, ClickEvent, ClipboardItem, Context, Div, DragMoveEvent, ElementId,
    Entity, EntityId, EventEmitter, ExternalPaths, FocusHandle, Focusable, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, PathPromptOptions, Pixels, Point, ScrollHandle, SharedString,
    Subscription, WeakEntity, Window, div, prelude::*, px, relative,
};
use rulogman_term::Charset;
use unicode_width::UnicodeWidthStr;

use crate::app_settings;
use crate::editor_pane::{LoadError, MAX_EDIT_BYTES, TextFile, file_path, read_file};
use crate::files::{FileEntry, FileError, FileSource, RootAccess};
use crate::i18n::{input_menu_labels, ts};
use crate::icons;
use crate::session::Session;
use rugpui::scrollbar::INSET;
use rugpui::{
    Button, ButtonVariant, ContextMenu, DraggedThumb, MenuEntry, ResizeHandle, Scrollbar,
    ScrollbarAxis, ScrollbarState, TextInput, Theme, hide_later, hide_now, scroll_to, scrolled,
    theme, tooltip_label,
};

/// Width the panel opens at, in pixels.
///
/// Wide enough for a typical file name plus its size column; dragging the right
/// edge takes it from there.
const DEFAULT_PANEL_WIDTH: f32 = 260.;

/// Narrowest the panel may be dragged, in pixels.
///
/// Below this the header's path and the toolbar buttons start colliding, and a
/// panel too narrow to read is indistinguishable from one the user meant to
/// close — which the toggle already does, and reversibly.
const MIN_PANEL_WIDTH: f32 = 180.;

/// Widest the panel may be dragged, in pixels.
///
/// The panel is a sidebar next to the terminals, not a half of the window; the
/// cap is what stops a slipped drag from squeezing the panes down to nothing on
/// a small display.
const MAX_PANEL_WIDTH: f32 = 560.;

/// Width of the grab area along the panel's right edge, in pixels.
///
/// The edge itself is the panel's hairline border, far too thin to hit. The
/// handle is laid over it absolutely so that widening the grab area costs the
/// listing no room. Named here rather than left to [`rugpui::ResizeHandle`]'s
/// own default because the listing's scrollbar is inset by the same number to
/// keep its thumb clear of the grab area: the two have to move together, so
/// there is one of them and the handle is told what it is.
const PANEL_HANDLE: f32 = 6.;

/// Element id of the listing's overlay scroll indicator.
///
/// One panel shows one session's listing at a time, so one id covers them all —
/// and it is what tells a drag of this bar from a drag of any other in the
/// window.
const LIST_SCROLLBAR: &str = "file-panel-scrollbar";

/// Horizontal padding of the header, per side, in pixels.
///
/// Also what [`fold_budget`] takes off the panel's width before working out how
/// much path fits, so the two cannot drift apart.
const HEADER_PADDING: f32 = 8.;

/// Width of one character of the header's path, in pixels — an average, not a
/// measurement.
///
/// The header is drawn in the UI font at 11px, which is proportional: `i` and
/// `W` are nothing like each other. This is the figure that makes a 260px
/// panel — the width the panel opens at — hold about 38 characters, which is
/// what the header showed in full before it could be resized.
const CRUMB_CHAR: f32 = 6.4;

/// Fewest characters of path the breadcrumb is ever folded down to.
///
/// At [`MIN_PANEL_WIDTH`] the arithmetic already leaves more than this; the
/// floor is here so that no future width, padding or font change can produce a
/// budget too small for the root and a leaf to survive it.
const MIN_PATH_CHARS: usize = 12;

/// Label of the piece standing for the root of a POSIX filesystem. Punctuation,
/// never translated — and, with the `C:/` a drive root spells itself, one of the
/// two labels that carry their own separator.
const ROOT_CRUMB: &str = "/";

/// Label of the piece standing for everything the header could not fit.
const FOLD_CRUMB: &str = "\u{2026}";

/// What goes between two breadcrumb pieces.
const CRUMB_SEPARATOR: &str = "/";

/// Width of one column of a listing row's name, in pixels — an average, like
/// [`CRUMB_CHAR`] and for the same reason.
///
/// The same figure scaled from the header's 11px to the row's 12px
/// (`6.4 × 12 ÷ 11 ≈ 6.98`) and rounded *up*. The rounding direction is the
/// point: a wider estimate yields a smaller budget and so a tooltip slightly
/// before the name actually needs one, which is the harmless way to be wrong.
const ROW_CHAR: f32 = 7.;

/// Horizontal padding of a listing row, in pixels.
const ROW_PADDING: f32 = 8.;

/// Gap between a listing row's icon, name, badge and size, in pixels.
const ROW_GAP: f32 = 6.;

/// Width of one character of the size column, in pixels.
///
/// The column is drawn at 11px, so this is [`CRUMB_CHAR`] itself — but the
/// strings are `"1023 B"` and `"1.5 MB"`, all digits, spaces and capitals,
/// which run wider than an average of ordinary prose. Rounded up accordingly.
const SIZE_CHAR: f32 = 7.;

/// Size of the icon leading a listing row, in pixels.
const ROW_ICON: f32 = 14.;

/// Size of the badge marking a symbolic link, in pixels.
const BADGE_ICON: f32 = 11.;

/// Size of a toolbar button's icon, in pixels.
const TOOLBAR_ICON: f32 = 15.;

/// Height of the transfer progress bar, in pixels.
///
/// A hairline rather than a widget: the panel is a sidebar, and the percentage
/// in the line above it is what a user actually reads. The bar is there to make
/// "still moving" visible at a glance.
const PROGRESS_BAR: f32 = 3.;

/// Style group of one toolbar button, so hovering the button recolours the
/// icon inside it: an SVG takes its tint from its own `text_color`, which —
/// unlike a text glyph's — does not inherit from the button around it.
const BUTTON_GROUP: &str = "file-panel-button";

/// The row standing for the parent directory. Punctuation, never translated.
const PARENT_NAME: &str = "..";

/// How long a success message stays on the status line before it goes away.
///
/// Long enough to be read after looking away from the panel, short enough that
/// the line is not still claiming something finished minutes after it did.
/// Failures are deliberately exempt — see [`Notice`].
const NOTICE_LINGER: Duration = Duration::from_secs(5);

/// The panel's right edge, while a drag is holding it.
///
/// Carries nothing: there is one panel and one edge, so the type alone says
/// what is being dragged. Being its own type is the point — it is what keeps
/// an edge drag from looking like the [`ExternalPaths`] drop the panel accepts,
/// since gpui routes both through the same drag machinery and tells them apart
/// by the payload's type.
struct DraggedPanelEdge;

/// Where one navigation gets its directory from.
enum Target {
    /// The login directory. Asked for once, when a session first appears in the
    /// panel and its shell has not reported a directory of its own.
    Home,
    /// A path to canonicalise before listing. This is how `..` is resolved:
    /// the server flattens `<current>/..` for us, so the panel never has to
    /// guess how the remote host spells a parent directory.
    Resolve(String),
    /// A path to list exactly as given.
    Exact(String),
}

/// The line along the bottom of the panel.
///
/// The two halves have deliberately different lifetimes:
///
/// * an **[`Notice::Error`]** stays until something works. A failure the user
///   did not happen to be looking at is a failure they never saw, and the next
///   successful listing is the earliest moment it stops being true.
/// * an **[`Notice::Info`]** goes away on its own after [`NOTICE_LINGER`]. It
///   reports something that has already finished, so leaving it up makes the
///   panel look permanently mid-transfer.
///
/// A successful listing also clears an error, except when the action that just
/// finished asked for that listing itself — see [`SessionState::keep_notice`],
/// which is what keeps "Deleted 3 items." on screen long enough for its own
/// timer to be the thing that removes it.
enum Notice {
    /// Progress, in the panel's muted text color.
    Info(SharedString),
    /// A failure, in the danger color.
    Error(SharedString),
}

impl Notice {
    /// Wraps a failure in the localised sentence that frames it.
    ///
    /// The detail itself stays in English: it comes from the server or from the
    /// local filesystem, and translating half a sentence would only make it
    /// harder to search for. `local` picks which filesystem the prefix names,
    /// so that a failure over a shell on this machine does not announce itself
    /// as a remote one.
    fn from_error(error: &FileError, local: bool) -> Self {
        Self::Error(ts!(
            key(local, "files.failed", "files.local.failed"),
            error = error.to_string()
        ))
    }
}

/// The status line for a file that could not be opened for editing.
///
/// The two refusals the editor itself raises are worded here rather than in
/// [`crate::editor_pane`], because they are the panel's to explain: they are
/// answers to a menu row the panel offered. A transport failure is folded
/// through the same sentence every other panel command uses, so "the server
/// said no" reads the same whether it was a listing or a file that was refused.
fn edit_notice(error: &LoadError, local: bool) -> Notice {
    match error {
        LoadError::TooLarge => Notice::Error(ts!(
            "files.edit_too_large",
            limit = MAX_EDIT_BYTES / 1024 / 1024
        )),
        LoadError::NotUtf8 => Notice::Error(ts!("files.edit_not_text")),
        LoadError::Transport(error) => Notice::from_error(error, local),
    }
}

/// Picks between a sentence worded for a remote filesystem and its local twin.
///
/// Keys rather than finished sentences, so the choice is made before the
/// lookup and no translation is ever assembled out of halves. Only the keys
/// whose English says *remote*, *server*, *upload* or *download* have a twin at
/// all; everything else — the loading line, the delete question, the naming
/// fields — describes the same act on either side and is shared, which is also
/// why this takes both keys instead of deriving one from the other.
fn key(local: bool, remote: &'static str, here: &'static str) -> &'static str {
    if local { here } else { remote }
}

/// What the one batch a session may run at a time is doing.
///
/// The three share a progress slot rather than getting one each because they
/// share the constraint that made the slot exist: all three walk the same
/// remote directory, and two of them running against it at once would show a
/// listing that matches neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activity {
    /// Bytes going out to the server.
    Upload,
    /// Bytes coming in from it.
    Download,
    /// Entries being removed from it. The counters hold entries rather than
    /// bytes — a delete moves nothing, and "3 of 40 removed" is what a user
    /// waiting on one actually wants to know.
    Delete,
}

/// A batch transfer in flight for one session.
///
/// One of these exists per session at most, which is what makes the progress
/// line unambiguous: a second upload started while the first is running would
/// otherwise overwrite the counters of a transfer still using them, and the bar
/// would jump around describing neither. New requests are refused instead.
struct TransferProgress {
    /// What the batch is doing. Picks the wording and the unit of the counters.
    activity: Activity,
    /// Name of the file being moved — or removed — right now.
    name: SharedString,
    /// Bytes moved since the batch started, across every file in it; entries
    /// removed, for a delete.
    done: u64,
    /// Bytes the whole batch will move, known before the first chunk because
    /// the plan is built from sizes. Zero when every file in it is empty. For a
    /// delete, the number of entries the walk found.
    total: u64,
    /// Whole percent last put on screen.
    ///
    /// A 64 KB chunk of a large file moves the bar by a fraction of a pixel, so
    /// a repaint per chunk would be wasted work; the panel only notifies when
    /// this changes. It is also what the status line shows, so the number and
    /// the bar can never disagree.
    percent: u8,
}

impl TransferProgress {
    /// A batch that has not moved anything yet.
    fn new(activity: Activity) -> Self {
        Self {
            activity,
            name: SharedString::default(),
            done: 0,
            total: 0,
            percent: 0,
        }
    }

    /// How far along the batch is, as a whole percent.
    ///
    /// A batch of nothing but empty files has no bytes to count, and reporting
    /// it as 0% forever would be a lie: it is as finished as it will ever be.
    fn percent_of(done: u64, total: u64) -> u8 {
        if total == 0 {
            return 100;
        }
        let percent = done.saturating_mul(100) / total;
        u8::try_from(percent.min(100)).unwrap_or(100)
    }

    /// The bar's fill, as a fraction of its track.
    fn fraction(&self) -> f32 {
        f32::from(self.percent) / 100.
    }

    /// The status line for this transfer.
    ///
    /// A local source collapses the two transfer directions into one sentence:
    /// copying a file into the listed directory and copying one out of it are
    /// the same act when both ends are this computer, and nothing in the line
    /// would distinguish them to the person reading it. A delete is worded the
    /// same either way — nothing moves, so nothing is being uploaded or
    /// downloaded to begin with.
    fn line(&self, local: bool) -> SharedString {
        let key = match (self.activity, local) {
            (Activity::Delete, _) => "files.deleting",
            (Activity::Upload | Activity::Download, true) => "files.local.copying",
            (Activity::Upload, false) => "files.uploading",
            (Activity::Download, false) => "files.downloading",
        };
        ts!(key, name = self.name.clone(), percent = self.percent)
    }
}

/// A question the panel is waiting on an answer to, drawn where the status line
/// would otherwise be.
///
/// All of these are modal in intent but not on screen: a dialog over the window
/// would hide the very listing that says what is about to be renamed, deleted
/// or created next to, so the question is asked in the panel, under the rows it
/// is about.
enum Prompt {
    /// Names selected for deletion, in display order, awaiting confirmation.
    ///
    /// Held as names rather than as entries so that a listing arriving in the
    /// meantime cannot leave the question pointing at rows that moved.
    Delete(Vec<String>),
    /// A rename of one entry: the name it has now, and the field holding the
    /// name it should get.
    Rename {
        /// Name as it is on the server right now.
        from: String,
        /// The field, prefilled with `from` so a small edit stays small.
        input: Entity<TextInput>,
    },
    /// A directory to create in the listed directory.
    NewFolder {
        /// The field, empty to start with — there is no name to edit yet, so it
        /// leans on its placeholder instead of on a prefill.
        input: Entity<TextInput>,
    },
}

impl Prompt {
    /// The text field this question is answered in, if it has one.
    ///
    /// Both of the naming questions need the keyboard the moment they appear
    /// and nothing else does, so the focus logic asks for the field rather than
    /// matching on the variant and having to grow a third arm each time.
    fn field(&self) -> Option<&Entity<TextInput>> {
        match self {
            Self::Delete(_) => None,
            Self::Rename { input, .. } | Self::NewFolder { input } => Some(input),
        }
    }
}

/// An open menu over the panel: where it hangs, and what it lists.
struct PanelMenu {
    /// Top-left corner of the panel, in window coordinates.
    at: Point<Pixels>,
    /// Which menu it is, and what it was built from.
    kind: MenuKind,
}

/// Which of the panel's menus is open.
///
/// One slot serves both because they cannot be open at once: every menu draws a
/// full-window backdrop that swallows the next press and closes itself, so the
/// gesture that would open the second one only ever dismisses the first.
enum MenuKind {
    /// A right-click over the listing. The flag says whether the press landed
    /// on a row rather than on empty space, which decides which commands the
    /// menu offers — not which rows are selected, since the selection was
    /// already settled before the menu opened.
    Listing { on_rows: bool },
    /// A breadcrumb piece's dropdown: the directories it offers to move to, in
    /// listing order.
    Crumb(Vec<CrumbTarget>),
}

/// One row of a breadcrumb dropdown: what it says, and where it goes.
#[derive(Clone)]
struct CrumbTarget {
    /// Name of the directory, as the row shows it.
    label: SharedString,
    /// Absolute remote path the row navigates to.
    path: String,
}

/// One directory on the way to the listed one, as the header draws it.
struct Crumb {
    /// Text drawn for the piece: a directory name, a root — `/`, or `C:/` on a
    /// drive of this machine — or an ellipsis for the pieces the header had no
    /// room for.
    label: SharedString,
    /// What a press on the piece offers.
    menu: CrumbMenu,
}

/// What a breadcrumb piece's dropdown lists.
#[derive(Clone)]
enum CrumbMenu {
    /// The leading piece — `/`, or a drive such as `C:/` — whose dropdown is
    /// whichever of two things the source has to offer. The string is the root
    /// itself, which is both the path the piece navigates to and the directory
    /// its fallback menu lists.
    ///
    /// A source with several roots — a Windows filesystem, whose drives are
    /// separate trees — offers *those*, because nothing else in the panel can
    /// reach them: `..` from `C:/` has nowhere to go, so without this a session
    /// that started on one drive would be shut inside it.
    ///
    /// A source with one root has nothing to choose between and falls back to
    /// listing the root's own subdirectories. That makes its menu identical to
    /// the first name's, which is the natural reading of "somewhere else at
    /// this level" for a piece that has no level above it, and is better than a
    /// root that cannot be pressed at all. Which of the two it is cannot be
    /// known until the source has been asked, so it is not decided here.
    Root(String),
    /// The directories beside this one, still to be read from the server. The
    /// string is the directory whose subdirectories they are — this piece's
    /// parent.
    Siblings(String),
    /// The pieces that were folded away, in path order. Already known, so this
    /// menu opens without asking the server anything.
    Folded(Vec<CrumbTarget>),
}

impl Crumb {
    /// The absolute path this piece stands for, or `None` for the ellipsis —
    /// which stands for several directories rather than for one.
    fn path(&self) -> Option<String> {
        match &self.menu {
            // A root is its own parent, so its path *is* the root it carries;
            // every other piece hangs off the directory it lists.
            CrumbMenu::Root(root) => Some(root.clone()),
            CrumbMenu::Siblings(directory) => Some(join(directory, &self.label)),
            CrumbMenu::Folded(_) => None,
        }
    }
}

/// What one session is looking at, kept while the session lives.
struct SessionState {
    /// Whether this session's filesystem is the one rulogman runs on.
    ///
    /// Read off [`FileSource::is_local`] when the state is created and never
    /// again: a session is bound to its transport for life, so the answer
    /// cannot change under it. Purely a wording switch — see the module
    /// documentation — and per session rather than per panel because the tab
    /// beside this one may well be the other kind.
    is_local: bool,
    /// Directory currently listed. `None` until the first listing lands.
    path: Option<String>,
    /// Entries of [`SessionState::path`], directories first and then by name.
    entries: Vec<FileEntry>,
    /// Names of the selected entries.
    ///
    /// A set rather than a list because membership is what every reader asks —
    /// "is this row selected?", once per row per frame — while the *order* of a
    /// selection is never stored: it is read back off [`SessionState::entries`]
    /// so that it always matches what is on screen.
    selected: BTreeSet<String>,
    /// Row a range selection measures from: the last row clicked without
    /// <kbd>Shift</kbd>. `None` until something is clicked.
    anchor: Option<String>,
    /// The question waiting for an answer under the listing, if any.
    prompt: Option<Prompt>,
    /// The shell directory the panel last followed.
    ///
    /// Compared against [`Session::cwd`] on every session notification; a
    /// difference is a `cd` the panel has not caught up with yet.
    followed: Option<String>,
    /// Whether the first listing has been attempted.
    ///
    /// Without this a failed initial listing would be retried on every chunk of
    /// terminal output, because the session notifies on each one and the panel's
    /// "no path yet" condition would still hold.
    attempted: bool,
    /// Bumped by every navigation. A reply carrying an older value belongs to a
    /// directory the user has already left, and is dropped.
    generation: u64,
    /// Whether a listing is in flight.
    busy: bool,
    /// The bottom status line.
    notice: Option<Notice>,
    /// Whether the next listing to land must leave the status line alone.
    ///
    /// An action that changes the directory — an upload, a delete, a rename —
    /// says how it went and *then* asks for a fresh listing. Without this the
    /// listing would arrive a moment later and clear the very sentence it was
    /// asked for, so every one of those messages would flash and vanish.
    keep_notice: bool,
    /// Bumped every time something is said on the status line.
    ///
    /// An expiring message leaves a timer running behind it, and by the time
    /// that timer fires the line may be carrying something else entirely. The
    /// timer therefore remembers the value this had when it was armed and does
    /// nothing unless it still matches — so a message never takes a later one
    /// down with it.
    notice_epoch: u64,
    /// The transfer running for this session, if any.
    ///
    /// Outranks [`SessionState::notice`] on screen and doubles as the lock that
    /// keeps a second transfer from starting.
    transfer: Option<TransferProgress>,
    /// Whether this session's overlay scroll indicator is on screen.
    scrollbar: ScrollbarState,
    /// Vertical scroll of the list, kept per session so returning to a tab
    /// returns to the same place in its directory.
    scroll: ScrollHandle,
}

impl SessionState {
    /// A state that has not listed anything yet, for a source that is — or is
    /// not — this computer's own filesystem.
    fn new(is_local: bool) -> Self {
        Self {
            is_local,
            path: None,
            entries: Vec::new(),
            selected: BTreeSet::new(),
            anchor: None,
            prompt: None,
            followed: None,
            attempted: false,
            generation: 0,
            busy: false,
            notice: None,
            keep_notice: false,
            notice_epoch: 0,
            transfer: None,
            scroll: ScrollHandle::new(),
            scrollbar: ScrollbarState::new(),
        }
    }

    /// The selected entries, in display order.
    ///
    /// Driven by [`SessionState::entries`] rather than by the set, so a name
    /// left over from a directory that is no longer listed simply drops out
    /// instead of having to be pruned.
    fn selection(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries
            .iter()
            .filter(|entry| self.selected.contains(&entry.name))
    }

    /// How many listed entries are selected.
    fn selected_count(&self) -> usize {
        self.selection().count()
    }

    /// The selected entry, or `None` unless the selection is exactly one.
    ///
    /// What every command phrased in the singular asks for: renaming, copying a
    /// name or a path, and opening a directory all need the one entry the menu
    /// is speaking about, and none of them means anything over a selection of
    /// several.
    fn only_selected(&self) -> Option<&FileEntry> {
        let mut selection = self.selection();
        let entry = selection.next()?;
        selection.next().is_none().then_some(entry)
    }

    /// Replaces the selection with `name` alone, and anchors ranges on it.
    fn select_only(&mut self, name: &str) {
        self.selected.clear();
        self.selected.insert(name.to_owned());
        self.anchor = Some(name.to_owned());
    }

    /// Puts `notice` on the status line, retiring whatever it replaced.
    ///
    /// Returns the epoch the message was said at, which is what an expiry timer
    /// has to be armed with. Every write to the line goes through here so that
    /// no message can be left with a timer belonging to an older one.
    fn say(&mut self, notice: Notice) -> u64 {
        self.notice_epoch = self.notice_epoch.wrapping_add(1);
        self.notice = Some(notice);
        self.notice_epoch
    }

    /// Drops the selection, the range anchor and any open question.
    ///
    /// Called wherever the listing stops describing what these refer to: a new
    /// directory, or a session going away from the panel.
    fn reset_selection(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.prompt = None;
    }
}

/// What the panel asks the workspace for.
///
/// One variant, and it is the one thing the panel cannot do for itself: a pane
/// belongs to a tab, and the panel does not own the tabs. Everything else the
/// menu offers happens inside the panel.
pub enum FilePanelEvent {
    /// A file the user asked to edit, already fetched and decoded.
    ///
    /// The reading and every refusal that comes with it — too large, not text,
    /// the transfer failed — happen here rather than in the pane, because this
    /// is where the status line is that explains a refusal. What the workspace
    /// receives is therefore always a file that can be shown.
    OpenEditor(Box<OpenEditor>),
}

/// A file the panel has read and wants a pane for.
///
/// Boxed into [`FilePanelEvent`] because it carries the whole file: a variant as
/// large as its payload would make every other event as expensive to move.
pub struct OpenEditor {
    /// The session the file was read out of, which is what decides the colours
    /// the pane draws it in.
    pub session: Entity<Session>,
    /// The filesystem it lives on, kept so the pane can write it back.
    pub source: Arc<dyn FileSource>,
    /// The directory holding it, in the source's own spelling.
    pub dir: String,
    /// Its name within [`OpenEditor::dir`].
    pub name: SharedString,
    /// Its contents, and what has to be restored to write them back.
    pub file: TextFile,
    /// Exact bytes read, used to reject a save after an external change.
    pub original_bytes: Vec<u8>,
    /// Whether saving it would have been permitted at the moment it was read.
    ///
    /// Carried on the event rather than asked for by the pane because the probe
    /// is a round trip on the same source the read just used, and the pane is
    /// built on the frame the event arrives on — asking there would stall the
    /// window for as long as the server took to answer. Everything else on this
    /// struct travels for the same reason, which is that the panel does the
    /// waiting and the workspace does the drawing.
    pub writable: bool,
    /// What the source would want in order to write it as *root*, asked only
    /// where [`OpenEditor::writable`] came back `false`.
    ///
    /// [`RootAccess::None`] on every writable file, and that is a statement
    /// about what was asked rather than about what is true: a source with a
    /// root to offer still has one, and nobody needs to know because the pane
    /// reads this only while its buffer is locked. Travelling on the event for
    /// the same reason `writable` does — the probe is up to three round trips
    /// on a session, and the frame that builds the pane cannot wait for it.
    pub root_access: RootAccess,
}

/// The remote file panel.
pub struct FilePanel {
    /// Session whose directory is on screen. `None` while no tab is open.
    session: Option<Entity<Session>>,
    /// Browsing state per session, keyed by the session entity.
    ///
    /// Entries are dropped by [`FilePanel::forget_session`] when a pane closes;
    /// nothing else removes them, so a session keeps its place for as long as
    /// it is open.
    states: HashMap<EntityId, SessionState>,
    /// How wide the panel is drawn, in pixels.
    ///
    /// Session state only, like the workspace's `panel_open` flag: persisting it
    /// would mean a settings key, and re-dragging an edge is cheap enough that
    /// the key would earn its keep only once there is more to remember about the
    /// panel than a flag and a number.
    width: f32,
    /// The menu currently open over the panel, if any.
    ///
    /// Panel state rather than session state: a menu is a gesture in progress,
    /// and a gesture does not survive the tab switch that would be the only way
    /// to leave it behind.
    context: Option<PanelMenu>,
    /// Whether a breadcrumb dropdown is waiting on the listing behind it.
    ///
    /// A breadcrumb press asks the server which directories sit beside the
    /// piece, and the menu opens only when the answer lands. Without this a
    /// second press meanwhile would put a second request in flight, and the
    /// menu would open twice — the second time at a position the pointer has
    /// already left.
    crumb_pending: bool,
    /// Whether the rename field should be given the keyboard on the next
    /// render.
    ///
    /// Focus cannot be moved from the click that opens the field, because the
    /// field does not exist yet at that point; this defers it by exactly one
    /// frame, the way the connection dialog focuses its first field.
    focus_prompt: bool,
    /// Keyboard focus for the panel as a whole.
    ///
    /// The panel has no key bindings of its own yet; the handle exists so that
    /// clicking the panel takes focus *away* from the terminal, which is what
    /// lets the accent frame say which side of the window a keystroke would go
    /// to. Nested handles — the rename field's, say — keep working because gpui
    /// runs the innermost auto-focus listener first and then prevents the
    /// default, so the root never steals focus back from its own children.
    focus_handle: FocusHandle,
    /// Watches the active session for directory and status changes.
    _observer: Option<Subscription>,
}

impl FilePanel {
    /// An empty panel, attached to no session.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            session: None,
            states: HashMap::new(),
            width: DEFAULT_PANEL_WIDTH,
            context: None,
            crumb_pending: false,
            focus_prompt: false,
            focus_handle: cx.focus_handle(),
            _observer: None,
        }
    }

    /// Widens or narrows the panel to follow a drag of its right edge.
    ///
    /// The width is read off the pointer rather than accumulated as a delta:
    /// the panel's left edge never moves, so the distance from it *is* the
    /// width, and a gesture that wandered outside the window comes back to the
    /// right place instead of to wherever the deltas summed to.
    fn drag_edge(&mut self, event: &DragMoveEvent<DraggedPanelEdge>, cx: &mut Context<Self>) {
        let width = f32::from(event.event.position.x - event.bounds.left());
        let width = width.clamp(MIN_PANEL_WIDTH, MAX_PANEL_WIDTH);
        if width == self.width || !width.is_finite() {
            return;
        }
        self.width = width;
        cx.notify();
    }

    /// Points the panel at `session`, keeping whatever it was showing before.
    ///
    /// Called from the workspace's render, so it must be cheap and idempotent:
    /// naming the session already on screen returns immediately, and only a real
    /// change re-subscribes and repaints.
    pub fn set_session(&mut self, session: Option<Entity<Session>>, cx: &mut Context<Self>) {
        let current = self.session.as_ref().map(Entity::entity_id);
        let next = session.as_ref().map(Entity::entity_id);
        if current == next {
            return;
        }

        // A question asked of the session leaving the panel is dropped rather
        // than parked: it names entries the user can no longer see, and a
        // confirmed delete has to be the one the user was just looking at.
        self.context = None;
        self.focus_prompt = false;
        if let Some(state) = current.and_then(|current| self.states.get_mut(&current)) {
            state.prompt = None;
        }

        // Only the active session is observed. A background session that
        // changes directory is caught the moment it becomes active again,
        // because `sync` compares against the directory last followed rather
        // than against the last one seen.
        self._observer = session
            .as_ref()
            .map(|session| cx.observe(session, |panel, _session, cx| panel.sync(cx)));
        self.session = session;
        self.sync(cx);
        cx.notify();
    }

    /// Drops the state of a session whose pane has closed.
    pub fn forget_session(&mut self, session: EntityId, cx: &mut Context<Self>) {
        if self.states.remove(&session).is_none() {
            return;
        }
        if self
            .session
            .as_ref()
            .is_some_and(|s| s.entity_id() == session)
        {
            self.session = None;
            self._observer = None;
        }
        cx.notify();
    }

    /// Brings the panel in step with the active session.
    ///
    /// Runs on every notification from that session — which means on every chunk
    /// of terminal output — so the common path is two string comparisons and no
    /// allocation beyond the directory itself.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let id = session.entity_id();
        let Some(source) = session.read(cx).files(cx) else {
            // Not connected (yet). The status change that connects the session
            // is itself a notification, so this is retried at the right moment.
            return;
        };
        let cwd = session.read(cx).cwd().map(str::to_owned);

        let target = {
            let state = self
                .states
                .entry(id)
                .or_insert_with(|| SessionState::new(source.is_local()));
            if !state.attempted {
                state.attempted = true;
                state.followed = cwd.clone();
                Some(cwd.clone().map_or(Target::Home, Target::Exact))
            } else if let Some(cwd) =
                cwd.filter(|cwd| state.followed.as_deref() != Some(cwd.as_str()))
            {
                state.followed = Some(cwd.clone());
                Some(Target::Exact(cwd))
            } else {
                None
            }
        };

        if let Some(target) = target {
            self.go(id, source, target, cx);
        }
    }

    /// Lists `target` for `session` and shows the result.
    ///
    /// Takes the session explicitly rather than reading the active one, so that
    /// a listing triggered by a finished transfer lands on the session that
    /// transferred, even if the user has since switched tabs.
    fn go(
        &mut self,
        session: EntityId,
        source: Arc<dyn FileSource>,
        target: Target,
        cx: &mut Context<Self>,
    ) {
        let generation = {
            let state = self
                .states
                .entry(session)
                .or_insert_with(|| SessionState::new(source.is_local()));
            state.generation = state.generation.wrapping_add(1);
            state.busy = true;
            state.generation
        };

        cx.spawn(async move |panel, cx| {
            let result = list(&source, target).await;
            panel
                .update(cx, |panel, cx| {
                    panel.listing_arrived(session, generation, result, cx);
                })
                .ok();
        })
        .detach();
        cx.notify();
    }

    /// Applies a listing, unless the user has moved on since it was asked for.
    fn listing_arrived(
        &mut self,
        session: EntityId,
        generation: u64,
        result: Result<(String, Vec<FileEntry>), FileError>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        // The stale-reply guard: a newer navigation has already bumped the
        // counter, so this answer describes a directory nobody is looking at.
        if state.generation != generation {
            return;
        }
        state.busy = false;

        match result {
            Ok((path, mut entries)) => {
                sort_entries(&mut entries);
                // A directory change invalidates the selection; staying on the
                // old names would let the download button — or, worse, the
                // delete — act on files from a directory that is no longer on
                // screen.
                let moved = state.path.as_deref() != Some(path.as_str());
                if moved {
                    state.reset_selection();
                    state.scroll.set_offset(Default::default());
                }
                state.path = Some(path);
                state.entries = entries;
                // Moving somewhere else drops whatever was said about the
                // directory being left, but a listing asked for *by* a finished
                // action has to leave that action's verdict on screen. Taken
                // unconditionally so the flag never survives into a listing it
                // was not set for.
                if !(std::mem::take(&mut state.keep_notice) && !moved) {
                    state.notice = None;
                }
            }
            Err(error) => {
                state.keep_notice = false;
                let notice = Notice::from_error(&error, state.is_local);
                state.say(notice);
            }
        }
        cx.notify();
    }

    /// Says `notice` on `session`'s status line, and retires it if it can be.
    ///
    /// A success message is given [`NOTICE_LINGER`] and then taken down by a
    /// timer; a failure is left alone, because only a later success can honestly
    /// replace it. The timer carries the epoch the message was said at, so a
    /// message that has since been replaced expires without touching whatever
    /// replaced it.
    fn show_notice(&mut self, session: EntityId, notice: Notice, cx: &mut Context<Self>) {
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        let expires = matches!(notice, Notice::Info(_));
        let epoch = state.say(notice);
        cx.notify();
        if !expires {
            return;
        }

        cx.spawn(async move |panel, cx| {
            cx.background_executor().timer(NOTICE_LINGER).await;
            panel
                .update(cx, |panel, cx| panel.expire_notice(session, epoch, cx))
                .ok();
        })
        .detach();
    }

    /// Takes the status line down, unless something newer is on it.
    fn expire_notice(&mut self, session: EntityId, epoch: u64, cx: &mut Context<Self>) {
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        if state.notice_epoch != epoch || state.notice.is_none() {
            return;
        }
        state.notice = None;
        cx.notify();
    }

    /// The listing's overlay scroll indicator, as it stands.
    ///
    /// Set in from the edge far enough to clear the panel's resize grip, which
    /// is pinned to that same edge and drawn after the listing — so a thumb any
    /// closer would be a thumb the grip took every press away from.
    fn scrollbar(state: &SessionState) -> Scrollbar {
        Scrollbar::for_handle(LIST_SCROLLBAR, ScrollbarAxis::Vertical, &state.scroll)
            .inset(PANEL_HANDLE + INSET)
            .fade(state.scrollbar.fade())
    }

    /// The state of the session the panel is showing, if it is showing one.
    fn active_state(&mut self) -> Option<&mut SessionState> {
        let session = self.session.as_ref()?.entity_id();
        self.states.get_mut(&session)
    }

    /// The same, for the readers that only look.
    fn showing(&self) -> Option<&SessionState> {
        let session = self.session.as_ref()?.entity_id();
        self.states.get(&session)
    }

    /// Puts the listing's bar up whenever it has been scrolled, and starts the
    /// clock that takes it down again.
    fn watch_list_scroll(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.active_state() else {
            return;
        };
        let scrolled = scrolled(&state.scroll, ScrollbarAxis::Vertical);
        let Some(epoch) = state.scrollbar.moved(scrolled) else {
            return;
        };

        // Looked up again when the timer fires rather than captured: the
        // session may be closed by then, and with it the listing this bar
        // belongs to.
        hide_later(epoch, cx, |panel| {
            panel.active_state().map(|s| &mut s.scrollbar)
        });
    }

    /// Scrolls the listing to wherever its thumb has been dragged.
    fn drag_scrollbar(&mut self, event: &DragMoveEvent<DraggedThumb>, cx: &mut Context<Self>) {
        let Some(state) = self.active_state() else {
            return;
        };
        let Some(progress) = Self::scrollbar(state).dragged(event, cx) else {
            return;
        };

        state.scrollbar.hold();
        scroll_to(&state.scroll, ScrollbarAxis::Vertical, progress);
        cx.notify();
    }

    /// Lets go of the listing's thumb, and starts the clock on the bar again.
    fn release_scrollbar(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.active_state() else {
            return;
        };
        let Some(epoch) = state.scrollbar.release() else {
            return;
        };

        hide_later(epoch, cx, |panel| {
            panel.active_state().map(|s| &mut s.scrollbar)
        });
        cx.notify();
    }

    /// Puts the listing's bar up while the pointer rests on the edge it rides,
    /// and starts it going the moment the pointer leaves.
    fn hover_scrollbar(&mut self, hovered: bool, cx: &mut Context<Self>) {
        let Some(state) = self.active_state() else {
            return;
        };
        if hovered {
            if state.scrollbar.hover_enter() {
                cx.notify();
            }
            return;
        }

        let Some(epoch) = state.scrollbar.hover_leave() else {
            return;
        };
        hide_now(self, epoch, cx, |panel| {
            panel.active_state().map(|s| &mut s.scrollbar)
        });
    }

    /// The active session's id, file source and current directory.
    ///
    /// `None` whenever an action has nothing to act on: no session, a session
    /// that is not connected, or one whose first listing has not landed.
    fn acting_on(&self, cx: &App) -> Option<(EntityId, Arc<dyn FileSource>, String)> {
        let session = self.session.as_ref()?;
        let id = session.entity_id();
        let source = session.read(cx).files(cx)?;
        let path = self.states.get(&id)?.path.clone()?;
        Some((id, source, path))
    }

    /// Whether `session` is browsing a filesystem on this computer.
    ///
    /// For the messages an action reports *about* a session, which may well not
    /// be the one on screen by the time they are said: a transfer keeps running
    /// through a tab switch, and its verdict belongs to the tab it started on.
    /// A session with no state yet has nothing in flight to report on, so the
    /// answer for it never reaches a sentence.
    fn source_is_local(&self, session: EntityId) -> bool {
        self.states
            .get(&session)
            .is_some_and(|state| state.is_local)
    }

    /// Whether the panel is currently drawing a filesystem on this computer.
    ///
    /// For the wording of the panel itself — the title, the tooltips, the menu
    /// rows. Answered from the browsing state, which took it from the source,
    /// and from the session itself only before the first source exists: that is
    /// the moment the placeholder has to say what *will* appear here, and a
    /// session's transport already decides the answer the source would give.
    fn showing_local(&self, cx: &App) -> bool {
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        match self.states.get(&session.entity_id()) {
            Some(state) => state.is_local,
            None => session.read(cx).is_local(),
        }
    }

    /// Claims the session's transfer slot, or refuses and says why.
    ///
    /// Returns `false` when a transfer is already running for that session, in
    /// which case the status line explains the refusal. Claiming happens before
    /// anything is scanned or asked of the server, so two drops in quick
    /// succession cannot both get past this.
    fn begin_transfer(
        &mut self,
        session: EntityId,
        activity: Activity,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(state) = self.states.get_mut(&session) else {
            return false;
        };
        if state.transfer.is_some() {
            state.say(Notice::Error(ts!("files.transfer_busy")));
            cx.notify();
            return false;
        }
        state.transfer = Some(TransferProgress::new(activity));
        state.notice = None;
        cx.notify();
        true
    }

    /// Applies a click on the row named `name` to the selection.
    ///
    /// The three gestures are the ones every file manager has, so they are
    /// spelled the same way here:
    ///
    /// * plain click — that row alone, and ranges measure from it afterwards;
    /// * <kbd>Ctrl</kbd> (<kbd>Cmd</kbd> on macOS) — add or drop that one row,
    ///   leaving the rest of the selection as it was;
    /// * <kbd>Shift</kbd> — everything between the anchor and this row, in the
    ///   order the listing is *displayed* in, which is the only order the user
    ///   can see and therefore the only one the gesture can mean.
    fn select(&mut self, name: &str, modifiers: Modifiers, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().map(Entity::entity_id) else {
            return;
        };
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        if !state.entries.iter().any(|entry| entry.name == name) {
            return;
        }

        if modifiers.secondary() {
            if !state.selected.remove(name) {
                state.selected.insert(name.to_owned());
            }
            // The anchor follows the last row touched even when the touch was
            // a removal: a Shift-click after a Ctrl-click extends from where
            // the pointer just was, not from wherever it started out.
            state.anchor = Some(name.to_owned());
        } else if let Some(anchor) = modifiers.shift.then(|| state.anchor.clone()).flatten() {
            let first = state.entries.iter().position(|entry| entry.name == anchor);
            let last = state.entries.iter().position(|entry| entry.name == name);
            match (first, last) {
                (Some(first), Some(last)) => {
                    let (low, high) = if first <= last {
                        (first, last)
                    } else {
                        (last, first)
                    };
                    state.selected = state
                        .entries
                        .iter()
                        .skip(low)
                        .take(high.saturating_sub(low).saturating_add(1))
                        .map(|entry| entry.name.clone())
                        .collect();
                }
                // The anchor is gone from the listing — a refresh dropped it —
                // so there is no range to speak of and the click stands alone.
                _ => state.select_only(name),
            }
        } else {
            state.select_only(name);
        }
        cx.notify();
    }
}

impl EventEmitter<FilePanelEvent> for FilePanel {}

impl Focusable for FilePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// One local file an upload will send, and where it goes.
struct PlannedUpload {
    /// Local file to read.
    local: PathBuf,
    /// Absolute remote directory it belongs in.
    directory: String,
    /// Size in bytes, so the batch total is known before the first chunk.
    size: u64,
}

/// A local tree flattened into the calls that reproduce it on the server.
#[derive(Default)]
struct UploadPlan {
    /// Remote directories to create, parents always before their children.
    directories: Vec<String>,
    /// Files to send, in the order they will be sent.
    files: Vec<PlannedUpload>,
    /// Bytes the whole batch will move.
    total: u64,
}

/// One remote file a download will fetch, and where it lands.
struct PlannedDownload {
    /// Absolute remote path to read.
    remote: String,
    /// Local path to write.
    local: PathBuf,
    /// Size in bytes, as the listing reported it.
    size: u64,
}

/// A remote tree flattened into the calls that reproduce it locally.
#[derive(Default)]
struct DownloadPlan {
    /// Local directories to create.
    directories: Vec<PathBuf>,
    /// Files to fetch, in the order they will be fetched.
    files: Vec<PlannedDownload>,
    /// Bytes the whole batch will move.
    total: u64,
}

impl DownloadPlan {
    /// Folds `other` into this plan, keeping both orderings intact.
    ///
    /// What makes this safe is that the plans being merged describe disjoint
    /// local trees — one per selected entry — so appending cannot put a file
    /// ahead of the directory it belongs in.
    fn absorb(&mut self, other: Self) {
        self.directories.extend(other.directories);
        self.files.extend(other.files);
        self.total = self.total.saturating_add(other.total);
    }
}

/// One remote entry a delete will remove.
struct Removal {
    /// Absolute remote path to remove.
    path: String,
    /// Name shown on the progress line while it goes.
    name: SharedString,
    /// Whether it needs the directory call rather than the file one.
    directory: bool,
}

/// How running a plan against the session's progress slot ended.
enum Ran {
    /// The batch ran to its end; `Some` carries the failure that stopped it.
    Finished(Option<FileError>),
    /// The panel went away part-way through, so there is nobody left to report
    /// to and the caller should simply stop.
    Abandoned,
}

/// Resolves `target` and lists what it points at.
async fn list(
    source: &Arc<dyn FileSource>,
    target: Target,
) -> Result<(String, Vec<FileEntry>), FileError> {
    let path = match target {
        Target::Home => source.home().await?,
        Target::Resolve(path) => source.realpath(&path).await?,
        Target::Exact(path) => path,
    };
    let entries = source.read_dir(&path).await?;
    Ok((path, entries))
}

/// Orders a listing the way a file manager does: directories first, then by
/// name ignoring case.
///
/// Case-insensitive order puts `Downloads` next to `documents` instead of in a
/// separate uppercase block, which is what makes a listing scannable. Ties —
/// two names differing only in case — fall back to the exact order so the
/// result is deterministic.
fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Joins a remote directory and a name with the protocol's separator.
///
/// SFTP paths are POSIX on the wire whatever the server runs on, so this never
/// goes through [`std::path`] — which would produce backslashes when rulogman
/// itself runs on Windows.
fn join(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else if directory.ends_with('/') {
        format!("{directory}{name}")
    } else {
        format!("{directory}/{name}")
    }
}

#[cfg(test)]
mod tests;
