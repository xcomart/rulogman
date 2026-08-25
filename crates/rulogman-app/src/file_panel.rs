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
    Button, ButtonVariant, ContextMenu, DraggedThumb, MenuEntry, Scrollbar, ScrollbarAxis,
    ScrollbarState, TextInput, Theme, hide_later, hide_now, scroll_to, scrolled, theme,
    tooltip_label,
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
/// listing no room.
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

    /// Records how many bytes the batch that just started will move.
    fn size_transfer(&mut self, session: EntityId, total: u64, cx: &mut Context<Self>) {
        let Some(transfer) = self
            .states
            .get_mut(&session)
            .and_then(|state| state.transfer.as_mut())
        else {
            return;
        };
        transfer.total = total;
        transfer.percent = TransferProgress::percent_of(transfer.done, total);
        cx.notify();
    }

    /// Moves the batch on to `name`, with `done` bytes already behind it.
    fn transfer_file(
        &mut self,
        session: EntityId,
        name: SharedString,
        done: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(transfer) = self
            .states
            .get_mut(&session)
            .and_then(|state| state.transfer.as_mut())
        else {
            return;
        };
        transfer.name = name;
        transfer.done = done;
        transfer.percent = TransferProgress::percent_of(done, transfer.total);
        cx.notify();
    }

    /// Records `done` bytes moved, repainting only when that is visible.
    ///
    /// Called once per 64 KB chunk. Repainting each time would redraw the whole
    /// panel hundreds of times a second for a bar that has not moved a pixel,
    /// so the notify is spent only when the whole percent changes.
    fn advance_transfer(&mut self, session: EntityId, done: u64, cx: &mut Context<Self>) {
        let Some(transfer) = self
            .states
            .get_mut(&session)
            .and_then(|state| state.transfer.as_mut())
        else {
            return;
        };
        transfer.done = done;
        let percent = TransferProgress::percent_of(done, transfer.total);
        if percent == transfer.percent {
            return;
        }
        transfer.percent = percent;
        cx.notify();
    }

    /// Releases the transfer slot and reports the outcome.
    fn finish_transfer(&mut self, session: EntityId, notice: Notice, cx: &mut Context<Self>) {
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        state.transfer = None;
        // Every caller that refreshes the listing does so straight after this,
        // and the answer must not take the verdict off the screen with it. What
        // does take it off is the expiry `show_notice` arms below, so a success
        // survives its own refresh and still does not stay up for good.
        state.keep_notice = true;
        self.show_notice(session, notice, cx);
    }

    /// Lists the current directory again.
    ///
    /// Also the way out of a failed first listing, which is not retried on its
    /// own.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let id = session.entity_id();
        let Some(source) = session.read(cx).files(cx) else {
            return;
        };
        let target = match self.states.get(&id).and_then(|state| state.path.clone()) {
            Some(path) => Target::Exact(path),
            None => Target::Home,
        };
        self.go(id, source, target, cx);
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

    /// Selects every entry of the listing.
    ///
    /// The anchor lands on the first entry rather than staying where the last
    /// click left it, so that a <kbd>Shift</kbd>-click afterwards reads as
    /// "everything from the top down to here" — which is the only reading a
    /// selection that starts at the top can be narrowed by.
    fn select_all(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.active_state() else {
            return;
        };
        if state.entries.is_empty() {
            return;
        }
        state.selected = state
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        state.anchor = state.entries.first().map(|entry| entry.name.clone());
        cx.notify();
    }

    /// Puts the selected entry's name on the clipboard.
    ///
    /// Nothing is said on the status line afterwards: the menu row named the
    /// act, the clipboard is where the result went, and a line reporting it
    /// would be the only one the panel writes for something that cannot fail.
    fn copy_name(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self
            .showing()
            .and_then(SessionState::only_selected)
            .map(|entry| entry.name.clone())
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(name));
    }

    /// Puts the selected entry's whole path on the clipboard.
    ///
    /// Built with [`join`], like every other path the panel hands to an
    /// operation, so what is copied is exactly the string a rename or a
    /// download would have aimed at — in the spelling the source itself uses
    /// rather than in this computer's.
    fn copy_path(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.showing() else {
            return;
        };
        let (Some(directory), Some(entry)) = (state.path.as_deref(), state.only_selected()) else {
            return;
        };
        let path = join(directory, &entry.name);
        cx.write_to_clipboard(ClipboardItem::new_string(path));
    }

    /// Opens the context menu at `at`, over the row named `name`.
    ///
    /// A right-click on a row that is *not* selected selects it first, the way
    /// every file manager does: the menu that follows has to act on what the
    /// pointer is on. A right-click inside an existing selection leaves that
    /// selection alone, which is how a multi-entry command is asked for.
    ///
    /// `name` is `None` for the background and for the `..` row, and the menu
    /// then offers what can be done to the directory rather than to its
    /// contents.
    fn open_context(&mut self, name: Option<&str>, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().map(Entity::entity_id) else {
            return;
        };
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        if state.path.is_none() {
            return;
        }

        let on_rows = match name {
            Some(name) if state.entries.iter().any(|entry| entry.name == name) => {
                if !state.selected.contains(name) {
                    state.select_only(name);
                }
                true
            }
            _ => false,
        };
        self.context = Some(PanelMenu {
            at,
            kind: MenuKind::Listing { on_rows },
        });
        cx.notify();
    }

    /// Puts the open menu away, and says whether there was one to put away.
    ///
    /// The answer is what lets the workspace layer <kbd>Escape</kbd>: the key
    /// belongs to this menu while it is up and to whatever is behind the panel
    /// once it is not, and only the panel knows which of those is the case.
    pub fn close_context(&mut self, cx: &mut Context<Self>) -> bool {
        if self.context.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// Opens the dropdown of the breadcrumb piece pressed at `at`.
    ///
    /// A folded piece already knows what it offers — the ancestors the header
    /// had no room for — so its menu opens on the spot. Every other piece has
    /// to ask the source something first and opens once that answer lands: the
    /// directories beside it, or, for the root, which roots there are at all.
    fn open_crumb(&mut self, menu: CrumbMenu, at: Point<Pixels>, cx: &mut Context<Self>) {
        // Every kind navigates, and a session that cannot navigate — one whose
        // connection has since dropped — must not be offered a menu of places
        // its rows could not take it.
        if self.acting_on(cx).is_none() {
            return;
        }
        match menu {
            CrumbMenu::Folded(targets) => {
                self.context = Some(PanelMenu {
                    at,
                    kind: MenuKind::Crumb(targets),
                });
                cx.notify();
            }
            CrumbMenu::Root(root) => self.list_roots(root, at, cx),
            CrumbMenu::Siblings(directory) => self.list_siblings(directory, at, cx),
        }
    }

    /// Asks which roots the source has, to open the root piece's dropdown on.
    ///
    /// `root` is the one the panel is standing in, and is what the fallback
    /// lists if the answer turns out to hold nothing to choose between.
    fn list_roots(&mut self, root: String, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some((session, source, _)) = self.acting_on(cx) else {
            return;
        };
        if self.crumb_pending {
            return;
        }
        self.crumb_pending = true;

        cx.spawn(async move |panel, cx| {
            let result = source.roots().await;
            panel
                .update(cx, |panel, cx| {
                    panel.roots_arrived(session, root, at, result, cx);
                })
                .ok();
        })
        .detach();
    }

    /// Opens the root piece's dropdown on the roots the source reported.
    ///
    /// Two or more and the menu is those roots, each row moving to the top of
    /// that tree — the current one included, since standing in `C:/Users` makes
    /// "go to `C:/`" as real a move as "go to `D:/`". Fewer than two and there
    /// is nothing a menu of roots could offer, so the piece falls back to the
    /// dropdown it has always had: the root's own subdirectories.
    ///
    /// A failure takes the same fallback rather than a notice. The only source
    /// that can fail here is the local one, whose failure means the drive
    /// letters could not be read — which leaves the panel knowing no less than
    /// a single-rooted source does, and the press still opens something.
    fn roots_arrived(
        &mut self,
        session: EntityId,
        root: String,
        at: Point<Pixels>,
        result: Result<Vec<String>, FileError>,
        cx: &mut Context<Self>,
    ) {
        self.crumb_pending = false;
        // The answer describes a session that may no longer be the one on
        // screen; opening a menu of its roots over another session's listing
        // would navigate the wrong panel.
        if self.session.as_ref().map(Entity::entity_id) != Some(session) {
            return;
        }

        let roots = match result {
            Ok(roots) => roots,
            Err(error) => {
                log::debug!("could not read the roots of this filesystem: {error}");
                Vec::new()
            }
        };
        let Some(targets) = root_targets(roots) else {
            self.list_siblings(root, at, cx);
            return;
        };
        self.context = Some(PanelMenu {
            at,
            kind: MenuKind::Crumb(targets),
        });
        cx.notify();
    }

    /// Asks for the contents of `directory`, to open a breadcrumb dropdown on.
    fn list_siblings(&mut self, directory: String, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some((session, source, _)) = self.acting_on(cx) else {
            return;
        };
        if self.crumb_pending {
            return;
        }
        self.crumb_pending = true;

        cx.spawn(async move |panel, cx| {
            let result = source.read_dir(&directory).await;
            panel
                .update(cx, |panel, cx| {
                    panel.siblings_arrived(session, directory, at, result, cx);
                })
                .ok();
        })
        .detach();
    }

    /// Opens the breadcrumb dropdown the listing of `directory` was asked for.
    ///
    /// Only directories go on it — a breadcrumb piece can only ever stand for
    /// one — and only those whose names are safe to append to a path, since the
    /// rows are built by joining a server-sent name onto `directory`. A
    /// directory with nothing in it to offer opens no menu at all: an empty
    /// panel hanging off the header says less than the header already did.
    fn siblings_arrived(
        &mut self,
        session: EntityId,
        directory: String,
        at: Point<Pixels>,
        result: Result<Vec<FileEntry>, FileError>,
        cx: &mut Context<Self>,
    ) {
        self.crumb_pending = false;
        // The answer describes the directory of a session that may no longer be
        // the one on screen; opening a menu of its paths over another session's
        // listing would navigate the wrong panel.
        if self.session.as_ref().map(Entity::entity_id) != Some(session) {
            return;
        }

        let mut entries = match result {
            Ok(entries) => entries,
            Err(error) => {
                let notice = Notice::from_error(&error, self.source_is_local(session));
                self.show_notice(session, notice, cx);
                return;
            }
        };
        // Sorted before the filter rather than after so the rows come out in
        // exactly the order the listing itself would show them in.
        sort_entries(&mut entries);
        let targets: Vec<CrumbTarget> = entries
            .into_iter()
            .filter(|entry| entry.is_dir && is_plain_name(&entry.name))
            .map(|entry| CrumbTarget {
                path: join(&directory, &entry.name),
                label: SharedString::from(entry.name),
            })
            .collect();

        if targets.is_empty() {
            cx.notify();
            return;
        }
        self.context = Some(PanelMenu {
            at,
            kind: MenuKind::Crumb(targets),
        });
        cx.notify();
    }

    /// Lists `path`, as a breadcrumb row asks.
    ///
    /// Allowed while a transfer is running, like the double-click that opens a
    /// directory: navigating changes what is listed, not what is moving.
    fn open_path(&mut self, path: String, cx: &mut Context<Self>) {
        let Some((session, source, _)) = self.acting_on(cx) else {
            return;
        };
        self.go(session, source, Target::Exact(path), cx);
    }

    /// Opens the entry named `name`, if it is a directory.
    fn activate(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some((session, source, path)) = self.acting_on(cx) else {
            return;
        };
        let is_dir = self
            .states
            .get(&session)
            .and_then(|state| state.entries.iter().find(|entry| entry.name == name))
            .is_some_and(|entry| entry.is_dir);
        if !is_dir {
            return;
        }
        self.go(session, source, Target::Exact(join(&path, name)), cx);
    }

    /// Opens the selected file in an editor pane, if it can be edited at all.
    ///
    /// The whole file is fetched here rather than in the pane, so that every
    /// refusal lands on the status line the user is already looking at instead
    /// of inside a pane that would have to be opened to say it could not be.
    /// The size is checked off the listing, before anything is transferred; the
    /// encoding cannot be, since only the bytes can answer it.
    fn edit(&mut self, cx: &mut Context<Self>) {
        let Some((id, source, directory)) = self.acting_on(cx) else {
            return;
        };
        let Some(session) = self.session.clone() else {
            return;
        };
        // Read out of the borrow before anything below wants `self` mutably.
        let Some((name, size)) = self
            .states
            .get(&id)
            .and_then(SessionState::only_selected)
            .filter(|entry| !entry.is_dir)
            .map(|entry| (SharedString::from(entry.name.clone()), entry.size))
        else {
            return;
        };

        let is_local = self.source_is_local(id);
        if size > MAX_EDIT_BYTES {
            self.show_notice(id, edit_notice(&LoadError::TooLarge, is_local), cx);
            return;
        }

        // The session's own charset is the opening guess, because a file on a
        // host whose shell speaks EUC-KR is overwhelmingly likely to be written
        // in it — and it is the same answer the terminal is already decoding
        // that host with. The status bar's picker is where a file that turns out
        // to disagree gets corrected.
        let charset = Charset::from_label_or_utf8(&session.read(cx).effective(cx).charset);

        cx.spawn(async move |panel, cx| {
            let loaded = match read_file(&source, &directory, &name).await {
                Ok(bytes) => TextFile::decode(&bytes, charset),
                Err(error) => Err(LoadError::Transport(error)),
            };
            // Asked here, beside the read, because this is the future that
            // already has the source and the path in hand and the only one that
            // can afford the round trip: the pane is built on the frame the
            // event lands on, and a probe there would hold the window up. Only
            // when there is going to be a pane at all — a file that could not be
            // read is refused above, and asking about it would buy nothing.
            let writable = match &loaded {
                Ok(_) => source.writable(&file_path(&directory, &name)).await,
                Err(_) => true,
            };
            // And only then the second question, which is what the *first* one
            // leads to: a file that can be written has no use for a way around
            // the account that can write it, and asking anyway would spend up
            // to three round trips on every file opened. A writable file
            // therefore carries `None`, and the pane reads the field only while
            // it is locked.
            let root_access = if writable {
                RootAccess::None
            } else {
                source.root_access().await
            };
            panel
                .update(cx, |panel, cx| match loaded {
                    Ok(file) => cx.emit(FilePanelEvent::OpenEditor(Box::new(OpenEditor {
                        session,
                        source,
                        dir: directory,
                        name,
                        file,
                        writable,
                        root_access,
                    }))),
                    Err(error) => panel.show_notice(id, edit_notice(&error, is_local), cx),
                })
                .ok();
        })
        .detach();
    }

    /// Moves to the parent of the current directory.
    fn open_parent(&mut self, cx: &mut Context<Self>) {
        let Some((session, source, path)) = self.acting_on(cx) else {
            return;
        };
        self.go(
            session,
            source,
            Target::Resolve(join(&path, PARENT_NAME)),
            cx,
        );
    }

    /// Asks the platform for files or a folder and copies them into the listed
    /// directory.
    ///
    /// `folders` picks which of the two pickers opens, because no single dialog
    /// offers both on every platform: macOS's `NSOpenPanel` can choose files and
    /// directories at once, but Windows' `IFileOpenDialog` turns into a folder
    /// browser once `FOS_PICKFOLDERS` is set, and the Linux portal's `directory`
    /// flag is just as exclusive. Two buttons behave the same everywhere; one
    /// button would behave differently on each.
    fn pick_upload(&mut self, folders: bool, cx: &mut Context<Self>) {
        let Some((_, source, _)) = self.acting_on(cx) else {
            return;
        };
        let is_local = source.is_local();
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: !folders,
            directories: folders,
            multiple: true,
            prompt: Some(if folders {
                ts!(key(
                    is_local,
                    "files.select_upload_folder",
                    "files.local.select_copy_in_folder"
                ))
            } else {
                ts!(key(
                    is_local,
                    "files.select_upload",
                    "files.local.select_copy_in"
                ))
            }),
        });

        cx.spawn(async move |panel, cx| {
            let chosen = match paths.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    log::warn!("the file picker could not be opened: {error:#}");
                    return;
                }
            };
            panel.update(cx, |panel, cx| panel.upload(chosen, cx)).ok();
        })
        .detach();
    }

    /// Copies `paths` into the current directory, recursing into folders.
    ///
    /// The whole tree is resolved before anything moves — see [`plan_upload`] —
    /// so the progress bar has a total to measure against from the first chunk,
    /// and so the walk itself happens off the UI thread. The directories are
    /// then created in order and the files sent one after another; a failure
    /// stops the batch, leaving what already landed in place, which the refresh
    /// at the end makes visible.
    fn upload(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some((session, source, directory)) = self.acting_on(cx) else {
            return;
        };
        if !self.begin_transfer(session, Activity::Upload, cx) {
            return;
        }

        // Read here, where the source is still in hand: the sentences below are
        // said long after this returns, and by then the panel may be showing
        // another tab entirely.
        let is_local = source.is_local();
        let listing = directory.clone();
        // A dropped folder can hold tens of thousands of entries and every one
        // of them costs a `stat`; on the UI thread that is dropped frames.
        let scan = cx
            .background_executor()
            .spawn(async move { plan_upload(paths, directory) });

        cx.spawn(async move |panel, cx| {
            let plan = scan.await;
            let folders = plan.directories.len();
            if panel
                .update(cx, |panel, cx| panel.size_transfer(session, plan.total, cx))
                .is_err()
            {
                return;
            }

            let mut failure = None;
            // Parents come first out of the plan, which is the whole ordering
            // requirement: SFTP has no `mkdir -p`.
            for directory in plan.directories {
                if let Err(error) = source.mkdir(&directory).await {
                    failure = Some(error);
                    break;
                }
            }

            let mut moved = 0u64;
            let mut sent = 0usize;
            let mut last = SharedString::default();
            if failure.is_none() {
                for file in plan.files {
                    let name = file_name(&file.local);
                    if panel
                        .update(cx, |panel, cx| {
                            panel.transfer_file(session, name.clone(), moved, cx);
                        })
                        .is_err()
                    {
                        return;
                    }

                    let (sender, receiver) = mpsc::unbounded();
                    let transfer = source.copy_in(file.local, &file.directory, Some(sender));
                    match follow(&panel, cx, session, moved, receiver, transfer).await {
                        Ok(_) => {
                            sent += 1;
                            last = name;
                            moved = moved.saturating_add(file.size);
                        }
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                }
            }

            let notice = match failure {
                Some(error) => Notice::from_error(&error, is_local),
                None if folders > 0 => Notice::Info(ts!(
                    key(is_local, "files.uploaded_tree", "files.local.copied_tree"),
                    files = sent,
                    folders = folders
                )),
                None if sent == 1 => Notice::Info(ts!(
                    key(is_local, "files.uploaded", "files.local.copied"),
                    name = last
                )),
                // Nothing at all could be read — every path was a broken link,
                // or vanished between the drop and the walk. Saying "uploaded
                // 0 files" would read as success, which it is not.
                None if sent == 0 => Notice::Error(ts!(key(
                    is_local,
                    "files.nothing_to_upload",
                    "files.local.nothing_to_copy"
                ))),
                None => Notice::Info(ts!(
                    key(is_local, "files.uploaded_many", "files.local.copied_many"),
                    count = sent
                )),
            };

            panel
                .update(cx, |panel, cx| {
                    panel.finish_transfer(session, notice, cx);
                    if sent > 0 || folders > 0 {
                        panel.go(session, source, Target::Exact(listing), cx);
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Saves the selection locally, asking where to put it first.
    ///
    /// The question differs with the size of the selection, because the answer
    /// has to: one entry can be renamed on the way down, so it gets a save
    /// dialog with its name in it, while several have to keep the names they
    /// have and so only need a folder to land in.
    fn download(&mut self, cx: &mut Context<Self>) {
        let Some((session, source, directory)) = self.acting_on(cx) else {
            return;
        };
        let Some(state) = self.states.get(&session) else {
            return;
        };
        let chosen: Vec<FileEntry> = state.selection().cloned().collect();

        match chosen.as_slice() {
            [] => (),
            [only] => self.download_one(session, source, &directory, only, cx),
            _ => self.download_many(session, source, &directory, chosen, cx),
        }
    }

    /// Saves one entry, asking for the path to write it to.
    ///
    /// A directory is copied whole: the remote tree is walked with `read_dir`,
    /// the local directories are created, and the files come down one after
    /// another against the same progress bar an upload uses.
    fn download_one(
        &mut self,
        session: EntityId,
        source: Arc<dyn FileSource>,
        directory: &str,
        entry: &FileEntry,
        cx: &mut Context<Self>,
    ) {
        let name = entry.name.clone();
        let is_dir = entry.is_dir;
        let size = entry.size;
        let remote = join(directory, &name);
        // Read while the source is in hand, as in `upload`: only the failure
        // sentence needs it here, since "Saved to <path>." is true of a copy
        // out of either kind of filesystem and is shared between them.
        let is_local = source.is_local();
        let prompt = cx.prompt_for_new_path(&suggested_directory(), Some(&name));

        cx.spawn(async move |panel, cx| {
            let local = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    log::warn!("the save dialog could not be opened: {error:#}");
                    return;
                }
            };

            // Claimed only now, not before the dialog: a save dialog can stand
            // open for minutes, and a transfer that was running when it opened
            // has very likely finished by the time a path comes back.
            let claimed = panel.update(cx, |panel, cx| {
                panel.begin_transfer(session, Activity::Download, cx)
            });
            if !matches!(claimed, Ok(true)) {
                return;
            }
            let shown = local.display().to_string();

            let plan = if is_dir {
                match plan_download(&source, remote, local).await {
                    Ok(plan) => plan,
                    Err(error) => {
                        panel
                            .update(cx, |panel, cx| {
                                let notice = Notice::from_error(&error, is_local);
                                panel.finish_transfer(session, notice, cx);
                            })
                            .ok();
                        return;
                    }
                }
            } else {
                DownloadPlan {
                    directories: Vec::new(),
                    total: size,
                    files: vec![PlannedDownload {
                        remote,
                        local,
                        size,
                    }],
                }
            };

            let count = plan.files.len();
            let failure = match run_download(&panel, cx, session, &source, plan).await {
                Ran::Finished(failure) => failure,
                Ran::Abandoned => return,
            };

            let notice = match failure {
                Some(error) => Notice::from_error(&error, is_local),
                None if is_dir => {
                    Notice::Info(ts!("files.downloaded_tree", count = count, path = shown))
                }
                None => Notice::Info(ts!("files.downloaded", path = shown)),
            };
            panel
                .update(cx, |panel, cx| panel.finish_transfer(session, notice, cx))
                .ok();
        })
        .detach();
    }

    /// Saves several entries into one folder, keeping their names.
    ///
    /// The whole selection moves as a single batch against a single progress
    /// bar: separate transfers would each want the session's one progress slot
    /// and all but the first would be refused. A local file of the same name is
    /// overwritten, exactly as a single download overwrites what the save
    /// dialog was pointed at.
    fn download_many(
        &mut self,
        session: EntityId,
        source: Arc<dyn FileSource>,
        directory: &str,
        chosen: Vec<FileEntry>,
        cx: &mut Context<Self>,
    ) {
        let directory = directory.to_owned();
        let is_local = source.is_local();
        // "Save" names the answer this dialog is asking for — a destination —
        // and says nothing about where the entries are coming from, so it is
        // the one picker label both kinds of source share.
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(ts!("files.select_download_folder")),
        });

        cx.spawn(async move |panel, cx| {
            let destination = match prompt.await {
                Ok(Ok(Some(paths))) => match paths.into_iter().next() {
                    Some(path) => path,
                    None => return,
                },
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    log::warn!("the folder picker could not be opened: {error:#}");
                    return;
                }
            };

            let claimed = panel.update(cx, |panel, cx| {
                panel.begin_transfer(session, Activity::Download, cx)
            });
            if !matches!(claimed, Ok(true)) {
                return;
            }
            let shown = destination.display().to_string();

            let mut plan = DownloadPlan::default();
            let mut failure = None;
            for entry in chosen {
                // Server-sent names reach `Path::join` here, so the same guard
                // the recursive walk uses has to stand at the top of it too.
                if !is_plain_name(&entry.name) {
                    log::debug!("not downloading {}/{}: odd name", directory, entry.name);
                    continue;
                }
                let remote = join(&directory, &entry.name);
                let local = destination.join(&entry.name);

                if entry.is_dir {
                    match plan_download(&source, remote, local).await {
                        Ok(part) => plan.absorb(part),
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                } else {
                    plan.total = plan.total.saturating_add(entry.size);
                    plan.files.push(PlannedDownload {
                        remote,
                        local,
                        size: entry.size,
                    });
                }
            }

            let count = plan.files.len();
            if failure.is_none() {
                failure = match run_download(&panel, cx, session, &source, plan).await {
                    Ran::Finished(failure) => failure,
                    Ran::Abandoned => return,
                };
            }

            let notice = match failure {
                Some(error) => Notice::from_error(&error, is_local),
                None => Notice::Info(ts!("files.downloaded_tree", count = count, path = shown)),
            };
            panel
                .update(cx, |panel, cx| panel.finish_transfer(session, notice, cx))
                .ok();
        })
        .detach();
    }

    /// Asks whether the selection should really be deleted.
    ///
    /// Nothing is sent until the question is answered — this only records what
    /// was asked about. Deleting is the one thing the panel does that cannot be
    /// undone by doing it again, and it is a *single* right-click away, so it
    /// gets the one confirmation step in the panel.
    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().map(Entity::entity_id) else {
            return;
        };
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        let names: Vec<String> = state
            .selection()
            .map(|entry| entry.name.clone())
            .filter(|name| is_plain_name(name))
            .collect();
        if names.is_empty() {
            return;
        }
        state.prompt = Some(Prompt::Delete(names));
        cx.notify();
    }

    /// Drops whatever question is open without acting on it.
    fn cancel_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().map(Entity::entity_id) else {
            return;
        };
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        if state.prompt.take().is_some() {
            self.focus_prompt = false;
            cx.notify();
        }
    }

    /// Deletes the entries the open confirmation names.
    ///
    /// Each is removed the way its own type requires: a file — or a symbolic
    /// link of any kind — with one call, a real directory by walking it and
    /// removing the contents from the leaves upwards, since SFTP has no
    /// recursive delete. The walk itself is remote round trips, so it runs
    /// under the progress slot rather than before it.
    fn delete(&mut self, cx: &mut Context<Self>) {
        let Some((session, source, directory)) = self.acting_on(cx) else {
            return;
        };
        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        // Read before it is cleared, so that a call arriving with some *other*
        // question open leaves that question standing instead of eating it.
        let names = match state.prompt.as_ref() {
            Some(Prompt::Delete(names)) => names.clone(),
            _ => return,
        };
        state.prompt = None;
        self.focus_prompt = false;

        // Resolved against the listing now rather than inside the walk: this is
        // where "is it a link?" is still answerable without another round trip,
        // and getting that wrong would delete a link's target instead of the
        // link.
        let Some(state) = self.states.get(&session) else {
            return;
        };
        let targets: Vec<FileEntry> = names
            .iter()
            .filter_map(|name| state.entries.iter().find(|entry| &entry.name == name))
            .cloned()
            .collect();
        let count = targets.len();
        let last = targets
            .first()
            .map(|entry| SharedString::from(entry.name.clone()))
            .unwrap_or_default();
        if targets.is_empty() || !self.begin_transfer(session, Activity::Delete, cx) {
            return;
        }
        // Only the failure sentence needs it: a delete removes the same thing
        // and reports it the same way whichever filesystem it runs on.
        let is_local = source.is_local();

        cx.spawn(async move |panel, cx| {
            let removals = match plan_delete(&source, &directory, targets).await {
                Ok(removals) => removals,
                Err(error) => {
                    panel
                        .update(cx, |panel, cx| {
                            let notice = Notice::from_error(&error, is_local);
                            panel.finish_transfer(session, notice, cx);
                            panel.go(session, source, Target::Exact(directory), cx);
                        })
                        .ok();
                    return;
                }
            };

            if panel
                .update(cx, |panel, cx| {
                    panel.size_transfer(session, removals.len() as u64, cx);
                })
                .is_err()
            {
                return;
            }

            let mut failure = None;
            let mut done = 0u64;
            for removal in removals {
                if panel
                    .update(cx, |panel, cx| {
                        panel.transfer_file(session, removal.name.clone(), done, cx);
                    })
                    .is_err()
                {
                    return;
                }
                let outcome = if removal.directory {
                    source.remove_dir(&removal.path).await
                } else {
                    source.remove_file(&removal.path).await
                };
                if let Err(error) = outcome {
                    failure = Some(error);
                    break;
                }
                done = done.saturating_add(1);
            }

            let notice = match failure {
                Some(error) => Notice::from_error(&error, is_local),
                None if count == 1 => Notice::Info(ts!("files.deleted", name = last)),
                None => Notice::Info(ts!("files.deleted_many", count = count)),
            };
            // Listed again either way: a batch stopped half-way has removed
            // real entries, and leaving them on screen would be worse than the
            // failure itself.
            panel
                .update(cx, |panel, cx| {
                    panel.finish_transfer(session, notice, cx);
                    if let Some(state) = panel.states.get_mut(&session) {
                        state.reset_selection();
                    }
                    panel.go(session, source, Target::Exact(directory), cx);
                })
                .ok();
        })
        .detach();
    }

    /// Opens the rename field over the one selected entry.
    fn begin_rename(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().map(Entity::entity_id) else {
            return;
        };
        let Some(state) = self.states.get(&session) else {
            return;
        };
        let from = {
            let mut selection = state.selection();
            match (selection.next(), selection.next()) {
                (Some(entry), None) => entry.name.clone(),
                _ => return,
            }
        };

        let panel = cx.entity().downgrade();
        let input = cx.new(|cx| {
            // The typed text arrives as an argument rather than being read back
            // off the field: this runs inside the field's own update, and
            // reading an entity that is currently leased panics.
            let mut input = TextInput::new(cx)
                .context_menu(input_menu_labels)
                .on_submit(move |typed, _window, cx| {
                    let typed = typed.to_owned();
                    panel
                        .update(cx, |panel, cx| panel.commit_rename(&typed, cx))
                        .ok();
                });
            input.set_content(from.clone(), cx);
            input
        });

        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        state.prompt = Some(Prompt::Rename { from, input });
        self.focus_prompt = true;
        cx.notify();
    }

    /// Applies `typed` as the new name of the entry the rename field is over.
    ///
    /// The name comes in as an argument rather than being read off the field,
    /// because one of the two callers is the field's own `Enter` handler and
    /// the field is leased while that runs.
    ///
    /// The new name is checked before it is sent, not after: the only thing a
    /// server can say about `../etc` is that it worked, and by then something
    /// outside the directory the user was looking at has been renamed.
    fn commit_rename(&mut self, typed: &str, cx: &mut Context<Self>) {
        let Some((session, source, directory)) = self.acting_on(cx) else {
            return;
        };
        let Some(state) = self.states.get(&session) else {
            return;
        };
        let Some(Prompt::Rename { from, .. }) = state.prompt.as_ref() else {
            return;
        };
        let from = from.clone();
        // Trimmed because a trailing space is legal on every server and almost
        // never meant: it produces a name that looks identical to one already
        // there and cannot be typed again by hand.
        let to = typed.trim().to_owned();

        if to == from {
            self.cancel_prompt(cx);
            return;
        }
        if !is_plain_name(&to) {
            self.show_notice(session, Notice::Error(ts!("files.invalid_name")), cx);
            return;
        }
        // Exclusive with a transfer for the same reason two transfers are:
        // both end in a listing, and the one that lands second would describe a
        // directory the other has already changed.
        if self
            .states
            .get(&session)
            .is_some_and(|state| state.transfer.is_some())
        {
            self.show_notice(session, Notice::Error(ts!("files.transfer_busy")), cx);
            return;
        }

        let old = join(&directory, &from);
        let new = join(&directory, &to);
        if let Some(state) = self.states.get_mut(&session) {
            state.prompt = None;
        }
        self.focus_prompt = false;
        cx.notify();

        cx.spawn(async move |panel, cx| {
            let outcome = source.rename(&old, &new).await;
            panel
                .update(cx, |panel, cx| match outcome {
                    Ok(()) => {
                        if let Some(state) = panel.states.get_mut(&session) {
                            state.keep_notice = true;
                            // Carried across the refresh: the listing keeps its
                            // selection when the path has not changed, so this
                            // leaves the renamed row highlighted where the old
                            // one was.
                            state.select_only(&to);
                        }
                        let said = ts!("files.renamed", name = to.clone());
                        panel.show_notice(session, Notice::Info(said), cx);
                        panel.go(session, source, Target::Exact(directory), cx);
                    }
                    Err(error) => {
                        let notice = Notice::from_error(&error, panel.source_is_local(session));
                        panel.show_notice(session, notice, cx);
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Opens the field that names a directory to create here.
    fn begin_new_folder(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref().map(Entity::entity_id) else {
            return;
        };
        // Nothing to create *in* until the first listing has landed, and the
        // path is what the name would be joined onto.
        if !self
            .states
            .get(&session)
            .is_some_and(|state| state.path.is_some())
        {
            return;
        }

        let panel = cx.entity().downgrade();
        let input = cx.new(|cx| {
            // As with rename: the typed text arrives as an argument because
            // this runs inside the field's own update, and reading an entity
            // that is currently leased panics.
            TextInput::new(cx)
                .context_menu(input_menu_labels)
                .placeholder(ts!("files.new_folder_placeholder"))
                .on_submit(move |typed, _window, cx| {
                    let typed = typed.to_owned();
                    panel
                        .update(cx, |panel, cx| panel.commit_new_folder(&typed, cx))
                        .ok();
                })
        });

        let Some(state) = self.states.get_mut(&session) else {
            return;
        };
        state.prompt = Some(Prompt::NewFolder { input });
        self.focus_prompt = true;
        cx.notify();
    }

    /// Creates the directory `typed` names, inside the one on screen.
    ///
    /// The mirror of [`FilePanel::commit_rename`], down to why the name comes in
    /// as an argument and why it is checked here rather than left to the server:
    /// `../backup` would create a directory the user cannot see, in a directory
    /// they were not looking at.
    ///
    /// **An existing directory of that name is not an error.** [`FileSource::mkdir`]
    /// is idempotent — the recursive upload depends on that — so asking for a
    /// name already taken by a directory simply selects the one already there.
    /// Nothing is overwritten and nothing inside it is touched, so there is no
    /// harm to report; a name taken by a *file* is a real collision and does
    /// surface as the server's own refusal.
    fn commit_new_folder(&mut self, typed: &str, cx: &mut Context<Self>) {
        let Some((session, source, directory)) = self.acting_on(cx) else {
            return;
        };
        if !self
            .states
            .get(&session)
            .is_some_and(|state| matches!(state.prompt, Some(Prompt::NewFolder { .. })))
        {
            return;
        }
        // Trimmed for the reason the rename field trims: a name with a trailing
        // space is legal, indistinguishable on screen from one without, and
        // essentially never what was meant.
        let name = typed.trim().to_owned();

        // Covers the empty field too — `is_plain_name` rejects it — so an
        // `Enter` on an untouched field says why instead of doing nothing.
        if !is_plain_name(&name) {
            self.show_notice(session, Notice::Error(ts!("files.invalid_name")), cx);
            return;
        }
        // Exclusive with a transfer exactly as a rename is: both end in a
        // listing, and the one that lands second would describe a directory the
        // other has already changed.
        if self
            .states
            .get(&session)
            .is_some_and(|state| state.transfer.is_some())
        {
            self.show_notice(session, Notice::Error(ts!("files.transfer_busy")), cx);
            return;
        }

        let path = join(&directory, &name);
        if let Some(state) = self.states.get_mut(&session) {
            state.prompt = None;
        }
        self.focus_prompt = false;
        cx.notify();

        cx.spawn(async move |panel, cx| {
            let outcome = source.mkdir(&path).await;
            panel
                .update(cx, |panel, cx| match outcome {
                    Ok(()) => {
                        if let Some(state) = panel.states.get_mut(&session) {
                            state.keep_notice = true;
                            // Selected before the listing that will contain it:
                            // the selection is held as names and filtered
                            // through the entries, so the new row arrives
                            // already highlighted.
                            state.select_only(&name);
                        }
                        let said = ts!("files.created", name = name.clone());
                        panel.show_notice(session, Notice::Info(said), cx);
                        panel.go(session, source, Target::Exact(directory), cx);
                    }
                    Err(error) => {
                        let notice = Notice::from_error(&error, panel.source_is_local(session));
                        panel.show_notice(session, notice, cx);
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Renders the header: the current path and the action buttons.
    fn render_header(&self, state: Option<&SessionState>, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let is_local = self.showing_local(cx);
        let path = state.and_then(|state| state.path.as_deref());
        let ready = state.is_some_and(|state| state.path.is_some());
        let selected = state.map_or(0, SessionState::selected_count);
        // Directories included: a selected folder is copied whole.
        let downloadable = selected > 0;
        // The same rules the context menu applies, so that a command is offered
        // in exactly one shape whichever way it is reached: renaming needs one
        // target and only one, deleting takes as many as are selected.
        let renameable = selected == 1;
        let deletable = selected > 0;

        let title = match path {
            Some(path) => self.render_crumbs(path, &theme, cx),
            // Nothing is listed yet, so there is no path to break up and the
            // header carries the panel's own name instead.
            None => {
                // Mirrors the status bar: `truncate` needs a row flexing the
                // text child, not a bare `w_full`, to resolve its width.
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(ts!(key(is_local, "files.title", "files.local.title"))),
                    )
                    .into_any_element()
            }
        };

        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(4.))
            .px(px(HEADER_PADDING))
            .py(px(6.))
            .border_b_1()
            .border_color(theme.border)
            .child(title)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(2.))
                    .child(icon_button(
                        "file-panel-refresh",
                        icons::REFRESH,
                        ts!("files.tip_refresh"),
                        self.session.is_some(),
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| panel.refresh(cx)),
                    ))
                    .child(icon_button(
                        "file-panel-new-folder",
                        icons::NEW_FOLDER,
                        ts!("files.tip_new_folder"),
                        ready,
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| {
                            panel.begin_new_folder(cx);
                        }),
                    ))
                    .child(icon_button(
                        "file-panel-upload",
                        icons::UPLOAD,
                        ts!(key(is_local, "files.tip_upload", "files.local.tip_copy_in")),
                        ready,
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| {
                            panel.pick_upload(false, cx);
                        }),
                    ))
                    .child(icon_button(
                        "file-panel-upload-folder",
                        icons::UPLOAD_FOLDER,
                        ts!(key(
                            is_local,
                            "files.tip_upload_folder",
                            "files.local.tip_copy_in_folder"
                        )),
                        ready,
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| {
                            panel.pick_upload(true, cx);
                        }),
                    ))
                    .child(icon_button(
                        "file-panel-download",
                        icons::DOWNLOAD,
                        ts!(key(
                            is_local,
                            "files.tip_download",
                            "files.local.tip_copy_out"
                        )),
                        ready && downloadable,
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| panel.download(cx)),
                    ))
                    .child(icon_button(
                        "file-panel-rename",
                        icons::RENAME,
                        ts!("files.tip_rename"),
                        ready && renameable,
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| panel.begin_rename(cx)),
                    ))
                    // Last, and deliberately: the row starts with the button
                    // pressed most often and ends with the one that cannot be
                    // undone, so a click that lands a button early hits a
                    // refresh rather than a delete.
                    .child(icon_button(
                        "file-panel-delete",
                        icons::DELETE,
                        ts!("files.tip_delete"),
                        ready && deletable,
                        &theme,
                        cx.listener(|panel, _: &ClickEvent, _window, cx| panel.confirm_delete(cx)),
                    )),
            )
            .into_any_element()
    }

    /// Renders the current path as a row of pressable pieces.
    ///
    /// Each piece opens a menu of the directories beside it, which is what
    /// makes the header a way of *moving* rather than a label: the way out of
    /// `/srv/app/releases/2026-07-30` into last week's release is one press on
    /// the last piece, not four double-clicks through `..`.
    ///
    /// The row wraps rather than truncating. [`crumbs`] has already folded away
    /// what the panel's own width could not hold, but [`fold_budget`] is an
    /// estimate over a proportional font; wrapping costs a line of header, while
    /// truncating would cost the leaf directory — the one piece the user needs
    /// to see. A drag of the panel's edge repaints, so the fold follows the
    /// width as it moves.
    fn render_crumbs(&self, path: &str, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let crumbs = crumbs(path, fold_budget(self.width));
        // Taken before the pieces are consumed: a separator belongs in front of
        // every piece except the first and those following the root, whose own
        // label is already the slash that would go there.
        let separators: Vec<bool> = std::iter::once(false)
            .chain(
                crumbs
                    .windows(2)
                    .map(|pair| needs_separator(&pair[0].label)),
            )
            .collect();
        let hover = theme.surface_hover;
        let text = theme.text;
        // Fainter than the pieces on either side of it: a separator is
        // punctuation, and the header's job is to read as a path with parts
        // rather than as a row of equally loud buttons.
        let separator = theme.text_muted.opacity(0.6);

        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .w_full()
            .min_w_0()
            .text_size(px(11.))
            .text_color(theme.text_muted);

        for (index, crumb) in crumbs.into_iter().enumerate() {
            if separators.get(index).copied().unwrap_or_default() {
                row = row.child(
                    div()
                        .flex_none()
                        .text_color(separator)
                        .child(CRUMB_SEPARATOR),
                );
            }
            let menu = crumb.menu;
            row = row.child(
                div()
                    .id(ElementId::from(("file-crumb", index)))
                    .flex_none()
                    .px(px(2.))
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover).text_color(text))
                    // A press rather than a click, for the position: a menu has
                    // to hang where the pointer is, and the press is where the
                    // right-click menus below take theirs from too.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |panel, event: &MouseDownEvent, _window, cx| {
                            panel.open_crumb(menu.clone(), event.position, cx);
                        }),
                    )
                    .child(crumb.label),
            );
        }

        row.into_any_element()
    }

    /// Renders the directory listing, or the placeholder standing in for it.
    fn render_list(&self, state: Option<&SessionState>, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let connected = self
            .session
            .as_ref()
            .is_some_and(|session| session.read(cx).files(cx).is_some());

        let Some(state) = state.filter(|state| state.path.is_some()) else {
            let message = if self.session.is_none() {
                ts!("files.no_session")
            } else if !connected {
                // The one wording chosen before a source exists to ask: a
                // session that has not started has no `FileSource` yet, and it
                // is its transport that decides which sentence is true.
                ts!(key(
                    self.showing_local(cx),
                    "files.not_connected",
                    "files.local.not_connected"
                ))
            } else {
                ts!("files.loading")
            };
            return placeholder(message, &theme);
        };

        // Safe by the filter above; kept as a match so a future change cannot
        // turn this into a panic.
        let path = state.path.as_deref().unwrap_or_default();
        let mut rows: Vec<AnyElement> = Vec::with_capacity(state.entries.len() + 1);

        if !is_root(path) && !path.is_empty() {
            rows.push(self.render_row(
                ElementId::from("file-row-parent"),
                PARENT_NAME,
                true,
                false,
                None,
                false,
                &theme,
                cx,
            ));
        }
        for (index, entry) in state.entries.iter().enumerate() {
            let size = (!entry.is_dir).then(|| SharedString::from(format_size(entry.size)));
            rows.push(self.render_row(
                ElementId::from(("file-row", index)),
                &entry.name,
                entry.is_dir,
                entry.is_symlink,
                size,
                state.selected.contains(&entry.name),
                &theme,
                cx,
            ));
        }

        // The placeholder goes *inside* the scroll box rather than replacing
        // it, so that an empty directory still has a background to right-click
        // — which is the only way to upload into one without the toolbar.
        if rows.is_empty() {
            rows.push(placeholder(ts!("files.empty"), &theme));
        }

        // The wrapper is what the overlay bar is placed against, and exists only
        // for that: the scrolling box cannot hold its own bar, because its
        // children are what scroll away.
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id("file-panel-list")
                    .flex()
                    .flex_col()
                    .size_full()
                    .py(px(2.))
                    .overflow_y_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&state.scroll)
                    // Reached by the `..` row and by empty space alike: neither
                    // has an entry behind it, and both mean "do something to
                    // this directory".
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|panel, event: &MouseDownEvent, _window, cx| {
                            panel.open_context(None, event.position, cx);
                        }),
                    )
                    .children(rows),
            )
            .children(
                Self::scrollbar(state)
                    .on_hover(cx.listener(|panel, hovered: &bool, _window, cx| {
                        panel.hover_scrollbar(*hovered, cx);
                    }))
                    .render(&theme),
            )
            .into_any_element()
    }

    /// Renders one row of the listing.
    #[allow(clippy::too_many_arguments)]
    fn render_row(
        &self,
        id: ElementId,
        name: &str,
        is_dir: bool,
        is_symlink: bool,
        size: Option<SharedString>,
        selected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = SharedString::from(name.to_owned());
        let parent = name == PARENT_NAME;
        let owned = label.clone();
        let clicked = label.clone();
        // Recomputed every frame rather than cached, exactly as the breadcrumb's
        // budget is: the answer depends on the panel's width, and the width
        // changes continuously while the edge is being dragged.
        let clipped = name_is_clipped(
            self.width,
            name,
            is_symlink,
            size.as_ref().map(SharedString::as_ref),
        );

        div()
            .id(id)
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .w_full()
            .px(px(8.))
            .py(px(3.))
            .text_size(px(12.))
            .text_color(theme.text)
            .cursor_pointer()
            .when(selected, |row| row.bg(theme.surface_active))
            .when(!selected, |row| {
                row.hover(|style| style.bg(theme.surface_hover))
            })
            // On the row rather than on the label: the label is a bare `div`
            // with no id, and giving it one to carry a tooltip would add a
            // second hitbox over every row for no gain — gpui places the
            // tooltip at the pointer either way, and the pointer is over the
            // name whenever the name is what the user is reading.
            .when(clipped, |row| row.tooltip(tooltip_label(label.clone())))
            .on_click(cx.listener(move |panel, event: &ClickEvent, _window, cx| {
                // A double click arrives as two events, so the first one has
                // already selected the row by the time this opens it.
                if event.click_count() >= 2 {
                    if parent {
                        panel.open_parent(cx);
                    } else {
                        panel.activate(&owned, cx);
                    }
                } else if !parent {
                    panel.select(&owned, event.modifiers(), cx);
                }
            }))
            // The `..` row deliberately has no handler of its own: letting the
            // press bubble to the list is what makes right-clicking it mean the
            // same as right-clicking empty space.
            .when(!parent, |row| {
                row.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |panel, event: &MouseDownEvent, _window, cx| {
                        // The press belongs to this row, not to the list under
                        // it, which would take it as a click on the background.
                        cx.stop_propagation();
                        panel.open_context(Some(&clicked), event.position, cx);
                    }),
                )
            })
            // The accent on directories is what makes a listing scannable at a
            // glance: it separates the folders from the files ahead of the
            // sort order, in both themes.
            .child(if is_dir {
                icons::icon(icons::FOLDER, px(ROW_ICON), theme.accent)
            } else {
                icons::icon(icons::FILE, px(ROW_ICON), theme.icon)
            })
            .child(div().flex_1().min_w_0().truncate().child(label))
            .when(is_symlink, |row| {
                row.child(icons::icon(icons::SYMLINK, px(BADGE_ICON), theme.icon))
            })
            .children(size.map(|size| {
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(size)
            }))
            .into_any_element()
    }

    /// Moves the keyboard into a question's text field the frame after it
    /// appears.
    ///
    /// The field is created inside a menu callback, where it has not been laid
    /// out yet; focusing it there would put the caret in a box that does not
    /// exist. Doing it from the render that first draws it is the same trick
    /// the connection dialog uses to focus its host field.
    fn apply_pending_focus(&mut self, window: &mut Window, cx: &mut App) {
        if !self.focus_prompt {
            return;
        }
        self.focus_prompt = false;
        let Some(state) = self
            .session
            .as_ref()
            .and_then(|session| self.states.get(&session.entity_id()))
        else {
            return;
        };
        if let Some(input) = state.prompt.as_ref().and_then(Prompt::field) {
            let handle = input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    /// Renders the open menu, if there is one.
    ///
    /// Three menus, picked by where the press landed: the two the listing
    /// offers, and the directories a breadcrumb piece can be swapped for. A row
    /// whose command would be refused is left out rather than shown greyed:
    /// "Rename…" appears only over a selection of exactly one, because renaming
    /// several things to one name is not a thing to offer and then decline.
    ///
    /// The listing menus come in three groups, separated in that order: what
    /// the press was on, what can be taken off it as text, and what acts on the
    /// listing as a whole. A group left empty contributes no rule of its own.
    fn render_context(
        &self,
        state: Option<&SessionState>,
        cx: &mut Context<Self>,
    ) -> Option<ContextMenu> {
        let menu = self.context.as_ref()?;
        let selected = state.map_or(0, SessionState::selected_count);
        let is_local = self.showing_local(cx);
        let this = cx.entity();

        let on_rows = match &menu.kind {
            MenuKind::Listing { on_rows } => *on_rows,
            // Nothing but destinations: a breadcrumb dropdown is navigation,
            // and the commands the listing menus carry act on a selection this
            // menu was never about.
            MenuKind::Crumb(targets) => {
                let entries = targets
                    .iter()
                    .map(|target| {
                        let this = this.clone();
                        let path = target.path.clone();
                        MenuEntry::new(target.label.clone()).on_activate(move |_window, cx| {
                            let path = path.clone();
                            this.update(cx, |panel, cx| panel.open_path(path, cx));
                        })
                    })
                    .collect();
                return Some(
                    ContextMenu::new("file-panel-context")
                        .position(menu.at)
                        .entries(entries)
                        .on_dismiss(move |_window, cx| {
                            this.update(cx, |panel, cx| panel.close_context(cx));
                        }),
                );
            }
        };

        // The one selected entry, when there is exactly one. Every row phrased
        // in the singular hangs off it, and the kind it carries is what tells
        // an openable directory from a file.
        let only = state.and_then(SessionState::only_selected);

        let mut primary = Vec::new();
        let mut clipboard = Vec::new();
        if on_rows && selected > 0 {
            // What a double-click on the row would have done, said out loud —
            // and offered only where it would do anything, which is over a
            // single directory.
            if let Some(name) = only.filter(|entry| entry.is_dir).map(|entry| &entry.name) {
                let name = name.clone();
                primary.push(MenuEntry::new(ts!("files.menu_open")).on_activate({
                    let this = this.clone();
                    move |_window, cx| {
                        let name = name.clone();
                        this.update(cx, |panel, cx| panel.activate(&name, cx));
                    }
                }));
            }
            // The file counterpart of `menu_open`, and deliberately in the same
            // slot: over a directory the obvious thing to do is go into it,
            // over a file it is to look inside it. Only over one file, because
            // the row opens one pane, and never over a directory, which has no
            // contents a text buffer could hold.
            if only.is_some_and(|entry| !entry.is_dir) {
                primary.push(MenuEntry::new(ts!("files.menu_edit")).on_activate({
                    let this = this.clone();
                    move |_window, cx| {
                        this.update(cx, |panel, cx| panel.edit(cx));
                    }
                }));
            }
            let copy_out = key(is_local, "files.menu_download", "files.local.menu_copy_out");
            primary.push(MenuEntry::new(ts!(copy_out)).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |panel, cx| panel.download(cx));
                }
            }));
            if only.is_some() {
                primary.push(MenuEntry::new(ts!("files.menu_rename")).on_activate({
                    let this = this.clone();
                    move |_window, cx| {
                        this.update(cx, |panel, cx| panel.begin_rename(cx));
                    }
                }));
            }
            primary.push(MenuEntry::new(ts!("files.menu_delete")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |panel, cx| panel.confirm_delete(cx));
                }
            }));

            // A group of their own: these two move nothing and ask nothing,
            // they only hand the entry's name — or the whole path an operation
            // would have used — to whatever the user pastes into next.
            if only.is_some() {
                clipboard.push(MenuEntry::new(ts!("files.menu_copy_name")).on_activate({
                    let this = this.clone();
                    move |_window, cx| {
                        this.update(cx, |panel, cx| panel.copy_name(cx));
                    }
                }));
                clipboard.push(MenuEntry::new(ts!("files.menu_copy_path")).on_activate({
                    let this = this.clone();
                    move |_window, cx| {
                        this.update(cx, |panel, cx| panel.copy_path(cx));
                    }
                }));
            }
        } else {
            // First, the way every file manager orders this menu: creating is
            // the one command here that acts on the directory itself rather
            // than moving something into it.
            primary.push(MenuEntry::new(ts!("files.menu_new_folder")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |panel, cx| panel.begin_new_folder(cx));
                }
            }));
            for (label, folders) in [
                (
                    ts!(key(
                        is_local,
                        "files.menu_upload",
                        "files.local.menu_copy_in"
                    )),
                    false,
                ),
                (
                    ts!(key(
                        is_local,
                        "files.menu_upload_folder",
                        "files.local.menu_copy_in_folder"
                    )),
                    true,
                ),
            ] {
                let this = this.clone();
                primary.push(MenuEntry::new(label).on_activate(move |_window, cx| {
                    this.update(cx, |panel, cx| panel.pick_upload(folders, cx));
                }));
            }
        }

        // Both of these speak for the listing rather than for whatever the
        // press landed on, so they are offered from either menu. Selecting
        // everything is left out once everything already is — there is nothing
        // for the row to change — and over an empty directory, which has
        // nothing to select.
        let listed = state.map_or(0, |state| state.entries.len());
        let mut whole = Vec::new();
        if listed > 0 && selected < listed {
            whole.push(MenuEntry::new(ts!("files.menu_select_all")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |panel, cx| panel.select_all(cx));
                }
            }));
        }
        whole.push(MenuEntry::new(ts!("files.menu_refresh")).on_activate({
            let this = this.clone();
            move |_window, cx| {
                this.update(cx, |panel, cx| panel.refresh(cx));
            }
        }));

        let mut entries = Vec::new();
        for group in [primary, clipboard, whole] {
            if group.is_empty() {
                continue;
            }
            if !entries.is_empty() {
                entries.push(MenuEntry::separator());
            }
            entries.extend(group);
        }

        Some(
            ContextMenu::new("file-panel-context")
                .position(menu.at)
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |panel, cx| panel.close_context(cx));
                }),
        )
    }

    /// Renders the open question — a delete confirmation, or a field naming
    /// something — if there is one.
    ///
    /// All of them sit under the listing rather than over the window, so the
    /// rows they are about stay readable while the question is being answered.
    fn render_prompt(
        &self,
        state: Option<&SessionState>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (question, confirm, body) = match state?.prompt.as_ref()? {
            Prompt::Delete(names) => {
                let question = match names.as_slice() {
                    [only] => ts!("files.delete_confirm_one", name = only.clone()),
                    names => ts!("files.delete_confirm", count = names.len()),
                };
                (
                    question,
                    Button::new("file-panel-delete", ts!("files.delete"))
                        .variant(ButtonVariant::Danger)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                            panel.delete(cx);
                        })),
                    None,
                )
            }
            Prompt::Rename { from, input } => {
                let field = input.clone();
                (
                    ts!("files.rename_prompt", name = from.clone()),
                    Button::new("file-panel-rename", ts!("files.rename")).on_click(cx.listener(
                        move |panel, _: &ClickEvent, _window, cx| {
                            // Safe to read here, unlike in the field's own
                            // `Enter` handler: this runs from the panel's
                            // update, so the field itself is not leased.
                            let typed = field.read(cx).content().to_owned();
                            panel.commit_rename(&typed, cx);
                        },
                    )),
                    Some(input.clone()),
                )
            }
            Prompt::NewFolder { input } => {
                let field = input.clone();
                (
                    ts!("files.new_folder_prompt"),
                    Button::new("file-panel-create", ts!("files.create")).on_click(cx.listener(
                        move |panel, _: &ClickEvent, _window, cx| {
                            let typed = field.read(cx).content().to_owned();
                            panel.commit_new_folder(&typed, cx);
                        },
                    )),
                    Some(input.clone()),
                )
            }
        };

        Some(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .gap(px(6.))
                .w_full()
                .min_w_0()
                .child(
                    // The header's recipe, for the header's reason: `truncate`
                    // resolves its width from a row flexing the text child, and
                    // a bare `w_full` leaves it with nothing to measure against
                    // — the whole line then collapses to an ellipsis.
                    div().flex().flex_row().w_full().child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.text)
                            .child(question),
                    ),
                )
                .children(body)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        // Wraps rather than overflowing: the panel can be
                        // dragged down to 180px, and a locale that spells
                        // "Cancel" and "Rename" long enough would otherwise
                        // push a button out past the panel's own border.
                        .flex_wrap()
                        .items_center()
                        .justify_end()
                        .gap(px(6.))
                        .child(
                            Button::new("file-panel-prompt-cancel", ts!("common.cancel"))
                                .variant(ButtonVariant::Secondary)
                                .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                                    panel.cancel_prompt(cx);
                                })),
                        )
                        .child(confirm),
                )
                .into_any_element(),
        )
    }

    /// Renders the status line, when there is anything to say.
    ///
    /// A running transfer outranks everything else here: it is the only state
    /// the user can neither see in the listing nor guess at, and while it lasts
    /// the line carries a progress bar under it. An open question is drawn
    /// *under* whatever the line says, so a refused name explains itself right
    /// above the field it was typed into.
    fn render_notice(
        &self,
        state: Option<&SessionState>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = theme(cx);
        let prompt = self.render_prompt(state, &theme, cx);
        let state = state?;
        let transfer = state.transfer.as_ref();
        let (text, color) = match (transfer, &state.notice, state.busy) {
            (Some(transfer), _, _) => (transfer.line(state.is_local), theme.text_muted),
            (None, Some(Notice::Error(text)), _) => (text.clone(), theme.danger),
            (None, Some(Notice::Info(text)), _) => (text.clone(), theme.text_muted),
            (None, None, true) => (ts!("files.loading"), theme.text_muted),
            (None, None, false) => {
                // Nothing to report, but a question may still be waiting; it
                // owns the whole strip in that case.
                return prompt.map(|prompt| {
                    notice_strip(&theme)
                        .text_size(px(11.))
                        .child(prompt)
                        .into_any_element()
                });
            }
        };

        // The track is the same hairline colour as the panel's own borders, so
        // an idle-looking bar reads as part of the frame rather than as a
        // control; only the accent fill claims attention.
        let bar = transfer.map(|transfer| {
            div()
                .flex_none()
                .w_full()
                .h(px(PROGRESS_BAR))
                .rounded_sm()
                .bg(theme.border)
                .child(
                    div()
                        .h_full()
                        .w(relative(transfer.fraction()))
                        .rounded_sm()
                        .bg(theme.accent),
                )
        });

        Some(
            notice_strip(&theme)
                .text_size(px(11.))
                .text_color(color)
                // Same recipe as the header: `truncate` needs a row flexing the
                // text child to resolve its width against, and a bare `w_full`
                // gives it none, so the whole line collapses to an ellipsis.
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .w_full()
                        .child(div().flex_1().min_w_0().truncate().child(text)),
                )
                .children(bar)
                .children(prompt)
                .into_any_element(),
        )
    }
}

impl EventEmitter<FilePanelEvent> for FilePanel {}

impl Focusable for FilePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FilePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        self.apply_pending_focus(window, cx);
        self.watch_list_scroll(cx);
        let state = self
            .session
            .as_ref()
            .and_then(|session| self.states.get(&session.entity_id()));

        let header = self.render_header(state, cx);
        let list = self.render_list(state, cx);
        let notice = self.render_notice(state, cx);
        let context = self.render_context(state, cx);
        let accent = theme.accent;
        // Asked of the focus tree here rather than remembered from a focus
        // listener: listeners run at the tail of a draw, so a remembered flag
        // would light the frame one input event late, while `window.focus`
        // itself schedules the repaint this read then sees. `contains_focused`
        // rather than `is_focused` so that typing in the rename field — a
        // handle nested under this one — still counts as the panel having the
        // keyboard.
        let focused = self.focus_handle.contains_focused(window, cx);

        // Kept wholly inside the panel and added last, so it wins the hit test
        // against the rows it covers. Straddling the border would put half the
        // grab area over the pane next door, which is drawn after the panel and
        // would take those pixels back.
        let handle = div()
            .id("file-panel-edge")
            .absolute()
            // A plain hitbox does not stop events reaching what is under it,
            // and under this one are listing rows that would take the press as
            // a selection.
            .occlude()
            .top_0()
            .bottom_0()
            .right_0()
            .w(px(PANEL_HANDLE))
            .cursor_ew_resize()
            // An empty preview: the edge follows the pointer directly, so a
            // ghost trailing it would only be a second thing to watch.
            .on_drag(DraggedPanelEdge, |_, _, _, cx| cx.new(|_| gpui::Empty));

        div()
            .id("file-panel")
            // Makes the panel a focus target, so a click anywhere in it moves
            // the keyboard off the terminal instead of leaving focus behind
            // where it was.
            .track_focus(&self.focus_handle)
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(self.width))
            .h_full()
            .min_h_0()
            // The panel is the only thing covering these pixels, so this is
            // where the window opacity lands on them. Exactly one such fill per
            // pixel — see `app_settings::window_tint`.
            .bg(app_settings::window_tint(theme.background, cx))
            // A full hairline rather than just the divider on the right, so the
            // drop highlight below can recolour a frame the user can see
            // without adding a second tinted fill over the panel. It doubles as
            // the focus indication, the same accent frame the panes carry — the
            // workspace drops the active pane's accent while this one is lit,
            // so only ever one frame claims the keyboard.
            .border_1()
            .border_color(if focused { accent } else { theme.border })
            // Refined over the base style, so a drag hovering the panel keeps
            // the accent whether or not the panel also holds focus.
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(accent))
            .can_drop(|dragged, _window, _cx| {
                <dyn Any>::downcast_ref::<ExternalPaths>(dragged).is_some()
            })
            .on_drop(cx.listener(|panel, paths: &ExternalPaths, _window, cx| {
                panel.upload(paths.paths().to_vec(), cx);
            }))
            // Listening on the panel, not on the handle: the handle slides out
            // from under the pointer as the drag goes on, while the panel's left
            // edge — the one the new width is measured from — stays put.
            .on_drag_move::<DraggedPanelEdge>(
                cx.listener(|panel, event, _window, cx| panel.drag_edge(event, cx)),
            )
            // Same reasoning one step over: the listing's thumb slides out from
            // under the pointer, and the panel is what stays mounted for the
            // whole gesture.
            .on_drag_move::<DraggedThumb>(cx.listener(
                |panel, event: &DragMoveEvent<DraggedThumb>, _window, cx| {
                    panel.drag_scrollbar(event, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|panel, _: &MouseUpEvent, _window, cx| panel.release_scrollbar(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|panel, _: &MouseUpEvent, _window, cx| panel.release_scrollbar(cx)),
            )
            .child(header)
            .child(list)
            .children(notice)
            .child(handle)
            .children(context)
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

/// Walks `paths` and works out everything an upload into `directory` will do.
///
/// Runs on a background thread, because a dropped folder can hold tens of
/// thousands of entries and every one of them costs a `stat`.
///
/// Two rules decide what is in the plan:
///
/// * **Symlinked directories are left out entirely**, not followed. A tree can
///   link back into itself, and a walk that followed such a link would recurse
///   until it ran out of memory. A symlinked *file* is sent as its target,
///   which is what dragging a link into a terminal usually means.
/// * **Anything that cannot be stat'ed is left out and logged** — a broken
///   link, or a file removed between the listing and the walk. Failing the
///   whole batch over one of them would be worse than copying the rest.
///
/// The walk is breadth-first so that [`UploadPlan::directories`] comes out with
/// parents before children: SFTP has no `mkdir -p`, so the list is created in
/// exactly that order.
fn plan_upload(paths: Vec<PathBuf>, directory: String) -> UploadPlan {
    let mut plan = UploadPlan::default();
    let mut queue: VecDeque<(PathBuf, String)> = paths
        .into_iter()
        .map(|path| (path, directory.clone()))
        .collect();

    while let Some((local, parent)) = queue.pop_front() {
        // `symlink_metadata` describes the link itself, so this is the real
        // directory test; a link to one falls through to the follow below.
        let Ok(link) = std::fs::symlink_metadata(&local) else {
            log::debug!("not uploading {}: it could not be read", local.display());
            continue;
        };
        let Some(name) = local
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            log::debug!("not uploading {}: it has no file name", local.display());
            continue;
        };

        if link.is_dir() {
            let remote = join(&parent, &name);
            match std::fs::read_dir(&local) {
                Ok(entries) => {
                    plan.directories.push(remote.clone());
                    for entry in entries.flatten() {
                        queue.push_back((entry.path(), remote.clone()));
                    }
                }
                Err(error) => {
                    log::debug!("not uploading {}: {error}", local.display());
                }
            }
            continue;
        }

        let Ok(target) = std::fs::metadata(&local) else {
            log::debug!(
                "not uploading {}: its target could not be read",
                local.display()
            );
            continue;
        };
        if target.is_dir() {
            log::debug!("not uploading {}: it links to a directory", local.display());
            continue;
        }
        plan.total = plan.total.saturating_add(target.len());
        plan.files.push(PlannedUpload {
            local,
            directory: parent,
            size: target.len(),
        });
    }
    plan
}

/// Walks the remote directory `remote` and works out what a download into
/// `local` will do.
///
/// Mirrors [`plan_upload`], with the same cycle rule in the other direction: an
/// entry that is a symlink *and* a directory is not descended into, because a
/// remote tree can link back into itself and there is no cheap way to prove it
/// does not. Sizes come from the listing, which is what gives the progress bar
/// a total without a `stat` per file.
async fn plan_download(
    source: &Arc<dyn FileSource>,
    remote: String,
    local: PathBuf,
) -> Result<DownloadPlan, FileError> {
    let mut plan = DownloadPlan {
        directories: vec![local.clone()],
        ..DownloadPlan::default()
    };
    let mut queue = VecDeque::from([(remote, local)]);

    while let Some((remote, local)) = queue.pop_front() {
        for entry in source.read_dir(&remote).await? {
            if !is_plain_name(&entry.name) {
                log::debug!("not downloading {}/{}: odd name", remote, entry.name);
                continue;
            }
            let child_remote = join(&remote, &entry.name);
            let child_local = local.join(&entry.name);

            if entry.is_dir {
                if entry.is_symlink {
                    log::debug!("not downloading {child_remote}: it links to a directory");
                    continue;
                }
                plan.directories.push(child_local.clone());
                queue.push_back((child_remote, child_local));
            } else {
                plan.total = plan.total.saturating_add(entry.size);
                plan.files.push(PlannedDownload {
                    remote: child_remote,
                    local: child_local,
                    size: entry.size,
                });
            }
        }
    }
    Ok(plan)
}

/// Creates the local directories of `plan` and fetches its files in order.
///
/// Shared by the one-entry and the many-entry download so that both measure
/// against the same bar in the same way; only the question asked beforehand and
/// the sentence said afterwards differ between them.
async fn run_download(
    panel: &WeakEntity<FilePanel>,
    cx: &mut AsyncApp,
    session: EntityId,
    source: &Arc<dyn FileSource>,
    plan: DownloadPlan,
) -> Ran {
    if panel
        .update(cx, |panel, cx| panel.size_transfer(session, plan.total, cx))
        .is_err()
    {
        return Ran::Abandoned;
    }

    // One hop to a background thread for every directory at once: the creations
    // are microseconds each but there can be thousands, and this is a UI thread
    // that also has to keep drawing the bar.
    if let Err(error) = create_all(cx, plan.directories).await {
        return Ran::Finished(Some(error));
    }

    let mut moved = 0u64;
    for file in plan.files {
        let label = file_name(&file.local);
        if panel
            .update(cx, |panel, cx| {
                panel.transfer_file(session, label, moved, cx);
            })
            .is_err()
        {
            return Ran::Abandoned;
        }

        let (sender, receiver) = mpsc::unbounded();
        let transfer = source.copy_out(&file.remote, file.local, Some(sender));
        match follow(panel, cx, session, moved, receiver, transfer).await {
            Ok(()) => moved = moved.saturating_add(file.size),
            Err(error) => return Ran::Finished(Some(error)),
        }
    }
    Ran::Finished(None)
}

/// Works out every remote call a delete of `targets` in `directory` will make.
///
/// Two rules, and both of them are about not deleting more than was asked:
///
/// * **A symbolic link is removed as a link**, never descended into, even when
///   it points at a directory. Walking one would delete the target's contents —
///   somewhere else entirely on the server — and leave the link behind.
/// * **A real directory is emptied from the leaves upwards.** SFTP refuses to
///   remove a directory that still holds anything, so the order is not a
///   preference but the only order that works: the walk collects directories
///   breadth-first and the plan hands them back reversed.
async fn plan_delete(
    source: &Arc<dyn FileSource>,
    directory: &str,
    targets: Vec<FileEntry>,
) -> Result<Vec<Removal>, FileError> {
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut queue = VecDeque::new();

    for entry in targets {
        let path = join(directory, &entry.name);
        if needs_walking(&entry) {
            directories.push(removal(path.clone(), &entry.name, true));
            queue.push_back(path);
        } else {
            files.push(removal(path, &entry.name, false));
        }
    }

    while let Some(parent) = queue.pop_front() {
        for entry in source.read_dir(&parent).await? {
            if !is_plain_name(&entry.name) {
                log::debug!("not deleting {}/{}: odd name", parent, entry.name);
                continue;
            }
            let path = join(&parent, &entry.name);
            if entry.is_dir && !entry.is_symlink {
                directories.push(removal(path.clone(), &entry.name, true));
                queue.push_back(path);
            } else {
                files.push(removal(path, &entry.name, false));
            }
        }
    }

    // Files first — they can go in any order — then the directories from the
    // deepest outwards, which is what the reversal of a breadth-first walk is.
    directories.reverse();
    files.extend(directories);
    Ok(files)
}

/// Whether a delete has to walk into `entry` before it can remove it.
///
/// True only for a *real* directory. A symbolic link is removed as itself no
/// matter what it points at: the listing reports a link to a directory with
/// `is_dir` set so that it can be navigated into, and treating that as a
/// directory here would walk somewhere else on the server and delete the
/// target's contents while leaving the link behind.
fn needs_walking(entry: &FileEntry) -> bool {
    entry.is_dir && !entry.is_symlink
}

/// Builds one entry of a delete plan.
fn removal(path: String, name: &str, directory: bool) -> Removal {
    Removal {
        path,
        name: SharedString::from(name.to_owned()),
        directory,
    }
}

/// Whether a name is safe to append to a path, local or remote.
///
/// Names come from the server, and one answering `..` or `a/b` would make
/// [`Path::join`] write *outside* the directory the user picked — or, on the
/// remote side, aim a delete at something the user never saw. A listing has no
/// legitimate use for either, so a name carrying one is dropped rather than
/// sanitised: there is no correct guess at what it was supposed to be.
///
/// The same test guards the rename field, where the name comes from the user
/// instead. It is the same hazard from the other direction — `../notes.txt`
/// typed into it would move the entry out of the directory on screen — and the
/// answer is the same: refuse it rather than interpret it.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\\')
}

/// Creates every directory in `directories`, on a background thread.
///
/// One hop for the whole list rather than one per directory: each creation is
/// microseconds, but a deep tree has thousands of them and this is a thread
/// that also has to keep drawing the progress bar.
async fn create_all(cx: &mut AsyncApp, directories: Vec<PathBuf>) -> Result<(), FileError> {
    if directories.is_empty() {
        return Ok(());
    }
    cx.background_executor()
        .spawn(async move {
            for directory in directories {
                std::fs::create_dir_all(&directory).map_err(|error| {
                    FileError::Local(format!("could not create {}: {error}", directory.display()))
                })?;
            }
            Ok(())
        })
        .await
}

/// Drives one file transfer while feeding its byte count into the status line.
///
/// The transfer future and its progress stream are polled *together*, which is
/// the whole reason the SFTP layer takes a channel: awaiting the transfer first
/// and reading the counts afterwards would leave a single large file showing no
/// movement at all until it landed. `base` is what the batch had already moved
/// before this file started, so the bar measures the batch and not the file.
///
/// The service drops its sender before answering, so the stream ends first and
/// the loop always leaves through the transfer arm.
async fn follow<T>(
    panel: &WeakEntity<FilePanel>,
    cx: &mut AsyncApp,
    session: EntityId,
    base: u64,
    mut receiver: UnboundedReceiver<u64>,
    transfer: impl Future<Output = Result<T, FileError>>,
) -> Result<T, FileError> {
    let transfer = futures::FutureExt::fuse(transfer);
    futures::pin_mut!(transfer);

    loop {
        futures::select! {
            outcome = transfer => return outcome,
            moved = receiver.next() => {
                let Some(moved) = moved else { continue };
                if panel
                    .update(cx, |panel, cx| {
                        panel.advance_transfer(session, base.saturating_add(moved), cx);
                    })
                    .is_err()
                {
                    // The panel is gone; the transfer still has to be waited
                    // for, or dropping it here would abandon a half-written
                    // file with no one to report it.
                    return transfer.await;
                }
            }
        }
    }
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

/// Whether a listing row's name is too long to be shown whole, and so wants a
/// tooltip carrying it in full.
///
/// An estimate, like [`fold_budget`], and chosen over measuring for a reason
/// specific to this list: **the listing is not virtualised**. Every entry of the
/// directory is built on every repaint, so a directory with ten thousand files
/// builds ten thousand rows a frame. Shaping each name through
/// [`Window::text_system`](gpui::Window::text_system) to learn its exact width
/// would put a text layout — the expensive half of drawing text — on that path,
/// multiplied by the size of the directory, to decide something no one sees
/// until they hover. The arithmetic below costs a few multiplications.
///
/// Being wrong is not symmetric here, so the estimate leans one way: a name cut
/// off with no way to read it is a real loss, while a tooltip on a name that
/// happened to fit is a moment of redundancy. Every constant therefore rounds
/// *up* — [`ROW_CHAR`], [`SIZE_CHAR`] — and every subtraction below is taken at
/// its most pessimistic, so the budget errs small and the tooltip errs present.
///
/// Widths count columns rather than characters: a Hangul or Han name occupies
/// two columns per character at the same font size, and counting `chars` would
/// let such a name run to twice the width before anyone thought it was long.
fn name_is_clipped(width: f32, name: &str, badge: bool, size: Option<&str>) -> bool {
    // Everything the row spends before the name gets what is left: the panel's
    // own hairline border on both sides, the row's padding, the leading icon
    // and the gap after it, then the symlink badge and the size column when
    // they are there — each with the gap that precedes it.
    let mut spent = 2. + 2. * ROW_PADDING + ROW_ICON + ROW_GAP;
    if badge {
        spent += ROW_GAP + BADGE_ICON;
    }
    if let Some(size) = size {
        spent += ROW_GAP + columns(size) as f32 * SIZE_CHAR;
    }

    let usable = width - spent;
    if !usable.is_finite() || usable <= 0. {
        // No room to draw a name at all, so anything at all is clipped.
        return !name.is_empty();
    }
    let budget = (usable / ROW_CHAR).floor();
    let budget = if budget >= 0. { budget as usize } else { 0 };
    columns(name) > budget
}

/// How many columns `text` occupies, counting East Asian wide characters twice.
///
/// [`UnicodeWidthStr`] answers the question the estimate actually asks — how
/// much room this will take — for the one distinction that matters at this
/// resolution. It is not a substitute for measuring a proportional font; it is
/// what keeps a CJK name from being treated as half its real width.
fn columns(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// How much path the header can hold at a panel `width` pixels wide, in
/// characters.
///
/// An estimate, and deliberately so: the header's font is proportional, so the
/// only exact answer would be to lay the row out and measure it, and a header
/// that reflowed after layout would need a second pass every repaint. The
/// estimate is allowed to be wrong because being wrong is cheap — the row wraps
/// rather than truncating, so a budget that came out too generous costs a line
/// of header and never the leaf directory the user is standing in.
///
/// Tied to the width rather than fixed because the panel is dragged between
/// [`MIN_PANEL_WIDTH`] and [`MAX_PANEL_WIDTH`]: one number for both ends would
/// fold a path that had room to spare at 560px, or overflow at 180px.
fn fold_budget(width: f32) -> usize {
    let usable = width - 2. * HEADER_PADDING;
    if !usable.is_finite() || usable <= 0. {
        return MIN_PATH_CHARS;
    }
    let chars = (usable / CRUMB_CHAR).floor();
    // Saturating rather than wrapping: `as` on a float out of range would give
    // a budget the row could never spend.
    let chars = if chars >= 0. { chars as usize } else { 0 };
    chars.max(MIN_PATH_CHARS)
}

/// The `C:` at the head of `path`, when it has one.
///
/// A session running a shell on Windows browses paths whose root is a drive
/// rather than a bare slash, and the header has to know: splitting `C:/Users/ada`
/// on `/` alone would make `C:` an ordinary piece hanging off `/`, and pressing
/// it would navigate to `/C:`, which names nothing on any filesystem.
///
/// Decided from the shape of the path rather than from `cfg!(windows)`, because
/// the panel holds paths of both kinds at once — the tab beside a local one may
/// be an SSH session, and *those* paths are POSIX and absolute whatever the
/// server runs on. That is also why the two can never be confused: a POSIX
/// absolute path begins with `/`, which is not a letter, so nothing an SFTP
/// source produces can be read as a drive.
fn drive_prefix(path: &str) -> Option<&str> {
    let mut chars = path.chars();
    let letter = chars.next()?;
    if !letter.is_ascii_alphabetic() || chars.next()? != ':' {
        return None;
    }
    // `C:` or `C:/…` and nothing else: `C:logs` is relative to that drive's own
    // current directory, which is not a shape the panel is ever handed.
    match chars.next() {
        None | Some('/') => Some(&path[..2]),
        Some(_) => None,
    }
}

/// Whether `path` is a filesystem root, and so has no parent to walk up to.
///
/// Two spellings of one idea, for the same reason [`drive_prefix`] exists: `/`
/// on a POSIX source, `C:/` on a drive of this machine.
fn is_root(path: &str) -> bool {
    match drive_prefix(path) {
        Some(drive) => &path[drive.len()..] == ROOT_CRUMB,
        None => path == ROOT_CRUMB,
    }
}

/// Breaks `path` into the pieces the header draws.
///
/// `/srv/app/logs` becomes `/`, `srv`, `app`, `logs`, each carrying the
/// directory whose subdirectories could take its place. What does not fit in
/// `budget` is folded away by [`fold`].
///
/// Paths are absolute and separated by `/` — remote ones because SFTP is POSIX
/// on the wire whatever the server runs on, local ones because the local source
/// spells them that way on the way out — so this splits on `/` and nothing else;
/// anything relative, which the panel never produces, is read as if it hung off
/// the root.
///
/// The root the pieces hang off is not always `/`, which is the one thing this
/// does not take on faith: see [`drive_prefix`].
fn crumbs(path: &str, budget: usize) -> Vec<Crumb> {
    let (root, rest) = match drive_prefix(path) {
        Some(drive) => (format!("{drive}/"), &path[drive.len()..]),
        None => (ROOT_CRUMB.to_owned(), path),
    };

    let mut crumbs = vec![Crumb {
        label: SharedString::from(root.clone()),
        menu: CrumbMenu::Root(root.clone()),
    }];

    let mut directory = root;
    for name in rest.split('/').filter(|name| !name.is_empty()) {
        crumbs.push(Crumb {
            label: SharedString::from(name.to_owned()),
            menu: CrumbMenu::Siblings(directory.clone()),
        });
        directory = join(&directory, name);
    }

    fold(crumbs, budget)
}

/// The rows the root breadcrumb offers for `roots`, or `None` for no menu.
///
/// `None` means "there is nothing to choose between" — one root, or none at
/// all — and is what keeps every POSIX source behaving exactly as it did before
/// roots existed: the caller falls back to listing the root's subdirectories.
/// The threshold is two rather than one *other* root because the row for the
/// tree the panel is already in is a real destination, not a no-op: `C:/` from
/// `C:/Users/ada` moves.
///
/// Each root labels itself. A root is already a short, complete path — `/`,
/// `C:/` — so there is no shorter name to give it, and the label a user knows a
/// drive by is precisely its letter.
fn root_targets(roots: Vec<String>) -> Option<Vec<CrumbTarget>> {
    if roots.len() < 2 {
        return None;
    }
    Some(
        roots
            .into_iter()
            .map(|root| CrumbTarget {
                label: SharedString::from(root.clone()),
                path: root,
            })
            .collect(),
    )
}

/// Replaces the pieces `budget` characters cannot hold with a single ellipsis.
///
/// The tail is what a header is for: `/srv/app/releases/2026-07-30/logs` says
/// where you are and `/srv/app/releases/2026-07…` does not, so the pieces are
/// kept from the *back* — the leaf always, then as many of its ancestors as
/// fit. The root survives whatever the budget, at the one or three characters it
/// spells itself with; it is the one destination reachable from nowhere else in
/// the row.
///
/// The folded pieces are not lost: they become the rows of the ellipsis's own
/// dropdown, which is the only reason a piece may be dropped at all.
fn fold(crumbs: Vec<Crumb>, budget: usize) -> Vec<Crumb> {
    if crumb_width(&crumbs) <= budget {
        return crumbs;
    }

    // The root — whose own width is `/` or `C:/`, so it is read off the row
    // rather than assumed — and the ellipsis, then every kept piece at its own
    // text plus the separator drawn in front of it: the same arithmetic
    // `crumb_width` does, over the row this is about to build. Neither of the
    // first two is preceded by a separator, because a root label ends in one.
    let root_width = crumbs.first().map_or(0, |root| root.label.chars().count());
    let mut spent = root_width + FOLD_CRUMB.chars().count();
    let mut kept = 0;
    for crumb in crumbs.iter().skip(1).rev() {
        let cost = crumb.label.chars().count() + CRUMB_SEPARATOR.chars().count();
        // The leaf is kept whatever it costs: a row that folded away the
        // directory you are standing in would say nothing at all.
        if kept > 0 && spent + cost > budget {
            break;
        }
        spent += cost;
        kept += 1;
    }

    let mut pieces = crumbs.into_iter();
    let Some(root) = pieces.next() else {
        return Vec::new();
    };
    let mut rest: Vec<Crumb> = pieces.collect();
    let tail = rest.split_off(rest.len().saturating_sub(kept));

    let folded: Vec<CrumbTarget> = rest
        .into_iter()
        .filter_map(|crumb| {
            let path = crumb.path()?;
            Some(CrumbTarget {
                label: crumb.label,
                path,
            })
        })
        .collect();
    // A single piece too long for the budget folds nothing away, and an
    // ellipsis with an empty menu behind it would be a dead end.
    if folded.is_empty() {
        return std::iter::once(root).chain(tail).collect();
    }

    let ellipsis = Crumb {
        label: SharedString::new_static(FOLD_CRUMB),
        menu: CrumbMenu::Folded(folded),
    };
    std::iter::once(root)
        .chain(std::iter::once(ellipsis))
        .chain(tail)
        .collect()
}

/// How many characters the header spends on a run of pieces.
///
/// Every piece's own text, plus the separator in front of it — which the piece
/// before it may already have supplied, as the root's `/` does.
fn crumb_width(crumbs: &[Crumb]) -> usize {
    crumbs
        .iter()
        .enumerate()
        .map(|(index, crumb)| {
            let separated = index
                .checked_sub(1)
                .is_some_and(|previous| needs_separator(&crumbs[previous].label));
            crumb.label.chars().count()
                + if separated {
                    CRUMB_SEPARATOR.chars().count()
                } else {
                    0
                }
        })
        .sum()
}

/// Whether a piece drawn after `label` needs a separator of its own.
///
/// Only a root does not ask for one: its label already ends in a slash — `/`
/// itself, or `C:/` on a drive — and a second one after it would read as `//`.
fn needs_separator(label: &str) -> bool {
    !label.ends_with('/')
}

/// Renders a byte count the way a file manager does.
///
/// The unit symbols are not translated: like the terminal grid size in the
/// status bar they are symbols rather than words, and every locale writes them
/// the same way.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit + 1 < UNITS.len() {
        value /= 1024.;
        unit += 1;
    }
    match UNITS.get(unit) {
        Some(_) if unit == 0 => format!("{bytes} B"),
        Some(symbol) => format!("{value:.1} {symbol}"),
        None => format!("{bytes} B"),
    }
}

/// The file name of `path`, for a status message.
fn file_name(path: &Path) -> SharedString {
    path.file_name()
        .map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
        .into()
}

/// Where the save dialog opens by default.
///
/// The platform picker remembers the last directory the user chose, so this
/// only has to be a sensible *first* answer; the home directory is one on every
/// platform, and an empty path leaves the choice to the picker.
fn suggested_directory() -> PathBuf {
    directories::UserDirs::new().map_or_else(PathBuf::new, |dirs| dirs.home_dir().to_owned())
}

/// The frame of the strip along the bottom of the panel.
///
/// Shared by the status line and the question below it so that the two never
/// draw two borders, two paddings or two hairlines between them: whatever is
/// showing, there is exactly one strip.
fn notice_strip(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(4.))
        .w_full()
        .min_w_0()
        .px(px(8.))
        .py(px(4.))
        .border_t_1()
        .border_color(theme.border)
}

/// A centred message standing in for a listing.
fn placeholder(message: SharedString, theme: &Theme) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .px(px(12.))
        .text_size(px(11.))
        .text_color(theme.text_muted)
        .child(message)
        .into_any_element()
}

/// A compact icon-only toolbar button, in the style of the tab strip's own.
fn icon_button(
    id: impl Into<ElementId>,
    path: &'static str,
    tip: SharedString,
    enabled: bool,
    theme: &Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let hover = theme.surface_hover;
    let text = theme.text;
    // The icon tint rather than the muted text of the labels around the panel:
    // these buttons are all mark and no word. A disabled one is faded from the
    // same colour, so the two states stay one family.
    let color = if enabled {
        theme.icon
    } else {
        theme.icon.opacity(0.4)
    };

    div()
        .id(id.into())
        .group(BUTTON_GROUP)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(22.))
        .rounded_sm()
        // Outside the `enabled` gate on purpose. These buttons carry no text, so
        // a dimmed one is a glyph with no explanation at all — and "what would
        // this have done?" is exactly the question a user has when a button will
        // not take a click. gpui builds the tooltip's hitbox from the tooltip
        // alone, so this keeps working with every listener below removed.
        .tooltip(tooltip_label(tip))
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .on_click(move |event, window, cx| on_click(event, window, cx))
        })
        .child(
            icons::icon(path, px(TOOLBAR_ICON), color).when(enabled, |icon| {
                icon.group_hover(BUTTON_GROUP, move |style| style.text_color(text))
            }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listing entry, for the ordering test.
    fn entry(name: &str, is_dir: bool) -> FileEntry {
        FileEntry {
            name: name.to_owned(),
            is_dir,
            is_symlink: false,
            size: 0,
        }
    }

    #[test]
    fn directories_sort_before_files_and_case_is_ignored() {
        let mut entries = vec![
            entry("notes.txt", false),
            entry("Zebra", true),
            entry("apple", true),
            entry("Beta.log", false),
        ];
        sort_entries(&mut entries);

        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["apple", "Zebra", "Beta.log", "notes.txt"]);
    }

    #[test]
    fn joining_adds_exactly_one_separator() {
        assert_eq!(join("/home/alice", "notes.txt"), "/home/alice/notes.txt");
        assert_eq!(join("/", ".."), "/..");
        assert_eq!(join("/srv/", "app"), "/srv/app");
    }

    /// The labels of a breadcrumb row, in the order the header draws them.
    fn labels(crumbs: &[Crumb]) -> Vec<&str> {
        crumbs.iter().map(|crumb| crumb.label.as_ref()).collect()
    }

    /// The directory a piece would list to fill its dropdown, or `None` for the
    /// ellipsis, which already knows.
    ///
    /// A root answers with itself: its dropdown is the source's other roots
    /// when there are any, and its own subdirectories when there are not.
    fn sibling_of(crumb: &Crumb) -> Option<&str> {
        match &crumb.menu {
            CrumbMenu::Root(root) => Some(root.as_str()),
            CrumbMenu::Siblings(directory) => Some(directory.as_str()),
            CrumbMenu::Folded(_) => None,
        }
    }

    /// Whether a piece is the leading one, which is the only kind whose
    /// dropdown may turn out to be a list of roots.
    fn is_root_crumb(crumb: &Crumb) -> bool {
        matches!(crumb.menu, CrumbMenu::Root(_))
    }

    /// The budget the panel gets at the width it opens at, which is the one
    /// every fold test below is written against.
    fn budget() -> usize {
        fold_budget(DEFAULT_PANEL_WIDTH)
    }

    #[test]
    fn the_root_is_a_single_crumb_listing_itself() {
        let crumbs = crumbs("/", budget());
        assert_eq!(labels(&crumbs), ["/"]);
        // Marked as the root rather than inferred from its label further down:
        // it is the one piece whose dropdown may be a list of roots instead.
        assert!(is_root_crumb(&crumbs[0]));
        // No parent to take siblings from, so the root offers what is inside
        // it: the alternative is a piece that cannot be pressed at all.
        assert_eq!(sibling_of(&crumbs[0]), Some("/"));
        assert_eq!(crumbs[0].path().as_deref(), Some("/"));
    }

    /// What the root breadcrumb does with the answer, which is the whole of the
    /// drive-switching feature: several roots become the menu, and anything
    /// less leaves the piece behaving as it did before roots existed.
    #[test]
    fn only_a_source_with_several_roots_offers_them() {
        let drives = root_targets(vec!["C:/".to_owned(), "D:/".to_owned()])
            .expect("two drives must be a menu");
        let labels: Vec<&str> = drives.iter().map(|target| target.label.as_ref()).collect();
        let paths: Vec<&str> = drives.iter().map(|target| target.path.as_str()).collect();
        // A drive names itself, and the row navigates to the drive's own top —
        // which is only a place at all if the separator survived.
        assert_eq!(labels, ["C:/", "D:/"]);
        assert_eq!(paths, ["C:/", "D:/"]);

        // The order is the source's, and the drive the panel is already on is a
        // row like any other: it is a real move from anywhere below it.
        let many = root_targets(vec!["A:/".to_owned(), "C:/".to_owned(), "Z:/".to_owned()])
            .expect("three drives must be a menu");
        assert_eq!(many.len(), 3);

        // A POSIX source — SFTP, WSL, and unix's own local source — reports the
        // single root, which must leave the piece listing subdirectories.
        assert!(root_targets(vec!["/".to_owned()]).is_none());
        // And a source that could not answer says no less than that one does.
        assert!(root_targets(Vec::new()).is_none());
    }

    #[test]
    fn a_short_path_keeps_every_crumb_and_names_its_parent() {
        let crumbs = crumbs("/srv/app/logs", budget());
        assert_eq!(labels(&crumbs), ["/", "srv", "app", "logs"]);

        let parents: Vec<Option<&str>> = crumbs.iter().map(sibling_of).collect();
        assert_eq!(
            parents,
            [Some("/"), Some("/"), Some("/srv"), Some("/srv/app")]
        );
        // Pressing a row of the leaf's menu must land beside the leaf, not
        // inside it.
        assert_eq!(crumbs[3].path().as_deref(), Some("/srv/app/logs"));
    }

    /// A local Windows session hands the panel `C:/Users/ada`, whose root is the
    /// drive. Reading `C:` as an ordinary piece would hang it off `/` and send
    /// every press on it to `/C:`, which names nothing.
    #[test]
    fn a_drive_path_roots_itself_at_the_drive() {
        let crumbs = crumbs("C:/Users/ada", budget());
        assert_eq!(labels(&crumbs), ["C:/", "Users", "ada"]);

        let parents: Vec<Option<&str>> = crumbs.iter().map(sibling_of).collect();
        assert_eq!(parents, [Some("C:/"), Some("C:/"), Some("C:/Users")]);

        // The drive is the piece that can offer the other drives; `Users`
        // carries the same directory but is an ordinary piece hanging off it.
        assert!(is_root_crumb(&crumbs[0]));
        assert!(!is_root_crumb(&crumbs[1]));

        // Every piece has to be a path the source can be asked for, which the
        // drive root is only if it kept its separator.
        assert_eq!(crumbs[0].path().as_deref(), Some("C:/"));
        assert_eq!(crumbs[1].path().as_deref(), Some("C:/Users"));
        assert_eq!(crumbs[2].path().as_deref(), Some("C:/Users/ada"));

        // The drive alone is a whole row, and the one with no parent above it.
        let alone = super::crumbs("C:/", budget());
        assert_eq!(labels(&alone), ["C:/"]);
        assert_eq!(alone[0].path().as_deref(), Some("C:/"));
        assert!(is_root("C:/") && is_root("/"));
        assert!(!is_root("C:/Users") && !is_root("/srv"));
    }

    /// A drive root is two characters wider than `/`, so the fold has to spend
    /// them: a row folded as if the root cost one would come back over budget.
    #[test]
    fn a_long_drive_path_folds_around_the_drive() {
        let path = "C:/Users/ada/AppData/Local/Programs/rulogman/releases/today";
        let crumbs = crumbs(path, budget());

        assert_eq!(labels(&crumbs).first(), Some(&"C:/"));
        assert_eq!(labels(&crumbs).get(1), Some(&"\u{2026}"));
        assert_eq!(labels(&crumbs).last(), Some(&"today"));
        assert!(
            crumb_width(&crumbs) <= budget(),
            "the folded row is still {} characters wide",
            crumb_width(&crumbs)
        );

        // The folded pieces stay reachable, and by paths that start at the
        // drive rather than at a root the machine does not have.
        let CrumbMenu::Folded(folded) = &crumbs[1].menu else {
            panic!("the second piece must carry the folded ancestors");
        };
        assert_eq!(
            folded.first().map(|target| target.path.as_str()),
            Some("C:/Users")
        );
        assert!(
            folded.iter().all(|target| target.path.starts_with("C:/")),
            "a folded piece left the drive"
        );
    }

    /// The drive test is a *shape* test, not a platform one, so the shapes that
    /// only look like drives have to stay ordinary pieces — a POSIX path can
    /// hold a `:` anywhere, including in its first name.
    #[test]
    fn only_an_absolute_drive_is_read_as_one() {
        assert_eq!(drive_prefix("C:/Users"), Some("C:"));
        assert_eq!(drive_prefix("c:"), Some("c:"));
        // Relative to that drive's own current directory, which the panel never
        // holds — and would be a path this header could not walk.
        assert_eq!(drive_prefix("C:logs"), None);
        // POSIX paths, one of which begins with a name containing a colon.
        assert_eq!(drive_prefix("/srv/app"), None);
        assert_eq!(drive_prefix("/C:/app"), None);
        assert_eq!(drive_prefix("1:/app"), None);
        assert_eq!(drive_prefix(""), None);
    }

    /// The budget follows the panel's edge: dragging it wider must never fold
    /// *more* of the path away, and the width the panel opens at must still
    /// hold the 38 characters the header showed before it could be resized.
    #[test]
    fn the_budget_grows_with_the_panel_and_never_falls_below_its_floor() {
        let narrow = fold_budget(MIN_PANEL_WIDTH);
        let default = fold_budget(DEFAULT_PANEL_WIDTH);
        let wide = fold_budget(MAX_PANEL_WIDTH);

        assert!(narrow < default, "{narrow} is not narrower than {default}");
        assert!(default < wide, "{default} is not narrower than {wide}");
        assert_eq!(default, 38);

        // The floor holds whatever arrives: a width smaller than the padding
        // itself, and the degenerate values a drag outside the window could
        // otherwise arrive with.
        assert!(narrow >= MIN_PATH_CHARS);
        assert_eq!(fold_budget(0.), MIN_PATH_CHARS);
        assert_eq!(fold_budget(-100.), MIN_PATH_CHARS);
        assert_eq!(fold_budget(f32::NAN), MIN_PATH_CHARS);

        // Even at the floor there is room for the root, the ellipsis and a leaf
        // of a useful length.
        let crumbs = crumbs("/srv/application/logs/today", MIN_PATH_CHARS);
        assert_eq!(labels(&crumbs).first(), Some(&"/"));
        assert_eq!(labels(&crumbs).last(), Some(&"today"));
    }

    /// The fold: what does not fit goes behind one ellipsis, and stays
    /// reachable through the menu that ellipsis carries.
    #[test]
    fn a_long_path_folds_its_middle_and_keeps_the_leaf() {
        let path = "/srv/application/releases/2026-07-30T12-00/logs/today";
        let crumbs = crumbs(path, budget());

        assert_eq!(labels(&crumbs).first(), Some(&"/"));
        assert_eq!(labels(&crumbs).get(1), Some(&"\u{2026}"));
        assert_eq!(labels(&crumbs).last(), Some(&"today"));
        assert!(
            crumb_width(&crumbs) <= budget(),
            "the folded row is still {} characters wide",
            crumb_width(&crumbs)
        );

        // Every dropped piece is on the ellipsis's menu, in path order, with
        // the absolute path that moves there.
        let CrumbMenu::Folded(folded) = &crumbs[1].menu else {
            panic!("the second piece must carry the folded ancestors");
        };
        let names: Vec<&str> = folded.iter().map(|target| target.label.as_ref()).collect();
        let paths: Vec<&str> = folded.iter().map(|target| target.path.as_str()).collect();
        assert_eq!(names, ["srv", "application", "releases"]);
        assert_eq!(
            paths,
            ["/srv", "/srv/application", "/srv/application/releases"]
        );
    }

    /// The budget counts characters, not bytes: a Korean directory name is
    /// three bytes a letter, and folding on bytes would hide a row that fits on
    /// screen with room to spare.
    #[test]
    fn the_fold_budget_counts_characters_rather_than_bytes() {
        let path = "/사용자문서/보고서모음/분기별매출자료";
        assert!(path.len() > budget(), "the byte length must exceed it");
        assert!(path.chars().count() <= budget(), "but the length must not");

        let crumbs = crumbs(path, budget());
        assert_eq!(
            labels(&crumbs),
            ["/", "사용자문서", "보고서모음", "분기별매출자료"]
        );
        assert_eq!(crumb_width(&crumbs), path.chars().count());
    }

    /// The narrowest the panel goes still has to say where you are, even when a
    /// single name is longer than the whole budget.
    #[test]
    fn a_leaf_wider_than_the_budget_is_kept_without_an_ellipsis() {
        let deep = crumbs("/srv/a-directory-with-a-very-long-name-indeed", 10);
        assert_eq!(
            labels(&deep),
            ["/", "\u{2026}", "a-directory-with-a-very-long-name-indeed"]
        );

        // Nothing but the root is left to fold when the leaf alone overflows.
        let only = crumbs("/a-directory-with-a-very-long-name-indeed", 10);
        assert_eq!(
            labels(&only),
            ["/", "a-directory-with-a-very-long-name-indeed"]
        );
    }

    #[test]
    fn every_refusal_to_edit_reaches_the_status_line_as_a_failure() {
        // All three are refusals, so none of them may expire on its own the way
        // an `Info` does: the file the user asked for is not open, and only a
        // later success can honestly take that sentence down.
        assert!(matches!(
            edit_notice(&LoadError::TooLarge, false),
            Notice::Error(_)
        ));
        assert!(matches!(
            edit_notice(&LoadError::NotUtf8, false),
            Notice::Error(_)
        ));
        // A transport failure is folded through the same sentence every other
        // panel command uses, so the wording of the failure is preserved whole.
        let transport = LoadError::Transport(FileError::Backend("denied".to_owned()));
        let Notice::Error(said) = edit_notice(&transport, false) else {
            panic!("a failed transfer must not be reported as an aside");
        };
        assert!(said.contains("denied"), "the reason was dropped: {said}");
    }

    #[test]
    fn the_size_cap_is_stated_in_whole_megabytes() {
        // The sentence spells the unit and interpolates a number, so the number
        // has to be one: `10485760 MB` would be a nonsense limit.
        assert_eq!(MAX_EDIT_BYTES % (1024 * 1024), 0);
        assert_eq!(MAX_EDIT_BYTES / 1024 / 1024, 10);
    }

    #[test]
    fn a_percentage_covers_both_ends_and_the_empty_batch() {
        assert_eq!(TransferProgress::percent_of(0, 1000), 0);
        assert_eq!(TransferProgress::percent_of(500, 1000), 50);
        assert_eq!(TransferProgress::percent_of(1000, 1000), 100);
        // Nothing to move is as finished as it will ever be, and must not
        // divide by zero on the way to saying so.
        assert_eq!(TransferProgress::percent_of(0, 0), 100);
        // A file that grew under us must not overflow the bar.
        assert_eq!(TransferProgress::percent_of(2000, 1000), 100);
    }

    /// All three activities carry two placeholders, on both sides of the
    /// wording split, and a translation that dropped one — or a stray `%` the
    /// interpolator choked on — would show the raw key text to the user instead
    /// of failing anywhere a test could see. The local twins are the reason
    /// this loops over both: a missing `files.local.copying` would otherwise
    /// only surface on a machine with a local session open.
    #[test]
    fn the_progress_line_carries_the_name_and_the_percentage() {
        for activity in [Activity::Upload, Activity::Download, Activity::Delete] {
            for is_local in [false, true] {
                let mut progress = TransferProgress::new(activity);
                progress.name = "notes.txt".into();
                progress.percent = 42;

                let line = progress.line(is_local);
                assert!(line.contains("notes.txt"), "saw {line}");
                assert!(line.contains("42"), "saw {line}");
                assert!(!line.contains("%{"), "unreplaced placeholder in {line}");
            }
        }
    }

    /// The guard that stops an expiring message from taking a later one with
    /// it: every message is said at its own epoch, and the timer left behind by
    /// the first one no longer matches once the second has been said.
    #[test]
    fn each_message_is_said_at_its_own_epoch() {
        let mut state = SessionState::new(false);

        let uploaded = state.say(Notice::Info("Uploaded notes.txt.".into()));
        assert_eq!(state.notice_epoch, uploaded);

        let failed = state.say(Notice::Error("could not list /etc".into()));
        assert_ne!(
            uploaded, failed,
            "the timer armed for the first message must not match the second"
        );
        assert_eq!(state.notice_epoch, failed);

        // The failure is what is on screen, so an expiry belonging to the
        // message before it has to be refused rather than clear the line.
        assert!(matches!(state.notice, Some(Notice::Error(_))));
    }

    /// A name the row has room for must not be explained, or every listing
    /// would sprout tooltips nobody asked for.
    #[test]
    fn a_short_name_needs_no_tooltip() {
        assert!(!name_is_clipped(
            DEFAULT_PANEL_WIDTH,
            "notes.txt",
            false,
            None
        ));
        assert!(!name_is_clipped(
            DEFAULT_PANEL_WIDTH,
            "notes.txt",
            true,
            Some("1.5 MB")
        ));
        // The narrowest the panel goes still holds an ordinary name.
        assert!(!name_is_clipped(MIN_PANEL_WIDTH, "app.log", false, None));
    }

    /// The case the tooltip exists for: a name the row has to cut.
    #[test]
    fn a_name_too_long_for_the_row_is_explained() {
        let long = "2026-07-30T12-00-00-application-server.log";
        assert!(name_is_clipped(DEFAULT_PANEL_WIDTH, long, false, None));
        assert!(name_is_clipped(MIN_PANEL_WIDTH, long, false, None));
        // Even at its widest the panel is a sidebar, not a window.
        assert!(name_is_clipped(
            MAX_PANEL_WIDTH,
            &"x".repeat(200),
            false,
            None
        ));
    }

    /// The budget either side of the cut. Named columns rather than characters
    /// because that is what the estimate counts.
    #[test]
    fn the_tooltip_appears_one_column_past_the_budget() {
        // 260 − 2 border − 16 padding − 14 icon − 6 gap = 222px ÷ 7 = 31.
        let budget = 31;
        assert!(!name_is_clipped(
            DEFAULT_PANEL_WIDTH,
            &"a".repeat(budget),
            false,
            None
        ));
        assert!(name_is_clipped(
            DEFAULT_PANEL_WIDTH,
            &"a".repeat(budget + 1),
            false,
            None
        ));
    }

    /// A Hangul name takes two columns per character, so counting `chars` would
    /// let it run to twice the width before anyone called it long — which is
    /// exactly the name most likely to be cut off.
    #[test]
    fn a_wide_name_is_measured_in_columns_not_characters() {
        let hangul = "가".repeat(16);
        let latin = "a".repeat(16);
        assert_eq!(hangul.chars().count(), latin.chars().count());

        assert!(name_is_clipped(DEFAULT_PANEL_WIDTH, &hangul, false, None));
        assert!(!name_is_clipped(DEFAULT_PANEL_WIDTH, &latin, false, None));
    }

    /// Everything drawn beside the name takes room from it, so the same name
    /// can fit on a directory row and not on a symlinked file's.
    #[test]
    fn the_badge_and_the_size_column_shrink_the_budget() {
        let name = "a".repeat(26);

        // A directory: no size column, no badge, so the name has the row.
        assert!(!name_is_clipped(DEFAULT_PANEL_WIDTH, &name, false, None));
        // The same name on a file, once the size column is beside it.
        assert!(name_is_clipped(
            DEFAULT_PANEL_WIDTH,
            &name,
            false,
            Some("1.5 MB")
        ));

        // And the badge takes a little more still: a name that survives the
        // size column alone can lose to the two together.
        let borderline = "a".repeat(23);
        assert!(!name_is_clipped(
            DEFAULT_PANEL_WIDTH,
            &borderline,
            false,
            Some("1.5 MB")
        ));
        assert!(name_is_clipped(
            DEFAULT_PANEL_WIDTH,
            &borderline,
            true,
            Some("1.5 MB")
        ));
    }

    /// A width that leaves no room at all must not divide its way to "it fits".
    #[test]
    fn a_row_with_no_room_clips_anything_but_an_empty_name() {
        assert!(name_is_clipped(0., "a", false, None));
        assert!(!name_is_clipped(0., "", false, None));
        assert!(name_is_clipped(f32::NAN, "a", false, None));
    }

    /// The rule that keeps a delete inside the directory it was asked about.
    /// A link to a directory looks exactly like a directory in the listing —
    /// that is deliberate, so it can be opened — and getting this wrong would
    /// empty the target instead of removing the link.
    #[test]
    fn a_delete_walks_into_real_directories_only() {
        let mut link = entry("shortcut", true);
        link.is_symlink = true;
        assert!(!needs_walking(&link));

        let mut broken = entry("dangling", false);
        broken.is_symlink = true;
        assert!(!needs_walking(&broken));

        assert!(needs_walking(&entry("logs", true)));
        assert!(!needs_walking(&entry("notes.txt", false)));
    }

    #[test]
    fn a_name_that_could_escape_the_destination_is_refused() {
        assert!(is_plain_name("notes.txt"));
        assert!(is_plain_name("a file with spaces"));
        assert!(!is_plain_name(""));
        assert!(!is_plain_name("."));
        assert!(!is_plain_name(".."));
        assert!(!is_plain_name("etc/passwd"));
        assert!(!is_plain_name("..\\windows"));
    }

    #[test]
    fn an_upload_plan_lists_parents_before_children() {
        let root = tempfile::tempdir().expect("creating the local tree must succeed");
        std::fs::create_dir_all(root.path().join("logs/old")).expect("nested dirs must be created");
        std::fs::write(root.path().join("logs/app.log"), b"line\n")
            .expect("a file must be written");
        std::fs::write(root.path().join("logs/old/app.log"), b"older\n")
            .expect("a nested file must be written");

        let plan = plan_upload(vec![root.path().join("logs")], "/srv".to_owned());

        let base = format!("/srv/{}", "logs");
        assert_eq!(plan.directories, [base.clone(), format!("{base}/old")]);
        assert_eq!(plan.files.len(), 2);
        assert_eq!(plan.total, b"line\n".len() as u64 + b"older\n".len() as u64);
        // Every file must land in a directory the plan also creates, or the
        // upload would run `create` inside a directory that is not there yet.
        for file in &plan.files {
            assert!(
                plan.directories.contains(&file.directory),
                "{} has no directory in the plan",
                file.local.display()
            );
        }
    }

    /// The cycle guard: a link back to an ancestor must not be walked, or the
    /// plan would grow until the process ran out of memory.
    #[cfg(unix)]
    #[test]
    fn an_upload_plan_does_not_follow_a_symlinked_directory() {
        let root = tempfile::tempdir().expect("creating the local tree must succeed");
        let tree = root.path().join("tree");
        std::fs::create_dir(&tree).expect("the directory must be created");
        std::fs::write(tree.join("note.txt"), b"hi\n").expect("a file must be written");
        std::os::unix::fs::symlink(&tree, tree.join("loop")).expect("the symlink must be created");
        std::os::unix::fs::symlink(tree.join("note.txt"), tree.join("alias"))
            .expect("the file symlink must be created");

        let plan = plan_upload(vec![tree], "/srv".to_owned());

        assert_eq!(plan.directories, ["/srv/tree"]);
        let mut names: Vec<String> = plan
            .files
            .iter()
            .map(|file| file_name(&file.local).to_string())
            .collect();
        names.sort();
        // The link to a directory is gone; the link to a file is sent as its
        // target, which is why its size counts twice in the total.
        assert_eq!(names, ["alias", "note.txt"]);
        assert_eq!(plan.total, 2 * b"hi\n".len() as u64);
    }

    #[test]
    fn sizes_read_the_way_a_file_manager_writes_them() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }
}
