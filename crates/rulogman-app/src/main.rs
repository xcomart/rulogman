// Rust links Windows binaries with the console subsystem by default, which
// flashes a console window before the GUI appears. Release builds use the GUI
// subsystem instead; debug builds keep the console so that env_logger output
// stays visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! rulogman — a multi-platform GUI SSH terminal.
//!
//! The binary owns the application shell: a tab strip of open [`Session`]s, the
//! terminal surface of the active one, a status bar, and the connection dialog
//! rendered on top of everything else. Session state lives in [`session`], the
//! terminal surface in [`terminal_view`], and every reusable widget in the
//! `rugpui` crate, which rulogman shares with its sibling tools.
//!
//! A tab is not one session but a tree of panes ([`rugpui_shell::pane`]), each
//! showing one session. Most tabs hold a single pane; splitting one is how a
//! tab comes to show several sessions side by side.
//!
//! The window's own frame is `rugpui-shell`'s too — the title bar it draws when
//! the platform will not, the caption buttons, the resize grips, the about and
//! update dialogs, the self-updater and the palette editor — and everything it
//! may not guess at about rulogman is injected in [`main`] before the first
//! window opens: [`IDENTITY`], [`AppStrings`] and [`IgnoredUpdate`].

mod app_settings;
mod connection;
// The colours `rugpui_editor` draws a buffer in, worked out from the session's
// *terminal* colour scheme rather than from the widget layer's own palette —
// the half of the old in-tree editor that is about a terminal and so stayed.
mod editor_palette;
// The pane that mounts the editor widget: one open file, read and written
// through the file panel's own `FileSource`.
mod editor_pane;
mod file_panel;
mod files;
mod i18n;
mod icons;
// Which languages a file may be coloured as: the widget's own table, the
// definitions rulogman ships, and whatever the user has put in `syntaxes`.
mod languages;
// What the launch asked to be opened: a path on the command line, or the
// `file://` URL macOS hands over in place of one.
mod launch;
// The terminal colour schemes, put in front of the shell's palette editor. The
// two catalogues `rugpui-shell` ships are over `rugpui`'s own formats; a scheme is
// Windows Terminal's, and a widget kit has no terminal.
mod scheme_catalog;
mod session;
mod settings_dialog;
// The pane a followed file is read in: a terminal, and a strip above it naming
// the file — see [`tail_view`] for why the name is worth a strip of its own.
mod tail_view;
mod terminal_view;
mod theme_store;
mod update;
mod verifier;
// Windows-only because it shells out to `wsl.exe`, and because the welcome
// screen it feeds only offers a choice of local shells on the platform that
// has one.
#[cfg(windows)]
mod wsl;

// Compiles `locales/*.yml` into the binary and defines the machinery `t!`
// expands to, which is why it has to sit in the crate root. `fallback = "en"`
// is per key, not per locale: a string a translator has not got to yet shows
// in English while the rest of that language stays translated.
rust_i18n::i18n!("locales", fallback = "en");

use std::path::PathBuf;
use std::rc::Rc;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, DragMoveEvent, ElementId, Entity, EntityId,
    FocusHandle, Focusable, Global, KeyBinding, Menu, MenuItem, MouseButton, MouseDownEvent,
    MouseUpEvent, Pixels, Point, QuitMode, ScrollHandle, SharedString, Subscription,
    TitlebarOptions, Window, WindowBounds, WindowControlArea, WindowHandle, WindowOptions, actions,
    div, img, point, prelude::*, px, size,
};
use rulogman_core::{
    Dashboard, DashboardPane, DashboardStore, FilesSettings, LayoutAxis, LayoutNode,
    SessionProfile, TitlebarStyle,
};
use rulogman_ssh::SshAuth;
use rulogman_term::Charset;
use uuid::Uuid;

use connection::{ConnectionDialog, ConnectionDialogEvent};
use editor_pane::{EditorPane, EditorPaneEvent, RootMode, RootPurpose};
use file_panel::{FilePanel, FilePanelEvent, OpenEditor};
use i18n::{input_menu_labels, ts};
use languages::language_label;
use rugpui::{
    Anchor, Button, ButtonVariant, Checkbox, ContextMenu, DraggedThumb, MenuButton, MenuEntry,
    Scrollbar, ScrollbarAxis, ScrollbarState, Splitter, TabBar, TabItem, TextInput, Theme,
    ThemeRegistry, hide_later, hide_now, modal, scroll_to, scrolled, set_theme, theme,
    tooltip_label,
};
use rugpui_shell::pane::{Axis, PaneId, PaneNode, PaneTree, SplitId};
use rugpui_shell::{
    AboutDialog, AboutDialogEvent, AppIdentity, UpdateDialog, UpdateDialogEvent,
    apply_caption_theme, chrome, update as shell_update,
};
use session::{Session, SessionStatus};
// Only a locally started shell carries one of these, and only Windows has more
// than one filesystem such a shell could be standing in.
#[cfg(windows)]
use session::LocalFilesystem;
use settings_dialog::{SettingsDialog, SettingsDialogEvent};
use tail_view::TailView;
use terminal_view::{PaneCaps, PaneCapsSource, PaneFocused, ReconnectRequested, TerminalView};

actions!(
    rulogman,
    [
        /// Quit the application.
        Quit,
        /// Open the connection dialog with an empty form.
        NewSession,
        /// Open a second window, with tabs of its own.
        NewWindow,
        /// Close the active pane, and with it the tab once it was the last one.
        CloseSession,
        /// Move keyboard focus to the next pane of the active tab.
        FocusNextPane,
        /// Move keyboard focus to the previous pane of the active tab.
        FocusPrevPane,
        /// Move the active pane out of its tab and into a tab of its own.
        BreakOutPane,
        /// Move the active tab out of this window and into a window of its own,
        /// sessions and splits intact.
        MoveTabToNewWindow,
        /// Split the active pane, opening a second connection to the same host
        /// in the new pane to its right.
        DuplicateSplitRight,
        /// Split the active pane, opening a second connection to the same host
        /// in the new pane below it.
        DuplicateSplitBelow,
        /// Give every column of the active tab the same width.
        EqualizeWidths,
        /// Give every row of the active tab the same height.
        EqualizeHeights,
        /// Show or hide the remote file panel.
        ToggleFilePanel,
        /// Capture the active dashboard tab's current arrangement back onto the
        /// dashboard it was opened from. A no-op on any other tab.
        SaveDashboardLayout,
        /// Open the settings dialog.
        OpenSettings,
        /// Open the about dialog.
        ShowAbout,
        /// Ask GitHub whether a newer release exists, showing the answer either
        /// way. Unlike the start-up check, this one is not silent and does not
        /// respect the ignored-version tag.
        CheckUpdates,
        /// Close the open dialog or dropdown menu, if there is one.
        DismissDialog,
    ]
);

/// Activate the tab at the zero-based index carried by the action.
#[derive(Clone, PartialEq, Default, Debug, gpui::Action)]
#[action(namespace = rulogman, no_json)]
struct SelectTab(
    /// Zero-based index of the tab to activate.
    usize,
);

/// Open the saved dashboard at the zero-based index carried by the action.
#[derive(Clone, PartialEq, Default, Debug, gpui::Action)]
#[action(namespace = rulogman, no_json)]
struct OpenDashboard(
    /// Zero-based index of the dashboard to open, into the saved order.
    usize,
);

/// Key context the workspace-wide shortcuts are scoped to.
const KEY_CONTEXT: &str = "Workspace";

/// Number of tabs reachable through the `Ctrl`/`Cmd` + digit shortcuts.
const QUICK_SELECT_TABS: usize = 9;

/// Number of dashboards reachable through the numbered shortcuts.
///
/// The same nine [`QUICK_SELECT_TABS`] offers, and deliberately: the two are
/// one gesture at two altitudes — pick the *n*th tab, open the *n*th
/// dashboard — so the count where they stop counting has to be the same.
const QUICK_OPEN_DASHBOARDS: usize = 9;

/// What a release archive holds that has to end up on disk.
///
/// One entry everywhere, because rulogman ships a single file: the executable,
/// or on macOS the application bundle it lives inside. The shell's install plan
/// takes the first entry as the one whose *installed* name may differ from the
/// published one, which is what lets a binary someone renamed still update
/// itself.
#[cfg(windows)]
const PAYLOAD: &[&str] = &["rulogman.exe"];
/// See the Windows variant above.
#[cfg(target_os = "macos")]
const PAYLOAD: &[&str] = &["rulogman.app"];
/// See the Windows variant above.
#[cfg(all(unix, not(target_os = "macos")))]
const PAYLOAD: &[&str] = &["rulogman"];

/// Everything `rugpui-shell` has to be told about rulogman.
///
/// Installed once in [`main`], before the first window and before anything can
/// start an update check. The shell composes none of it — it only reads — which
/// is why every field is a constant of this crate, [`AppIdentity::version`]
/// above all: `rugpui-shell` has a version of its own and it is not this one.
const IDENTITY: AppIdentity = AppIdentity {
    name: "rulogman",
    version: env!("CARGO_PKG_VERSION"),
    repository_url: "https://github.com/xcomart/rulogman",
    repository_label: "github.com/xcomart/rulogman",
    latest_release_api: "https://api.github.com/repos/xcomart/rulogman/releases/latest",
    releases_page: "https://github.com/xcomart/rulogman/releases",
    fallback_archive: "rulogman-update",
    payload: PAYLOAD,
    bundle_executable: "Contents/MacOS/rulogman",
    windows_arp_key: update::ARP_KEY,
    // Whether an install has to leave its renames to the next launch. The
    // question is whether this process holds an open handle on a file the swap
    // is about to rename, which on Windows is what a loaded runtime would do —
    // and rulogman loads none: it is one executable and nothing beside it.
    must_defer: || false,
};

/// The shell's window onto rulogman's translations.
///
/// One line over `t!`, and deliberately no more: the shell looks its words up
/// by the very keys `locales/*.yml` already carries, and the `%{marker}`s come
/// back intact for it to fill in — which is what lets it interpolate an
/// application name into a sentence whose key never mentions one.
struct AppStrings;

impl rugpui_shell::Strings for AppStrings {
    fn text(&self, key: &str) -> SharedString {
        ts!(key)
    }
}

/// The shell's window onto the "never tell me about this version again" tag.
///
/// The tag lives in `settings.json`, which the shell does not own; both halves
/// run on the UI thread, so the settings global is reachable directly. Written
/// through immediately rather than at the next save: this is a decision the
/// user has just made in a dialog, and it should survive a crash the way a
/// saved setting does.
struct IgnoredUpdate;

impl rugpui_shell::UpdatePolicy for IgnoredUpdate {
    fn ignored(&self, cx: &App) -> Option<String> {
        app_settings::current(cx).ignored_update
    }

    fn set_ignored(&self, tag: Option<String>, cx: &mut App) {
        let mut settings = app_settings::current(cx);
        settings.ignored_update = tag;
        if let Err(error) = settings.save() {
            log::warn!("could not record the ignored release: {error:#}");
        }
        app_settings::replace(settings, cx);
    }
}

/// Which title bar style the shell should draw for, given rulogman's own.
///
/// `rulogman-core` is free of gpui and stays that way, so it keeps a
/// [`TitlebarStyle`] of its own rather than re-exporting the shell's; this is
/// the two-line conversion at the boundary.
fn chrome_style(style: TitlebarStyle) -> chrome::TitlebarStyle {
    match style {
        TitlebarStyle::Custom => chrome::TitlebarStyle::Custom,
        TitlebarStyle::System => chrome::TitlebarStyle::System,
    }
}

/// Height of the toolbar row holding the application menu and the tab strip.
///
/// Must match the height [`TabBar`] gives itself, otherwise the menu button cell
/// and the tab strip would not line up.
const TOOLBAR_HEIGHT: f32 = 36.;

/// Distance from the top left of the window to the top left of the macOS
/// traffic lights, in the custom title bar style.
///
/// The buttons are 14 pt tall, so half the difference to [`TOOLBAR_HEIGHT`]
/// centres them in the toolbar band.
const TRAFFIC_LIGHT_ORIGIN: Point<Pixels> = Point {
    x: px(12.),
    y: px(11.),
};

/// Width kept clear at the left of the toolbar for the macOS traffic lights.
///
/// Three 14 pt buttons, 20 pt apart, starting at [`TRAFFIC_LIGHT_ORIGIN`], plus
/// the same margin again after the last one.
const TRAFFIC_LIGHT_GAP: f32 = 78.;

/// Modifier key named in the shortcut hints of the dropdown menu and the empty
/// state.
///
/// Never translated: it is the name printed on the key. It follows
/// [`bind_shortcuts`] on every platform so the two never drift.
const SHORTCUT_MODIFIER: &str = if cfg!(target_os = "macos") {
    "Cmd"
} else {
    "Ctrl"
};

/// Modifier key named in the shortcut hints of the pane commands.
///
/// Not [`SHORTCUT_MODIFIER`]: the pane shortcuts avoid `Ctrl` off macOS so that
/// the remote shell keeps it. Follows `pane_modifier` in [`bind_shortcuts`], and
/// like the other modifier name it is never translated.
const PANE_SHORTCUT_MODIFIER: &str = if cfg!(target_os = "macos") {
    "Cmd"
} else {
    "Alt"
};

/// Chord that shows and hides the remote file panel, as [`bind_shortcuts`]
/// registers it.
///
/// `Cmd+B` on macOS, where the modifier never reaches the shell. Elsewhere the
/// obvious `Ctrl+B` is out: it is tmux's prefix key and readline's
/// *backward-char*, and `Alt+B` — the modifier the pane commands fall back to —
/// is readline's *backward-word*. The shifted chord is free in a way neither of
/// those is, because a terminal cannot encode `Ctrl+Shift+B` distinctly from
/// `Ctrl+B` in the first place: taking it costs the remote shell nothing.
const PANEL_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "cmd-b"
} else {
    "ctrl-shift-b"
};

/// Name of [`PANEL_SHORTCUT`] as the menus print it. Never translated, for the
/// same reason [`SHORTCUT_MODIFIER`] is not.
const PANEL_SHORTCUT_LABEL: &str = if cfg!(target_os = "macos") {
    "Cmd+B"
} else {
    "Ctrl+Shift+B"
};

/// Chord that opens a second window, as [`bind_shortcuts`] registers it.
///
/// `Cmd+N` on macOS, which is what iTerm2, Terminal.app and every other macOS
/// application bind a new window to. Elsewhere `Ctrl+N` belongs to the remote
/// shell — it is readline's *next-history* — so the chord is shifted, which
/// costs the shell nothing: a terminal cannot encode `Ctrl+Shift+N` distinctly
/// from `Ctrl+N` in the first place, so nothing that was reaching the shell
/// stops reaching it.
const WINDOW_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "cmd-n"
} else {
    "ctrl-shift-n"
};

/// Name of [`WINDOW_SHORTCUT`] as the menus print it. Never translated, for the
/// same reason [`SHORTCUT_MODIFIER`] is not.
const WINDOW_SHORTCUT_LABEL: &str = if cfg!(target_os = "macos") {
    "Cmd+N"
} else {
    "Ctrl+Shift+N"
};

/// How far a window opened from the menu steps down and across from the one the
/// command came from, in pixels.
///
/// Enough that the new window's title bar and the one underneath it are both
/// visible, so the two read as two rather than as one that moved.
const WINDOW_CASCADE: f32 = 32.;

/// Style group of the toolbar button that shows and hides the remote file
/// panel, so hovering the button recolours the icon inside it.
const PANEL_TOGGLE_GROUP: &str = "toggle-file-panel";

/// Narrowest pane, in terminal columns, a horizontal split may produce.
///
/// A pane below this is unusable — a shell prompt alone is wider — so a split
/// that would create one is refused instead.
const MIN_PANE_COLS: u16 = 20;

/// Shortest pane, in terminal rows, a vertical split may produce.
const MIN_PANE_ROWS: u16 = 6;

/// Whether a grid of `cols` by `rows` leaves both halves of a split along `axis`
/// a pane worth having.
///
/// The two halves inherit roughly half of the grid each, so the rule is one
/// division against [`MIN_PANE_COLS`] or [`MIN_PANE_ROWS`] — and it is written
/// once, here, because two callers reach it by different routes: the workspace,
/// which reads the size off the pane it is about to split, and a pane rendering
/// its own menu, which can only hand its size over (see
/// [`Workspace::can_split_sized`]). Free of both, so the arithmetic can be
/// tested without either.
const fn split_fits(axis: Axis, cols: u16, rows: u16) -> bool {
    match axis {
        Axis::Horizontal => cols / 2 >= MIN_PANE_COLS,
        Axis::Vertical => rows / 2 >= MIN_PANE_ROWS,
    }
}

/// A surface of the workspace that scrolls, and so wears an overlay bar.
///
/// Two of them, on different axes and never on screen together in the way that
/// matters: the tab strip runs sideways once the tabs outgrow it, the empty
/// state runs down once its buttons outgrow the window. Naming them lets one
/// set of handlers answer for both instead of one set each — the same shape the
/// settings dialog uses for its three surfaces.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Surface {
    /// The tab strip.
    Tabs,
    /// The placeholder shown while no session is open.
    Empty,
}

impl Surface {
    /// Which way the surface scrolls, and so which way its bar lies.
    fn axis(self) -> ScrollbarAxis {
        match self {
            Self::Tabs => ScrollbarAxis::Horizontal,
            Self::Empty => ScrollbarAxis::Vertical,
        }
    }
}

/// Every scrolling surface, with the element id its bar is drawn under.
///
/// The ids live here rather than inside the elements they overlay — [`TabBar`]
/// would be the obvious home for the first — because a drag of a thumb is
/// answered by the workspace, and the id is what tells one bar's drag from any
/// other bar's in the window. Iterating this is how the drag and release paths
/// find which bar an event belongs to.
const SCROLLBARS: [(&str, Surface); 2] = [
    ("tab-scrollbar", Surface::Tabs),
    ("empty-scrollbar", Surface::Empty),
];

/// Element id of the empty state's scrolling box.
const EMPTY_STATE: &str = "empty-state";

/// Room left above and below a column that [`centered_scroll`] is scrolling.
///
/// Only ever seen once there is scrolling to do — while the column fits, the
/// automatic margins dwarf it — and there it is what keeps the first and last
/// buttons off the edges of the body at either end of the travel.
const SCROLL_MARGIN: f32 = 24.;

/// What one pane is showing.
///
/// A tab is still a tab *of sessions* — the strip, the status bar and every
/// shortcut speak for a session — but a pane no longer has to be one. An editor
/// pane belongs to the session it was opened out of without *being* it, which is
/// the whole of the difference the two arms below encode: only a terminal
/// answers [`PaneView::session`], so only a terminal is closed when its session
/// hangs up, counted when the workspace disconnects everything, or offered to
/// the file panel.
enum PaneView {
    /// A shell, over SSH or on this machine. Owns its [`Session`] entity.
    Terminal(Entity<TerminalView>),
    /// A file opened out of the file panel.
    Editor(Entity<EditorPane>),
    /// A remote file being followed, `tail -f` style, over a session of its own.
    ///
    /// A session like any other, which is the whole reason it is not an editor:
    /// it connects, it can drop, it can be reconnected, and it wears a status
    /// dot in the strip — so it answers [`PaneView::session`] exactly as a
    /// terminal does, and every rule written against that answer applies to it
    /// unchanged. What makes it its own arm rather than a terminal is the strip
    /// above the grid; see [`TailView`].
    Tail(Entity<TailView>),
}

impl PaneView {
    /// A second handle on the same surface.
    ///
    /// Not a copy of anything: an [`Entity`] is a handle into the application's
    /// entity map, so what this clones is the reference and not the terminal or
    /// the buffer behind it. That is what lets a pane be taken out of one
    /// window's wiring and put into another's without the surface it draws being
    /// rebuilt — see [`Workspace::adopt_tab`], the only caller.
    ///
    /// Spelled out rather than derived so that the paragraph above has somewhere
    /// to live: a bare `Clone` on this type would read as "duplicate the pane",
    /// which is a different command this application also has.
    fn handle(&self) -> Self {
        match self {
            Self::Terminal(view) => Self::Terminal(view.clone()),
            Self::Editor(pane) => Self::Editor(pane.clone()),
            Self::Tail(view) => Self::Tail(view.clone()),
        }
    }

    /// The entity behind the pane, which is what a focus event names.
    fn entity_id(&self) -> EntityId {
        match self {
            Self::Terminal(view) => view.entity_id(),
            Self::Editor(pane) => pane.entity_id(),
            Self::Tail(view) => view.entity_id(),
        }
    }

    /// Where the keyboard goes when this pane is made active.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::Terminal(view) => view.read(cx).focus_handle(cx),
            Self::Editor(pane) => pane.read(cx).focus_handle(cx),
            // The grid's own, handed on by the strip above it: a followed file
            // is read, selected and copied out of exactly as a shell is.
            Self::Tail(view) => view.read(cx).focus_handle(cx),
        }
    }

    /// The session this pane *is*, if it is one.
    ///
    /// `None` for an editor, which merely came from one. That is what keeps an
    /// open file on screen after the shell it was read from exits: the
    /// disconnect closes the panes showing that session, and this pane is not
    /// one of them.
    fn session(&self, cx: &App) -> Option<Entity<Session>> {
        match self {
            Self::Terminal(view) => Some(view.read(cx).session().clone()),
            Self::Editor(_) => None,
            // A followed file *is* its session, unlike an editor: the tab is
            // named by its title, dotted by its status, closed when it hangs up
            // and offered a reconnect when it fails, all through this answer.
            Self::Tail(view) => Some(view.read(cx).session().clone()),
        }
    }

    /// The session an editor pane was opened out of, if this pane is one.
    ///
    /// The counterpart of [`PaneView::session`] and pointedly not a widening of
    /// it: that one answers "which session *is* this pane", which is what the
    /// tab label, the status bar and the disconnect path all ask, and an editor
    /// has to keep answering `None` there or a tab of open files would report
    /// itself as a connection. This one answers "which filesystem is this file
    /// on", which only the file panel asks.
    fn editor_session(&self, cx: &App) -> Option<Entity<Session>> {
        match self {
            Self::Terminal(_) => None,
            Self::Editor(pane) => Some(pane.read(cx).session().clone()),
            // Nothing to add: this question is asked by the file panel, and a
            // pane that answers [`PaneView::session`] has already answered it.
            Self::Tail(_) => None,
        }
    }

    /// What the tab strip calls this pane when there is no session to name it
    /// after.
    fn label(&self, cx: &App) -> SharedString {
        match self {
            Self::Terminal(view) => view.read(cx).session().read(cx).title(),
            Self::Editor(pane) => {
                let pane = pane.read(cx);
                editor_tab_label(pane.name(), &pane.session().read(cx).title())
            }
            // Its session's title, which is already `file - connection`; see
            // [`Session::title`], which is where a followed file is named.
            Self::Tail(view) => view.read(cx).session().read(cx).title(),
        }
    }

    /// The pane's surface, as an element.
    fn element(&self) -> AnyElement {
        match self {
            Self::Terminal(view) => view.clone().into_any_element(),
            Self::Editor(pane) => pane.clone().into_any_element(),
            Self::Tail(view) => view.clone().into_any_element(),
        }
    }
}

/// Width of the status bar's file-type picker, in pixels.
///
/// Set by the longest thing in it — a language name, which is one word —
/// rather than by the application menus' own width, which is set by a command
/// that names what it acts on and carries a shortcut hint beside it.
const LANGUAGE_MENU_WIDTH: f32 = 180.;

/// The mark on the file-type button, pointing the way its list opens.
const CHEVRON_UP: &str = "\u{25b4}";

/// The caret's place in the file, as the status bar prints it.
///
/// `12/200 : 5` — the line, out of the lines there are, and then the column.
/// Digits and punctuation, with not a word in it, for the same reason the grid
/// size beside it is written `80x24`: a status bar has room for a number and no
/// room for a sentence, and a number needs no translating. The line comes first
/// and carries the total because "where am I in this file" is the question a
/// reader actually has; the column answers a different one and is set off by the
/// colon rather than crowded against it.
///
/// Free and pure so the format is checked without a window; every argument is
/// already one-based when it arrives — see
/// [`EditorView::caret_position`](rugpui_editor::EditorView::caret_position).
fn caret_summary(line: usize, lines: usize, column: usize) -> SharedString {
    SharedString::from(format!("{line}/{lines} : {column}"))
}

/// What the tab strip calls a tab holding one open file.
///
/// The file first and the connection after it, because the strip is read from
/// the left and truncates on the right: what tells two tabs apart is usually the
/// file, and the connection is the qualifier — the same order the file panel's
/// own heading puts them in.
///
/// The connection is passed in rather than remembered when the file was opened,
/// so a shell that retitles itself retitles the files opened out of it too. A
/// session with no title to give — nothing but an empty profile name — leaves
/// the tab called after the file alone rather than trailing a dash with nothing
/// behind it.
///
/// Free rather than a method because it is a sentence, not a lookup: no word of
/// it is translated — a name, a dash and a name — and none of it needs a pane,
/// a session or a window in order to be checked.
fn editor_tab_label(name: &str, connection: &str) -> SharedString {
    if connection.trim().is_empty() {
        SharedString::from(name.to_owned())
    } else {
        SharedString::from(format!("{name} - {connection}"))
    }
}

/// The hover label of a tab's tunnel mark, or `None` for a session holding no
/// forwarding — which is what leaves such a tab unmarked.
///
/// The transport names a rule the way the profile writes it,
/// `8080:db:5432`, which reads as three ports until it is taken apart. The
/// arrow puts the local end and the remote one on either side of the thing
/// that actually happens, so the line says where traffic enters and where it
/// comes out. Anything that is not in that shape — there is no such rule
/// today, but this must not become the place a stranger one goes missing — is
/// shown as it arrived.
///
/// One line however many rules there are, because a tooltip is one line by
/// construction (see [`rugpui::tooltip`]); a host forwarding more ports than fit
/// on one is answered by the connection dialog, which lists them all.
///
/// Free rather than a method for the same reason as [`editor_tab_label`]: it
/// is a sentence about a list of strings, and needs neither a session nor a
/// window to be checked.
fn tunnel_tooltip(tunnels: &[SharedString]) -> Option<SharedString> {
    if tunnels.is_empty() {
        return None;
    }

    let rules = tunnels
        .iter()
        .map(|label| match label.split_once(':') {
            Some((local, remote)) => format!("{local} \u{2192} {remote}"),
            None => label.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(ts!("tab.tip_tunnels", rules = rules))
}

/// Whether any of `sessions` other than `except` was opened from profile
/// `profile` and is holding that profile's forwardings open right now.
///
/// The rule a session about to connect is judged by: one `true` and it leaves
/// the profile's rules alone, because the ports are taken by a tab of its own
/// and asking for them could only fail. `except` is the session that is itself
/// about to (re)start, so that what it holds this instant — and is about to
/// drop on the way to reconnecting — cannot talk it out of taking the ports
/// back.
///
/// Each session arrives already reduced to the three things the rule turns on:
/// the profile it came from (`None` for a local shell, which came from none),
/// which session it is, and whether it is holding anything. Free rather than a
/// method for the reason [`tunnel_tooltip`] is: the rule is a sentence about
/// those three, and asserting it needs neither a window nor a running session.
fn tunnels_held_for(
    profile: Uuid,
    except: Option<EntityId>,
    sessions: impl IntoIterator<Item = (Option<Uuid>, EntityId, bool)>,
) -> bool {
    sessions
        .into_iter()
        .any(|(from, session, holding)| holding && from == Some(profile) && Some(session) != except)
}

/// Whether the tab a freshly opened session is about to be given shows the
/// file panel.
///
/// The two kinds of session are asked in two different places because they have
/// two different things to be asked. A remote session came from a
/// [`SessionProfile`], and whether a host's filesystem is worth a third of the
/// window is a fact about *that host*: the box whose configuration is edited all
/// day earns the panel, the one that is only ever tailed does not. A local shell
/// came from no profile — there is nothing to write the answer on — and every
/// local shell stands on the same one filesystem, so the setting speaks for all
/// of them at once.
///
/// Free rather than a method for the reason [`tunnels_held_for`] is: it is a
/// sentence about a profile and a setting, and asserting it needs neither a
/// window nor a session that connects.
fn panel_opens_with(profile: Option<&SessionProfile>, files: &FilesSettings) -> bool {
    match profile {
        Some(profile) => profile.show_files,
        None => files.local_panel,
    }
}

/// The pane-tree axis a saved [`LayoutAxis`] restores to.
///
/// The two enums are declared to line up one-to-one — `rulogman-core` keeps its
/// own copy so nothing GUI leaks into the config layer, see [`LayoutAxis`] — so
/// this is a rename, written out only because there is no shared type to derive
/// it from. Its inverse is [`layout_axis_of`].
fn layout_axis(axis: LayoutAxis) -> Axis {
    match axis {
        LayoutAxis::Horizontal => Axis::Horizontal,
        LayoutAxis::Vertical => Axis::Vertical,
    }
}

/// The saved [`LayoutAxis`] for a live pane-tree [`Axis`]. The inverse of
/// [`layout_axis`], for capturing an arrangement back to disk.
fn layout_axis_of(axis: Axis) -> LayoutAxis {
    match axis {
        Axis::Horizontal => LayoutAxis::Horizontal,
        Axis::Vertical => LayoutAxis::Vertical,
    }
}

/// One pane: the view showing a session, plus the wiring that keeps the
/// workspace in step with it.
struct PaneLeaf {
    /// The surface this pane draws.
    view: PaneView,
    /// Repaints the workspace when what it draws *about* this pane changes.
    ///
    /// Two different subscriptions behind one field, because the two kinds of
    /// pane have different things worth watching. A terminal's watches its
    /// *session*: the tab strip prints its title and its status dot. An
    /// editor's watches the *pane*, because the status bar prints the caret's
    /// line and the file's language, and both are read off the pane — a caret
    /// move changes nothing the workspace would otherwise be asked to redraw.
    ///
    /// `Option` because it was once terminals only; it is now always `Some`,
    /// and stays an `Option` so that a pane kind with nothing to watch can be
    /// added without threading a dummy subscription through.
    _observer: Option<Subscription>,
    /// Records this pane as the active one when a click focuses its view.
    ///
    /// Driven by [`PaneFocused`] rather than `cx.on_focus`: gpui fires focus
    /// listeners after the frame that carried the click was already drawn, so
    /// a frame-swap driven that way would not show up until the next input
    /// event — the active-pane frame would visibly trail the click.
    _clicked: Subscription,
    /// Backstop for focus arriving by any route other than a click, e.g. a
    /// future programmatic `window.focus`. One frame late by gpui's dispatch
    /// order, which does not matter for paths that repaint anyway.
    _focus: Subscription,
    /// Carries the pane's *Reconnect* button to the workspace that owns the
    /// pane right now.
    ///
    /// Kept beside the three above rather than detached, which is what it used
    /// to be. A detached subscription outlives the leaf and goes on speaking for
    /// the workspace that made it, so a tab moved into another window would
    /// still be reconnecting through the workspace it left — and its button
    /// would go dead the moment that window closed. Held here, it is dropped and
    /// remade with the leaf; see [`Workspace::adopt_tab`].
    ///
    /// `Option` for the reason [`Self::_observer`] is one: only a terminal has a
    /// connection to offer, and an editor pane has nothing to listen for.
    _reconnect: Option<Subscription>,
}

/// One tab: a tree of panes, one of which is active.
struct SessionTab {
    /// The panes of this tab. Never empty — the last pane closes the tab.
    panes: PaneTree<PaneLeaf>,
    /// The pane the tab label, the status bar and the shortcuts act on.
    active_pane: PaneId,
    /// Every pane of this tab in the order the keyboard last visited them, the
    /// most recent last.
    ///
    /// What [`SessionTab::focus_successor`] reads, and the only reason it is
    /// kept: closing the pane you are working in should hand the keyboard back
    /// to the one you came from, not to whichever pane happens to sit next in
    /// layout order. On a tab split three ways those are routinely different
    /// panes, and the layout answer sends the user somewhere they have not
    /// looked at since the split was made.
    ///
    /// Holds ids rather than an index, so a pane closing elsewhere in the tree
    /// cannot silently rename an entry; ids are never reused, so a stale one
    /// reads as gone. Entries are pruned as panes go — see
    /// [`SessionTab::prune_focus_order`] — and reads tolerate a stale one
    /// anyway, because a pane can leave by a path that never came through here.
    focus_order: Vec<PaneId>,
    /// Whether the file panel is showing beside this tab's panes.
    ///
    /// One flag per tab rather than one for the window, because what the panel
    /// browses is per tab already: it follows the active tab's session, so a
    /// window-wide switch meant that opening the panel for the host being
    /// configured also opened it, at the same width, over the tab that was only
    /// tailing a log. Where the flag starts is [`panel_opens_with`]; from then
    /// on it is the tab's own, and the toggle only ever moves the active one's.
    ///
    /// Session state, not persisted: the profile — or the setting, for a local
    /// shell — is what the next session is opened from, and a tab that outlived
    /// the choice is not worth a second place to write it down.
    panel_open: bool,
    /// A name for the tab that outranks whatever its active pane is showing.
    ///
    /// `None` on every tab that was opened as a connection or grown by hand,
    /// and those are right to be named after their active pane: such a tab *is*
    /// whichever pane the user is looking at, and a split whose halves went to
    /// two different hosts would otherwise go on claiming to be the one it
    /// started as.
    ///
    /// A dashboard tab is the other kind of thing. It is a named arrangement
    /// the user made, opened as a whole and closed as a whole, and naming it
    /// after whichever of its panes last held focus would leave the strip
    /// saying `error.log - db-01` for a tab called *Deploy watch* — a label
    /// that changes as the keyboard moves, for a tab that did not.
    label: Option<SharedString>,
    /// The dashboard this tab was opened from, if it is a dashboard tab.
    ///
    /// The write-target for *Save layout to dashboard*: a tab that carries an
    /// id is one whose current arrangement can be captured back onto the stored
    /// [`Dashboard`], and one that does not — a connection or a hand-grown tab —
    /// has no dashboard to save to. `None` on every tab but the ones
    /// [`Workspace::open_dashboard`] opens, which is why it rides alongside
    /// [`Self::label`] and is set the same way.
    dashboard: Option<Uuid>,
}

impl SessionTab {
    /// A tab of a single pane showing `leaf`, with the file panel showing.
    ///
    /// The panel is what every tab used to open with, so it is what a tab whose
    /// caller has nothing better to say still opens with; the callers that do
    /// have something to say follow this with [`SessionTab::with_panel`].
    fn single(leaf: PaneLeaf) -> Self {
        let panes = PaneTree::single(leaf);
        let active_pane = panes.first_leaf().0;
        Self {
            panes,
            active_pane,
            focus_order: vec![active_pane],
            panel_open: true,
            label: None,
            dashboard: None,
        }
    }

    /// The same tab, opening with the file panel showing or not.
    fn with_panel(mut self, open: bool) -> Self {
        self.panel_open = open;
        self
    }

    /// The same tab, carrying a name of its own. See [`SessionTab::label`].
    fn with_label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The same tab, remembering the dashboard it was opened from. See
    /// [`SessionTab::dashboard`].
    fn with_dashboard(mut self, id: Uuid) -> Self {
        self.dashboard = Some(id);
        self
    }

    /// The active pane, falling back to the first one.
    ///
    /// The fallback only matters if [`SessionTab::active_pane`] ever went stale;
    /// a tab always has a pane to speak for it, so this never fails.
    fn active_pane(&self) -> PaneId {
        if self.panes.contains(self.active_pane) {
            self.active_pane
        } else {
            self.panes.first_leaf().0
        }
    }

    /// Marks `pane` as the active one and as the most recently focused.
    ///
    /// Every path that moves the active-pane marker goes through here, so that
    /// the order below is a record of where the keyboard has actually been
    /// rather than of the subset of moves someone remembered to log. The pane
    /// is lifted out of the order before being pushed, so an id appears once
    /// and revisiting a pane moves it to the front rather than stacking up.
    fn focus(&mut self, pane: PaneId) {
        self.active_pane = pane;
        self.focus_order.retain(|id| *id != pane);
        self.focus_order.push(pane);
    }

    /// The pane to hand the keyboard to once `closing` has gone: the most
    /// recently focused pane that is still standing.
    ///
    /// `None` on a tab whose order has nothing else live in it — a pane that
    /// was never focused closing beside one that never was either — and the
    /// caller falls back to layout order, which is what this replaced and is
    /// still the right answer when there is no history to go on.
    ///
    /// `closing` is skipped explicitly rather than relied on to be gone: this is
    /// asked *before* the removal, while the pane is still in the tree, because
    /// the order it is read from is about to be pruned.
    fn focus_successor(&self, closing: PaneId) -> Option<PaneId> {
        self.focus_order
            .iter()
            .rev()
            .copied()
            .find(|id| *id != closing && self.panes.contains(*id))
    }

    /// Drops from the focus order every pane the tree no longer holds.
    ///
    /// Called after a removal. Without it the order grows for the life of the
    /// tab and a long-dead pane could be picked as a successor — `contains`
    /// guards the read as well, so this is about the list not growing without
    /// bound rather than about correctness.
    fn prune_focus_order(&mut self) {
        let Self {
            panes, focus_order, ..
        } = self;
        focus_order.retain(|id| panes.contains(*id));
    }

    /// The view of the active pane.
    fn active_view(&self) -> &PaneView {
        let pane = self.active_pane();
        match self.panes.get(pane) {
            Some(leaf) => &leaf.view,
            None => &self.panes.first_leaf().1.view,
        }
    }

    /// The session this tab speaks for: the active pane's, or the first one it
    /// has if the active pane is an editor.
    ///
    /// The fallback is what keeps the tab label and the status bar describing a
    /// *session* while the keyboard happens to be in a file of a split tab. A
    /// tab with no terminal in it at all — a file opened into a tab of its own,
    /// or one outliving the shell it came from — has no session, and every
    /// caller says so in its own words rather than inventing one. The file panel
    /// is the exception, and asks [`SessionTab::panel_session`] instead.
    fn active_session(&self, cx: &App) -> Option<Entity<Session>> {
        self.active_view().session(cx).or_else(|| {
            self.panes
                .leaves()
                .into_iter()
                .find_map(|(_, leaf)| leaf.view.session(cx))
        })
    }

    /// The session the file panel browses while this tab is active.
    ///
    /// [`SessionTab::active_session`] first, so a tab that has a terminal in it
    /// browses exactly what it always did. Only a tab that has none — an open
    /// file in a tab of its own, which is what "Edit" now makes — falls through
    /// to the session that file was *read from*, which is the one filesystem the
    /// panel could usefully be showing beside it.
    ///
    /// Kept apart from `active_session` rather than folded into it because the
    /// two are asked by different callers for different reasons: the tab label,
    /// the status dot and the tab menu's connection rows all read that one, and
    /// answering them with an editor's origin would dress a tab of files up as a
    /// live connection — offering to reconnect a session the tab is not showing.
    fn panel_session(&self, cx: &App) -> Option<Entity<Session>> {
        self.active_session(cx).or_else(|| {
            self.active_view().editor_session(cx).or_else(|| {
                self.panes
                    .leaves()
                    .into_iter()
                    .find_map(|(_, leaf)| leaf.view.editor_session(cx))
            })
        })
    }

    /// The pane a close aimed at this whole tab has to ask about first.
    ///
    /// Only ever the tab's *only* pane; see [`tab_close_asks`] for why a split
    /// tab is not covered.
    fn unsaved_lone_editor(&self, cx: &App) -> Option<PaneId> {
        let (id, leaf) = self.panes.first_leaf();
        let unsaved = matches!(
            &leaf.view,
            PaneView::Editor(editor) if editor.read(cx).is_dirty(cx)
        );
        tab_close_asks(self.panes.leaf_count(), unsaved).then_some(id)
    }

    /// Whether closing this tab outright would lose edits nobody was asked
    /// about.
    fn holds_unsaved_work(&self, cx: &App) -> bool {
        self.panes.leaves().into_iter().any(|(_, leaf)| {
            matches!(
                &leaf.view,
                PaneView::Editor(editor) if editor.read(cx).is_dirty(cx)
            )
        })
    }

    /// Every session in this tab, one per terminal pane.
    fn sessions(&self, cx: &App) -> Vec<Entity<Session>> {
        self.panes
            .leaves()
            .into_iter()
            .filter_map(|(_, leaf)| leaf.view.session(cx))
            .collect()
    }

    /// Every open file in this tab, one per editor pane.
    ///
    /// The counterpart of [`Self::sessions`] for the other kind of leaf, and
    /// here for the same reason: a setting that changes has to reach the panes
    /// of the background tabs too, and a leaf is the only place a pane is
    /// reachable from.
    fn editors(&self) -> Vec<Entity<EditorPane>> {
        self.panes
            .leaves()
            .into_iter()
            .filter_map(|(_, leaf)| match &leaf.view {
                PaneView::Editor(pane) => Some(pane.clone()),
                PaneView::Terminal(_) | PaneView::Tail(_) => None,
            })
            .collect()
    }

    /// The pane rendering `view`, if any.
    ///
    /// Panes are found by view rather than by id because a focus event only
    /// says which surface was focused, and a pane keeps its view across merges
    /// and break-outs.
    fn pane_of(&self, view: EntityId) -> Option<PaneId> {
        self.panes
            .leaves()
            .into_iter()
            .find(|(_, leaf)| leaf.view.entity_id() == view)
            .map(|(id, _)| id)
    }
}

/// Whether closing a whole tab has to put the unsaved-changes question up
/// first.
///
/// One pane, and that pane an edited file: that is the tab "Edit" opens, and the
/// tab strip's own close button is now the usual way it goes, so the question
/// has to be asked from there as it is from the pane's close button.
///
/// A *split* tab holding an edited file beside a shell is deliberately not
/// covered. The question closes one pane, and this close was aimed at the whole
/// tab, so answering it would leave the tab standing and the command unhonoured;
/// asking once per file would mean a queue of modals. Such a tab can only be
/// made by merging one, and the bulk closes below leave it alone entirely rather
/// than discarding it — see [`Workspace::close_other_tabs`].
const fn tab_close_asks(panes: usize, unsaved_editor: bool) -> bool {
    panes == 1 && unsaved_editor
}

/// Whether a window holding `tabs` tabs may send one of them off into a window
/// of its own.
///
/// Only when it has another to keep. Moving the one tab a window has would carry
/// its contents across and leave an empty window standing where they were, which
/// is a window split into two halves of nothing — every browser refuses the same
/// command on a lone tab for the same reason.
///
/// Free and pure so that the menu rows and the command itself read one rule:
/// [`Workspace::detach_tab`] refuses on exactly this, and the rows offering it
/// grey out on exactly this.
const fn tab_can_move_out(tabs: usize) -> bool {
    tabs > 1
}

/// Where the tab at `index` sits once the tab at `removed` has been taken out.
///
/// `removed` is never `index`: everything after the hole moves down a slot, and
/// a tab that is itself the hole has no slot to move to.
const fn shifted(index: usize, removed: usize) -> usize {
    if removed < index { index - 1 } else { index }
}

/// Where the focus sits once the tab at `removed` has been taken out.
///
/// Every index is numbered for the strip as it stands *before* the removal:
/// `active` is where the focus is, and `survivor` is where it goes if the tab it
/// was on is the one going — the tab the close was aimed from, which is never
/// itself removed and which shifts along with everything else behind the hole.
///
/// Free and pure because the bulk closes both run it in a loop while the strip
/// changes under them, and an off-by-one there is a focus landing on the wrong
/// tab, which no test of the closing itself would catch.
const fn active_after_close(active: usize, removed: usize, survivor: usize) -> usize {
    if active == removed {
        shifted(survivor, removed)
    } else {
        shifted(active, removed)
    }
}

/// The password question an editor pane asked the window to put up for it.
///
/// The pane cannot ask for itself — it is one of several on a screen, with a
/// header two lines high — so it says what it needs through
/// [`EditorPaneEvent::PasswordRequested`] and this holds everything the answer
/// has to be routed back with.
///
/// **The password is not in here.** It lives in the [`TextInput`] while it is
/// being typed and goes straight from there to the source that was asked to
/// validate or use it; nothing on this struct, and nothing on the workspace,
/// keeps a copy. Whether it survives the dialog at all is the source's decision
/// to make and `remember`'s to ask for.
struct SudoPrompt {
    /// The pane waiting on the answer.
    ///
    /// The entity rather than a [`PaneId`], because everything done with the
    /// answer is done to this pane and to no other — and a pane whose tab was
    /// closed while the question stood simply stops being reachable, which its
    /// dropped entity says as well as a lookup would.
    pane: Entity<EditorPane>,
    /// What the password is for, which is what the answer does with it.
    purpose: RootPurpose,
    /// The masked field the password is typed into.
    ///
    /// Built with the question and dropped with it, so that the characters
    /// live no longer than the dialog does. A field kept on the workspace and
    /// reused would be a field still holding a password after the dialog it
    /// belonged to had gone.
    input: Entity<TextInput>,
    /// Whether the source should keep the password for the rest of the session.
    ///
    /// Unchecked by default, deliberately: a password nothing keeps cannot be
    /// found later by anything that goes looking, and the cost of that choice —
    /// this dialog again at the next save — is one the user can see and change.
    remember: bool,
    /// What the last attempt was refused with, if there was one.
    ///
    /// `sudo`'s own sentence, from the remote host, in that host's language:
    /// not translated, because it was not written here. Its presence is what
    /// makes this dialog a retry loop rather than a one-shot — a wrong password
    /// leaves the question up with the reason under the field.
    error: Option<SharedString>,
    /// Whether an attempt is in flight, which is also the lock keeping a second
    /// one from starting.
    busy: bool,
}

/// The root view: tab strip, terminal surface, status bar and dialog.
struct Workspace {
    /// Focus target while no session is open, so the shortcuts stay live.
    focus_handle: FocusHandle,
    /// Open sessions, in tab order.
    tabs: Vec<SessionTab>,
    /// Index of the active tab; meaningless while [`Workspace::tabs`] is empty.
    active: usize,
    /// Horizontal scroll of the tab strip, used to reveal the active tab.
    tab_scroll: ScrollHandle,
    /// Whether the tab strip's overlay scroll indicator is on screen.
    tab_scrollbar: ScrollbarState,
    /// Vertical scroll of the empty state.
    ///
    /// The placeholder is as tall as it has shells and saved profiles to offer,
    /// which on Windows grows with every WSL distribution installed, so it
    /// outgrows a short window rather than the other way round.
    empty_scroll: ScrollHandle,
    /// Whether the empty state's overlay scroll indicator is on screen.
    empty_scrollbar: ScrollbarState,
    /// The connection dialog, rendered only while it reports itself open.
    dialog: Entity<ConnectionDialog>,
    /// The settings dialog, rendered only while it reports itself open.
    settings: Entity<SettingsDialog>,
    /// The about dialog, rendered only while it reports itself open.
    about: Entity<AboutDialog>,
    /// The update dialog, rendered only while it reports itself open.
    ///
    /// Two things open it: the start-up check in [`update`], at most once per
    /// run and only when it found something worth saying, and the "Check for
    /// updates" command, as often as the user asks. It also owns the download
    /// and the swap that "Update" starts, which is why it is the one dialog the
    /// shell cannot always close.
    update: Entity<UpdateDialog>,
    /// The remote file panel, shown to the left of the panes.
    ///
    /// One panel for the whole window rather than one per session: it keeps the
    /// browsing state of every session itself and shows whichever one the active
    /// pane belongs to.
    panel: Entity<FilePanel>,
    /// The saved dashboards, as the welcome screen offers them.
    ///
    /// A copy rather than a read of the file per frame: the welcome screen asks
    /// for the list on every frame it draws, and the answer changes only when
    /// the settings dialog has been applied — which is the one moment this is
    /// re-read. See [`Workspace::reload_dashboards`].
    ///
    /// Held here rather than behind the connection dialog, as the profiles are:
    /// no dialog of this window owns dashboards — the settings dialog edits its
    /// own copy and writes the file — so there is no store to borrow, and a
    /// window that shows them needs one of its own.
    dashboards: DashboardStore,
    /// The editor pane whose close is waiting to be confirmed, if any.
    ///
    /// Held by [`PaneId`] rather than by tab index and pane: ids are never
    /// reused, so a pane that has gone in the meantime — its tab closed from
    /// somewhere else — reads as "not found" and the answer is simply dropped.
    close_confirm: Option<PaneId>,
    /// The password question an editor pane is waiting on, if one is up.
    ///
    /// One at a time, like every other modal in the window: `dialog_open` counts
    /// it, so nothing opens over it, and opening anything else takes it down.
    sudo_prompt: Option<SudoPrompt>,
    /// Whether the application dropdown menu is showing.
    menu_open: bool,
    /// Whether the tab strip's dropdown tab list is showing.
    tab_menu_open: bool,
    /// The tab a right-click opened a context menu for, and where the pointer
    /// was when it did. `None` while no tab menu is showing.
    tab_context: Option<(usize, Point<Pixels>)>,
    /// Where the pointer was when the status bar's file-type picker was opened,
    /// and `None` while it is closed.
    ///
    /// The position rather than a flag because the menu opens at the pointer,
    /// the way every other menu in the window does; what differs is that it
    /// stands on that point and grows upward — see
    /// [`Workspace::render_language_menu`].
    ///
    /// No pane is remembered with it: the menu acts on whatever the active pane
    /// is when a row is picked, and it is dismissed by anything that could
    /// change which pane that is.
    language_menu: Option<Point<Pixels>>,
    /// Where the pointer was when the status bar's character-encoding picker was
    /// opened, and `None` while it is closed.
    ///
    /// Everything [`Workspace::language_menu`] says applies here too — the point
    /// rather than a flag, the upward growth, no pane remembered — with one
    /// thing more: the two are mutually exclusive, since they stand a few pixels
    /// apart on the same bar and a press that opens one lands on the other's
    /// backdrop.
    charset_menu: Option<Point<Pixels>>,
    /// The saved profile a right-click on the empty state opened a context menu
    /// for, and where the pointer was when it did.
    ///
    /// The profile is held by id rather than by its place in the list: the menu
    /// outlives the frame that opened it, and the row it hangs off can have
    /// moved — or gone — by the time a row of the menu is activated, which is
    /// exactly what duplicating and deleting from it do.
    empty_context: Option<(Uuid, Point<Pixels>)>,
    /// The followed file a connection dialog is standing between the user and.
    ///
    /// [`Workspace::open_tail`] connects on the click when the profile's
    /// credentials are already known, and otherwise has to send the user
    /// through the form first — at which point the request itself would be
    /// lost, because what comes back from the dialog is a
    /// [`ConnectionDialogEvent::Connect`] and nothing else: the very same event
    /// that opens a shell. This is the memory of what was actually asked for,
    /// and the profile's id is carried with the path so that a form the user
    /// then pointed at *another* connection cannot open the first one's log.
    ///
    /// Cleared by [`Workspace::close_overlays`], which every other route into
    /// the dialog passes through, and by the dialog's own dismissal — a request
    /// nobody finished is a request nobody made.
    pending_tail: Option<(Uuid, String)>,
    /// Title bar style currently *on the window*.
    ///
    /// Starts as the style the window was created with and is re-set whenever
    /// the setting is applied, in the same breath as the window is told to
    /// switch. Not read from the settings directly: the toolbar has to branch on
    /// what the window actually carries, and only this field follows the
    /// platform call rather than the stored preference.
    titlebar: TitlebarStyle,
    /// WSL distributions the welcome screen offers a shell in.
    ///
    /// Empty until the discovery started in [`Workspace::new`] answers, and
    /// empty for good on a machine without WSL. Found once per run rather than
    /// per frame: it costs a process, and installing a distribution while the
    /// application is open is rare enough that a restart is a fair price.
    #[cfg(windows)]
    wsl_distros: Vec<String>,
    /// Keeps the connection dialog subscription alive.
    _dialog_events: Subscription,
    /// Keeps the settings dialog subscription alive.
    _settings_events: Subscription,
    /// Keeps the about dialog subscription alive.
    _about_events: Subscription,
    /// Keeps the update dialog subscription alive.
    _update_events: Subscription,
    /// Keeps the file panel subscription alive.
    _panel_events: Subscription,
    /// Disconnects every session before the process exits.
    _quit: Subscription,
    /// Redraws the title bar when the desktop moves its caption buttons.
    _button_layout: Subscription,
}

impl Workspace {
    /// Builds an empty workspace and wires up the connection dialog.
    ///
    /// `titlebar` is the style the window was opened with; from then on the
    /// field tracks whatever the applied settings switched the window to.
    fn new(titlebar: TitlebarStyle, window: &Window, cx: &mut Context<Self>) -> Self {
        let dialog = cx.new(ConnectionDialog::new);
        let dialog_events =
            cx.subscribe_in(
                &dialog,
                window,
                |this, dialog, event, window, cx| match event {
                    ConnectionDialogEvent::Connect { profile, auth } => {
                        dialog.update(cx, |dialog, cx| dialog.close(cx));
                        // The dialog says "connect" and nothing more, so what
                        // the connection is *for* has to be remembered on this
                        // side: a form opened by [`Workspace::open_tail`]
                        // finishes that request rather than opening a shell the
                        // user never asked for. The id has to match — the form
                        // can be pointed at another connection while it is up,
                        // and that is a different request, which discards this
                        // one rather than following the wrong host's log.
                        match this.pending_tail.take().filter(|(id, _)| *id == profile.id) {
                            Some((_, path)) => this.open_tail_session(
                                profile.clone(),
                                auth.clone(),
                                path,
                                window,
                                cx,
                            ),
                            None => this.open_session(profile.clone(), auth.clone(), window, cx),
                        }
                    }
                    #[cfg(unix)]
                    ConnectionDialogEvent::ConnectLocal => {
                        dialog.update(cx, |dialog, cx| dialog.close(cx));
                        this.open_local_session(window, cx);
                    }
                    #[cfg(windows)]
                    ConnectionDialogEvent::ConnectLocalShell(shell) => {
                        dialog.update(cx, |dialog, cx| dialog.close(cx));
                        this.open_local_command(
                            shell.name.clone(),
                            shell.command.clone(),
                            shell.filesystem.clone(),
                            window,
                            cx,
                        );
                    }
                    ConnectionDialogEvent::Dismissed => {
                        dialog.update(cx, |dialog, cx| dialog.close(cx));
                        // A followed file the form was opened for is dropped
                        // with the form: the user answered the question by
                        // walking away from it.
                        this.pending_tail = None;
                        this.focus_active(window, cx);
                    }
                },
            );

        let settings = cx.new(SettingsDialog::new);
        let settings_events = cx.subscribe_in(
            &settings,
            window,
            |this, dialog, event, window, cx| match event {
                // The dialog has already replaced and persisted the settings
                // global by the time it emits this; the shell re-applies the
                // parts that touch live windows and sessions.
                SettingsDialogEvent::Applied => {
                    this.apply_settings(window, cx);
                    // The dashboards are edited in that dialog and written by
                    // it, so this is the moment the window's copy of them stops
                    // describing the file. Before the refocus and the redraw,
                    // so the welcome screen's next frame is the new list.
                    this.reload_dashboards();
                    // The settings are one answer for the application, not for
                    // the window they were saved in: every other window has to
                    // come back in the new theme and the new language too.
                    apply_settings_elsewhere(window, cx);
                    // The dialog closes itself after applying; without a refocus
                    // the window focus dangles on its unrendered controls and
                    // macOS disables every menu item validated through it.
                    this.focus_active(window, cx);
                }
                // The same work, minus the refocus: the dialog is still open and
                // the user is still typing in it, so taking the focus back to
                // the terminal here would pull it out from under them.
                SettingsDialogEvent::ThemesChanged => {
                    this.apply_settings(window, cx);
                    apply_settings_elsewhere(window, cx);
                }
                SettingsDialogEvent::Dismissed => {
                    dialog.update(cx, |dialog, cx| dialog.close(cx));
                    this.focus_active(window, cx);
                }
            },
        );

        let about = cx.new(AboutDialog::new);
        let about_events =
            cx.subscribe_in(
                &about,
                window,
                |this, dialog, event, window, cx| match event {
                    AboutDialogEvent::Dismissed => {
                        dialog.update(cx, |dialog, cx| dialog.close(cx));
                        this.focus_active(window, cx);
                    }
                },
            );

        let update = cx.new(UpdateDialog::new);
        let update_events = cx.subscribe_in(&update, window, |this, dialog, event, window, cx| {
            match event {
                UpdateDialogEvent::Ignored { tag } => {
                    // The dialog has already closed itself; writing the file
                    // goes through the policy installed in `main`, because the
                    // application is what owns the settings.
                    shell_update::remember_ignored(tag, cx);
                    this.focus_active(window, cx);
                }
                UpdateDialogEvent::Installed(_) => {
                    // The new build is on disk and the restart is the
                    // application's to perform. The path is named explicitly:
                    // the swap renames the running image aside, so on Linux
                    // gpui's own fallback — `current_exe()` — would follow it
                    // and come back on the *old* build. The shell recorded the
                    // right answer when `rugpui_shell::init` installed the
                    // identity, which is before anything could move it. The
                    // dialog stays on screen: the process is about to go, and
                    // closing it first would flash the window back into view
                    // for a fraction of a second.
                    if let Some(path) = rugpui_shell::restart_path() {
                        cx.set_restart_path(path);
                    }
                    cx.restart();
                }
                UpdateDialogEvent::Dismissed => {
                    dialog.update(cx, |dialog, cx| dialog.close(cx));
                    this.focus_active(window, cx);
                }
            }
        });

        let quit = cx.on_app_quit(|this, cx| {
            for session in this.sessions(cx) {
                session.update(cx, |session, cx| session.disconnect(cx));
            }
            async {}
        });

        // The desktop decides where the caption buttons go, and it can be told
        // to change its mind while the window is open — the settings dialog of
        // GNOME or KDE moves them the moment the choice is made. Nothing else
        // in the window changes when it does, so the layout is read afresh on
        // every frame (see [`Workspace::render_toolbar`]) and this only has to
        // ask for a frame.
        let this = cx.weak_entity();
        let button_layout = window.observe_button_layout_changed(move |_window, cx| {
            this.update(cx, |_, cx| cx.notify()).ok();
        });

        // Read once, here, for the same reason the profile store is read once
        // when the connection dialog is built — and skipped in a test build for
        // the same reason too: `cfg!(test)` compiled into `rulogman-core` is
        // that crate's build, so only this crate can keep a test from reading
        // the config directory of whoever is running it.
        let dashboards = if cfg!(test) {
            DashboardStore::default()
        } else {
            DashboardStore::load().unwrap_or_else(|err| {
                log::warn!("starting with no dashboards: {err:#}");
                DashboardStore::default()
            })
        };

        let panel = cx.new(FilePanel::new);
        // The panel reads the file and decides every refusal itself; what
        // arrives here is a file that can be shown, needing only a pane to show
        // it in — which is the one thing the panel cannot make for itself.
        let panel_events = cx.subscribe_in(
            &panel,
            window,
            |this, _panel, event: &FilePanelEvent, window, cx| {
                let FilePanelEvent::OpenEditor(opened) = event;
                this.open_editor(opened, window, cx);
            },
        );

        // Off the UI thread and off the critical path of the first frame:
        // `wsl.exe` is a process spawn, and the welcome screen has plenty to
        // show without it. The buttons appear underneath the fixed ones when
        // the answer lands, which is well before a user reaches for them.
        //
        // Run once and handed to both places that offer a local shell — the
        // welcome screen from the field, the connection dialog from its own
        // copy — rather than discovered twice for the same answer.
        #[cfg(windows)]
        cx.spawn(async move |this, cx| {
            let distros = cx
                .background_executor()
                .spawn(async { wsl::list_distros() })
                .await;
            this.update(cx, |workspace, cx| {
                workspace.dialog.update(cx, |dialog, cx| {
                    dialog.set_wsl_distros(&distros, cx);
                });
                workspace.wsl_distros = distros;
                cx.notify();
            })
            .ok();
        })
        .detach();

        // The update check, likewise off the UI thread: it is an HTTPS request
        // to GitHub, and nothing on screen waits for it. The tag the user may
        // have ignored is read here, on the UI thread, because the settings
        // global is only reachable from it.
        //
        // The answer opens a dialog, so it deliberately does *not* go through
        // `open_about`'s `close_overlays` route: this is the one dialog nobody
        // asked for, arriving at a moment nobody chose, and it must never take
        // the screen from something the user opened themselves — a half-typed
        // connection form above all. If anything is already up, the check simply
        // says nothing and tries again next launch.
        //
        // The guard is here, in this crate, rather than inside the check:
        // `cfg!(test)` compiled into a dependency is that dependency's build,
        // so `rugpui_shell::update::check` cannot tell a test build of *this*
        // crate from a release one, and every test that opens a window would
        // otherwise make a real request to GitHub.
        //
        // And once per process, not once per window: the check belongs to the
        // launch rather than to a window, so a second window opened from the
        // menu must not ask GitHub again — see [`claim_startup_check`].
        let ignored = shell_update::ignored_release(cx);
        if !cfg!(test) && claim_startup_check(cx) {
            cx.spawn(async move |this, cx| {
                let found = cx
                    .background_executor()
                    .spawn(async move { shell_update::check(ignored.as_deref()) })
                    .await;
                let Some(release) = found else {
                    return;
                };
                this.update(cx, |workspace, cx| {
                    if workspace.dialog_open(cx) {
                        log::debug!("update {} announced while a dialog is open", release.tag);
                        return;
                    }
                    workspace.update.update(cx, |dialog, cx| {
                        dialog.open(release, cx);
                    });
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }

        Self {
            focus_handle: cx.focus_handle(),
            tabs: Vec::new(),
            active: 0,
            tab_scroll: ScrollHandle::new(),
            tab_scrollbar: ScrollbarState::new(),
            empty_scroll: ScrollHandle::new(),
            empty_scrollbar: ScrollbarState::new(),
            dialog,
            settings,
            about,
            update,
            panel,
            dashboards,
            close_confirm: None,
            sudo_prompt: None,
            menu_open: false,
            tab_menu_open: false,
            tab_context: None,
            language_menu: None,
            charset_menu: None,
            empty_context: None,
            pending_tail: None,
            titlebar,
            #[cfg(windows)]
            wsl_distros: Vec::new(),
            _dialog_events: dialog_events,
            _settings_events: settings_events,
            _about_events: about_events,
            _update_events: update_events,
            _panel_events: panel_events,
            _quit: quit,
            _button_layout: button_layout,
        }
    }

    /// Every session the workspace holds, across all tabs and panes.
    fn sessions(&self, cx: &App) -> Vec<Entity<Session>> {
        self.tabs.iter().flat_map(|tab| tab.sessions(cx)).collect()
    }

    /// Every open file the workspace holds, across all tabs and panes.
    fn editors(&self) -> Vec<Entity<EditorPane>> {
        self.tabs.iter().flat_map(SessionTab::editors).collect()
    }

    /// Whether any session other than `except`, opened from profile `id`, is
    /// currently holding port forwardings open.
    ///
    /// Every pane of every tab, not only the active ones: the tab holding the
    /// ports is very often a background one, which is the whole reason the tab
    /// strip marks it.
    ///
    /// And every pane of every *window*, not only this one's. A port is bound
    /// once per machine, so the question was never really about a window; it
    /// only looked that way while there could be one. A tab carried into a
    /// second window takes its forwardings with it, and a session here that
    /// asked only its own window would be told the ports are free, ask the
    /// server for them, and print a bind failure over its fresh screen. `window`
    /// is this window, which answers for itself and is left out of the sweep —
    /// see [`other_workspace_windows`] for why it has to be.
    ///
    /// No liveness test to go with it, because [`Session::open_tunnels`] is
    /// already one — a session that has disconnected, failed or been closed has
    /// dropped the listeners with its transport and reports nothing here. A
    /// non-empty answer therefore means "live, and holding these ports this
    /// instant".
    fn tunnels_held_elsewhere(
        &self,
        id: Uuid,
        except: Option<EntityId>,
        window: &Window,
        cx: &App,
    ) -> bool {
        tunnels_held_for(
            id,
            except,
            self.sessions(cx)
                .into_iter()
                .chain(sessions_in_other_windows(window, cx))
                .map(|entity| {
                    let session = entity.read(cx);
                    (
                        session.profile_id(),
                        entity.entity_id(),
                        !session.open_tunnels().is_empty(),
                    )
                }),
        )
    }

    /// Whether a session starting on `session`'s profile must leave that
    /// profile's forwardings alone.
    ///
    /// The two shapes the question comes in differ only in `except`: a
    /// duplicate passes `None`, since the session it was copied from is exactly
    /// the sibling to stay off, while a reconnect passes its own id. A local
    /// session came from no profile and has no forwardings either way.
    fn tunnels_taken_from(
        &self,
        session: &Entity<Session>,
        except: Option<EntityId>,
        window: &Window,
        cx: &App,
    ) -> bool {
        session
            .read(cx)
            .profile_id()
            .is_some_and(|id| self.tunnels_held_elsewhere(id, except, window, cx))
    }

    /// Opens `session` again, after deciding whether it may take its profile's
    /// forwardings back.
    ///
    /// The one route to [`Session::reconnect`], because that decision has to be
    /// made against the sessions that are live *now*: a tab whose sibling has
    /// since gone picks the forwardings up, and one reconnecting while the
    /// sibling still holds them stays off them and prints no failure notice
    /// over its fresh screen. The sibling may be in another window by now, which
    /// is what `window` is here for — see
    /// [`Workspace::tunnels_held_elsewhere`].
    fn reconnect_session(
        &mut self,
        session: &Entity<Session>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let suppressed = self.tunnels_taken_from(session, Some(session.entity_id()), window, cx);
        session.update(cx, |session, cx| {
            session.set_tunnels_suppressed(suppressed);
            session.reconnect(cx);
        });
    }

    /// Re-applies the current settings to the window and every open session.
    ///
    /// Shared by the two things that can make the settings mean something new:
    /// saving them, and changing a theme or scheme file the settings point at.
    /// Deliberately does *not* move the focus — where the focus belongs after
    /// this depends on whether the dialog closed, which only the caller knows.
    fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = app_settings::current(cx);
        // Before the repaint below, so the next frame is already drawn in the
        // newly chosen language.
        i18n::apply(settings.language.as_deref());
        // The native macOS menu bar is built once and owned by the platform, so
        // unlike the in-app menu it does not follow a repaint; it has to be
        // handed over again.
        cx.set_menus(app_menus());
        apply_ui_theme(&settings.ui_theme, cx);
        // Ahead of the repaint, so the toolbar's next frame already knows
        // whether it has to stand in for a title bar; and ahead of the two
        // calls below, which leave the accent policy and the caption colors on
        // the window, so a caption that comes back here comes back already
        // themed.
        //
        // The field follows the call rather than the stored setting: everything
        // that branches on it is asking what the window carries, not what was
        // last saved.
        if settings.window.titlebar != self.titlebar {
            self.titlebar = settings.window.titlebar;
            let custom = self.titlebar == TitlebarStyle::Custom;
            window.set_titlebar_transparent(custom, custom.then_some(TRAFFIC_LIGHT_ORIGIN));
            // The Linux counterpart of the call above, which only the Windows
            // and macOS backends implement: swap the compositor's frame for
            // client-side decorations (or back) on the live window.
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            window.request_decorations(if custom {
                gpui::WindowDecorations::Client
            } else {
                gpui::WindowDecorations::Server
            });
        }
        cx.refresh_windows();
        // The two halves of a translucent window, in this order: the platform
        // surface is told to permit alpha, and the fills are told how much of
        // it to use.
        window.set_background_appearance(chrome::window_appearance(
            settings.window.background_blur,
            settings.window.background_opacity,
        ));
        app_settings::set_tint(&settings, cx);
        // After the background appearance, never before: on Windows that call
        // re-arms the accent policy that would otherwise repaint the caption
        // out from under us.
        apply_caption_theme(window, &theme(cx), cx);
        // Every pane of every tab, not just the visible one: a background tab's
        // terminal has to come back in the newly chosen scheme too.
        for session in self.sessions(cx) {
            session.update(cx, |session, cx| session.apply_settings(cx));
        }
        // And every open file, for the same reason: whether long lines are
        // broken is one answer for the whole window, and a file left in a
        // background tab has to come back wrapped the way the one on screen is.
        for editor in self.editors() {
            editor.update(cx, |editor, cx| editor.apply_settings(cx));
        }
    }

    /// Opens a session for `profile` and makes its tab active.
    ///
    /// A profile that also names files to follow — [`SessionProfile::tails`] —
    /// gets more than the shell: see [`Workspace::open_session_with_tails`],
    /// which this defers to so that a profile with nothing to follow keeps
    /// taking the plain, single-pane route through [`Workspace::adopt_session`]
    /// unchanged.
    fn open_session(
        &mut self,
        profile: SessionProfile,
        auth: SshAuth,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::info!("opening a session to {}", profile.label());
        // Asked before the session exists, because connecting starts inside the
        // constructor: a tab already holding this profile's ports means the new
        // one must not ask for them.
        let suppressed = self.tunnels_held_elsewhere(profile.id, None, window, cx);
        let panel_open = Self::panel_opens_for(Some(&profile), cx);
        if profile.tails.is_empty() {
            let session = cx.new(|cx| Session::new(profile, auth, suppressed, cx));
            self.adopt_session(session, panel_open, window, cx);
            return;
        }
        self.open_session_with_tails(profile, auth, suppressed, panel_open, window, cx);
    }

    /// [`Workspace::open_session`] for a profile that also names files to
    /// follow: one tab holding the shell *and* one tail pane per rule,
    /// stacked below it in the rules' own order, rather than the tails each
    /// getting a tab of their own the way [`Workspace::open_tail`] opens one
    /// on request.
    ///
    /// Builds every pane itself rather than delegating to
    /// [`Workspace::adopt_session`], because that call is shaped for exactly
    /// one pane and is left alone for the plain sessions — remote and local —
    /// that still want it untouched. The actual arrangement of the panes is
    /// [`Workspace::compose_tailed_tab`], kept separate so it can be tested
    /// without a transport.
    ///
    /// Tunnels are suppressed unconditionally on every tail session, exactly
    /// as [`Workspace::open_tail_session`] suppresses them: a profile's local
    /// ports belong to the one session the user is typing into, not to a pane
    /// that only reads a log alongside it.
    fn open_session_with_tails(
        &mut self,
        profile: SessionProfile,
        auth: SshAuth,
        suppressed: bool,
        panel_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let caps = Self::pane_caps_source(cx);
        let session = cx.new(|cx| Session::new(profile.clone(), auth.clone(), suppressed, cx));
        let view = cx.new(|cx| TerminalView::new(session.clone(), caps.clone(), window, cx));
        let shell_leaf = self.new_pane(view, session, window, cx);

        let mut tail_leaves = Vec::with_capacity(profile.tails.len());
        for rule in &profile.tails {
            let tail_session = cx.new(|cx| {
                Session::new_tail(profile.clone(), auth.clone(), rule.path.clone(), true, cx)
            });
            let terminal =
                cx.new(|cx| TerminalView::new(tail_session.clone(), caps.clone(), window, cx));
            let tail_view = cx.new(|cx| {
                TailView::new(
                    terminal,
                    tail_session.clone(),
                    rule.path.clone(),
                    // Every pane of this tab is on the one host the shell
                    // above them is on, so the name would be the same answer
                    // repeated: the strip carries the path alone.
                    SharedString::default(),
                    cx,
                )
            });
            tail_leaves.push(self.new_tail_pane(tail_view, tail_session, window, cx));
        }

        let tab = Self::compose_tailed_tab(shell_leaf, tail_leaves, panel_open);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Arranges a shell pane and its tail panes into one tab: the shell on
    /// top, the tails stacked below it in the order they are given, all rows
    /// the same height.
    ///
    /// Split out of [`Workspace::open_session_with_tails`] so it can be
    /// exercised on leaves built from dormant sessions — see
    /// `workspace_tests` — rather than only against a real connection: what
    /// this does is arrange leaves that already exist, not decide what they
    /// hold.
    ///
    /// Each split targets the pane the *previous* split returned — the
    /// shell's own id for the first tail — rather than always the shell, so
    /// each new tail lands below the one before it and the stack reads top to
    /// bottom in the rules' own order instead of growing upward from the
    /// shell in reverse. The shell pane is left both the active pane and the
    /// whole of [`SessionTab::focus_order`]: it is what the user asked to
    /// connect to, and a tail pane nobody has looked at yet has nothing to
    /// hand the keyboard back to if it were made a candidate.
    fn compose_tailed_tab(
        shell_leaf: PaneLeaf,
        tail_leaves: Vec<PaneLeaf>,
        panel_open: bool,
    ) -> SessionTab {
        let mut tab = SessionTab::single(shell_leaf).with_panel(panel_open);
        let mut target = tab.panes.first_leaf().0;
        for leaf in tail_leaves {
            match tab.panes.split(target, Axis::Vertical, leaf) {
                Some(pane) => target = pane,
                None => {
                    // `target` is either the shell leaf `SessionTab::single`
                    // just built the tree around, or a pane id this very loop
                    // got back from `split` a moment ago, so this is
                    // unreachable; logged rather than ignored because
                    // reaching it would mean a live tail session has been
                    // dropped on the floor. Same stance as
                    // `duplicate_split`'s identical arm.
                    log::error!(
                        "the pane to split for a tail rule has vanished; the tail session was dropped"
                    );
                }
            }
        }
        tab.panes.equalize(Axis::Vertical);
        tab
    }

    /// Arranges `leaves` into one tab as a balanced grid, filled row by row in
    /// the order they are given.
    ///
    /// Balanced meaning as square as the count allows: `ceil(sqrt(n))` columns
    /// and as many rows as that needs, so eight panes land four-by-two rather
    /// than in a column eight high that gives each log three lines. The last
    /// row is the short one, which is what filling row-major leaves over.
    ///
    /// The tree is built rows first and cells second, and it has to be that
    /// way round: a split is aimed at *a pane*, so once a row has been divided
    /// into cells there is no longer any pane that stands for the whole row to
    /// aim the next row's split at. So the founding pane is split downward
    /// `rows - 1` times — each split aimed at the row above's leading pane, so
    /// the bands come out top to bottom rather than growing upward — and only
    /// then is each band divided rightward, each cell aimed at the one placed
    /// before it. Both passes therefore lay leaves down in exactly the order
    /// they arrived, which is what makes the grid readable as the list the
    /// dashboard was written as.
    ///
    /// Equalising along both axes afterwards is what makes it a grid rather
    /// than a nest of halves: [`PaneTree::equalize`] shares an area out by how
    /// many panes each side spans, so a chain of three stacked bands comes out
    /// in thirds instead of a half and two quarters.
    ///
    /// The first leaf keeps the active pane and the whole of
    /// [`SessionTab::focus_order`], as it does in
    /// [`Workspace::compose_tailed_tab`]: it is the top-left pane, which is
    /// where a reader starts, and no other pane has been looked at yet.
    ///
    /// Associated rather than a method, and taking leaves that already exist,
    /// so the arrangement can be exercised on dormant sessions without a
    /// transport — see `workspace_tests`.
    ///
    /// # Panics
    ///
    /// If `leaves` is empty. A tab has to have a pane, and the one caller
    /// returns before this on a dashboard that resolved to none.
    fn compose_dashboard_tab(leaves: Vec<PaneLeaf>, panel_open: bool) -> SessionTab {
        let count = leaves.len();
        let cols = (count as f64).sqrt().ceil() as usize;

        // The rows, as leaves, before any of them is a pane: chunked by hand
        // rather than with `chunks`, which wants a slice of something `Clone`
        // and a `PaneLeaf` is neither.
        let mut bands: Vec<Vec<PaneLeaf>> = Vec::new();
        for leaf in leaves {
            match bands.last_mut() {
                Some(band) if band.len() < cols => band.push(leaf),
                _ => bands.push(vec![leaf]),
            }
        }

        let mut bands = bands.into_iter();
        let mut first_band = bands
            .next()
            .expect("a dashboard tab is composed from at least one pane");
        let mut tab = SessionTab::single(first_band.remove(0)).with_panel(panel_open);

        // Pass one: the bands. `heads` is the leading pane of each row and
        // `rests` what still has to go beside it, kept in step so that a split
        // that could not be made drops its whole row rather than silently
        // hanging its cells off the row above.
        let mut heads = vec![tab.panes.first_leaf().0];
        let mut rests = vec![first_band];
        for mut band in bands {
            let previous = heads[heads.len() - 1];
            match tab.panes.split(previous, Axis::Vertical, band.remove(0)) {
                Some(pane) => {
                    heads.push(pane);
                    rests.push(band);
                }
                // `previous` is either the pane `SessionTab::single` founded
                // the tree on or one this very loop was handed by `split`, so
                // this cannot happen; logged rather than ignored because
                // reaching it means a row of live tail sessions has been
                // dropped on the floor. Same stance as `compose_tailed_tab`.
                None => log::error!(
                    "the pane to split for a dashboard row has vanished; a row of tail sessions was dropped"
                ),
            }
        }

        // Pass two: the cells of each band, left to right.
        for (head, rest) in heads.into_iter().zip(rests) {
            let mut target = head;
            for leaf in rest {
                match tab.panes.split(target, Axis::Horizontal, leaf) {
                    Some(pane) => target = pane,
                    None => log::error!(
                        "the pane to split for a dashboard cell has vanished; the tail session was dropped"
                    ),
                }
            }
        }

        tab.panes.equalize(Axis::Vertical);
        tab.panes.equalize(Axis::Horizontal);
        tab
    }

    /// Arranges `leaves` into a tab following the saved geometry `layout`,
    /// divider positions and all, rather than the fresh grid
    /// [`Workspace::compose_dashboard_tab`] lays down.
    ///
    /// `leaves` are in [`Dashboard::panes`] order and a [`LayoutNode::Leaf`]
    /// names its pane by index into that same order, so leaf `pane` is
    /// `leaves[pane]`. The caller only reaches here with a `layout` that
    /// [`Dashboard::valid_layout`] has already confirmed is a permutation of
    /// `0..leaves.len()`, which is what lets every leaf be placed exactly once
    /// and read back below without a missing or repeated index.
    ///
    /// A geometry that turns out not to match after all — which should be
    /// impossible past `valid_layout` — is not worth a broken tab: it is logged
    /// and the grid takes over, so a bug here degrades to the arrangement the
    /// user would have got before layouts existed.
    ///
    /// Windowless-testable like the other composers: it builds the tree and
    /// nothing that needs a window.
    fn compose_dashboard_layout(
        leaves: Vec<PaneLeaf>,
        layout: &LayoutNode,
        panel_open: bool,
    ) -> SessionTab {
        /// The index of the leftmost pane of `node` — the one that ends up
        /// top-left of the space `node` fills, and the leaf every enclosing
        /// split shares as its own first child's head.
        fn head(node: &LayoutNode) -> usize {
            let mut node = node;
            loop {
                match node {
                    LayoutNode::Leaf { pane } => return *pane,
                    LayoutNode::Split { first, .. } => node = first,
                }
            }
        }

        /// Whether the leaves of `node` are exactly `0..count`, each once. The
        /// same permutation [`Dashboard::valid_layout`] enforces, re-checked
        /// here against the leaves actually handed over so a build never indexes
        /// out of range or drops a pane on a caller that skipped the check.
        fn covers(node: &LayoutNode, count: usize) -> bool {
            fn walk(node: &LayoutNode, seen: &mut [bool], placed: &mut usize) -> bool {
                match node {
                    LayoutNode::Leaf { pane } => match seen.get_mut(*pane) {
                        Some(slot) if !*slot => {
                            *slot = true;
                            *placed += 1;
                            true
                        }
                        _ => false,
                    },
                    LayoutNode::Split { first, second, .. } => {
                        walk(first, seen, placed) && walk(second, seen, placed)
                    }
                }
            }
            let mut seen = vec![false; count];
            let mut placed = 0;
            walk(node, &mut seen, &mut placed) && placed == count
        }

        /// Grows the placeholder leaf `anchor` into the arrangement `node`.
        ///
        /// [`PaneTree`] can only ever attach an incoming subtree as the *second*
        /// child of a split whose first child is a single existing leaf, so an
        /// arbitrary tree is built by expanding in place: the split is made
        /// while its first child is still the lone `anchor`, then each child is
        /// grown into the leaf it now sits on. The invariant that makes this
        /// consume every pane exactly once is that `anchor` already holds the
        /// leaf `head(node)` on entry — seeded once at the root, and re-seeded
        /// for each split's second child from the pane its right subtree leads
        /// with.
        fn expand(
            panes: &mut PaneTree<PaneLeaf>,
            anchor: PaneId,
            node: &LayoutNode,
            slots: &mut [Option<PaneLeaf>],
        ) -> bool {
            let LayoutNode::Split {
                axis,
                first,
                second,
                ..
            } = node
            else {
                // A leaf: `anchor` was seeded with this pane already, so the
                // arrangement here is complete.
                return true;
            };
            let Some(second_leaf) = slots.get_mut(head(second)).and_then(Option::take) else {
                log::error!("a dashboard layout named a pane out of range or twice");
                return false;
            };
            let axis = layout_axis(*axis);
            let Some(new_id) = panes.split(anchor, axis, second_leaf) else {
                log::error!("the pane to grow a dashboard layout onto has vanished");
                return false;
            };
            // The first child stays on `anchor`, which still holds `head(first)`
            // — the same pane as `head(node)`; the second grows onto the leaf
            // just seeded with `head(second)`.
            expand(panes, anchor, first, slots) && expand(panes, new_id, second, slots)
        }

        /// Pairs each split of `spec` with the live split that was built from it
        /// and records the ratio to restore. The two trees have the same shape
        /// by construction, so the walk stays in lockstep; a divergence that
        /// should be impossible is logged and abandons the ratios rather than
        /// guessing.
        fn ratios(
            spec: &LayoutNode,
            live: &PaneNode<PaneLeaf>,
            out: &mut Vec<(SplitId, f32)>,
        ) -> bool {
            match (spec, live) {
                (LayoutNode::Leaf { .. }, PaneNode::Leaf { .. }) => true,
                (
                    LayoutNode::Split {
                        ratio,
                        first,
                        second,
                        ..
                    },
                    PaneNode::Split {
                        id,
                        first: live_first,
                        second: live_second,
                        ..
                    },
                ) => {
                    out.push((*id, *ratio));
                    ratios(first, live_first, out) && ratios(second, live_second, out)
                }
                _ => {
                    log::error!("a dashboard layout diverged from the tree it built");
                    false
                }
            }
        }

        let count = leaves.len();
        if !covers(layout, count) {
            log::error!(
                "a dashboard layout does not match its panes; falling back to a grid of {count}"
            );
            return Self::compose_dashboard_tab(leaves, panel_open);
        }

        // Consumed by index, since a leaf names its pane by position and the
        // order is not the tree's own.
        let mut slots: Vec<Option<PaneLeaf>> = leaves.into_iter().map(Some).collect();
        // Seed the root with its leftmost pane; `expand` re-seeds each split's
        // second child in turn, so every pane is placed exactly once.
        let Some(root_leaf) = slots.get_mut(head(layout)).and_then(Option::take) else {
            // `covers` just proved the index is in range, so this cannot happen.
            log::error!("a dashboard layout lost its first pane between checks");
            // Nothing left to fall back with — the leaves are half-taken — so
            // rebuild the survivors into a grid rather than panic.
            let survivors: Vec<PaneLeaf> = slots.into_iter().flatten().collect();
            return Self::compose_dashboard_tab(survivors, panel_open);
        };
        let mut tab = SessionTab::single(root_leaf).with_panel(panel_open);
        let anchor = tab.panes.first_leaf().0;
        if !expand(&mut tab.panes, anchor, layout, &mut slots) {
            // Half the leaves are already in the tree, so the grid is no longer
            // an option; the tab keeps the shape built so far, which is still a
            // usable arrangement of the panes that made it in.
            log::error!("a dashboard layout could not be fully built; showing what was arranged");
            return tab;
        }

        // A second pass, because `split` mints its dividers at an even ratio and
        // the ids to move them are only knowable once the tree exists.
        let mut wanted = Vec::new();
        if ratios(layout, tab.panes.root(), &mut wanted) {
            for (id, ratio) in wanted {
                tab.panes.set_ratio(id, ratio);
            }
        }
        tab
    }

    /// [`panel_opens_with`] asked against the settings this run is on.
    ///
    /// The one place the global is read for this, so that the three local
    /// openers and the remote one all reach the same setting the same way.
    fn panel_opens_for(profile: Option<&SessionProfile>, cx: &App) -> bool {
        panel_opens_with(profile, &app_settings::current(cx).files)
    }

    /// Opens a shell on this machine and makes its tab active.
    ///
    /// Takes nothing, because a local session is configured by nothing: the
    /// shell is the user's login shell and everything else comes from the
    /// global terminal settings.
    #[cfg(unix)]
    fn open_local_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = cx.new(Session::new_local);
        log::info!(
            "opening a local session running {}",
            session.read(cx).label()
        );
        let panel_open = Self::panel_opens_for(None, cx);
        self.adopt_session(session, panel_open, window, cx);
    }

    /// Opens a shell running `command` on this machine and makes its tab
    /// active.
    ///
    /// The Windows counterpart of [`Workspace::open_local_session`], which
    /// takes nothing because unix has one local shell to start. Here there are
    /// several, so the caller — a button on the welcome screen — says which:
    /// `label` names it for the tab strip, `command` is the command line that
    /// starts it, and `filesystem` says whether the shell it starts stands on
    /// this machine's own filesystem or in a named WSL distribution's.
    #[cfg(windows)]
    fn open_local_command(
        &mut self,
        label: SharedString,
        command: Vec<String>,
        filesystem: LocalFilesystem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::info!("opening a local session running {}", command.join(" "));
        let session = cx.new(|cx| Session::new_local_command(label, command, filesystem, cx));
        let panel_open = Self::panel_opens_for(None, cx);
        self.adopt_session(session, panel_open, window, cx);
    }

    /// Opens a shell on this machine standing in `dir`, and makes its tab
    /// active.
    ///
    /// The launch path: a directory named on the command line, or one a file
    /// manager's *Open with* handed over. It is deliberately not the same call
    /// as [`Workspace::open_local_session`] with an argument, because the two
    /// platforms disagree about what is missing. On unix nothing is: there is
    /// one login shell and the directory is all the caller had to add. On
    /// Windows there is no single local shell, and a path says nothing about
    /// which one was meant, so this picks the first of the shells this machine
    /// can start — PowerShell, standing on this machine's own filesystem, which
    /// is the only kind of filesystem a path from Explorer or the command line
    /// can be naming. A WSL distribution's shell is never opened this way: its
    /// filesystem is not the one the path was resolved against.
    fn open_local_directory(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        log::info!("opening a local session in {}", dir.display());
        #[cfg(unix)]
        let session = cx.new(|cx| Session::new_local_at(dir, cx));
        #[cfg(windows)]
        let session = {
            // `local_shells` promises the fixed shells first and in a stable
            // order, so the first entry is PowerShell whatever else the machine
            // turns out to have.
            let shell = session::local_shells(&[]).remove(0);
            cx.new(|cx| {
                Session::new_local_command_at(shell.name, shell.command, shell.filesystem, dir, cx)
            })
        };
        let panel_open = Self::panel_opens_for(None, cx);
        self.adopt_session(session, panel_open, window, cx);
    }

    /// Gives a freshly built session a view, a pane and a tab of its own, and
    /// activates that tab.
    ///
    /// Everything past the constructor is identical for a remote and a local
    /// session, which is the whole point of them being one type. `panel_open` is
    /// the one thing that is not, and it arrives already decided — by
    /// [`panel_opens_with`], which the caller asks because only the caller still
    /// has the profile in hand.
    fn adopt_session(
        &mut self,
        session: Entity<Session>,
        panel_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let caps = Self::pane_caps_source(cx);
        let view = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
        let leaf = self.new_pane(view, session, window, cx);

        self.tabs
            .push(SessionTab::single(leaf).with_panel(panel_open));
        self.active = self.tabs.len() - 1;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Wires a freshly created terminal view up as a pane.
    fn new_pane(
        &mut self,
        view: Entity<TerminalView>,
        session: Entity<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneLeaf {
        // Repaints on any session change; on a disconnect it also retires the
        // pane. `observe_in` rather than `observe` because closing a pane moves
        // focus, and focus needs the window.
        let observer = cx.observe_in(&session, window, |this, session, window, cx| {
            if matches!(
                session.read(cx).status(),
                SessionStatus::Disconnected { .. }
            ) {
                this.close_pane_for_session(session.entity_id(), window, cx);
            }
            cx.notify();
        });
        let handle = view.read(cx).focus_handle(cx);
        let id = view.entity_id();
        let clicked = cx.subscribe(&view, |this, view, _: &PaneFocused, cx| {
            this.on_pane_focused(view.entity_id(), cx);
        });
        // Both places that offer a reconnect raise this rather than calling the
        // session, because only the workspace can see what the *other* tabs are
        // forwarding — see [`Workspace::reconnect_session`], which is also why
        // this is `subscribe_in`: the answer now takes in the other windows too,
        // and naming them means naming the one to leave out.
        //
        // Kept rather than detached, unlike every earlier version of this line;
        // [`PaneLeaf::_reconnect`] says why.
        let reconnect = cx.subscribe_in(
            &view,
            window,
            |this, view, _: &ReconnectRequested, window, cx| {
                let session = view.read(cx).session().clone();
                this.reconnect_session(&session, window, cx);
            },
        );
        let focus = cx.on_focus(&handle, window, move |this, _window, cx| {
            this.on_pane_focused(id, cx);
        });

        PaneLeaf {
            view: PaneView::Terminal(view),
            _observer: Some(observer),
            _clicked: clicked,
            _focus: focus,
            _reconnect: Some(reconnect),
        }
    }

    /// Wires a freshly created editor pane up as a pane.
    ///
    /// Nothing here watches the *session*, unlike [`Workspace::new_pane`]: a
    /// session that hangs up takes its terminal with it, but not a file — the
    /// buffer is still open, still editable, and still saveable if the source
    /// can be reached, see [`EditorPane`]. What is watched instead is the pane
    /// itself, because the status bar prints where the caret is and what the
    /// file is being coloured as, and a caret move touches nothing else the
    /// workspace draws.
    fn new_editor_pane(
        &mut self,
        pane: Entity<EditorPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneLeaf {
        let handle = pane.read(cx).focus_handle(cx);
        let id = pane.entity_id();
        let clicked = cx.subscribe_in(
            &pane,
            window,
            move |this, pane, event: &EditorPaneEvent, window, cx| match event {
                EditorPaneEvent::Focused => this.on_pane_focused(pane.entity_id(), cx),
                EditorPaneEvent::CloseRequested => this.close_editor_pane(pane, window, cx),
                EditorPaneEvent::SavedForClose => this.close_saved_editor_pane(pane, window, cx),
                EditorPaneEvent::PasswordRequested(purpose) => {
                    this.ask_sudo_password(pane.clone(), *purpose, window, cx);
                }
            },
        );
        let focus = cx.on_focus(&handle, window, move |this, _window, cx| {
            this.on_pane_focused(id, cx);
        });
        let observer = cx.observe(&pane, |_this, _pane, cx| cx.notify());

        PaneLeaf {
            view: PaneView::Editor(pane),
            _observer: Some(observer),
            _clicked: clicked,
            _focus: focus,
            // A file has no connection to offer, so there is no *Reconnect*
            // button on it to carry anywhere.
            _reconnect: None,
        }
    }

    /// Wires a freshly created tail pane up as a pane.
    ///
    /// [`Workspace::new_pane`] with one entity swapped, and deliberately no
    /// more than that: a followed file is a connection, so it wants every rule
    /// a terminal pane gets — the pane retires when the session hangs up, the
    /// strip repaints on every change to it, and the *Reconnect* button on its
    /// overlay reaches the workspace that can say whether the profile's
    /// forwardings are free. The events are the grid's own, re-emitted by
    /// [`TailView`] under the entity the workspace knows the pane by.
    fn new_tail_pane(
        &mut self,
        view: Entity<TailView>,
        session: Entity<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneLeaf {
        let observer = cx.observe_in(&session, window, |this, session, window, cx| {
            if matches!(
                session.read(cx).status(),
                SessionStatus::Disconnected { .. }
            ) {
                this.close_pane_for_session(session.entity_id(), window, cx);
            }
            cx.notify();
        });
        let handle = view.read(cx).focus_handle(cx);
        let id = view.entity_id();
        let clicked = cx.subscribe(&view, |this, view, _: &PaneFocused, cx| {
            this.on_pane_focused(view.entity_id(), cx);
        });
        let reconnect = cx.subscribe_in(
            &view,
            window,
            |this, view, _: &ReconnectRequested, window, cx| {
                let session = view.read(cx).session().clone();
                this.reconnect_session(&session, window, cx);
            },
        );
        let focus = cx.on_focus(&handle, window, move |this, _window, cx| {
            this.on_pane_focused(id, cx);
        });

        PaneLeaf {
            view: PaneView::Tail(view),
            _observer: Some(observer),
            _clicked: clicked,
            _focus: focus,
            _reconnect: Some(reconnect),
        }
    }

    /// Records the pane rendering `view` as the active one of its tab.
    ///
    /// This is what makes a click inside a pane — [`TerminalView`] focuses
    /// itself on mouse down — move the active-pane marker, the status bar and
    /// the tab label onto that pane.
    fn on_pane_focused(&mut self, view: EntityId, cx: &mut Context<Self>) {
        for tab in &mut self.tabs {
            let Some(pane) = tab.pane_of(view) else {
                continue;
            };
            if tab.active_pane != pane {
                tab.focus(pane);
                // The file-type and encoding pickers name the pane they were
                // opened over, and act on whichever pane is active when a row is
                // picked. Once those are two different panes they are asking
                // about one file and answering about another, so they go. A
                // press elsewhere in the window is caught by the menu's own
                // backdrop; this is for the keyboard, which moves the focus
                // without one.
                self.language_menu = None;
                self.charset_menu = None;
                cx.notify();
            }
            return;
        }
    }

    /// Activates the tab at `index`, if it exists.
    ///
    /// Selecting the tab that is already active is not a no-op: it scrolls the
    /// strip back to it, which is the point of picking it from the tab list.
    fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        // See [`Workspace::on_pane_focused`]: a picker opened over one file must
        // not be answered against another. The shortcuts reach here without a
        // press for the menu's backdrop to catch.
        self.language_menu = None;
        self.charset_menu = None;
        self.active = index;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Takes the tab at `index` out of the strip and hangs up everything in it.
    ///
    /// The half of closing a tab that is the same however many tabs are going.
    /// It deliberately leaves [`Workspace::active`], the strip scroll and the
    /// focus alone: which tab should be active afterwards depends on how many
    /// more are still about to be removed, so only the caller can decide it.
    ///
    /// `index` must be in range; every caller has already checked it.
    fn retire_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let tab = self.tabs.remove(index);
        for session in tab.sessions(cx) {
            self.forget_panel_session(session.entity_id(), cx);
            session.update(cx, |session, cx| session.disconnect(cx));
        }
    }

    /// Disconnects and removes the tab at `index`, panes and all — asking first
    /// if the tab is one file with unsaved changes in it.
    ///
    /// This is the tab strip's close button and the tab menu's close row: a tab
    /// that was split closes as a unit. Closing one pane at a time is
    /// [`Workspace::close_active_pane`].
    ///
    /// The question is [`tab_close_asks`]'s to decide. Answering it comes back
    /// through [`Workspace::confirm_close_editor`] and lands in
    /// [`Workspace::remove_pane`], which takes the last pane of a tab down by
    /// calling [`Workspace::close_tab_now`] — the unguarded half of this — so the
    /// question is asked once rather than again by the close it authorised.
    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if let Some(pane) = tab.unsaved_lone_editor(cx) {
            self.ask_before_closing(pane, window, cx);
            return;
        }
        self.close_tab_now(index, window, cx);
    }

    /// Disconnects and removes the tab at `index` without asking about anything.
    ///
    /// Every caller has already settled whatever question the tab raised, or
    /// there was none to raise; see [`Workspace::close_tab`].
    fn close_tab_now(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }

        self.retire_tab(index, cx);

        // Removing a tab in front of the active one shifts it down a slot.
        if index < self.active {
            self.active -= 1;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Closes every tab except the one at `index` — and except any tab holding
    /// unsaved edits.
    ///
    /// A bulk close asks nothing, because there is no honest way to ask: a
    /// command aimed at a dozen tabs would have to put a dozen questions up in
    /// turn, and a user working through them has no way back to the one they
    /// already answered. So a tab with an edited file in it is simply left
    /// standing — the command still empties the strip of everything that had
    /// nothing to lose, and what is left is exactly what would have been lost.
    /// Closing one of those tabs afterwards asks about it the ordinary way.
    ///
    /// The focus ends on the tab the close was aimed from unless it was already
    /// on one of the survivors, in which case it stays where it is.
    fn close_other_tabs(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }

        // Both indices follow the strip as it shrinks: `kept` is the tab this
        // was aimed from, which never goes, and `active` is where the focus is.
        let mut kept = index;
        let mut active = self.active;
        // Back to front, so that removing a tab never moves one that is still
        // to be visited.
        for other in (0..self.tabs.len()).rev() {
            if other == kept || self.tabs[other].holds_unsaved_work(cx) {
                continue;
            }
            self.retire_tab(other, cx);
            active = active_after_close(active, other, kept);
            kept = shifted(kept, other);
        }

        self.active = active;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Closes every tab after the one at `index`, bar any holding unsaved edits.
    ///
    /// A tab with an edited file in it is left standing, for the reason
    /// [`Workspace::close_other_tabs`] gives.
    ///
    /// A tab in front of the clicked one keeps the focus if it had it — nothing
    /// it was showing has gone anywhere. Only an active tab that was itself
    /// closed hands the focus over, and it hands it to the clicked tab, which is
    /// the nearest one still standing.
    fn close_tabs_right(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index + 1 >= self.tabs.len() {
            return;
        }

        // The clicked tab cannot move — everything closing sits behind it — so
        // it is the survivor at the same index throughout.
        let mut active = self.active;
        for other in (index + 1..self.tabs.len()).rev() {
            if self.tabs[other].holds_unsaved_work(cx) {
                continue;
            }
            self.retire_tab(other, cx);
            active = active_after_close(active, other, index);
        }

        self.active = active;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Opens a second connection to the target of the tab at `index`, in a tab
    /// of its own right after it.
    ///
    /// The tab-sized counterpart of [`Workspace::duplicate_split`], and it takes
    /// the credentials the same way — through [`Session::duplicate`], the only
    /// place that can read them. What differs is where the new session lands and
    /// therefore what can refuse it: a split has to fit beside the pane it comes
    /// from, while a new tab is given the whole body and can always be had.
    ///
    /// Which pane of a split source tab is duplicated is the one its label
    /// already names — the active one — so the tab that appears is a second
    /// connection to whatever the strip said the tab was.
    fn duplicate_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };

        // The tab may be showing nothing but open files by now, its shell
        // having exited; there is then no target to open a second connection to.
        let Some(session) = tab.active_session(cx) else {
            return;
        };
        // The second connection is to the same host as the first, so it opens
        // looking the way the first one looks now: the profile has already had
        // its say, and the tab being duplicated may have moved on from it.
        let panel_open = tab.panel_open;
        log::info!("opening a second session to {}", session.read(cx).title());

        // `None`, not this session: a second connection to a profile whose
        // ports *this* tab is holding is precisely the case to stay off them.
        let suppressed = self.tunnels_taken_from(&session, None, window, cx);
        let session = session.update(cx, |session, cx| session.duplicate(suppressed, cx));
        let caps = Self::pane_caps_source(cx);
        let view = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
        // A duplicate of a followed file follows that same file — see
        // [`Session::duplicate`] — so it belongs in the pane a followed file
        // belongs in, strip and all, rather than in a bare grid that could not
        // say which file it was showing.
        let leaf = match session.read(cx).tail_path().map(str::to_owned) {
            Some(path) => {
                // The duplicate opens in a tab of its own, so — like every
                // other pane that is the only one in its tab — it needs no
                // name to be told apart by.
                let tail = cx.new(|cx| {
                    TailView::new(view, session.clone(), path, SharedString::default(), cx)
                });
                self.new_tail_pane(tail, session, window, cx)
            }
            None => self.new_pane(view, session, window, cx),
        };

        let at = index + 1;
        self.tabs
            .insert(at, SessionTab::single(leaf).with_panel(panel_open));
        self.active = at;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Disconnects and removes the active pane of the active tab.
    ///
    /// The pane's sibling grows into the space it leaves. On the last pane of a
    /// tab there is no sibling to grow, so the tab goes with it.
    ///
    /// An editor pane with unsaved changes asks before it goes, whichever way it
    /// was closed — the shortcut here, or the pane's own close button.
    fn close_active_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let pane = tab.active_pane();
        let unsaved = matches!(
            tab.panes.get(pane).map(|leaf| &leaf.view),
            Some(PaneView::Editor(editor)) if editor.read(cx).is_dirty(cx)
        );
        if unsaved {
            self.ask_before_closing(pane, window, cx);
            return;
        }
        self.remove_pane(self.active, pane, window, cx);
    }

    /// Retires the pane of a session whose connection has ended.
    ///
    /// This is the automatic arm of the close policy, driven by the session
    /// observer in [`Self::new_pane`]:
    ///
    /// * `Disconnected` — the remote shell exited or the server hung up — the
    ///   pane closes by itself; its sibling grows, and the tab goes once its
    ///   last pane does. When the last tab goes, the workspace shows the start
    ///   screen again rather than quitting.
    /// * `Failed` never lands here: a session that could not connect keeps its
    ///   pane, so the error and its Reconnect button stay readable.
    ///
    /// A session that is no longer in any tab — the manual close paths remove
    /// the pane *before* disconnecting it — is a no-op, which is also what
    /// makes the observer re-entrancy safe.
    fn close_pane_for_session(
        &mut self,
        session: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let found = self.tabs.iter().enumerate().find_map(|(index, tab)| {
            tab.panes.leaves().into_iter().find_map(|(pane, leaf)| {
                let shown = leaf.view.session(cx)?;
                (shown.entity_id() == session).then_some((index, pane))
            })
        });
        let Some((index, pane)) = found else {
            return;
        };
        self.remove_pane(index, pane, window, cx);
    }

    /// Disconnects and removes one pane of the tab at `index`.
    ///
    /// The pane's sibling grows into the space it leaves. On the last pane of a
    /// tab there is no sibling to grow, so the tab goes with it. Focus only
    /// moves when the removed pane sat in the active tab; a background tab
    /// shrinking must not steal the keyboard.
    fn remove_pane(
        &mut self,
        index: usize,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.panes.leaf_count() < 2 {
            // Unguarded: whatever the pane going had to be asked about was asked
            // before this was called — by the close question, or by the save it
            // ended in — and the tab is that pane.
            self.close_tab_now(index, window, cx);
            return;
        }

        // Read before the removal, while the pane being closed is still in the
        // tree and the order still names it: the pane the keyboard was in
        // before this one, and layout order only if there is no such pane.
        let successor = tab
            .focus_successor(pane)
            .or_else(|| tab.panes.next_leaf(pane));

        let tab = &mut self.tabs[index];
        let Some(leaf) = tab.panes.remove(pane) else {
            return;
        };
        tab.prune_focus_order();
        // The removed pane may not have been the active one — an idle split
        // closing in the background — in which case the active pane stands.
        if !tab.panes.contains(tab.active_pane) {
            let next = successor
                .filter(|id| tab.panes.contains(*id))
                .unwrap_or_else(|| tab.panes.first_leaf().0);
            tab.focus(next);
        }

        // Dropping the leaf takes its subscriptions and its view with it, so the
        // session has to be told to hang up first. Hanging up twice — the
        // automatic path arrives here already disconnected — is a no-op. An
        // editor pane owns no session and so hangs nothing up; the session it
        // was opened out of is still being shown by the terminal pane beside it,
        // or has already gone.
        if let Some(session) = leaf.view.session(cx) {
            self.forget_panel_session(session.entity_id(), cx);
            session.update(cx, |session, cx| session.disconnect(cx));
        }

        if index == self.active {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    /// Turns the tab at `source` into a split of the active tab.
    ///
    /// The source tab leaves the strip and its panes — the whole subtree, if it
    /// was itself split — appear next to the active pane, along `axis`. Focus
    /// follows the panes that moved.
    ///
    /// Splitting is always "merge another open tab in", so it needs a target the
    /// user picks: [`Workspace::render_tab_context`] is the only way in, and
    /// there is no shortcut for it.
    pub(crate) fn merge_tab_into_active(
        &mut self,
        source: usize,
        axis: Axis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if source >= self.tabs.len() || source == self.active {
            return;
        }
        if !self.can_split_active(axis, cx) {
            // Only reachable from a stale menu: the rows offering a split are
            // left out while the pane is this small.
            log::info!("refusing to merge tab {source}: the active pane is too small to split");
            return;
        }

        let target_pane = self.tabs[self.active].active_pane();
        let incoming = self.tabs.remove(source);
        // Removing a tab in front of the active one shifts it down a slot.
        if source < self.active {
            self.active -= 1;
        }

        let follow = incoming.active_pane();
        // Taken apart rather than read field by field so the panes can be moved
        // into the merge while the focus order they came with is kept.
        let SessionTab {
            panes: arriving,
            focus_order: history,
            ..
        } = incoming;
        let tab = &mut self.tabs[self.active];
        if !tab.panes.merge_subtree(target_pane, axis, arriving) {
            // `target_pane` came from this very tab a moment ago, so this is
            // unreachable; logged rather than ignored because reaching it would
            // mean a pane has been dropped on the floor.
            log::error!("the pane to split has vanished; the merge was dropped");
            return;
        }
        // The arriving panes keep the order the keyboard visited them in, and
        // land on top of this tab's: they are what the user is looking at now,
        // so closing one of them steps back through the tab it came from before
        // reaching the tab it was merged into.
        tab.focus_order.extend(history);
        tab.focus(follow);

        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Splits the active pane along `axis` and opens a second connection to the
    /// same host in the new half.
    ///
    /// The other half of splitting, and the one that needs no target: everything
    /// it has to know — the profile and the credentials — is already in the pane
    /// the user is looking at, which is why this one *can* have a shortcut where
    /// [`Workspace::merge_tab_into_active`] cannot.
    ///
    /// The new session is independent from the moment it is created: its own
    /// transport, its own shell, its own scrollback. Nothing about the state of
    /// the original matters, so a pane whose connection failed can still be
    /// split — that is how the user retries without losing the error on screen.
    pub(crate) fn duplicate_split(
        &mut self,
        axis: Axis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if !self.can_split_active(axis, cx) {
            // Reachable from the keyboard at any size, unlike the menu rows,
            // which are left out while the pane is this small.
            log::info!("refusing to split: the active pane is too small");
            return;
        }

        let target_pane = tab.active_pane();
        // `can_split_active` already refused an editor pane above, so the active
        // pane is a terminal and this is its session.
        let Some(session) = tab.active_session(cx) else {
            return;
        };
        log::info!("opening a second session to {}", session.read(cx).title());

        // As in [`Workspace::duplicate_tab`]: the pane being split is itself a
        // sibling, so its forwardings are a reason for the new pane to stay off
        // the ports rather than an exception to it.
        let suppressed = self.tunnels_taken_from(&session, None, window, cx);
        let session = session.update(cx, |session, cx| session.duplicate(suppressed, cx));
        let caps = Self::pane_caps_source(cx);
        let view = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
        let leaf = self.new_pane(view, session, window, cx);

        let tab = &mut self.tabs[self.active];
        let Some(pane) = tab.panes.split(target_pane, axis, leaf) else {
            // `target_pane` came out of this very tab a moment ago, so this is
            // unreachable; logged rather than ignored because reaching it would
            // mean a live session has been dropped on the floor.
            log::error!("the pane to split has vanished; the new session was dropped");
            return;
        };
        tab.focus(pane);

        self.focus_active(window, cx);
        cx.notify();
    }

    /// Moves the active pane into a tab of its own, right after the current one.
    ///
    /// The session keeps running throughout: the pane, its view and its
    /// subscriptions move over unchanged. A no-op on an unsplit tab, which is
    /// already exactly this.
    pub(crate) fn break_out_active_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.panes.leaf_count() < 2 {
            return;
        }

        let pane = tab.active_pane();
        // As in [`Workspace::remove_pane`]: the pane the keyboard was in before
        // this one, read while the order still holds both of them.
        let successor = tab
            .focus_successor(pane)
            .or_else(|| tab.panes.next_leaf(pane));

        let tab = &mut self.tabs[self.active];
        let Some(leaf) = tab.panes.remove(pane) else {
            return;
        };
        tab.prune_focus_order();
        let next = successor
            .filter(|id| tab.panes.contains(*id))
            .unwrap_or_else(|| tab.panes.first_leaf().0);
        tab.focus(next);
        // Nothing about the pane changed on the way out, and neither does what
        // stands beside it.
        let panel_open = tab.panel_open;

        let index = self.active + 1;
        self.tabs
            .insert(index, SessionTab::single(leaf).with_panel(panel_open));
        self.active = index;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Moves the tab at `index` into a window of its own.
    ///
    /// The counterpart of [`Workspace::break_out_active_pane`] one size up: a
    /// pane leaves its tab there, a tab leaves its window here, and neither
    /// disturbs what is on the other end. The sessions keep running throughout —
    /// nothing is disconnected, nothing is reconnected, no scrollback is lost —
    /// because a session is an entity of the *application* and only its wiring
    /// belongs to a window. Taking the tab out and putting it back are
    /// [`Workspace::detach_tab`] and [`Workspace::adopt_tab`]; this is the pair
    /// of them with a window opened in between.
    ///
    /// A refusal costs nothing: the tab is only taken out once the move can
    /// still be undone, and a window that fails to open puts it straight back
    /// where it was rather than dropping live sessions on the floor.
    fn move_tab_to_new_window(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Read before the window opens and directly, without the `cx.defer`
        // [`NewWindow`] needs: that one is a global handler with no window in
        // hand, and asking gpui for the front window mid-dispatch finds it
        // lifted off the map. Here the window is the argument.
        let bounds = cascaded(window.bounds());
        let Some(tab) = self.detach_tab(index, window, cx) else {
            return;
        };

        let opened = match open_workspace_window_at(bounds, cx) {
            Ok(handle) => handle,
            Err(error) => {
                log::warn!("could not open a window for the tab: {error:#}");
                // Back into the strip it just left, wired up afresh to the
                // window it never left. The tab holds live sessions and the user
                // asked for it to be *moved*, so the one thing this must not do
                // is let it fall.
                self.adopt_tab(tab, window, cx);
                return;
            }
        };

        let moved = opened.update(cx, |workspace, window, cx| {
            workspace.adopt_tab(tab, window, cx);
            // The tab is what the user is now looking at, so the window holding
            // it comes forward. gpui gives a freshly opened window the focus on
            // most platforms and not on all of them; asking is what makes the
            // two agree.
            window.activate_window();
        });
        if let Err(error) = moved {
            log::error!("the window opened for the tab went away with it: {error}");
        }
    }

    /// Takes the tab at `index` out of the strip without hanging anything up,
    /// and hands it to the caller.
    ///
    /// [`Workspace::retire_tab`] minus the disconnect and plus the tidying up
    /// that [`Workspace::close_tab_now`] does, because from this window's side a
    /// tab that has left is a tab that has gone whichever way it went: the
    /// active tab has to be corrected, the strip scrolled and the focus put
    /// somewhere that still exists.
    ///
    /// The file panel forgets the sessions that are leaving. The panel is one
    /// per window and its browsing state — the directory each session was left
    /// in, and what was expanded there — belongs to the window, not to the tab,
    /// so it cannot travel. The tab arrives in its new window browsing the
    /// session's home directory again, which is a small cost and the honest one.
    ///
    /// `None` when there is nothing to hand over: an index off the end, or
    /// [`tab_can_move_out`] refusing the only tab this window has.
    fn detach_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<SessionTab> {
        if index >= self.tabs.len() || !tab_can_move_out(self.tabs.len()) {
            return None;
        }

        let tab = self.tabs.remove(index);
        for session in tab.sessions(cx) {
            self.forget_panel_session(session.entity_id(), cx);
        }
        // See [`Workspace::select_tab`]: a picker opened over one file must not
        // be answered against another, and the tab under it has just gone.
        self.language_menu = None;
        self.charset_menu = None;

        // Removing a tab in front of the active one shifts it down a slot.
        if index < self.active {
            self.active -= 1;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();

        Some(tab)
    }

    /// Puts a tab detached from some window — possibly this one — into this
    /// window's strip, and makes it active.
    ///
    /// Every pane is wired up again from scratch. The views are not rebuilt and
    /// the sessions are not touched: what is remade is the *wiring*, all of
    /// which named the window the tab came from. The workspace subscriptions
    /// hanging off each leaf were made in that window and pointed at that
    /// workspace, and a terminal view carries three more of its own — see
    /// [`TerminalView::rebind`]. Overwriting the leaf is what unsubscribes the
    /// old ones, a [`Subscription`] being what it is.
    ///
    /// The tab keeps its shape: the same split tree, the same active pane, the
    /// same file panel state. It lands at the end of the strip rather than
    /// beside the active tab, because it did not come from beside it — a tab
    /// arriving from elsewhere has no neighbour here to be put back next to.
    fn adopt_tab(&mut self, mut tab: SessionTab, window: &mut Window, cx: &mut Context<Self>) {
        // One source for every leaf: it captures this workspace and nothing
        // about the pane, and it is an `Rc`, so a clone per pane costs a count.
        let caps = Self::pane_caps_source(cx);
        for id in tab.panes.leaf_ids() {
            // The handle comes out before anything is built with it, so the leaf
            // it came from is no longer borrowed when its replacement goes into
            // the slot.
            let Some(view) = tab.panes.get(id).map(|leaf| leaf.view.handle()) else {
                continue;
            };
            let rewired = match view {
                PaneView::Terminal(view) => {
                    let session = view.read(cx).session().clone();
                    view.update(cx, |view, cx| view.rebind(caps.clone(), window, cx));
                    self.new_pane(view, session, window, cx)
                }
                PaneView::Editor(pane) => self.new_editor_pane(pane, window, cx),
                // The same two steps as a terminal, one entity further in: the
                // grid is what holds the window-bound subscriptions, and the
                // strip above it holds nothing that a move invalidates.
                PaneView::Tail(view) => {
                    let session = view.read(cx).session().clone();
                    view.update(cx, |view, cx| view.rebind(caps.clone(), window, cx));
                    self.new_tail_pane(view, session, window, cx)
                }
            };
            if let Some(slot) = tab.panes.get_mut(id) {
                *slot = rewired;
            }
        }

        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Shows a file the panel has read, in a tab of its own.
    ///
    /// A tab rather than a split of the pane that asked. A split gives the file
    /// half of a terminal that was already only as wide as it needed to be, and
    /// it gives it *permanently*: there is no way back to the whole width while
    /// the file is open. A tab costs the file nothing and the shell nothing, and
    /// it is what every editor the user already has does with an opened file.
    /// The strip is where tabs are switched, listed and closed, so the file gets
    /// all of that for free — the close button included, which is why
    /// [`Workspace::close_tab`] asks about unsaved changes.
    ///
    /// It lands right after the active tab, where a duplicated tab and a broken
    /// out pane also land: the new tab belongs beside the one it came from
    /// rather than at the far end of a strip the user may have to scroll.
    ///
    /// No check that the session's tab is still open, which the split needed
    /// because it had to have a pane to split. The session itself arrives with
    /// the file — [`OpenEditor::session`] holds the entity — so everything the
    /// editor needs is here whether or not the tab that asked survived the read:
    /// the bytes are in hand, the [`files::FileSource`] outlives a disconnect,
    /// and the file panel keeps browsing that session through
    /// [`SessionTab::panel_session`]. Refusing would throw a file away for a
    /// reason the user cannot see.
    fn open_editor(&mut self, opened: &OpenEditor, window: &mut Window, cx: &mut Context<Self>) {
        let session = opened.session.entity_id();
        let path = editor_pane::file_path(&opened.dir, &opened.name);

        // Asking for a file that is already open is a request to look at it,
        // not for a second buffer over the same bytes: two panes editing one
        // file would each write the other's work away at the next save.
        if let Some((index, pane)) = self.pane_of_file(session, &path, cx) {
            self.active = index;
            self.tabs[index].focus(pane);
            self.reveal_active_tab();
            self.focus_active(window, cx);
            cx.notify();
            return;
        }

        let editor = cx.new(|cx| {
            EditorPane::new(
                opened.session.clone(),
                opened.source.clone(),
                opened.dir.clone(),
                opened.name.clone(),
                opened.file.clone(),
                opened.writable,
                opened.root_access,
                cx,
            )
        });
        let leaf = self.new_editor_pane(editor, window, cx);

        // Right after the active tab, or at the head of an empty strip — which
        // is where the file lands if the shell it was read from has since been
        // closed and was the last one open.
        let at = if self.tabs.is_empty() {
            0
        } else {
            self.active + 1
        };
        // The file was opened out of the panel, so the panel was open; the tab
        // carries that over rather than shutting it, which is how the next file
        // is reached. An empty strip — the shell the file came from having been
        // closed since — has no tab to carry anything over from, and the panel
        // is what the file arrived through either way.
        let panel_open = self.tabs.get(self.active).is_none_or(|tab| tab.panel_open);
        log::info!("opening {path} in a tab of its own");
        self.tabs
            .insert(at, SessionTab::single(leaf).with_panel(panel_open));
        self.active = at;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// The pane already editing `path` on `session`, if there is one.
    fn pane_of_file(&self, session: EntityId, path: &str, cx: &App) -> Option<(usize, PaneId)> {
        self.tabs.iter().enumerate().find_map(|(index, tab)| {
            tab.panes.leaves().into_iter().find_map(|(pane, leaf)| {
                let PaneView::Editor(editor) = &leaf.view else {
                    return None;
                };
                let editor = editor.read(cx);
                // Both halves matter: the same path on two hosts is two files,
                // and one host's file opened from two tabs is still one file.
                (editor.session().entity_id() == session && editor.path() == path)
                    .then_some((index, pane))
            })
        })
    }

    /// The tab and pane a view is rendered in, wherever it is.
    fn locate_pane(&self, view: EntityId) -> Option<(usize, PaneId)> {
        self.tabs
            .iter()
            .enumerate()
            .find_map(|(index, tab)| tab.pane_of(view).map(|pane| (index, pane)))
    }

    /// Closes an editor pane, asking first if it holds unsaved changes.
    fn close_editor_pane(
        &mut self,
        editor: &Entity<EditorPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((index, pane)) = self.locate_pane(editor.entity_id()) else {
            return;
        };
        if editor.read(cx).is_dirty(cx) {
            self.ask_before_closing(pane, window, cx);
            return;
        }
        self.remove_pane(index, pane, window, cx);
    }

    /// Closes an editor pane whose save-and-close write has landed.
    ///
    /// No unsaved-changes check on the way through, unlike
    /// [`Self::close_editor_pane`]: the pane only reports this once a write that
    /// covered every edit in it succeeded, so asking the question again would be
    /// asking about something the save already answered. A pane that has gone
    /// while the bytes were in flight — its tab closed from the strip — is
    /// simply not there to close, and there is nothing to say about it.
    fn close_saved_editor_pane(
        &mut self,
        editor: &Entity<EditorPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((index, pane)) = self.locate_pane(editor.entity_id()) else {
            return;
        };
        self.remove_pane(index, pane, window, cx);
    }

    /// Puts the unsaved-changes question up over `pane`.
    ///
    /// The keyboard comes to the workspace itself while it stands, for the same
    /// reason every dialog takes it: <kbd>Esc</kbd> has to reach the handler
    /// that cancels the question rather than the editor underneath, which binds
    /// the key for its own find bar.
    fn ask_before_closing(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        self.close_confirm = Some(pane);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Closes the pane whose close was confirmed, unsaved changes and all.
    fn confirm_close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.close_confirm.take() else {
            return;
        };
        cx.notify();
        // The pane can have gone while the question stood — its tab closed from
        // the strip — in which case there is nothing left to close.
        let Some(index) = self.tabs.iter().position(|tab| tab.panes.contains(pane)) else {
            return;
        };
        self.remove_pane(index, pane, window, cx);
    }

    /// Hands the pane the close question was about the save it just offered, and
    /// takes the question down.
    ///
    /// The question goes on the press rather than staying up over the transfer:
    /// the pane's own header already says a save is running, and holding a modal
    /// over a write that may be crossing an SSH session would block the whole
    /// window on the slowest thing in it. Everything after this is the pane's —
    /// see [`EditorPane::save_and_close`] — and the only way back here is
    /// [`EditorPaneEvent::SavedForClose`], which arrives only if the write
    /// landed and covered every edit. A failure never returns: it stays on the
    /// pane, under the file it is about.
    fn save_and_close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.close_confirm.take() else {
            return;
        };
        cx.notify();
        // The pane can have gone while the question stood — its tab closed from
        // the strip — in which case there is nothing left to save.
        let editor =
            self.tabs
                .iter()
                .find_map(|tab| match tab.panes.get(pane).map(|leaf| &leaf.view) {
                    Some(PaneView::Editor(editor)) => Some(editor.clone()),
                    _ => None,
                });
        let Some(editor) = editor else {
            return;
        };
        editor.update(cx, |editor, cx| editor.save_and_close(cx));
        // Back into the file, which stays open and editable while the bytes are
        // in flight — and which stays for good if they never arrive.
        self.focus_active(window, cx);
    }

    /// Puts the close question away, leaving the pane open.
    ///
    /// Deliberately does *not* restore the focus: the two callers want different
    /// things done with it, and only they know which — the button hands it back
    /// to the pane the question was about, while `Escape` goes through the
    /// dismissal path every other overlay uses.
    fn cancel_close_editor(&mut self, cx: &mut Context<Self>) {
        if self.close_confirm.take().is_some() {
            cx.notify();
        }
    }

    /// Puts up the password question an editor pane asked for.
    ///
    /// The pane is held rather than looked up again later, and the field is
    /// built here rather than kept on the workspace, so that the whole question
    /// — what it is for, what has been typed into it, what the last attempt was
    /// told — lives and dies together. See [`SudoPrompt`].
    ///
    /// The field takes the keyboard at once: there is one thing to do with this
    /// dialog and it is to type, and a modal that has to be clicked into first
    /// is a modal that has interrupted the user twice.
    fn ask_sudo_password(
        &mut self,
        pane: Entity<EditorPane>,
        purpose: RootPurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Every other modal opens through here, and this one has to as well:
        // two questions on one screen would leave the user answering whichever
        // was drawn last.
        self.close_overlays(cx);

        let workspace = cx.weak_entity();
        let input = cx.new(|cx| {
            TextInput::new(cx)
                .context_menu(input_menu_labels)
                .masked(true)
                .tab_index(0)
                .on_submit({
                    move |password, window, cx| {
                        let (workspace, password) = (workspace.clone(), password.to_owned());
                        // Deferred for the reason the connection dialog defers its
                        // own submit: this fires from inside the field's `update`,
                        // so the field is leased out of the entity map, and the
                        // answer below may well be the thing that drops it.
                        window.defer(cx, move |window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    workspace.submit_sudo_password(password, window, cx);
                                })
                                .ok();
                        });
                    }
                })
        });
        let handle = input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);

        self.sudo_prompt = Some(SudoPrompt {
            pane,
            purpose,
            input,
            remember: false,
            error: None,
            busy: false,
        });
        cx.notify();
    }

    /// Reads what has been typed and answers the question with it.
    ///
    /// The OK button's path; <kbd>Enter</kbd> in the field takes the text
    /// straight to [`Workspace::submit_sudo_password`] instead, because it
    /// already has it in hand and the field it would be read back out of is
    /// leased at that moment.
    fn confirm_sudo_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = &self.sudo_prompt else {
            return;
        };
        let password = prompt.input.read(cx).content().to_owned();
        self.submit_sudo_password(password, window, cx);
    }

    /// Hands `password` to whatever the question was asked for.
    ///
    /// Three routes out of here, and which one is taken says everything about
    /// what the pane's mode becomes:
    ///
    /// * **Unlock** — the source validates the password and, where the box is
    ///   ticked, keeps it. The pane unlocks on success, in the mode that says
    ///   where the next save's password will come from. A refusal leaves the
    ///   dialog up with the reason under the field, which is the whole reason
    ///   for validating before a buffer is unlocked at all.
    /// * **Save, remembered** — the same validation first, which is what makes
    ///   the tick box a promise rather than a hope: a password that is going to
    ///   be kept is proved before it is. The pane's mode is upgraded and the
    ///   save goes out needing nothing further.
    /// * **Save, not remembered** — no validation at all, and the dialog goes
    ///   down on the press. The password travels with the write, and a wrong
    ///   one comes back as an ordinary failed save in the pane's own strip; a
    ///   round trip to learn that a moment earlier would buy nothing.
    ///
    /// An empty field is sent like anything else. An empty password is still a
    /// password as far as `sudo` is concerned, and refusing it here would mean
    /// writing our own version of the sentence the host is about to give.
    fn submit_sudo_password(
        &mut self,
        password: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(prompt) = &self.sudo_prompt else {
            return;
        };
        if prompt.busy {
            return;
        }
        let (pane, purpose, remember) = (prompt.pane.clone(), prompt.purpose, prompt.remember);

        // The one route that asks the source nothing: the password travels with
        // the bytes, and the write is where it is judged.
        if purpose == RootPurpose::Save && !remember {
            self.close_sudo_prompt(window, cx);
            pane.update(cx, |pane, cx| pane.save_with_password(password, cx));
            return;
        }

        if let Some(prompt) = &mut self.sudo_prompt {
            prompt.busy = true;
            prompt.error = None;
        }
        cx.notify();

        let source = pane.read(cx).source().clone();
        cx.spawn_in(window, async move |workspace, cx| {
            let result = source.unlock_root(Some(&password), remember).await;
            workspace
                .update_in(cx, |workspace, window, cx| match result {
                    Ok(()) => {
                        // `Remembered` whenever the box was ticked, and the
                        // `Save` route only ever arrives here with it ticked —
                        // the other one never asked the source anything.
                        let mode = if remember {
                            RootMode::Remembered
                        } else {
                            RootMode::EveryTime
                        };
                        workspace.close_sudo_prompt(window, cx);
                        pane.update(cx, |pane, cx| {
                            pane.unlock_as_root(mode, cx);
                            // The source keeps the password from here on, so
                            // the save that was waiting needs none of its own.
                            if purpose == RootPurpose::Save {
                                pane.resume_save(cx);
                            }
                        });
                    }
                    // Left up, with the reason: the whole point of validating
                    // before anything is written is that there is still a field
                    // on screen to try again in.
                    Err(error) => {
                        if let Some(prompt) = &mut workspace.sudo_prompt {
                            prompt.busy = false;
                            prompt.error = Some(SharedString::from(error.to_string()));
                        }
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Puts the password question away without answering it.
    ///
    /// Cancel, `Escape`, and a click on the backdrop all land here, and none of
    /// them changes anything about the pane: a locked buffer stays locked, and
    /// an unsaved one stays unsaved — see
    /// [`EditorPane::abandon_root_save`] for the one intent that has to be let
    /// go of with it.
    fn cancel_sudo_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = &self.sudo_prompt else {
            return;
        };
        let (pane, purpose) = (prompt.pane.clone(), prompt.purpose);
        self.close_sudo_prompt(window, cx);
        if purpose == RootPurpose::Save {
            pane.update(cx, |pane, cx| pane.abandon_root_save(cx));
        }
    }

    /// Drops the question and hands the keyboard back to the file.
    ///
    /// The password field goes with it, which is the only place a typed
    /// password ever was.
    fn close_sudo_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sudo_prompt.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Moves focus to the next pane of the active tab, wrapping around.
    pub(crate) fn focus_next_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_pane(true, window, cx);
    }

    /// Moves focus to the previous pane of the active tab, wrapping around.
    pub(crate) fn focus_prev_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_pane(false, window, cx);
    }

    /// Steps the active pane one place through the active tab's focus cycle.
    fn cycle_pane(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.panes.leaf_count() < 2 {
            return;
        }

        let from = tab.active_pane();
        let next = if forward {
            tab.panes.next_leaf(from)
        } else {
            tab.panes.prev_leaf(from)
        };
        let Some(next) = next else {
            return;
        };

        tab.focus(next);
        // Focusing the pane's grid also runs `on_pane_focused`, which is
        // harmless: it finds the pane already marked active.
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Whether the active pane is big enough to be split along `axis`.
    ///
    /// The two halves inherit roughly half of the pane's current grid each, so
    /// the check is on the live column or row count rather than on pixels: a
    /// pane that would come out narrower than [`MIN_PANE_COLS`] or shorter than
    /// [`MIN_PANE_ROWS`] is not worth having.
    ///
    /// Silent, because every menu carrying a split asks this on each frame it is
    /// open, to decide which rows to grey or to leave out; the refusal is logged
    /// where it happens.
    ///
    /// Always `false` over an editor pane. Every split the workspace offers puts
    /// a *second connection to the same host* in the new half, and an editor is
    /// not a connection: there is nothing to open a second one of. Over such a
    /// pane the rows asking for it are greyed in the application and pane menus
    /// and left out of the tab menu — see [`MenuEntry::enabled`] for which menu
    /// does which — and the shortcuts do nothing.
    fn can_split_active(&self, axis: Axis, cx: &App) -> bool {
        let Some(tab) = self.tabs.get(self.active) else {
            return false;
        };
        let PaneView::Terminal(view) = tab.active_view() else {
            return false;
        };
        let (cols, rows) = view.read(cx).session().read(cx).terminal().size();
        split_fits(axis, cols, rows)
    }

    /// [`Workspace::can_split_active`] for a pane that has handed its grid size
    /// over instead of being read for it.
    ///
    /// Same verdict, same order of questions; only the size arrives by argument.
    /// A pane asks this way while it is rendering its own menu, when reading the
    /// view back would panic — see [`PaneCapsSource`] — and the size it passes
    /// is the size that read would have returned.
    fn can_split_sized(&self, axis: Axis, cols: u16, rows: u16) -> bool {
        let Some(tab) = self.tabs.get(self.active) else {
            return false;
        };
        if !matches!(tab.active_view(), PaneView::Terminal(_)) {
            return false;
        }
        split_fits(axis, cols, rows)
    }

    /// Whether the active pane may be broken out into a tab of its own.
    ///
    /// A tab with one pane already *is* that tab, so the command has nothing to
    /// move; [`Workspace::break_out_active_pane`] returns on exactly this
    /// condition, and the rows offering it read the same rule from here.
    fn can_break_out_active(&self) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| tab.panes.leaf_count() > 1)
    }

    /// The three pane commands' verdicts in one answer, for a menu that needs
    /// all of them; see [`PaneCaps`].
    fn pane_caps(&self, cx: &App) -> PaneCaps {
        PaneCaps {
            split_right: self.can_split_active(Axis::Horizontal, cx),
            split_below: self.can_split_active(Axis::Vertical, cx),
            break_out: self.can_break_out_active(),
            equalize_widths: self.can_equalize(Axis::Horizontal),
            equalize_heights: self.can_equalize(Axis::Vertical),
        }
    }

    /// [`Workspace::pane_caps`] for a pane asking about itself mid-render, which
    /// reports its grid size rather than being read for it.
    fn pane_caps_sized(&self, cols: u16, rows: u16) -> PaneCaps {
        PaneCaps {
            split_right: self.can_split_sized(Axis::Horizontal, cols, rows),
            split_below: self.can_split_sized(Axis::Vertical, cols, rows),
            break_out: self.can_break_out_active(),
            equalize_widths: self.can_equalize(Axis::Horizontal),
            equalize_heights: self.can_equalize(Axis::Vertical),
        }
    }

    /// Builds the callback a terminal view asks the question above through.
    ///
    /// Weak on purpose, and not only to avoid a cycle through a view the
    /// workspace owns: a pane can outlive the workspace by a frame while the
    /// window is tearing down, and a menu drawn in that frame is better off
    /// offering nothing than keeping the workspace alive to answer it.
    fn pane_caps_source(cx: &mut Context<Self>) -> PaneCapsSource {
        let workspace = cx.weak_entity();
        Rc::new(
            move |cols: u16, rows: u16, cx: &App| match workspace.upgrade() {
                Some(workspace) => workspace.read(cx).pane_caps_sized(cols, rows),
                None => PaneCaps::default(),
            },
        )
    }

    /// Scrolls the tab strip so that the active tab is on screen.
    ///
    /// The strip applies this during its next prepaint, so callers have to ask
    /// for a repaint as well.
    fn reveal_active_tab(&self) {
        if !self.tabs.is_empty() {
            self.tab_scroll.scroll_to_item(self.active);
        }
    }

    /// Moves keyboard focus onto the active pane's terminal, or onto the
    /// workspace itself when no session is open.
    ///
    /// Without this the shortcuts stop working after the last tab is closed,
    /// because their key context only exists while something inside the
    /// workspace is focused.
    fn focus_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.tabs.get(self.active) {
            Some(tab) => {
                let handle = tab.active_view().focus_handle(cx);
                window.focus(&handle, cx);
            }
            None => window.focus(&self.focus_handle, cx),
        }
    }

    /// Whether one of the modal dialogs is on screen.
    ///
    /// A modal takes the window over, so anything the strip would otherwise open
    /// on top of it has to stand down.
    fn dialog_open(&self, cx: &App) -> bool {
        self.dialog.read(cx).is_open()
            || self.settings.read(cx).is_open()
            || self.about.read(cx).is_open()
            || self.update.read(cx).is_open()
            // A question rather than a dialog, but it takes the window the same
            // way and must not be drawn under a menu opened over it.
            || self.close_confirm.is_some()
            // And so is the password an elevated save asks for.
            || self.sudo_prompt.is_some()
    }

    /// Closes every dialog and the dropdown menu.
    ///
    /// Every `open_*` method starts here, which is what keeps the modals
    /// mutually exclusive: only one of them can ever be on screen, and opening
    /// one always puts the menu away. The update dialog is closed here like the
    /// rest, so a user who reaches for a command instead of one of its buttons
    /// is not left with a stale announcement floating over the window — except
    /// while it is installing, when its own `close` refuses and the swap is
    /// allowed to finish; see [`UpdateDialog::close`].
    fn close_overlays(&mut self, cx: &mut Context<Self>) {
        self.menu_open = false;
        self.tab_menu_open = false;
        self.tab_context = None;
        self.language_menu = None;
        self.charset_menu = None;
        self.empty_context = None;
        // Anything that opens an overlay is a fresh intention, and a followed
        // file nobody got round to is stale by the time the next one arrives —
        // see [`Workspace::pending_tail`]. Set again, by `open_tail`, *after*
        // this call.
        self.pending_tail = None;
        // Cancelled rather than parked. The safe answer to "close it and lose
        // the changes?" is no, and a user who has just reached for a different
        // command has plainly stopped answering this one; leaving it up would
        // put two modals on the screen at once.
        self.close_confirm = None;
        // The password question goes the same way, and taking it rather than
        // clearing it is what drops the field the password was typed into. Its
        // pane keeps whatever it had: still locked, or still holding an unsaved
        // buffer — with the one intent that has to be let go of let go of here
        // too. Nothing focuses anything: `ask_sudo_password` calls this on its
        // way *in*, and the focus it wants is the field it is about to build.
        if let Some(prompt) = self.sudo_prompt.take()
            && prompt.purpose == RootPurpose::Save
        {
            prompt
                .pane
                .update(cx, |pane, cx| pane.abandon_root_save(cx));
        }
        if self.dialog.read(cx).is_open() {
            self.dialog.update(cx, |dialog, cx| dialog.close(cx));
        }
        if self.settings.read(cx).is_open() {
            self.settings.update(cx, |dialog, cx| dialog.close(cx));
        }
        if self.about.read(cx).is_open() {
            self.about.update(cx, |dialog, cx| dialog.close(cx));
        }
        if self.update.read(cx).is_open() {
            self.update.update(cx, |dialog, cx| dialog.close(cx));
        }
    }

    /// Shows the connection dialog with an empty form.
    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        self.close_overlays(cx);
        self.dialog.update(cx, |dialog, cx| dialog.open_new(cx));
        cx.notify();
    }

    /// Opens a saved profile, showing the connection dialog only if it has to.
    ///
    /// A profile the user already finished configuring — a remembered password,
    /// or a key that needs no passphrase — carries everything the transport
    /// needs, so presenting the dialog again would be one form to dismiss
    /// before a session the user has already asked for. Those profiles connect
    /// on the click.
    ///
    /// The dialog still opens, pre-filled, whenever anything would have to be
    /// typed or corrected: a password that was never remembered, an encrypted
    /// key with no stored passphrase, a key file that has gone missing, or the
    /// agent method, which the transport does not implement.
    ///
    /// Deciding that reads the OS keychain, and possibly the key file,
    /// synchronously on the UI thread — the same work the dialog's Connect
    /// button does, one click earlier.
    fn open_profile(
        &mut self,
        profile: &SessionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_overlays(cx);
        if let Some(auth) = connection::saved_credentials(profile) {
            self.open_session(profile.clone(), auth, window, cx);
            return;
        }
        let id = profile.id;
        self.dialog
            .update(cx, |dialog, cx| dialog.open_profile(id, cx));
        cx.notify();
    }

    /// The saved profile `id`, as the store has it right now.
    ///
    /// Through the connection dialog because that is where the store lives —
    /// see [`Workspace::duplicate_profile`] for why there is exactly one — and
    /// by value because that is what the dialog hands out: the caller is
    /// usually about to put the profile in a closure that outlives the frame.
    fn profile(&self, id: Uuid, cx: &App) -> Option<SessionProfile> {
        self.dialog
            .read(cx)
            .profiles()
            .into_iter()
            .find(|profile| profile.id == id)
    }

    /// Opens `path` on `profile` as a followed file, in a tab of its own.
    ///
    /// [`Workspace::open_profile`] for the other thing a saved profile can be
    /// asked for, and it makes the same two decisions in the same order: a
    /// profile that carries everything the transport needs follows the file on
    /// the click, and one that does not gets the pre-filled form first. The
    /// difference is on the far side of that form — the dialog can only say
    /// *connect*, so the request is put down in [`Workspace::pending_tail`] and
    /// picked up again when the credentials come back.
    fn open_tail(
        &mut self,
        profile: &SessionProfile,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_overlays(cx);
        if let Some(auth) = connection::saved_credentials(profile) {
            self.open_tail_session(profile.clone(), auth, path, window, cx);
            return;
        }
        // After `close_overlays`, which clears this very field: the request is
        // being made now, and what it clears is whatever request was abandoned
        // before it.
        self.pending_tail = Some((profile.id, path));
        let id = profile.id;
        self.dialog
            .update(cx, |dialog, cx| dialog.open_profile(id, cx));
        cx.notify();
    }

    /// Follows `path` on `profile` with `auth`, in a tab right after the active
    /// one.
    ///
    /// The tab lands beside the tab it was asked for from, exactly as an opened
    /// file does and for the same reason — see [`Workspace::open_editor`],
    /// whose insertion this mirrors — and it opens with the file panel shut
    /// whatever the profile says about panels: there is no shell on the other
    /// end to browse a filesystem beside, and [`Session::files`] answers
    /// nothing for such a session anyway.
    ///
    /// The forwardings are suppressed unconditionally. A profile's local ports
    /// belong to one session at a time, and the one that should hold them is
    /// the shell the user works in — not a pane that opened to read a log and
    /// would take them from it, or fail to bind them and say so in yellow over
    /// the first screen of the file.
    fn open_tail_session(
        &mut self,
        profile: SessionProfile,
        auth: SshAuth,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::info!("following {path} on {}", profile.label());
        let caps = Self::pane_caps_source(cx);
        let session = cx.new(|cx| Session::new_tail(profile, auth, path.clone(), true, cx));
        let terminal = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
        // Alone in its tab, with nothing to be told apart from.
        let view = cx
            .new(|cx| TailView::new(terminal, session.clone(), path, SharedString::default(), cx));
        let leaf = self.new_tail_pane(view, session, window, cx);

        let at = if self.tabs.is_empty() {
            0
        } else {
            self.active + 1
        };
        self.tabs
            .insert(at, SessionTab::single(leaf).with_panel(false));
        self.active = at;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Follows `path` on the connection `profile_id` in a new pane *below* the
    /// active pane of the tab at `tab_index`.
    ///
    /// [`Workspace::open_tail`] opens a followed file in a tab of its own; this
    /// opens one in a tab that already exists, which is the difference between
    /// looking at a log and building an arrangement of them. It is what makes a
    /// dashboard tab something the user can compose by hand: add a pane, drag
    /// the divider, add another — and then *Save layout to dashboard* writes
    /// exactly what is on screen back to the store. Nothing else in the
    /// application can grow a dashboard tab a pane, so without this the only way
    /// to change what a dashboard holds is the settings dialog's list.
    ///
    /// Below rather than beside, because a log is a wide thing: two half-width
    /// panes each wrap their lines twice, while two half-height ones each show
    /// half as many whole lines. Vertical is the axis the pane count can grow
    /// along without the content becoming unreadable, which is also why the
    /// default grid [`Workspace::compose_dashboard_tab`] lays down stacks rows.
    ///
    /// The focus is left exactly where it was. The user is adding a pane to
    /// something they are reading, not switching to it, and a followed file has
    /// no input to take anyway.
    ///
    /// A connection with nothing saved gets its form put up and nothing else,
    /// the same one-more-click answer [`Workspace::open_dashboard`] gives and
    /// for the same reason: the dialog can only say *connect*, so it cannot
    /// come back to a pane it was never told about. Unlike
    /// [`Workspace::open_tail`], no request is parked in
    /// [`Workspace::pending_tail`] — that field opens a *tab*, and resuming
    /// through it would put the file somewhere other than the tab that was
    /// asked about, which is worse than not resuming at all.
    fn add_tail_to_tab(
        &mut self,
        tab_index: usize,
        profile_id: Uuid,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Also clears any parked `pending_tail`, which this call deliberately
        // does not set again; see the doc comment.
        self.close_overlays(cx);

        let Some(profile) = self.profile(profile_id, cx) else {
            log::warn!("the connection {path} would be followed over no longer exists");
            return;
        };
        let Some(auth) = connection::saved_credentials(&profile) else {
            log::info!(
                "{} is waiting on saved credentials before {path} can be added",
                profile.name
            );
            self.dialog
                .update(cx, |dialog, cx| dialog.open_profile(profile_id, cx));
            cx.notify();
            return;
        };
        let Some(target) = self.tabs.get(tab_index).map(SessionTab::active_pane) else {
            // The menu and the tab it speaks for are a frame apart, so the tab
            // can have closed since the row was drawn.
            return;
        };
        log::info!("adding {path} on {} to a tab", profile.label());

        // Built in the order [`Workspace::open_dashboard`] builds a pane in,
        // and with the same two decisions: the connection's name rides along,
        // because a tab grown this way may well mix hosts and two `access.log`s
        // have to be tellable apart; and the profile's forwardings are
        // suppressed, because a pane that only reads a log must not take a
        // profile's local ports from the shell the user works in.
        let connection = SharedString::from(profile.name.clone());
        let caps = Self::pane_caps_source(cx);
        let session = cx.new(|cx| Session::new_tail(profile, auth, path.clone(), true, cx));
        let terminal = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
        let view = cx.new(|cx| TailView::new(terminal, session.clone(), path, connection, cx));
        let leaf = self.new_tail_pane(view, session, window, cx);

        let tab = &mut self.tabs[tab_index];
        if tab.panes.split(target, Axis::Vertical, leaf).is_none() {
            // `target` came out of this very tab a moment ago, so this is
            // unreachable; logged rather than ignored because reaching it would
            // mean a live session has been dropped on the floor.
            log::error!("the pane to split has vanished; the followed file was dropped");
            return;
        }
        cx.notify();
    }

    /// Re-reads the saved dashboards from disk.
    ///
    /// A failure keeps the list the window already has, exactly as
    /// `ConnectionDialog::reload_store` keeps the profiles it already has: the
    /// alternative is emptying the welcome screen because a write was
    /// interrupted, which loses the user a click and tells them nothing.
    fn reload_dashboards(&mut self) {
        match DashboardStore::load() {
            Ok(store) => self.dashboards = store,
            Err(err) => log::warn!("keeping the dashboards already loaded: {err:#}"),
        }
    }

    /// Opens the dashboard `id`: every file it names, each followed over the
    /// connection that reaches it, in one tab arranged as a grid.
    ///
    /// This is [`Workspace::open_tail`] several times over, and it makes the
    /// same two decisions that call makes — resolve, then check the
    /// credentials — but it has to make them for the whole set before it opens
    /// anything, because what it opens is a single tab.
    ///
    /// A pane whose profile has been deleted is skipped rather than fatal: a
    /// dangling reference is a state the store keeps on purpose (see
    /// [`rulogman_core::dashboard`]), and the four logs that *can* be opened
    /// are worth more than a refusal naming the fifth. A dashboard with nothing
    /// left to open is the one case that opens no tab.
    ///
    /// # Why the credentials are all or nothing
    ///
    /// A profile with nothing saved needs the connection form, and the form can
    /// only answer for one connection: a dashboard spanning three such hosts
    /// would be three dialogs in a row, each one having to be remembered
    /// against a tab that does not exist yet. So this version does not open a
    /// partial tab and does not queue anything. It says which connections are
    /// unsaved, opens the form pre-filled on the first of them — the fix is one
    /// *Save* away — and leaves the dashboard to be clicked again. A second
    /// click is a smaller price than a queue of dialogs, and unlike the queue
    /// it is a thing the user can see the shape of.
    fn open_dashboard(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.close_overlays(cx);

        let Some(dashboard) = self.dashboards.get(id).cloned() else {
            log::warn!("the dashboard that was asked for is no longer in the store");
            return;
        };
        if dashboard.panes.is_empty() {
            log::info!("dashboard {} names no files to follow", dashboard.name);
            return;
        }
        log::info!(
            "opening dashboard {} over {} file(s)",
            dashboard.name,
            dashboard.panes.len()
        );

        let mut resolved: Vec<(SessionProfile, String)> = Vec::with_capacity(dashboard.panes.len());
        for pane in &dashboard.panes {
            match self.profile(pane.profile, cx) {
                Some(profile) => resolved.push((profile, pane.path.clone())),
                None => log::warn!(
                    "dashboard {} follows {} over a connection that no longer exists; the pane is skipped",
                    dashboard.name,
                    pane.path
                ),
            }
        }
        if resolved.is_empty() {
            log::warn!(
                "dashboard {} has no file left whose connection still exists",
                dashboard.name
            );
            return;
        }

        // Once per distinct connection rather than once per pane: reading the
        // keychain, and possibly a key file, is what this asks, and two panes
        // on one host are asking it the same question.
        let mut credentials: Vec<(Uuid, SshAuth)> = Vec::new();
        let mut missing: Vec<(Uuid, String)> = Vec::new();
        for (profile, _) in &resolved {
            if credentials.iter().any(|(id, _)| *id == profile.id)
                || missing.iter().any(|(id, _)| *id == profile.id)
            {
                continue;
            }
            match connection::saved_credentials(profile) {
                Some(auth) => credentials.push((profile.id, auth)),
                None => missing.push((profile.id, profile.name.clone())),
            }
        }
        if let Some(first) = missing.first().map(|(id, _)| *id) {
            let names: Vec<&str> = missing.iter().map(|(_, name)| name.as_str()).collect();
            log::info!(
                "dashboard {} is waiting on saved credentials for {}",
                dashboard.name,
                names.join(", ")
            );
            self.dialog
                .update(cx, |dialog, cx| dialog.open_profile(first, cx));
            cx.notify();
            return;
        }

        let caps = Self::pane_caps_source(cx);
        let mut leaves = Vec::with_capacity(resolved.len());
        for (profile, path) in resolved {
            let Some(auth) = credentials
                .iter()
                .find(|(id, _)| *id == profile.id)
                .map(|(_, auth)| auth.clone())
            else {
                // Unreachable: the sweep above filed every distinct profile
                // under one list or the other, and a non-empty `missing` has
                // already returned.
                log::error!("a dashboard pane lost the credentials it was just checked for");
                continue;
            };
            // The connection's name, for the pane's own header: it is what
            // tells two hosts' `access.log`s apart, and this is the only place
            // that still has the profile to read it from.
            let connection = SharedString::from(profile.name.clone());
            // Tunnels suppressed on every one of them, for the reason
            // [`Workspace::open_tail_session`] suppresses them: a pane that
            // only reads a log must not take a profile's local ports from the
            // shell the user works in.
            let session = cx.new(|cx| Session::new_tail(profile, auth, path.clone(), true, cx));
            let terminal =
                cx.new(|cx| TerminalView::new(session.clone(), caps.clone(), window, cx));
            let view = cx.new(|cx| TailView::new(terminal, session.clone(), path, connection, cx));
            leaves.push(self.new_tail_pane(view, session, window, cx));
        }

        if leaves.is_empty() {
            // Only reachable through the `else` arm above, which is itself
            // unreachable; the guard is here because the alternative is
            // composing a tab out of no panes, which panics.
            log::error!("dashboard {} built no panes to open", dashboard.name);
            return;
        }

        // No panel, for the reason a single followed file opens without one:
        // there is no shell on the other end of any of these panes to browse a
        // filesystem beside.
        //
        // The saved geometry is honoured only when every pane made it in: a
        // leaf names its pane by position in `dashboard.panes`, and skipping a
        // pane whose profile is gone would shift those positions out from under
        // the layout. A short set falls back to the grid, which needs no such
        // correspondence; `valid_layout` guards the rest.
        let tab = match dashboard.valid_layout() {
            Some(layout) if leaves.len() == dashboard.panes.len() => {
                Self::compose_dashboard_layout(leaves, layout, false)
            }
            _ => Self::compose_dashboard_tab(leaves, false),
        }
        .with_label(dashboard.name)
        .with_dashboard(id);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.reveal_active_tab();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Reads the arrangement of `tab` back into the pair a dashboard is stored
    /// as: the panes it shows, in depth-first layout order, and the geometry
    /// tree laid over them.
    ///
    /// The running pane index and the panes vector are grown together in one
    /// depth-first walk, so a [`LayoutNode::Leaf`] and its [`DashboardPane`]
    /// always agree on which pane they mean without a second lookup. Every leaf
    /// must be a followed file — a session that answers both a profile and a
    /// tail path — because a dashboard is nothing but followed files; a pane
    /// that is anything else (a shell the user split in, an opened editor)
    /// aborts the whole capture with `None`, since saving a partial set would
    /// silently drop it.
    ///
    /// Free of any window, so it is testable the way the composers are.
    fn capture_tab_layout(tab: &SessionTab, cx: &App) -> Option<(Vec<DashboardPane>, LayoutNode)> {
        fn walk(
            node: &PaneNode<PaneLeaf>,
            panes: &mut Vec<DashboardPane>,
            cx: &App,
        ) -> Option<LayoutNode> {
            match node {
                PaneNode::Leaf { payload, .. } => {
                    let session = payload.view.session(cx)?;
                    let session = session.read(cx);
                    // Both or neither: a followed file answers a profile and a
                    // path, and anything missing one is not a pane a dashboard
                    // can name.
                    let profile = session.profile_id()?;
                    let path = session.tail_path()?.to_owned();
                    let pane = panes.len();
                    panes.push(DashboardPane { profile, path });
                    Some(LayoutNode::Leaf { pane })
                }
                PaneNode::Split {
                    axis,
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    // First then second, the same order the panes vector is
                    // grown in, so leaf indices stay in step with it.
                    let first = walk(first, panes, cx)?;
                    let second = walk(second, panes, cx)?;
                    Some(LayoutNode::Split {
                        axis: layout_axis_of(*axis),
                        ratio: *ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    })
                }
            }
        }

        let mut panes = Vec::new();
        let layout = walk(tab.panes.root(), &mut panes, cx)?;
        Some((panes, layout))
    }

    /// Captures the arrangement of the tab at `index` onto the dashboard it was
    /// opened from, replacing that dashboard's panes and geometry with what is
    /// on screen now.
    ///
    /// A no-op on a tab that is not a dashboard: there is nowhere to write the
    /// arrangement, so the command simply says so and stops.
    ///
    /// This deliberately captures the *current* pane set, not only the dividers:
    /// a pane the user closed since opening the dashboard is gone from the save,
    /// and one they split in that is not a followed file makes the capture
    /// refuse rather than drop it. Saving the layout is thus also how the user
    /// prunes or reshuffles a dashboard from the tab itself.
    ///
    /// There is no toast surface to report through, so success and every
    /// failure are logged; the menu entry that invokes this is only offered on a
    /// dashboard tab, which is the one confirmation the user does see.
    fn save_tab_layout(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(id) = tab.dashboard else {
            log::info!("the tab whose layout was asked for is not a dashboard; nothing to save");
            return;
        };
        let Some((panes, layout)) = Self::capture_tab_layout(tab, cx) else {
            log::warn!(
                "dashboard {id} has a pane that is not a followed file; its layout was not saved"
            );
            return;
        };
        let name = tab
            .label
            .as_ref()
            .map(|label| label.to_string())
            .unwrap_or_default();

        // The stored entry is the one to update; it must still be there, but a
        // fresh one keeps the id if it somehow is not, rather than losing the
        // capture.
        let mut dashboard = self.dashboards.get(id).cloned().unwrap_or_else(|| {
            log::error!("dashboard {id} vanished from the store before its layout could be saved");
            let mut fresh = Dashboard::new(name);
            fresh.id = id;
            fresh
        });
        dashboard.panes = panes;
        dashboard.layout = Some(layout);
        self.dashboards.upsert(dashboard);
        if let Err(err) = self.dashboards.save() {
            log::error!("could not write dashboards.json after capturing a layout: {err:#}");
            return;
        }
        log::info!("saved the current arrangement to dashboard {id}");
    }

    /// Shows the connection dialog with the saved profile `id` loaded into the
    /// form, ready to be changed.
    ///
    /// The sibling of [`Workspace::open_profile`], for the other thing a saved
    /// profile can be asked for: that one is on its way to a session and only
    /// shows the form when something is missing, while this one is the form.
    fn edit_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.close_overlays(cx);
        self.dialog
            .update(cx, |dialog, cx| dialog.edit_profile(id, cx));
        cx.notify();
    }

    /// Copies the saved profile `id` and shows the copy in the list.
    ///
    /// Routed through the dialog rather than through a store of the workspace's
    /// own, because there is only one store: the dialog holds it, and the empty
    /// state lists what the dialog holds.
    ///
    /// The same goes for [`Workspace::delete_profile`] below — one deletion, one
    /// code path — and with it goes the dialog's message strip, which is where
    /// either of them says that the list could not be written. From here that
    /// message has nowhere to appear; the log line the storage layer writes is
    /// what is left of it.
    fn duplicate_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.dialog
            .update(cx, |dialog, cx| dialog.duplicate_profile(id, cx));
        cx.notify();
    }

    /// Forgets the saved profile `id`, keychain entry and all.
    fn delete_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.dialog
            .update(cx, |dialog, cx| dialog.delete_profile(id, cx));
        cx.notify();
    }

    /// Shows the settings dialog.
    fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.close_overlays(cx);
        self.settings.update(cx, |dialog, cx| dialog.open(cx));
        cx.notify();
    }

    /// Shows the about dialog.
    fn open_about(&mut self, cx: &mut Context<Self>) {
        self.close_overlays(cx);
        self.about.update(cx, |dialog, cx| dialog.open(cx));
        cx.notify();
    }

    /// Asks GitHub for the latest release and shows the answer.
    ///
    /// Goes through `close_overlays` where the start-up check pointedly does
    /// not: this dialog was asked for, so it is entitled to the screen the way
    /// every other menu command is.
    ///
    /// Refuses while an install is already running, which is the one case where
    /// the update dialog cannot be closed and so must not be reopened into a
    /// different state. An install in *any* window counts — see
    /// [`Workspace::update_installing`].
    fn check_updates(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.update_installing(window, cx) {
            return;
        }
        self.close_overlays(cx);
        self.update.update(cx, |dialog, cx| dialog.start_check(cx));
        cx.notify();
    }

    /// Whether an update install is running, in this window or in any other.
    ///
    /// Asked of every window because the answer is about the process: the
    /// install rewrites the running image, so a second window must not start a
    /// download over one already being written.
    ///
    /// This window answers for itself and is left out of the sweep — see
    /// [`other_workspace_windows`] for why it has to be.
    fn update_installing(&self, window: &Window, cx: &App) -> bool {
        self.update.read(cx).is_busy() || installing_elsewhere(window, cx)
    }

    /// Shows or hides the file panel of the active tab.
    ///
    /// One command whichever session is active: a remote one browses the server
    /// over SFTP and a local one browses this computer, so every open session
    /// has a filesystem behind the panel and none of them is a reason to refuse.
    ///
    /// The active tab's flag and no other, which is the whole of what the
    /// command means now that the panel is opened per connection: a tab told to
    /// show the panel goes on showing it while the user works in the tab beside
    /// it, and the profile — or the setting a local shell follows — decides only
    /// where a tab starts, never where it stays.
    ///
    /// No tab is a reason to refuse. The welcome screen takes the place of the
    /// body the panel is drawn beside, so there is nothing to browse, nowhere to
    /// draw it, and no tab to write the answer on. The menu row greys out for
    /// the same reason, and this guard is what makes the shortcut and the macOS
    /// menu item agree with it.
    fn toggle_file_panel(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        tab.panel_open = !tab.panel_open;
        cx.notify();
    }

    /// Whether the file panel is standing beside the body as things are.
    ///
    /// The active tab's flag, and `false` when there is no active tab: the
    /// welcome screen takes the place of the body the panel would be drawn
    /// beside, so a window with nothing open is a window with no panel. Both
    /// render paths that turn on the panel ask here — the body, which puts it on
    /// screen, and the toolbar, whose button lights up to say that it is there —
    /// so that what the strip claims and what stands under it cannot come apart.
    fn panel_showing(&self) -> bool {
        self.tabs.get(self.active).is_some_and(|tab| tab.panel_open)
    }

    /// Tells the file panel which session it is looking at.
    ///
    /// Called from the render pass rather than from each of the eight places
    /// that can change the active pane, so there is no site left to forget. The
    /// panel compares the session against the one it already holds and returns
    /// without repainting when they match, which is every frame but the ones
    /// that actually switch.
    ///
    /// [`SessionTab::panel_session`] rather than the session the tab speaks for,
    /// so that switching to an open file does not empty the panel: the file came
    /// from a filesystem, and that filesystem is what the panel goes on showing
    /// beside it — which is how the next file is opened.
    fn sync_file_panel(&self, cx: &mut Context<Self>) {
        let session = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.panel_session(cx));
        self.panel
            .update(cx, |panel, cx| panel.set_session(session, cx));
    }

    /// Drops a closed session's browsing state from the file panel.
    fn forget_panel_session(&self, session: EntityId, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.forget_session(session, cx));
    }

    /// Shows or hides the application dropdown menu.
    fn set_menu_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.menu_open == open {
            return;
        }
        self.menu_open = open;
        cx.notify();
    }

    /// Shows or hides the tab strip's dropdown tab list.
    fn set_tab_menu_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.tab_menu_open == open {
            return;
        }
        self.tab_menu_open = open;
        cx.notify();
    }

    /// Opens the context menu of the tab at `index`, with its corner at `at`.
    ///
    /// The right-click that gets here does not change the active tab, so `index`
    /// and [`Workspace::active`] are independent — which is what the menu's
    /// commands are built around.
    fn open_tab_context(&mut self, index: usize, at: Point<Pixels>, cx: &mut Context<Self>) {
        if index >= self.tabs.len() || self.dialog_open(cx) {
            return;
        }
        // Not `close_overlays`: a modal dialog outranks the strip — the guard
        // above leaves it alone — while the other dropdowns are simply mutually
        // exclusive with this menu.
        self.menu_open = false;
        self.tab_menu_open = false;
        self.language_menu = None;
        self.charset_menu = None;
        self.tab_context = Some((index, at));
        cx.notify();
    }

    /// Puts the tab context menu away, if one is open.
    fn close_tab_context(&mut self, cx: &mut Context<Self>) {
        if self.tab_context.take().is_some() {
            cx.notify();
        }
    }

    /// Opens the status bar's file-type picker, with its foot at `at`.
    ///
    /// Guarded like [`Workspace::open_tab_context`]: a modal outranks the bar
    /// underneath it, and the other dropdowns are mutually exclusive with this
    /// one. Refused outright when the active pane is not a file, which is also
    /// when the trigger is not drawn — the guard is for the frame between a
    /// press and the pane changing under it.
    fn open_language_menu(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        if self.dialog_open(cx) || self.active_editor().is_none() {
            return;
        }
        self.menu_open = false;
        self.tab_menu_open = false;
        self.tab_context = None;
        self.charset_menu = None;
        self.language_menu = Some(at);
        cx.notify();
    }

    /// Puts the file-type picker away, if it is open.
    fn close_language_menu(&mut self, cx: &mut Context<Self>) {
        if self.language_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Opens the status bar's character-encoding picker, with its foot at `at`.
    ///
    /// Guarded exactly like [`Workspace::open_language_menu`], and it closes
    /// that one: the two triggers sit side by side on the bar, so opening this
    /// list while the other stood would leave two menus overlapping the button
    /// they both hang off.
    fn open_charset_menu(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        if self.dialog_open(cx) || self.active_editor().is_none() {
            return;
        }
        self.menu_open = false;
        self.tab_menu_open = false;
        self.tab_context = None;
        self.language_menu = None;
        self.charset_menu = Some(at);
        cx.notify();
    }

    /// Puts the character-encoding picker away, if it is open.
    fn close_charset_menu(&mut self, cx: &mut Context<Self>) {
        if self.charset_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Colours the active file as `language`.
    ///
    /// A no-op on a tab whose active pane is not a file, which is only reachable
    /// from a menu that outlived the pane it was opened over.
    fn set_active_language(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else {
            return;
        };
        editor.update(cx, |editor, cx| editor.set_language(id, cx));
        cx.notify();
    }

    /// Re-reads the active file in `charset`.
    ///
    /// A no-op on a tab whose active pane is not a file, for the same reason
    /// [`Workspace::set_active_language`] is. Unlike the language, this one can
    /// decline — an unsaved buffer, or bytes that are not text in the charset
    /// asked for — and it says so on the pane itself, where the file is.
    fn set_active_charset(&mut self, charset: Charset, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else {
            return;
        };
        editor.update(cx, |editor, cx| editor.set_charset(charset, cx));
        cx.notify();
    }

    /// The open file the keyboard is in, if the active pane is one.
    fn active_editor(&self) -> Option<&Entity<EditorPane>> {
        match self.tabs.get(self.active)?.active_view() {
            PaneView::Editor(editor) => Some(editor),
            PaneView::Terminal(_) | PaneView::Tail(_) => None,
        }
    }

    /// Opens the context menu of the saved profile `id`, with its corner at
    /// `at`.
    ///
    /// Guarded like [`Workspace::open_tab_context`], and for the same reasons:
    /// a modal outranks the empty state behind it, while the two dropdowns are
    /// simply mutually exclusive with this menu.
    fn open_empty_context(&mut self, id: Uuid, at: Point<Pixels>, cx: &mut Context<Self>) {
        if self.dialog_open(cx) {
            return;
        }
        self.menu_open = false;
        self.tab_menu_open = false;
        self.language_menu = None;
        self.charset_menu = None;
        self.empty_context = Some((id, at));
        cx.notify();
    }

    /// Puts the empty state's context menu away, if one is open.
    fn close_empty_context(&mut self, cx: &mut Context<Self>) {
        if self.empty_context.take().is_some() {
            cx.notify();
        }
    }

    /// Handles <kbd>Ctrl</kbd>/<kbd>Cmd</kbd> + <kbd>T</kbd>.
    fn new_session_action(&mut self, _: &NewSession, _window: &mut Window, cx: &mut Context<Self>) {
        self.open_dialog(cx);
    }

    /// Handles <kbd>Ctrl</kbd>/<kbd>Cmd</kbd> + <kbd>W</kbd>.
    ///
    /// Closes the active pane rather than the whole tab, the way a split editor
    /// or terminal does: on an unsplit tab the two are the same thing, and on a
    /// split one closing every pane in turn ends up closing the tab.
    fn close_session_action(
        &mut self,
        _: &CloseSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_active_pane(window, cx);
    }

    /// Handles the pane focus shortcut for the next pane.
    fn focus_next_pane_action(
        &mut self,
        _: &FocusNextPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_next_pane(window, cx);
    }

    /// Handles the pane focus shortcut for the previous pane.
    fn focus_prev_pane_action(
        &mut self,
        _: &FocusPrevPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_prev_pane(window, cx);
    }

    /// Handles the shortcut that pulls the active pane out into its own tab.
    fn break_out_pane_action(
        &mut self,
        _: &BreakOutPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.break_out_active_pane(window, cx);
    }

    /// Handles the shortcut that sends the active tab off into a window of its
    /// own.
    ///
    /// The active tab, where the tab context menu's row acts on the tab that was
    /// right-clicked; both end in the same call.
    fn move_tab_to_new_window_action(
        &mut self,
        _: &MoveTabToNewWindow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_tab_to_new_window(self.active, window, cx);
    }

    /// Handles the shortcut that splits the active pane to the right.
    fn duplicate_split_right_action(
        &mut self,
        _: &DuplicateSplitRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.duplicate_split(Axis::Horizontal, window, cx);
    }

    /// Handles the shortcut that splits the active pane downwards.
    fn duplicate_split_below_action(
        &mut self,
        _: &DuplicateSplitBelow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.duplicate_split(Axis::Vertical, window, cx);
    }

    /// Handles the command that squares the columns of the active tab up.
    fn equalize_widths_action(
        &mut self,
        _: &EqualizeWidths,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.equalize_panes(Axis::Horizontal, cx);
    }

    /// Handles the command that squares the rows of the active tab up.
    fn equalize_heights_action(
        &mut self,
        _: &EqualizeHeights,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.equalize_panes(Axis::Vertical, cx);
    }

    /// Handles the command that saves the active tab's arrangement to its
    /// dashboard.
    ///
    /// The active tab, where the tab context menu's row acts on the tab that
    /// was right-clicked; both end in the same call, which is a no-op on a tab
    /// that is not a dashboard.
    fn save_dashboard_layout_action(
        &mut self,
        _: &SaveDashboardLayout,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_tab_layout(self.active, cx);
    }

    /// Handles the shortcut that shows and hides the remote file panel.
    fn toggle_file_panel_action(
        &mut self,
        _: &ToggleFilePanel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_file_panel(cx);
    }

    /// Handles <kbd>Ctrl</kbd>/<kbd>Cmd</kbd> + <kbd>,</kbd>.
    fn open_settings_action(
        &mut self,
        _: &OpenSettings,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings(cx);
    }

    /// Handles the "About rulogman" menu item.
    fn show_about_action(&mut self, _: &ShowAbout, _window: &mut Window, cx: &mut Context<Self>) {
        self.open_about(cx);
    }

    /// Handles the "Check for updates" menu item.
    fn check_updates_action(
        &mut self,
        _: &CheckUpdates,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.check_updates(window, cx);
    }

    /// Handles <kbd>Ctrl</kbd>/<kbd>Cmd</kbd> + a digit.
    fn select_tab_action(
        &mut self,
        action: &SelectTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_tab(action.0, window, cx);
    }

    /// Handles <kbd>Ctrl</kbd>/<kbd>Cmd</kbd> + <kbd>Alt</kbd> + a digit.
    ///
    /// A digit past the end of the store does nothing at all — no log, no
    /// beep. Nine chords are bound whatever the user has saved, so most of them
    /// name nothing on most installations, and a shortcut that names nothing is
    /// not a mistake to report: it is a key that is simply not in use yet.
    fn open_dashboard_action(
        &mut self,
        action: &OpenDashboard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self
            .dashboards
            .dashboards()
            .get(action.0)
            .map(|dashboard| dashboard.id)
        else {
            return;
        };
        self.open_dashboard(id, window, cx);
    }

    /// Handles <kbd>Esc</kbd>: closes whichever overlay is open, or lets the key
    /// through to the terminal when none is.
    fn dismiss_dialog_action(
        &mut self,
        _: &DismissDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The dropdown menus paint above everything else, so they are dismissed
        // first. The file panel's menu is one of them even though the panel
        // owns it: it is drawn over the window like the rest, and the key
        // reaches this handler rather than the panel, which binds nothing.
        if self.tab_context.is_some() {
            self.close_tab_context(cx);
            return;
        }
        if self.empty_context.is_some() {
            self.close_empty_context(cx);
            return;
        }
        if self.panel.update(cx, |panel, cx| panel.close_context(cx)) {
            return;
        }
        if self.menu_open {
            self.set_menu_open(false, cx);
            return;
        }
        if self.tab_menu_open {
            self.set_tab_menu_open(false, cx);
            return;
        }
        // Ahead of the dialogs, because none of them can be open at the same
        // time as this one: `dialog_open` counts the question, so nothing else
        // opens over it. Escape is the cancelling answer — the pane stays.
        if self.close_confirm.is_some() {
            self.cancel_close_editor(cx);
            self.focus_active(window, cx);
            return;
        }
        // Beside it, and for the same reasons: nothing can be open over this
        // one either, and `Escape` is the answer that leaves the pane as it
        // stands — locked, or unsaved.
        if self.sudo_prompt.is_some() {
            self.cancel_sudo_password(window, cx);
            return;
        }
        if self.about.read(cx).is_open() {
            self.about.update(cx, |dialog, cx| dialog.close(cx));
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        if self.update.read(cx).is_open() {
            // Swallowed rather than propagated while an install runs: the key
            // must not reach the terminal, but nothing may take the screen from
            // a swap either, so `Escape` simply does nothing until it is over.
            if !self.update.read(cx).is_busy() {
                self.update.update(cx, |dialog, cx| dialog.close(cx));
                self.focus_active(window, cx);
                cx.notify();
            }
            return;
        }
        if self.dialog.read(cx).is_open() {
            // Route through `dismiss` rather than closing directly: the dialog
            // also binds `Escape` internally, and going through one path keeps
            // `Dismissed` firing exactly once no matter which handler wins the
            // dispatch. Closing and restoring focus is then the subscription's
            // job.
            self.dialog.update(cx, |dialog, cx| dialog.dismiss(cx));
            cx.notify();
            return;
        }
        if self.settings.read(cx).is_open() {
            self.settings.update(cx, |dialog, cx| dialog.close(cx));
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        cx.propagate();
    }

    /// Renders the toolbar: the application menu button and the tab strip.
    ///
    /// The button is left out on macOS, where [`app_menus`] puts the same
    /// commands in the system menu bar.
    ///
    /// In the custom title bar style this row *is* the title bar. It then marks
    /// itself as the window's drag area, takes over writing the application's
    /// name at its left end, and — off macOS, which keeps its native traffic
    /// lights — grows a set of caption buttons at its right end. Every
    /// *control* inside it occludes, so the drag area only ever answers for the
    /// gaps between them; see [`rugpui::window_controls`]. The name is not a
    /// control and deliberately does not.
    fn render_toolbar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let custom = chrome::draws_own_titlebar(chrome_style(self.titlebar), window);
        let menu = (!cfg!(target_os = "macos")).then(|| self.render_app_menu(window, cx));
        // Nothing to browse without a session, so the toggle goes with the panel
        // it would open. A session of either kind has a filesystem behind it —
        // the server's, or this computer's — so nothing finer is asked here.
        let toggle = self.tabs.get(self.active).is_some().then(|| {
            let open = self.panel_showing();
            let hover = theme.surface_hover;
            // The open state is already carried by the accent colour, so only
            // the closed button brightens on hover. The icon is tinted by its
            // own `text_color` rather than the button's, so the hover shade has
            // to reach it through the group.
            let hover_text = if open { theme.accent } else { theme.text };
            div()
                .id("toggle-file-panel")
                // The row behind it may be a window drag area; see
                // [`rugpui::window_controls`].
                .occlude()
                .group(PANEL_TOGGLE_GROUP)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(28.))
                .rounded_md()
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .on_click(cx.listener(|workspace, _, _window, cx| {
                    workspace.toggle_file_panel(cx);
                }))
                // The shortcut rides along, the way the dropdown row for the
                // same command carries it: this button is the only place the
                // binding is discoverable on macOS, where there is no in-app
                // menu to read it off.
                .tooltip(tooltip_label(ts!(
                    "files.tip_toggle",
                    shortcut = PANEL_SHORTCUT_LABEL
                )))
                .child(
                    icons::icon(
                        icons::PANEL,
                        px(16.),
                        if open { theme.accent } else { theme.icon },
                    )
                    .group_hover(PANEL_TOGGLE_GROUP, move |style| {
                        style.text_color(hover_text)
                    }),
                )
        });

        // One cell for the leading controls, so the menu button and the panel
        // toggle share the toolbar's fill and bottom hairline with the strip.
        let leading = (menu.is_some() || toggle.is_some()).then(|| {
            div()
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .gap(px(2.))
                .h(px(TOOLBAR_HEIGHT))
                .px(px(4.))
                .bg(theme.surface)
                .border_b_1()
                .border_color(theme.border)
                .children(menu)
                .children(toggle)
        });

        // Room for the traffic lights AppKit still draws over the transparent
        // title bar. Painted like the leading cell rather than left empty, so
        // the band reads as one strip. Fullscreen hides the buttons, and the
        // gap goes with them.
        let traffic_lights =
            (custom && cfg!(target_os = "macos") && !window.is_fullscreen()).then(|| {
                div()
                    .flex_none()
                    .w(px(TRAFFIC_LIGHT_GAP))
                    .h(px(TOOLBAR_HEIGHT))
                    .bg(theme.surface)
                    .border_b_1()
                    .border_color(theme.border)
            });

        // The application's own name, which only the custom style has to write:
        // a system title bar already carries it, and drawing it twice would put
        // it in two places at once.
        //
        // Windows and the GTK/KDE captions set an application icon beside the
        // title and macOS does not, so the mark follows that split.
        //
        // Nothing here is interactive, and — unlike every control in this row —
        // nothing here occludes either. The name and the mark are part of the
        // *empty* title bar as far as the window is concerned, so a press on
        // them has to reach the drag area underneath and move the window.
        let title = custom.then(|| {
            // The shipped icon in its own colours, not a theme-tinted sprite:
            // the current icon's embossed ring keeps its tile legible on dark
            // chrome, which is what used to force the tinted stand-in. See
            // [`icons::APP_ICON`].
            let icon = (!cfg!(target_os = "macos"))
                .then(|| img(icons::APP_ICON).w(px(16.)).h(px(16.)).flex_none());
            div()
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .h(px(TOOLBAR_HEIGHT))
                .px(px(10.))
                .bg(theme.surface)
                .border_b_1()
                .border_color(theme.border)
                // A shade quieter than a tab title, which is the one label in
                // this row that has to be read.
                .text_size(px(12.))
                .text_color(theme.text_muted)
                .children(icon)
                .child("rulogman")
        });

        // The caption buttons the other two platforms have to draw themselves,
        // as the two ends a Linux desktop may ask for: putting them on the left
        // is a setting people actually use, and the shell's own glyphs are what
        // they are drawn with.
        let (leading_controls, trailing_controls) = chrome::window_control_strips(
            &rugpui_shell::window_control_icons(),
            custom,
            window,
            cx,
        );

        div()
            .id("toolbar")
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .w_full()
            .h(px(TOOLBAR_HEIGHT))
            .when(custom, |this| {
                // Occluding is load-bearing, not just hygiene: the workspace
                // root tracks focus, and gpui's focus transfer marks every
                // mouse down over it `default_prevented` — which the Windows
                // backend reads as "the app took this press", swallowing the
                // `HTCAPTION` down that would have started the system drag.
                // Cutting the root's hitbox out from under the strip keeps the
                // press unclaimed, and spares the terminal a focus loss for a
                // click that was aimed at the window, not the app.
                chrome::titlebar_gestures(
                    this.occlude().window_control_area(WindowControlArea::Drag),
                )
            })
            // Ahead of the wordmark, which is where a desktop that asks for
            // left-hand caption buttons expects them: the buttons are the
            // window's, the name is the application's.
            .children(leading_controls)
            .children(traffic_lights)
            .children(title)
            .children(leading)
            .child(div().flex_1().min_w_0().child(self.render_tab_bar(cx)))
            .children(trailing_controls)
            .into_any_element()
    }

    /// Builds the dropdown menu shown on the platforms without a native one.
    ///
    /// Every row dispatches the action its keyboard shortcut dispatches, so the
    /// menu adds a way in rather than a second implementation.
    ///
    /// Splitting with a second connection is here, and so is breaking a pane
    /// out; merging a tab in is not, and cannot be: a merge needs a *source*
    /// tab, which a menu of static commands has no way to name. That one half of
    /// splitting lives in the tab context menu alone — see
    /// [`Workspace::render_tab_context`] — and the same asymmetry shapes
    /// [`app_menus`].
    ///
    /// The list is the same one on every frame, so a command that cannot run
    /// now is greyed rather than dropped — see [`MenuEntry::enabled`]. That is
    /// most of the menu on the welcome screen, where there is no pane to split,
    /// break out, or hang a file panel beside; only opening a session, the
    /// settings, the update check, the about box and quitting mean anything
    /// without one.
    fn render_app_menu(&self, window: &Window, cx: &mut Context<Self>) -> MenuButton {
        let this = cx.entity();
        let caps = self.pane_caps(cx);
        // A tab is what the file panel is drawn beside: the welcome screen
        // replaces the body the panel lives in, and there is no filesystem to
        // browse until a session opens one.
        let has_tab = self.tabs.get(self.active).is_some();
        // The same guard `check_updates` applies: an install already running
        // owns the dialog, which cannot be closed and so must not be reopened.
        let updating = self.update_installing(window, cx);
        let entries = vec![
            MenuEntry::new(ts!("menu.new_session"))
                .shortcut(format!("{SHORTCUT_MODIFIER}+T"))
                .on_activate(|window, cx| window.dispatch_action(Box::new(NewSession), cx)),
            // Next to the new session, because the two are the same command at
            // two sizes: one opens a tab here, the other a window of its own.
            MenuEntry::new(ts!("menu.new_window"))
                .shortcut(WINDOW_SHORTCUT_LABEL)
                .on_activate(|window, cx| window.dispatch_action(Box::new(NewWindow), cx)),
            MenuEntry::new(ts!("menu.duplicate_right"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+D"))
                .disabled(!caps.split_right)
                .on_activate(|window, cx| {
                    window.dispatch_action(Box::new(DuplicateSplitRight), cx)
                }),
            MenuEntry::new(ts!("menu.duplicate_below"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+S"))
                .disabled(!caps.split_below)
                .on_activate(|window, cx| {
                    window.dispatch_action(Box::new(DuplicateSplitBelow), cx)
                }),
            // Under the two splits, because they are what make the command
            // worth having: a third pane comes out half the width of the first,
            // and this is how it stops being.
            MenuEntry::new(ts!("menu.equalize_widths"))
                .disabled(!caps.equalize_widths)
                .on_activate(|window, cx| window.dispatch_action(Box::new(EqualizeWidths), cx)),
            MenuEntry::new(ts!("menu.equalize_heights"))
                .disabled(!caps.equalize_heights)
                .on_activate(|window, cx| window.dispatch_action(Box::new(EqualizeHeights), cx)),
            MenuEntry::new(ts!("menu.break_out_pane"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+B"))
                .disabled(!caps.break_out)
                .on_activate(|window, cx| window.dispatch_action(Box::new(BreakOutPane), cx)),
            // After the break-out, because it is the same command a size up: one
            // moves a pane out of its tab, the other a tab out of its window.
            MenuEntry::new(ts!("menu.tab_to_window"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+N"))
                .disabled(!tab_can_move_out(self.tabs.len()))
                .on_activate(|window, cx| window.dispatch_action(Box::new(MoveTabToNewWindow), cx)),
            MenuEntry::new(ts!("files.toggle"))
                .shortcut(PANEL_SHORTCUT_LABEL)
                .disabled(!has_tab)
                .on_activate(|window, cx| window.dispatch_action(Box::new(ToggleFilePanel), cx)),
            MenuEntry::new(ts!("menu.settings"))
                .shortcut(format!("{SHORTCUT_MODIFIER}+,"))
                .on_activate(|window, cx| window.dispatch_action(Box::new(OpenSettings), cx)),
            MenuEntry::separator(),
            // Next to About, where a Help menu would put it and where users of
            // every other desktop application look for it.
            MenuEntry::new(ts!("menu.check_updates"))
                .disabled(updating)
                .on_activate(|window, cx| window.dispatch_action(Box::new(CheckUpdates), cx)),
            MenuEntry::new(ts!("menu.about"))
                .on_activate(|window, cx| window.dispatch_action(Box::new(ShowAbout), cx)),
            MenuEntry::separator(),
            MenuEntry::new(ts!("menu.quit"))
                .shortcut(format!("{SHORTCUT_MODIFIER}+Q"))
                .on_activate(|window, cx| window.dispatch_action(Box::new(Quit), cx)),
        ];

        MenuButton::new("app-menu")
            .tooltip(ts!("menu.tip_menu"))
            .open(self.menu_open)
            .entries(entries)
            .on_open_change(move |open, _window, cx| {
                this.update(cx, |workspace, cx| workspace.set_menu_open(open, cx));
            })
    }

    /// Renders the tab strip.
    fn render_tab_bar(&self, cx: &mut Context<Self>) -> TabBar {
        let this = cx.entity();
        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                // A split tab is labelled after its active pane, so the strip
                // says what the user is looking at rather than what the tab
                // happened to be opened as. A tab holding nothing but open files
                // — which is what "Edit" opens — is named after the active one
                // of them and wears no status dot: the tab is not a connection,
                // so there is nothing for a dot to report on. See
                // [`editor_tab_label`] for what such a tab is called.
                // A tab that carries a name of its own — a dashboard — keeps
                // it in both arms: the name is the whole point of that tab, and
                // the status dot is still the active session's to report. See
                // [`SessionTab::label`].
                let named = tab.label.clone();
                match tab.active_session(cx) {
                    Some(session) => {
                        let session = session.read(cx);
                        let title = named.unwrap_or_else(|| session.title());
                        let item = TabItem::new(("session-tab", index), title)
                            .status(session.tab_status());
                        // Only the session that won the bind reports any, so
                        // the mark appears on exactly one tab per rule: the tab
                        // whose shell the forwarded traffic is actually riding
                        // on. Opening the same profile again leaves the second
                        // tab unmarked, which is the answer to the question the
                        // mark exists for.
                        match tunnel_tooltip(session.open_tunnels()) {
                            Some(tooltip) => item.mark(icons::TUNNEL, tooltip),
                            None => item,
                        }
                    }
                    None => TabItem::new(
                        ("session-tab", index),
                        named.unwrap_or_else(|| tab.active_view().label(cx)),
                    ),
                }
            })
            .collect();

        TabBar::new("session-tabs")
            .tabs(tabs)
            .active(self.active)
            .scroll_handle(&self.tab_scroll)
            .scrollbar(self.hovering_scrollbar(SCROLLBARS[0].0, Surface::Tabs, cx))
            .menu_icon(icons::TAB_LIST)
            .new_icon(icons::NEW_TAB)
            // The close button reuses the tab menu's own row: it is the same
            // command, worded the same way, and neither takes an ellipsis.
            .tooltips(
                ts!("tab.tip_list"),
                ts!("tab.tip_new", shortcut = format!("{SHORTCUT_MODIFIER}+T")),
                ts!("tab.close"),
            )
            .menu_open(self.tab_menu_open)
            .on_menu_open_change({
                let this = this.clone();
                move |open, _window, cx| {
                    this.update(cx, |workspace, cx| workspace.set_tab_menu_open(open, cx));
                }
            })
            .on_select({
                let this = this.clone();
                move |index, window, cx| {
                    this.update(cx, |workspace, cx| workspace.select_tab(index, window, cx));
                }
            })
            .on_close({
                let this = this.clone();
                move |index, window, cx| {
                    this.update(cx, |workspace, cx| workspace.close_tab(index, window, cx));
                }
            })
            .on_context_menu({
                let this = this.clone();
                move |index, at, _window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.open_tab_context(index, at, cx)
                    });
                }
            })
            .on_new(move |_window, cx| {
                this.update(cx, |workspace, cx| workspace.open_dialog(cx));
            })
    }

    /// Renders the context menu of a right-clicked tab, if one is open.
    ///
    /// The commands depend on which tab was clicked, because both of them are
    /// about the active tab:
    ///
    /// * on another tab, the menu merges *that* tab into the active one as a
    ///   split — the only way to bring an existing session in, and the reason
    ///   that half of splitting has no shortcut;
    /// * on the active tab, it splits the active pane off into a second
    ///   connection to the same host, and offers the reverse of a merge: moving
    ///   the active pane back out into a tab of its own, which needs the tab to
    ///   actually be split.
    ///
    /// One row is the exception and acts on the clicked tab wherever it sits:
    /// moving that tab into a window of its own, which is how a tab is dragged
    /// out of a crowded window without first bringing it to the front.
    ///
    /// A row whose command would be refused is left out rather than shown doing
    /// nothing, so the menu can come down to nothing but "close this tab".
    ///
    /// The rows come in three groups, separated in that order: rearranging the
    /// panes of the strip, opening a connection, and closing tabs. A group whose
    /// every row was left out contributes no rule of its own.
    fn render_tab_context(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let (index, position) = self.tab_context?;
        // The strip and the stored index are a frame apart: a tab can be gone by
        // now — closed from the menu itself, or by the session that owned it.
        let tab = self.tabs.get(index)?;
        let this = cx.entity();

        let mut splits = Vec::new();
        let mut break_out = Vec::new();
        if index == self.active {
            // A split that would leave an unusably small pane is refused, so the
            // row asking for it is left out rather than offered and ignored.
            if self.can_split_active(Axis::Horizontal, cx) {
                splits.push(
                    MenuEntry::new(ts!("tab.duplicate_right"))
                        .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+D"))
                        .on_activate(|window, cx| {
                            window.dispatch_action(Box::new(DuplicateSplitRight), cx)
                        }),
                );
            }
            if self.can_split_active(Axis::Vertical, cx) {
                splits.push(
                    MenuEntry::new(ts!("tab.duplicate_below"))
                        .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+S"))
                        .on_activate(|window, cx| {
                            window.dispatch_action(Box::new(DuplicateSplitBelow), cx)
                        }),
                );
            }
            // In the split group rather than a group of their own: they are
            // about the shape of the panes the splits above made.
            for (label, axis) in [
                (ts!("menu.equalize_widths"), Axis::Horizontal),
                (ts!("menu.equalize_heights"), Axis::Vertical),
            ] {
                if !self.can_equalize(axis) {
                    continue;
                }
                let this = this.clone();
                splits.push(MenuEntry::new(label).on_activate(move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.equalize_panes(axis, cx));
                }));
            }
            if self.can_break_out_active() {
                break_out.push(
                    MenuEntry::new(ts!("menu.break_out_pane"))
                        .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+B"))
                        .on_activate(|window, cx| {
                            window.dispatch_action(Box::new(BreakOutPane), cx)
                        }),
                );
            }
        } else {
            // A split that would leave an unusably small pane is refused, so the
            // row asking for it is left out rather than offered and ignored.
            for (label, axis) in [
                (ts!("tab.split_right"), Axis::Horizontal),
                (ts!("tab.split_below"), Axis::Vertical),
            ] {
                if !self.can_split_active(axis, cx) {
                    continue;
                }
                let this = this.clone();
                splits.push(MenuEntry::new(label).on_activate(move |window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.merge_tab_into_active(index, axis, window, cx);
                    });
                }));
            }
        }

        // The one command in this menu that acts on the clicked tab whether or
        // not it is the active one, which is what it is for: a tab is sent off
        // to a window of its own by right-clicking *it*, wherever the keyboard
        // happens to be. It closes whichever group is about rearranging the
        // strip — the break-out on the active tab, the splits on any other — and
        // it is left out on a window's only tab, for which see
        // [`tab_can_move_out`]. The chord is named only on the active tab,
        // because that is the tab it would act on.
        if tab_can_move_out(self.tabs.len()) {
            let this = this.clone();
            let mut row =
                MenuEntry::new(ts!("menu.tab_to_window")).on_activate(move |window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.move_tab_to_new_window(index, window, cx);
                    });
                });
            if index == self.active {
                row = row.shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+N"));
                break_out.push(row);
            } else {
                splits.push(row);
            }
        }

        // The one dashboard-specific command, and the primary way to reach it:
        // capture what this tab looks like now back onto the dashboard it was
        // opened from. Offered only on a dashboard tab — the only tab with
        // somewhere to save to — and acting on the clicked tab whether or not it
        // is the active one. The chord is named only on the active tab, because
        // that is the tab the shortcut would save.
        let mut dashboard_actions = Vec::new();
        if tab.dashboard.is_some() {
            let this = this.clone();
            let mut row = MenuEntry::new(ts!("tab.save_layout")).on_activate(move |_window, cx| {
                this.update(cx, |workspace, cx| workspace.save_tab_layout(index, cx));
            });
            if index == self.active {
                row = row.shortcut(format!("{SHORTCUT_MODIFIER}+Shift+L"));
            }
            dashboard_actions.push(row);
        }

        // Both rows speak for the session the tab label already names, which on
        // a split tab is the active pane's rather than the tab's first. A tab
        // holding nothing but open files names no session, and neither row means
        // anything without one.
        let mut connect = Vec::new();
        if let Some(entity) = tab.active_session(cx) {
            let session = entity.read(cx);
            connect.push(MenuEntry::new(ts!("tab.duplicate")).on_activate({
                let this = this.clone();
                move |window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.duplicate_tab(index, window, cx);
                    });
                }
            }));
            if !session.status().is_live() {
                // The same command the connection overlay's button carries,
                // worded the way that button words it: a local shell is started
                // again, not reconnected to.
                let label = if session.is_local() {
                    ts!("session.restart")
                } else {
                    ts!("session.reconnect")
                };
                let session = entity.clone();
                let this = this.clone();
                connect.push(MenuEntry::new(label).on_activate(move |window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.reconnect_session(&session, window, cx);
                    });
                }));
            }
            // The followed files of the profile this session came from, worded
            // and ordered exactly as the profile row's own menu words them:
            // a shell on a host is where the user is standing when they want a
            // log off that host, and having to go back to the welcome screen
            // for it would be a trip through a screen this window is not even
            // showing.
            //
            // Read out of the profile store rather than off the session, which
            // holds the profile as it was when the tab was opened: a file added
            // to the connection since then is a file the user has just asked
            // for, and a store lookup is what the empty state does too. A
            // session whose profile has since been forgotten simply offers no
            // rows.
            if let Some(profile) = session
                .profile_id()
                .and_then(|id| self.profile(id, cx))
                .filter(|profile| !profile.tails.is_empty())
            {
                for rule in &profile.tails {
                    let path = rule.path.clone();
                    let label = ts!(
                        "empty.menu_tail",
                        name = session::remote_file_name(&path).to_owned()
                    );
                    let this = this.clone();
                    let profile = profile.clone();
                    connect.push(MenuEntry::new(label).on_activate(move |window, cx| {
                        let (profile, path) = (profile.clone(), path.clone());
                        this.update(cx, |workspace, cx| {
                            workspace.open_tail(&profile, path, window, cx);
                        });
                    }));
                }
            }
        }

        // One row per followed file of every saved connection, which is the
        // group that closes the authoring loop: adding panes to a tab, dragging
        // the dividers between them and then *Save layout to dashboard* is how
        // a dashboard is composed by hand, and this is the only thing in the
        // application that can add the pane. Every connection rather than this
        // tab's — unlike the `connect` group above, which offers the files of
        // the session the tab already holds — because an arrangement worth
        // saving is usually one that spans hosts: the point of a dashboard is
        // the deploy watched across all of them at once. The list is as long as
        // the user's own configuration makes it and is not capped; a menu of
        // twenty rows is a configuration of twenty followed files, and hiding
        // some of them would only make the missing ones unreachable.
        //
        // Offered on the active tab alone, and gated exactly as the split rows
        // are: the size check can only answer for the *active* pane, so on any
        // other tab it would be measuring one pane and splitting another. A
        // pane already too small to split in half does not offer to be.
        let mut add_tails = Vec::new();
        if index == self.active && self.can_split_active(Axis::Vertical, cx) {
            for profile in self.dialog.read(cx).profiles() {
                for rule in &profile.tails {
                    let path = rule.path.clone();
                    let label = ts!(
                        "tab.add_tail",
                        file = session::remote_file_name(&path).to_owned(),
                        connection = profile.name.clone()
                    );
                    let this = this.clone();
                    let profile_id = profile.id;
                    add_tails.push(MenuEntry::new(label).on_activate(move |window, cx| {
                        let path = path.clone();
                        this.update(cx, |workspace, cx| {
                            workspace.add_tail_to_tab(index, profile_id, path, window, cx);
                        });
                    }));
                }
            }
        }

        let mut close = vec![MenuEntry::new(ts!("tab.close")).on_activate({
            let this = this.clone();
            move |window, cx| {
                this.update(cx, |workspace, cx| workspace.close_tab(index, window, cx));
            }
        })];
        if self.tabs.len() > 1 {
            close.push(MenuEntry::new(ts!("tab.close_others")).on_activate({
                let this = this.clone();
                move |window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.close_other_tabs(index, window, cx);
                    });
                }
            }));
        }
        if index + 1 < self.tabs.len() {
            close.push(MenuEntry::new(ts!("tab.close_right")).on_activate({
                let this = this.clone();
                move |window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.close_tabs_right(index, window, cx);
                    });
                }
            }));
        }

        let mut entries = Vec::new();
        for group in [
            splits,
            break_out,
            dashboard_actions,
            connect,
            add_tails,
            close,
        ] {
            if group.is_empty() {
                continue;
            }
            if !entries.is_empty() {
                entries.push(MenuEntry::separator());
            }
            entries.extend(group);
        }

        Some(
            ContextMenu::new("tab-context")
                .position(position)
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.close_tab_context(cx));
                }),
        )
    }

    /// Renders the context menu of an empty-state profile row, if one is open.
    ///
    /// Four rows and no conditions on them: every saved profile can be
    /// connected to, edited, copied and forgotten, whatever it holds. What can
    /// go is the profile itself — the store is re-read whenever the dialog
    /// opens, and this menu can outlive the row that opened it — in which case
    /// there is nothing left for the menu to speak for and it draws nothing.
    fn render_empty_context(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let (id, position) = self.empty_context?;
        let profile = self.profile(id, cx)?;
        let this = cx.entity();

        let mut entries = vec![MenuEntry::new(ts!("connection.connect")).on_activate({
            let this = this.clone();
            let profile = profile.clone();
            move |window, cx| {
                let profile = profile.clone();
                this.update(cx, |workspace, cx| {
                    workspace.open_profile(&profile, window, cx);
                });
            }
        })];

        // One row per file the profile follows, straight under *Connect*,
        // because that is what they are: a second way to open this connection,
        // pointed at a file rather than at a shell. They are named after the
        // file rather than after the path — a menu row is one line wide and the
        // last component is what tells two logs apart — and the whole path is
        // read in the pane the row opens, where there is room for it.
        for rule in &profile.tails {
            let path = rule.path.clone();
            let label = ts!(
                "empty.menu_tail",
                name = session::remote_file_name(&path).to_owned()
            );
            let this = this.clone();
            let profile = profile.clone();
            entries.push(MenuEntry::new(label).on_activate(move |window, cx| {
                let (profile, path) = (profile.clone(), path.clone());
                this.update(cx, |workspace, cx| {
                    workspace.open_tail(&profile, path, window, cx);
                });
            }));
        }

        entries.extend([
            // The ellipsis the dialog's own Edit button does without: from here
            // the form is not on screen yet, so this row promises it.
            MenuEntry::new(ts!("empty.menu_edit")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.edit_profile(id, cx));
                }
            }),
            MenuEntry::new(ts!("connection.duplicate")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.duplicate_profile(id, cx));
                }
            }),
            MenuEntry::separator(),
            MenuEntry::new(ts!("connection.delete")).on_activate({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.delete_profile(id, cx));
                }
            }),
        ]);

        Some(
            ContextMenu::new("empty-profile-context")
                .position(position)
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.close_empty_context(cx));
                }),
        )
    }

    /// Renders the question asked before an edited file is thrown away.
    ///
    /// Three answers, and "Save" is the one that does not finish here. A save is
    /// a transfer that can fail — over a session that may well be the reason the
    /// pane is being closed — so rather than hold the question up over a write
    /// crossing the network, the dialog goes down on the press and the pane
    /// carries on from there: it says "saving…" in its header as it does for
    /// every other save, and closes itself only once the bytes are on the far
    /// end. A failure keeps the pane, with the reason in the strip beneath the
    /// file, which is the one place a reason belongs; the question is not asked
    /// again, because nothing about the file has changed since it was answered.
    fn render_close_confirm(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pane = self.close_confirm?;
        // The pane can have gone since the question was asked; so has the
        // question, then.
        let name =
            self.tabs
                .iter()
                .find_map(|tab| match tab.panes.get(pane).map(|leaf| &leaf.view) {
                    Some(PaneView::Editor(editor)) => Some(editor.read(cx).name().clone()),
                    _ => None,
                })?;

        let theme = theme(cx);
        let this = cx.entity();
        let body = div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.text)
                    .child(ts!("editor.close_unsaved", name = name.to_string())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("editor-close-cancel", ts!("editor.close_cancel"))
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                workspace.cancel_close_editor(cx);
                                // Straight back into the file the question was
                                // about, which is where the caret already was.
                                workspace.focus_active(window, cx);
                            })),
                    )
                    .child(
                        Button::new("editor-close-discard", ts!("editor.close_discard"))
                            .variant(ButtonVariant::Danger)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                workspace.confirm_close_editor(window, cx);
                            })),
                    )
                    // Last, where every dialog in the application puts the
                    // answer it expects — and the one place the destructive
                    // button must not be, since that is where a hurried hand
                    // goes.
                    .child(
                        Button::new("editor-close-save", ts!("common.save"))
                            .variant(ButtonVariant::Primary)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                workspace.save_and_close_editor(window, cx);
                            })),
                    ),
            );

        Some(
            div()
                .absolute()
                .inset_0()
                .child(modal(
                    "editor-close-confirm",
                    ts!("editor.close_title"),
                    px(400.),
                    body,
                    move |window, cx| {
                        this.update(cx, |workspace, cx| {
                            workspace.cancel_close_editor(cx);
                            workspace.focus_active(window, cx);
                        });
                    },
                ))
                .into_any_element(),
        )
    }

    /// Renders the password an elevated save is waiting on.
    ///
    /// Built like the close question above — the same [`modal`], the same
    /// button row with the expected answer last — and different in the one way
    /// that matters: this dialog can be answered *wrongly*, so it does not
    /// always go down on the press. A refusal comes back into it, under the
    /// field, and the field keeps what was typed so a mistyped character is
    /// corrected rather than retyped. See
    /// [`Workspace::submit_sudo_password`] for which answers can fail.
    ///
    /// The prompt names the file, because a window can hold several open ones
    /// and the dialog covers whichever pane it belongs to.
    fn render_sudo_prompt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self.sudo_prompt.as_ref()?;
        let theme = theme(cx);
        let this = cx.entity();
        let name = prompt.pane.read(cx).name().to_string();

        let body = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.text)
                    .child(ts!("editor.sudo_prompt", name = name)),
            )
            .child(prompt.input.clone())
            .child(
                Checkbox::new("editor-sudo-remember", ts!("editor.sudo_remember"))
                    .checked(prompt.remember)
                    .tab_index(1)
                    .on_toggle({
                        let this = this.clone();
                        move |checked, _window, cx| {
                            this.update(cx, |workspace, cx| {
                                if let Some(prompt) = &mut workspace.sudo_prompt {
                                    prompt.remember = checked;
                                }
                                cx.notify();
                            });
                        }
                    }),
            )
            // The host's own words, in the host's own language, and drawn in
            // the colour the pane draws a failed save in — because that is what
            // this is, caught early enough to try again.
            .children(prompt.error.clone().map(|error| {
                div()
                    .text_size(px(12.))
                    .text_color(theme.danger)
                    .child(error)
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("editor-sudo-cancel", ts!("common.cancel"))
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                workspace.cancel_sudo_password(window, cx);
                            })),
                    )
                    .child(
                        Button::new("editor-sudo-confirm", ts!("common.ok"))
                            .variant(ButtonVariant::Primary)
                            // While an attempt is in flight there is nothing to
                            // press: the answer is on its way and a second one
                            // would only queue behind it.
                            .disabled(prompt.busy)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                workspace.confirm_sudo_password(window, cx);
                            })),
                    ),
            );

        Some(
            div()
                .absolute()
                .inset_0()
                .child(modal(
                    "editor-sudo-prompt",
                    ts!("editor.sudo_title"),
                    px(400.),
                    body,
                    move |window, cx| {
                        this.update(cx, |workspace, cx| {
                            workspace.cancel_sudo_password(window, cx);
                        });
                    },
                ))
                .into_any_element(),
        )
    }

    /// Renders the panes of the active tab, or the empty state.
    fn render_body(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.tabs.get(self.active) else {
            return self.render_empty_state(cx);
        };

        let theme = theme(cx);
        let panel_open = self.panel_showing();
        // A lone terminal with nothing beside it is drawn exactly as it was
        // before panes existed: no frame, no divider, the terminal filling the
        // body. Once it is split, or once the file panel is open next to it,
        // there is a second thing that can hold the keyboard and the frame has
        // to be there to say which one does.
        let frame = tab.panes.leaf_count() > 1 || panel_open;
        // Asked of the focus tree at render time for the same reason the panel
        // asks it — see `FilePanel::render`. Only one of the two frames wears
        // the accent, so the active pane gives its own up while the panel has
        // the keyboard.
        let panel_focused = panel_open && self.panel.focus_handle(cx).contains_focused(window, cx);
        let active = tab.active_pane();
        let root = tab.panes.root();
        let panel = panel_open.then(|| self.panel.clone());

        div()
            .flex()
            .flex_row()
            .flex_grow_1()
            .min_w_0()
            .min_h_0()
            .children(panel)
            .child(div().flex().flex_1().min_w_0().min_h_0().child(render_pane(
                root,
                active,
                frame,
                panel_focused,
                &theme,
                cx,
            )))
            .into_any_element()
    }

    /// Evens the panes of the active tab out along `axis`.
    ///
    /// The counterpart of dragging every divider by hand, which is what a tab
    /// split more than twice otherwise needs: splitting the same pane twice
    /// leaves the first half twice the width of the other two, and no sequence
    /// of splits produces even thirds on its own.
    ///
    /// Only the dividers along `axis` move, so a width pass leaves a stacked
    /// pair inside a column exactly where the user dragged it; the arithmetic
    /// is [`PaneTree::equalize`]'s. Each pane hears about its new size the way
    /// it hears about a window resize — the grid is measured on the next frame
    /// and the pty told only if the cell count actually changed — so there is
    /// nothing to push from here.
    pub(crate) fn equalize_panes(&mut self, axis: Axis, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.panes.equalize(axis) {
            cx.notify();
        }
    }

    /// Whether the active tab has a divider an `axis` pass would move.
    ///
    /// A tab with no split along that axis has nothing to even out — one pane,
    /// or panes stacked the other way — and the rows offering it are greyed or
    /// left out rather than shown doing nothing. A tab whose panes are *already*
    /// even still offers the command: the answer would flicker as a divider is
    /// dragged, and running it there costs nothing.
    fn can_equalize(&self, axis: Axis) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| tab.panes.splits_along(axis) > 0)
    }

    /// Records where the divider of `split` has been dragged to.
    ///
    /// The share arrives from [`Splitter`] already measured against the split's
    /// own box, already clamped short of either edge and already a number, so
    /// there is nothing to sanitise here — only a tab to find. It is looked up
    /// now rather than captured when the divider was drawn, because the active
    /// tab can change between the frame that drew the handle and this event.
    fn set_split_ratio(&mut self, split: SplitId, ratio: f32, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.panes.set_ratio(split, ratio) {
            cx.notify();
        }
    }

    /// One surface's scroll offset and the state of the bar over it.
    ///
    /// The pair is what every handler below works on, and taking it by one
    /// lookup is what lets them be written once for both surfaces rather than
    /// once each.
    fn surface(&mut self, surface: Surface) -> (&ScrollHandle, &mut ScrollbarState) {
        match surface {
            Surface::Tabs => (&self.tab_scroll, &mut self.tab_scrollbar),
            Surface::Empty => (&self.empty_scroll, &mut self.empty_scrollbar),
        }
    }

    /// The same pair, for the render paths that only read it.
    fn surface_ref(&self, surface: Surface) -> (&ScrollHandle, &ScrollbarState) {
        match surface {
            Surface::Tabs => (&self.tab_scroll, &self.tab_scrollbar),
            Surface::Empty => (&self.empty_scroll, &self.empty_scrollbar),
        }
    }

    /// One surface's overlay scroll indicator, as it stands.
    ///
    /// Rebuilt on demand rather than kept, because everything it is made of —
    /// the surface's box, how far it overflows, where it sits — is measured
    /// afresh by gpui on every layout pass.
    fn scrollbar(&self, id: &'static str, surface: Surface) -> Scrollbar {
        let (handle, state) = self.surface_ref(surface);
        Scrollbar::for_handle(id, surface.axis(), handle).fade(state.fade())
    }

    /// The same bar, listening for the pointer reaching the edge it rides.
    ///
    /// Only the bars that are drawn need it: the ones the drag path builds are
    /// there to be measured, and never reach an element tree.
    fn hovering_scrollbar(
        &self,
        id: &'static str,
        surface: Surface,
        cx: &mut Context<Self>,
    ) -> Scrollbar {
        self.scrollbar(id, surface).on_hover(cx.listener(
            move |workspace, hovered: &bool, _window, cx| {
                workspace.hover_scrollbar(surface, *hovered, cx);
            },
        ))
    }

    /// Puts each surface's bar up whenever that surface has moved, and starts
    /// the clock that takes it down again.
    ///
    /// Called from `render` because that is where every way of scrolling them
    /// meets: a wheel over the tabs or the empty state, and the jump that brings
    /// a newly activated tab back into view.
    fn watch_scroll(&mut self, cx: &mut Context<Self>) {
        for (_, surface) in SCROLLBARS {
            let (handle, state) = self.surface(surface);
            let scrolled = scrolled(handle, surface.axis());
            if let Some(epoch) = state.moved(scrolled) {
                hide_later(epoch, cx, move |workspace| {
                    Some(workspace.surface(surface).1)
                });
            }
        }
    }

    /// Scrolls whichever surface's thumb has been dragged.
    ///
    /// Every element listening for this drag type hears every such drag, so each
    /// bar checks that the one being dragged is its own before answering.
    fn drag_scrollbar(&mut self, event: &DragMoveEvent<DraggedThumb>, cx: &mut Context<Self>) {
        for (id, surface) in SCROLLBARS {
            let Some(progress) = self.scrollbar(id, surface).dragged(event, cx) else {
                continue;
            };

            // Held even when the pointer moved along the other axis and the
            // surface has not budged: the bar has to stay up for as long as it
            // is being held, and a still pointer moves nothing to notice.
            let (handle, state) = self.surface(surface);
            state.hold();
            scroll_to(handle, surface.axis(), progress);
            cx.notify();
            return;
        }
    }

    /// Lets go of whichever thumb was being held, and starts its clock again.
    ///
    /// Every mouse release in the window arrives here; all but the one ending a
    /// drag of a bar find nothing to let go of.
    fn release_scrollbars(&mut self, cx: &mut Context<Self>) {
        for (_, surface) in SCROLLBARS {
            if let Some(epoch) = self.surface(surface).1.release() {
                hide_later(epoch, cx, move |workspace| {
                    Some(workspace.surface(surface).1)
                });
                cx.notify();
            }
        }
    }

    /// Puts one surface's bar up while the pointer rests on the edge it rides,
    /// and starts it going the moment the pointer leaves.
    ///
    /// Told which surface rather than asked to work it out: each strip carries
    /// this listener already and knows only its own.
    fn hover_scrollbar(&mut self, surface: Surface, hovered: bool, cx: &mut Context<Self>) {
        let state = self.surface(surface).1;
        if hovered {
            if state.hover_enter() {
                cx.notify();
            }
            return;
        }

        let Some(epoch) = state.hover_leave() else {
            return;
        };
        hide_now(self, epoch, cx, move |workspace| {
            Some(workspace.surface(surface).1)
        });
    }

    /// Renders the placeholder shown while no session is open.
    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let this = cx.entity();
        let profiles = self.dialog.read(cx).profiles();

        // Above the saved profiles, and deliberately: a dashboard is one click
        // to every log a deploy is watched through, while a profile below it is
        // one click to one shell. The aggregate is the bigger thing to be
        // offered, so it is offered first.
        let dashboards = (!self.dashboards.is_empty()).then(|| {
            let rows = self
                .dashboards
                .dashboards()
                .iter()
                .enumerate()
                .map(|(index, dashboard)| {
                    let id = dashboard.id;
                    Button::new(
                        ElementId::from(("dashboard", index)),
                        dashboard.name.clone(),
                    )
                    .variant(ButtonVariant::Ghost)
                    .full_width(true)
                    .on_click({
                        let this = this.clone();
                        move |_, window, cx| {
                            this.update(cx, |workspace, cx| {
                                workspace.open_dashboard(id, window, cx)
                            });
                        }
                    })
                })
                .collect::<Vec<_>>();

            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .w(px(320.))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.text_muted)
                        .child(ts!("empty.dashboards")),
                )
                .children(rows)
        });

        let saved = (!profiles.is_empty()).then(|| {
            let rows = profiles.into_iter().enumerate().map(|(index, profile)| {
                let id = ElementId::from(("saved-profile", index));
                let label = format!("{}  ·  {}", profile.name, profile.label());
                let profile_id = profile.id;
                let button = Button::new(id, label)
                    .variant(ButtonVariant::Ghost)
                    .full_width(true)
                    .on_click({
                        let this = this.clone();
                        move |_, window, cx| {
                            this.update(cx, |workspace, cx| {
                                workspace.open_profile(&profile, window, cx)
                            });
                        }
                    });

                // The right-click is answered by a wrapper rather than by the
                // button, which takes clicks and nothing else: a `Button` is
                // the application's one push control and has no business
                // growing a menu hook for the single place that wants one.
                div()
                    .id(ElementId::from(("saved-profile-row", index)))
                    .w_full()
                    .on_mouse_down(MouseButton::Right, {
                        let this = this.clone();
                        move |event: &MouseDownEvent, _window, cx| {
                            cx.stop_propagation();
                            this.update(cx, |workspace, cx| {
                                workspace.open_empty_context(profile_id, event.position, cx);
                            });
                        }
                    })
                    .child(button)
            });

            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .w(px(320.))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.text_muted)
                        .child(ts!("empty.saved_profiles")),
                )
                .children(rows)
        });

        let local = self.render_empty_local(cx);
        let shortcut = ts!("empty.hint", shortcut = format!("{SHORTCUT_MODIFIER}+T"));
        let bar = self.hovering_scrollbar(SCROLLBARS[1].0, Surface::Empty, cx);

        let content = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(14.))
            .child(
                div()
                    .text_size(px(30.))
                    .text_color(theme.text)
                    .child("rulogman"),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.text_muted)
                    .child(shortcut),
            )
            .child(
                div().w(px(320.)).child(
                    Button::new("empty-new-session", ts!("menu.new_session"))
                        .full_width(true)
                        .on_click({
                            let this = this.clone();
                            move |_, _window, cx| {
                                this.update(cx, |workspace, cx| workspace.open_dialog(cx));
                            }
                        }),
                ),
            )
            .children(local)
            .children(dashboards)
            .children(saved);

        // The fill goes on the box the helper hands back, which is the whole of
        // the body: the tint has to cover it however little of it the column
        // reaches, this being the only fill over the body while no session is
        // open and so where the window opacity lands on the empty state.
        centered_scroll(EMPTY_STATE, &self.empty_scroll, bar, &theme, content)
            .bg(app_settings::window_tint(theme.background, cx))
            .into_any_element()
    }

    /// The empty state's local terminal buttons.
    ///
    /// Sits between the button that opens the connection dialog and the saved
    /// profiles, and unlike either of them it opens a session outright rather
    /// than a dialog: a local shell has no host, no credentials and nothing to
    /// save, so there is nothing for a dialog to ask. The shell's name rides
    /// along after a separator, exactly as a profile row carries its
    /// `user@host`, so each button says which shell the press will start.
    ///
    /// Unix has one of them, the login shell, and so needs no choosing.
    /// Windows has as many as it has shells to start — PowerShell, `cmd`, and
    /// one per installed WSL distribution — and the WSL ones appear only once
    /// the discovery started in [`Workspace::new`] has answered, so this can
    /// return one button on one frame and four on the next.
    fn render_empty_local(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        #[cfg(windows)]
        {
            let this = cx.entity();

            // The same list the connection dialog pins above its saved
            // profiles, built from the one place that knows how each of these
            // shells is started — a button that opened a shell the dialog does
            // not offer, or the other way round, would be a difference between
            // two ways of asking for the same thing.
            //
            // A WSL entry labels itself `WSL` rather than as another local
            // terminal: the shell it opens is a Linux one on a filesystem of
            // its own, which is a different place to be than the two above it
            // — and that difference travels into the session, so that its file
            // panel browses the distribution the shell is standing in rather
            // than this machine's disk.
            let rows = session::local_shells(&self.wsl_distros)
                .into_iter()
                .enumerate()
                .map(|(index, shell)| {
                    let this = this.clone();
                    let text = format!("{}  ·  {}", shell.kind_label(), shell.name);
                    Button::new(("empty-local", index), text)
                        .variant(ButtonVariant::Secondary)
                        .full_width(true)
                        .on_click(move |_, window, cx| {
                            // Cloned per press rather than moved: the handler is
                            // kept for the life of the button and may be pressed
                            // again, opening a second tab on the same shell.
                            let (label, command, filesystem) = (
                                shell.name.clone(),
                                shell.command.clone(),
                                shell.filesystem.clone(),
                            );
                            this.update(cx, |workspace, cx| {
                                workspace.open_local_command(label, command, filesystem, window, cx)
                            });
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>();

            Some(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .w(px(320.))
                    .children(rows)
                    .into_any_element(),
            )
        }

        #[cfg(unix)]
        {
            let this = cx.entity();
            let label = format!(
                "{}  ·  {}",
                ts!("connection.local.name"),
                rulogman_pty::login_shell_name()
            );
            Some(
                div()
                    .w(px(320.))
                    .child(
                        Button::new("empty-local-session", label)
                            .variant(ButtonVariant::Secondary)
                            .full_width(true)
                            .on_click(move |_, window, cx| {
                                this.update(cx, |workspace, cx| {
                                    workspace.open_local_session(window, cx)
                                });
                            }),
                    )
                    .into_any_element(),
            )
        }
    }

    /// Renders the file-type picker, if it is open.
    ///
    /// Anchored by its **bottom** left corner, which is the whole reason
    /// [`ContextMenu::anchor`] exists: the trigger sits in the last two dozen
    /// pixels of the window, so a list hanging down from it would be snapped
    /// back over the point it was opened from and cover the answer it is asking
    /// about. Standing it on the pointer opens it into the window instead.
    ///
    /// Narrower than the application's own menus as well. These rows are one
    /// word each — `JSON`, `Rust` — and the width that fits "Split right of
    /// current tab" reads as a dialog that lost its contents.
    ///
    /// The list is the language registry every time it is built rather than
    /// once: building it on the press is what keeps this from being a second
    /// copy of that table.
    fn render_language_menu(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let position = self.language_menu?;
        // The picker acts on the active pane, so a pane that stopped being a
        // file while the menu stood leaves nothing for the rows to pick for.
        self.active_editor()?;
        let this = cx.entity();

        // Every row is live, the one already in force included. A greyed row
        // runs nothing *and* leaves the menu standing — which is right for a
        // command that cannot be run and wrong for a list of answers, where the
        // obvious way to say "never mind" is to pick what is already picked. The
        // current language needs no mark of its own either: it is written on the
        // button this list is standing on, a row's height below it.
        let entries = languages::registry(cx)
            .all()
            .iter()
            .map(|entry| {
                let this = this.clone();
                let id = entry.id.clone();
                MenuEntry::new(language_label(entry)).on_activate(move |_window, cx| {
                    let id = id.clone();
                    this.update(cx, |workspace, cx| {
                        workspace.set_active_language(&id, cx);
                    });
                })
            })
            .collect();

        Some(
            ContextMenu::new("language-menu")
                .position(position)
                .anchor(Anchor::BottomLeft)
                .width(px(LANGUAGE_MENU_WIDTH))
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.close_language_menu(cx));
                }),
        )
    }

    /// Builds the status bar's character-encoding picker, if it is open.
    ///
    /// The file-type picker's twin in every mechanical respect — see
    /// [`Workspace::render_language_menu`] for why it stands on the pointer and
    /// grows upward, why every row is live and why none of them is marked. The
    /// rows are [`Charset::SUPPORTED`] and are not translated: `EUC-KR` and
    /// `windows-1252` are the names of the encodings themselves, and the same
    /// width fits them as fits `Dockerfile`.
    fn render_charset_menu(&self, cx: &mut Context<Self>) -> Option<ContextMenu> {
        let position = self.charset_menu?;
        self.active_editor()?;
        let this = cx.entity();

        let entries = Charset::SUPPORTED
            .into_iter()
            .map(|charset| {
                let this = this.clone();
                MenuEntry::new(SharedString::new_static(charset.name())).on_activate(
                    move |_window, cx| {
                        this.update(cx, |workspace, cx| {
                            workspace.set_active_charset(charset, cx);
                        });
                    },
                )
            })
            .collect();

        Some(
            ContextMenu::new("charset-menu")
                .position(position)
                .anchor(Anchor::BottomLeft)
                .width(px(LANGUAGE_MENU_WIDTH))
                .entries(entries)
                .on_dismiss(move |_window, cx| {
                    this.update(cx, |workspace, cx| workspace.close_charset_menu(cx));
                }),
        )
    }

    /// Renders the right end of the status bar while the keyboard is in a file:
    /// what it is being coloured as, what it is being decoded as, and where the
    /// caret is in it.
    ///
    /// The first two are controls and the position is not, which is why only
    /// they take a hover and a pointer cursor. The chevron points *up* because
    /// that is where the list opens, and it is what says the name is a button at
    /// all — a status bar is otherwise a place where nothing can be clicked.
    ///
    /// The order is the order they were added in: the file type keeps the place
    /// it has always had, the encoding takes the one next to it, and the caret
    /// stays at the far right where a number belongs. The two pickers are
    /// neighbours because they answer the same kind of question — how this file
    /// is being read — and neither is worth hunting for at the other end of the
    /// bar.
    fn render_editor_status(
        &self,
        editor: &Entity<EditorPane>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let pane = editor.read(cx);
        let registry = languages::registry(cx);
        // An id the registry has never heard of cannot happen — the pane took
        // its own out of this table — but a button with nothing written on it
        // would be the worst possible way to find that out, so the id stands in.
        let language = registry.get(pane.language()).map_or_else(
            || SharedString::from(pane.language().to_string()),
            language_label,
        );
        let charset = SharedString::new_static(pane.charset().name());
        let (line, lines, column) = pane.caret_summary(cx);
        let this = cx.entity();

        // One recipe for both triggers, so they cannot drift apart on the bar:
        // the press handler is all that differs, and it is chained on after.
        let trigger = |id: &'static str, label: SharedString, tip: SharedString| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .gap(px(4.))
                .h(px(18.))
                .px(px(6.))
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme.surface_hover).text_color(theme.text))
                .tooltip(tooltip_label(tip))
                .child(div().whitespace_nowrap().child(label))
                .child(div().flex_none().text_size(px(8.)).child(CHEVRON_UP))
        };

        let open_language = this.clone();
        vec![
            trigger("status-language", language, ts!("editor.language_tip"))
                // On the press rather than on the click, so the list is up by
                // the time the button comes back up — the same moment every
                // other menu in the window opens at, and the reason a second
                // press lands on the backdrop and closes it again.
                .on_mouse_down(
                    MouseButton::Left,
                    move |event: &MouseDownEvent, _window, cx| {
                        let at = event.position;
                        open_language
                            .update(cx, |workspace, cx| workspace.open_language_menu(at, cx));
                    },
                )
                .into_any_element(),
            trigger("status-charset", charset, ts!("editor.charset_tip"))
                .on_mouse_down(
                    MouseButton::Left,
                    move |event: &MouseDownEvent, _window, cx| {
                        let at = event.position;
                        this.update(cx, |workspace, cx| workspace.open_charset_menu(at, cx));
                    },
                )
                .into_any_element(),
            div()
                .flex_none()
                .whitespace_nowrap()
                .child(caret_summary(line, lines, column))
                .into_any_element(),
        ]
    }

    /// Renders the bottom status bar.
    fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let (target, status, grid): (SharedString, SharedString, SharedString) =
            match self.tabs.get(self.active) {
                // The active pane, not the tab: on a split tab the bar reports
                // the session the keyboard is aimed at. A tab holding nothing
                // but open files reports as no session at all rather than
                // inventing a state for one that has gone.
                Some(tab) => match tab.active_session(cx) {
                    Some(session) => {
                        let session = session.read(cx);
                        let (cols, rows) = session.terminal().size();
                        (
                            session.label(),
                            session.status().summary(),
                            format!("{cols}x{rows}").into(),
                        )
                    }
                    None => (
                        ts!("statusbar.no_session"),
                        ts!("statusbar.idle"),
                        SharedString::new_static("-"),
                    ),
                },
                None => (
                    ts!("statusbar.no_session"),
                    ts!("statusbar.idle"),
                    // A dash standing in for the grid size: punctuation, not a
                    // word, so it is the same in every language.
                    SharedString::new_static("-"),
                ),
            };

        // The left of the bar speaks for the tab's session and the right for
        // the *pane*: a terminal reports the grid it is drawing, and a file
        // reports what it is being coloured as and where the caret is. The two
        // are never both worth showing, since only one surface has the keyboard.
        let trailing = match self.active_editor().cloned() {
            Some(editor) => self.render_editor_status(&editor, &theme, cx),
            None => vec![
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .child(grid)
                    .into_any_element(),
            ],
        };

        div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(14.))
            .h(px(24.))
            .px(px(10.))
            // The bar is inert, so a press on it must not move the keyboard.
            // Without this the workspace root's `track_focus` would claim the
            // click, and the accent frame would jump to the active pane even
            // though no pane received focus.
            .on_any_mouse_down(|_, window, _cx| window.prevent_default())
            .bg(theme.surface)
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(11.))
            .text_color(theme.text_muted)
            .child(div().flex_none().whitespace_nowrap().child(target))
            // The status summary carries the failure reason, which can be far
            // wider than the window; letting it shrink and ellipsize keeps the
            // right end of the bar pinned to the right edge instead of pushing
            // it out.
            .child(div().flex_1().min_w_0().truncate().child(status))
            .children(trailing)
            .into_any_element()
    }
}

/// A box that keeps `content` in the middle while it fits, and lets it be
/// scrolled from the top once it does not.
///
/// `justify_center` does the first half and ruins the second. With more content
/// than room, a centred column hangs off both ends of its box, and scrolling
/// only ever reaches what lies past the *end* of one — so the head of the column
/// goes off the top edge and stays there, unreachable. Automatic margins share
/// out whatever room is spare, which centres the column exactly as `justify_center`
/// would, and collapse to nothing when there is none, which leaves the column at
/// the top with all of it below the fold and so all of it reachable.
///
/// Three boxes. The outermost is what the overlay bar hangs off, because the
/// scrolling box cannot hold it — its children are what scroll away underneath
/// it — and it is what the caller styles, the fill included. Inside it is the
/// box that scrolls, and inside that the one carrying the margins and the
/// breathing room that keeps either end of the scroll off the edge.
fn centered_scroll(
    id: &'static str,
    scroll: &ScrollHandle,
    bar: Scrollbar,
    theme: &Theme,
    content: impl IntoElement,
) -> Div {
    div()
        .relative()
        .flex()
        .flex_col()
        .flex_grow_1()
        .min_h_0()
        .child(
            div()
                .id(id)
                .track_scroll(scroll)
                .flex()
                .flex_col()
                .flex_grow_1()
                .min_h_0()
                .items_center()
                .overflow_y_scroll()
                .restrict_scroll_to_axis()
                .child(
                    // `flex_none` so that a column taller than the box overflows
                    // it — and is scrolled to — rather than being squeezed into
                    // it, which is what a flex item does by default.
                    div()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .items_center()
                        .my_auto()
                        .py(px(SCROLL_MARGIN))
                        .child(content),
                ),
        )
        .children(bar.render(theme))
}

/// Renders one node of a pane tree.
///
/// A split becomes a flex box in the direction of its axis, with each child
/// sized by `flex_basis`; the `min_w_0` / `min_h_0` on the box *and* on both
/// children is what lets those bases actually divide the space, instead of the
/// terminals inside insisting on their measured width. The pty follows on its
/// own: [`TerminalView`]'s element recomputes the grid from whatever bounds it
/// is given and only pushes a resize when the cell count changed.
///
/// A leaf renders the terminal view itself. When `frame` is set — a split tab,
/// or a single pane with the file panel beside it — every leaf is framed with a
/// hairline, accent coloured on the active one. The frames double as the
/// divider between neighbours, which is why there is no separate divider
/// element — a third hairline squeezed between two of them would only thicken
/// the seam. Every pane is framed, not just the active one, so that moving
/// focus recolours the frame without shifting the layout by a pixel. It is a
/// border rather than a fill because a translucent window allows only one
/// tinted fill per pixel and the terminal surface already owns it.
///
/// `panel_focused` demotes the active leaf back to the plain border colour: the
/// file panel wears the accent frame while it holds the keyboard, and two
/// accent frames at once would say the keystroke is going to both places.
///
/// A split is a [`Splitter`], which lays its own grab band over the divider and
/// hands back the ratio the pointer asks for. It is asked to draw no seam of
/// its own: the pane frames on either side already meet there, and a third
/// hairline between two of them would only thicken the line.
fn render_pane(
    node: &PaneNode<PaneLeaf>,
    active: PaneId,
    frame: bool,
    panel_focused: bool,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    match node {
        PaneNode::Leaf { id, payload } => {
            let border = if *id == active && !panel_focused {
                theme.accent
            } else {
                theme.border
            };
            div()
                .id(("pane", id.as_u64()))
                .flex()
                .size_full()
                .min_w_0()
                .min_h_0()
                .when(frame, |pane| pane.border_1().border_color(border))
                .child(payload.view.element())
                .into_any_element()
        }
        PaneNode::Split {
            id,
            axis,
            ratio,
            first,
            second,
        } => {
            let id = *id;
            let workspace = cx.entity();
            // Both children are rendered up front because each one needs `cx`
            // for the splitters further down the tree, and a closure holding it
            // could not then be called twice.
            let first = render_pane(first, active, frame, panel_focused, theme, cx);
            let second = render_pane(second, active, frame, panel_focused, theme, cx);

            // The tree's own axis and gpui's are two enums of the same two
            // words: the pane crate names a direction without depending on a
            // framework, and the widget takes the framework's.
            let axis = match axis {
                Axis::Horizontal => gpui::Axis::Horizontal,
                Axis::Vertical => gpui::Axis::Vertical,
            };

            Splitter::new(("split", id.as_u64()), axis)
                .ratio(*ratio)
                .seamless()
                .first(first)
                .second(second)
                .on_change(move |ratio, _window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.set_split_ratio(id, ratio, cx);
                    });
                })
                .into_any_element()
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        // Before anything is built, so the panel is already pointed at the
        // active pane's session by the time it renders itself as a child.
        self.sync_file_panel(cx);
        self.watch_scroll(cx);
        let toolbar = self.render_toolbar(window, cx);
        let body = self.render_body(window, cx);
        let status_bar = self.render_status_bar(cx);
        let tab_context = self.render_tab_context(cx);
        // Only ever open over the empty state, which is what the body draws
        // while there is no tab; a session opened from the menu itself takes
        // the state — and with `close_overlays`, the menu — off the screen.
        let empty_context = self.render_empty_context(cx);
        let language_menu = self.render_language_menu(cx);
        let charset_menu = self.render_charset_menu(cx);
        let close_confirm = self.render_close_confirm(cx);
        let sudo_prompt = self.render_sudo_prompt(cx);
        let dialog = self
            .dialog
            .read(cx)
            .is_open()
            .then(|| div().absolute().inset_0().child(self.dialog.clone()));
        let settings = self
            .settings
            .read(cx)
            .is_open()
            .then(|| div().absolute().inset_0().child(self.settings.clone()));
        let about = self
            .about
            .read(cx)
            .is_open()
            .then(|| div().absolute().inset_0().child(self.about.clone()));
        let update = self
            .update
            .read(cx)
            .is_open()
            .then(|| div().absolute().inset_0().child(self.update.clone()));

        // With client-side decorations the compositor stops drawing the drop
        // shadow along with the frame, so the window has to bring its own:
        // the surface grows a transparent band all round, the content is
        // inset by it, and the shadow is painted into it. The inset call
        // keeps `_GTK_FRAME_EXTENTS` in step so the compositor treats the
        // content edge, not the surface edge, as the window.
        let tiling = chrome::client_tiling(window);
        if tiling.is_some() {
            window.set_client_inset(px(chrome::SHADOW_BAND));
        } else {
            // Clears the extents a client-side frame may have left behind
            // when the setting switches back to the system title bar on a
            // live window; a no-op under decorations that never set any.
            window.set_client_inset(px(0.));
        }

        // No background fill here on purpose. The three bands below — toolbar,
        // body and status bar — cover the window between them, and each paints
        // its own. A fill at this level would sit *under* the translucent
        // terminal and empty-state fills and compose back to opaque, which is
        // exactly what made `window.background_opacity` and `background_blur`
        // look like they did nothing.
        let content = div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .text_color(theme.text)
            .text_size(px(13.))
            // The overlay bars are answered from here rather than from the
            // surfaces they ride: gpui hands a drag move to every listener of
            // that type wherever it sits, and the root is the one element that
            // is always mounted while a drag of one is in flight.
            .on_drag_move::<DraggedThumb>(cx.listener(
                move |workspace, event: &DragMoveEvent<DraggedThumb>, _window, cx| {
                    workspace.drag_scrollbar(event, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|workspace, _: &MouseUpEvent, _window, cx| {
                    workspace.release_scrollbars(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|workspace, _: &MouseUpEvent, _window, cx| {
                    workspace.release_scrollbars(cx);
                }),
            )
            .on_action(cx.listener(Self::new_session_action))
            .on_action(cx.listener(Self::close_session_action))
            .on_action(cx.listener(Self::focus_next_pane_action))
            .on_action(cx.listener(Self::focus_prev_pane_action))
            .on_action(cx.listener(Self::break_out_pane_action))
            .on_action(cx.listener(Self::equalize_widths_action))
            .on_action(cx.listener(Self::equalize_heights_action))
            .on_action(cx.listener(Self::move_tab_to_new_window_action))
            .on_action(cx.listener(Self::duplicate_split_right_action))
            .on_action(cx.listener(Self::duplicate_split_below_action))
            .on_action(cx.listener(Self::toggle_file_panel_action))
            .on_action(cx.listener(Self::save_dashboard_layout_action))
            .on_action(cx.listener(Self::open_settings_action))
            .on_action(cx.listener(Self::show_about_action))
            .on_action(cx.listener(Self::check_updates_action))
            .on_action(cx.listener(Self::select_tab_action))
            .on_action(cx.listener(Self::open_dashboard_action))
            .on_action(cx.listener(Self::dismiss_dialog_action))
            .child(toolbar)
            .child(body)
            .child(status_bar)
            // Deferred inside, so it paints above the three bands whatever its
            // place in this list.
            .children(tab_context)
            .children(empty_context)
            .children(language_menu)
            .children(charset_menu)
            .children(dialog)
            .children(settings)
            .children(about)
            .children(update)
            .children(close_confirm)
            .children(sudo_prompt);

        let Some(tiling) = tiling else {
            // A server-decorated window: the compositor frames and shadows
            // it, and the content is the whole surface.
            return content.into_any_element();
        };

        div()
            .size_full()
            .relative()
            .bg(gpui::transparent_black())
            .when(!tiling.top, |outer| outer.pt(px(chrome::SHADOW_BAND)))
            .when(!tiling.bottom, |outer| outer.pb(px(chrome::SHADOW_BAND)))
            .when(!tiling.left, |outer| outer.pl(px(chrome::SHADOW_BAND)))
            .when(!tiling.right, |outer| outer.pr(px(chrome::SHADOW_BAND)))
            .child(
                content
                    // A hairline where the frame's own outline used to be,
                    // per untiled edge; a tiled edge meets the neighbour
                    // flush, the way the compositor would have drawn it.
                    .border_color(theme.border)
                    .when(!tiling.top, |content| content.border_t_1())
                    .when(!tiling.bottom, |content| content.border_b_1())
                    .when(!tiling.left, |content| content.border_l_1())
                    .when(!tiling.right, |content| content.border_r_1())
                    .when(!tiling.is_tiled(), |content| {
                        content.shadow(vec![gpui::BoxShadow {
                            color: gpui::hsla(0., 0., 0., 0.35),
                            blur_radius: px(chrome::SHADOW_BAND / 2.),
                            spread_radius: px(0.),
                            offset: gpui::point(px(0.), px(2.)),
                            // The band is drawn outside the window, not inside
                            // its content, which is what this frame casts.
                            inset: false,
                        }])
                    }),
            )
            // Last on purpose: the window border outranks whatever it
            // crosses, dialogs included, the way a compositor frame would.
            .children(chrome::render_resize_edges(tiling))
            .into_any_element()
    }
}

/// Installs the widget theme the configured id names.
///
/// An id nothing answers to — a theme file the user has since deleted — falls
/// back to the default theme rather than failing; see
/// [`ThemeRegistry::resolve`].
fn apply_ui_theme(id: &str, cx: &mut App) {
    let theme = ThemeRegistry::resolve(id, cx);
    set_theme(theme, cx);
}

/// The application menu bar, in macOS layout.
///
/// gpui only turns this into a real menu bar on macOS — the Windows and Linux
/// backends store it and draw nothing — so the other platforms get the same
/// commands from the in-app dropdown built by
/// [`Workspace::render_app_menu`]. Every item dispatches an action that is also
/// bound to a shortcut in [`bind_shortcuts`], which is what lets the macOS
/// backend label the items with their key equivalents; register the bindings
/// first so the keymap it reads is already populated.
///
/// About, Check for updates, Settings and Quit live in the application menu
/// because that is where macOS users look for them.
///
/// The item labels are translated, but the application menu's own name is the
/// "rulogman" wordmark and stays as it is. Rebuilt and re-installed whenever the
/// language changes, because gpui takes the menu bar by value.
fn app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "rulogman".into(),
            items: vec![
                MenuItem::action(ts!("menu.about"), ShowAbout),
                MenuItem::action(ts!("menu.check_updates"), CheckUpdates),
                MenuItem::separator(),
                MenuItem::action(ts!("menu.settings"), OpenSettings),
                MenuItem::separator(),
                MenuItem::action(ts!("menu.mac.quit"), Quit),
            ],
            disabled: false,
        },
        Menu {
            name: ts!("menu.session"),
            items: vec![
                MenuItem::action(ts!("menu.mac.new_session"), NewSession),
                MenuItem::action(ts!("menu.mac.new_window"), NewWindow),
                MenuItem::action(ts!("menu.mac.close_session"), CloseSession),
                // Only half of splitting is here, for the reason given on
                // [`Workspace::render_app_menu`]: a merge has to name a source
                // tab, so it belongs to the tab context menu alone.
                MenuItem::action(ts!("menu.mac.duplicate_right"), DuplicateSplitRight),
                MenuItem::action(ts!("menu.mac.duplicate_below"), DuplicateSplitBelow),
                MenuItem::action(ts!("menu.mac.equalize_widths"), EqualizeWidths),
                MenuItem::action(ts!("menu.mac.equalize_heights"), EqualizeHeights),
                MenuItem::action(ts!("menu.mac.break_out_pane"), BreakOutPane),
                MenuItem::action(ts!("menu.mac.tab_to_window"), MoveTabToNewWindow),
                MenuItem::separator(),
                MenuItem::action(ts!("files.mac.toggle"), ToggleFilePanel),
            ],
            disabled: false,
        },
    ]
}

/// Registers every shortcut the workspace listens for.
///
/// A binding here beats the terminal: gpui matches key bindings along the whole
/// dispatch path before it delivers the key event itself, so every chord bound
/// in this function is taken away from the remote shell. That is what decides
/// the pane modifier below.
fn bind_shortcuts(cx: &mut App) {
    let modifier = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };

    // Pane navigation follows iTerm2 on macOS, where `cmd` never reaches the
    // shell. Elsewhere the same chords would swallow `Ctrl+[` — which every
    // remote shell reads as ESC — and `Ctrl+]`, so those platforms use `alt`
    // instead, the modifier Windows Terminal also keeps for pane navigation.
    // The bracket keys stay unshifted on purpose: both macOS and Windows report
    // a shifted bracket as `}` with the shift flag already consumed, so a
    // `shift-]` binding would never match. Hence a letter for the break-out.
    let pane_modifier = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "alt"
    };

    let mut bindings = vec![
        KeyBinding::new(&format!("{modifier}-q"), Quit, None),
        KeyBinding::new(&format!("{modifier}-t"), NewSession, Some(KEY_CONTEXT)),
        KeyBinding::new(WINDOW_SHORTCUT, NewWindow, Some(KEY_CONTEXT)),
        KeyBinding::new(&format!("{modifier}-w"), CloseSession, Some(KEY_CONTEXT)),
        KeyBinding::new(&format!("{modifier}-,"), OpenSettings, Some(KEY_CONTEXT)),
        KeyBinding::new("escape", DismissDialog, Some(KEY_CONTEXT)),
        KeyBinding::new(
            &format!("{pane_modifier}-]"),
            FocusNextPane,
            Some(KEY_CONTEXT),
        ),
        KeyBinding::new(
            &format!("{pane_modifier}-["),
            FocusPrevPane,
            Some(KEY_CONTEXT),
        ),
        KeyBinding::new(
            &format!("{pane_modifier}-shift-b"),
            BreakOutPane,
            Some(KEY_CONTEXT),
        ),
        // Shifted for the reason the break-out is, and by the same arithmetic:
        // off macOS the pane modifier is `alt`, and bare `Alt+N` is readline's
        // *non-incremental-forward-search-history*. The shifted chord costs the
        // remote shell nothing, because a terminal cannot encode `Alt+Shift+N`
        // distinctly from `Alt+N` in the first place — see the split bindings
        // below. `N` rather than a letter of its own so that it reads with the
        // window commands: `Ctrl+Shift+N` opens an empty window, and this fills
        // one.
        KeyBinding::new(
            &format!("{pane_modifier}-shift-n"),
            MoveTabToNewWindow,
            Some(KEY_CONTEXT),
        ),
        // Shifted for the same reason the break-out is: off macOS the pane
        // modifier is `alt`, and bare `Alt+D` is readline's *kill-word*, which
        // a user typing in the pane being split would miss immediately. The
        // shifted chord is free in a way the bare one is not — a terminal
        // cannot encode `Alt+Shift+D` distinctly from `Alt+D` — so taking it
        // costs the remote shell nothing. `Alt+S` is shifted to match, since
        // the two split directions have to read as one pair of commands.
        KeyBinding::new(
            &format!("{pane_modifier}-shift-d"),
            DuplicateSplitRight,
            Some(KEY_CONTEXT),
        ),
        KeyBinding::new(
            &format!("{pane_modifier}-shift-s"),
            DuplicateSplitBelow,
            Some(KEY_CONTEXT),
        ),
        KeyBinding::new(PANEL_SHORTCUT, ToggleFilePanel, Some(KEY_CONTEXT)),
        // Shifted to stay clear of the shell for the reason the window chord is:
        // bare `Ctrl+L` is readline's *clear-screen*, and a terminal cannot
        // encode `Ctrl+Shift+L` distinctly from it, so the shifted chord is free
        // to take. `L` for layout; a no-op on any tab that is not a dashboard.
        KeyBinding::new(
            &format!("{modifier}-shift-l"),
            SaveDashboardLayout,
            Some(KEY_CONTEXT),
        ),
    ];
    for index in 0..QUICK_SELECT_TABS {
        bindings.push(KeyBinding::new(
            &format!("{modifier}-{}", index + 1),
            SelectTab(index),
            Some(KEY_CONTEXT),
        ));
    }
    // The digits again with `Alt` added, which reads as what it is: the tab
    // chord one level up — `Ctrl+1` picks the first tab, `Ctrl+Alt+1` opens the
    // first dashboard. `Alt` is what is left to add: every other chord this
    // function registers is `{modifier}`, `{pane_modifier}` or one of those
    // shifted, and none of them is the pair, so nothing in the application is
    // being taken away from.
    //
    // Nor is anything being taken from the remote shell, on either half of the
    // split. On macOS `cmd` never reaches it at all. Elsewhere the chord is
    // `Ctrl+Alt+digit`, which a terminal cannot encode distinctly in the first
    // place — there is no control code for a digit — so what a shell would have
    // received for it is at most the `ESC digit` of a bare `Alt+digit`, and
    // that chord is untouched: the two arrive here as different modifier sets
    // and only the one with `Ctrl` is bound.
    //
    // The index is into the saved order of the dashboard store, which is the
    // order the welcome screen lists them in — so the number to press is the
    // number of the row the user is already looking at.
    for index in 0..QUICK_OPEN_DASHBOARDS {
        bindings.push(KeyBinding::new(
            &format!("{modifier}-alt-{}", index + 1),
            OpenDashboard(index),
            Some(KEY_CONTEXT),
        ));
    }

    cx.bind_keys(bindings);
}

fn main() {
    env_logger::init();

    // Before anything reads a configuration file — and before the app runs at
    // all, since this is pure filesystem work that needs no window. A user
    // updating from a release published under the old name still has their
    // profiles, settings and themes in the directory that name derived; this
    // copies them across once. Failing is survivable: the app then starts with
    // an empty configuration, exactly as it would have without the attempt.
    if let Err(error) = rulogman_core::migrate_from_logman() {
        log::warn!("could not migrate the configuration of the previous release: {error:#}");
    }

    // Read before anything else touches the launch, because everything about
    // it is filesystem work that wants no window: what is left is a list of
    // directories, and a directory that was named but is not there has already
    // been dropped with a warning by the time the app starts.
    //
    // The argv is split first, because one of the things in it is not a path:
    // `--dashboard <name>` asks for a saved arrangement rather than a folder,
    // and the two are answered in different places. See
    // [`launch::split_launch_args`].
    let (path_args, dashboard_names) = launch::split_launch_args(std::env::args_os().skip(1));
    let start_dirs = launch::start_dirs(path_args);
    // KDE's *Open Terminal Here* — and any launcher that treats rulogman as
    // the desktop's default terminal — never puts the folder in argv at all:
    // `KTerminalLauncherJob` only knows how to pass `--workdir` to konsole, so
    // for every other terminal it runs the desktop entry's `Exec=` line
    // unchanged and communicates the folder solely by setting the child's
    // working directory. Without this, that arrives here as zero paths and
    // opens the welcome screen instead of a shell in the folder Dolphin meant.
    //
    // A launch that named a dashboard is excluded, and has to be: the working
    // directory is only a signal *because* the launch said nothing else, and
    // `rulogman --dashboard morning` typed in a project folder has said
    // something else. Reading the folder as a request too would open a shell
    // beside the dashboard that nobody asked for.
    #[cfg(all(unix, not(target_os = "macos")))]
    let start_dirs = if start_dirs.is_empty() && dashboard_names.is_empty() {
        launch::implicit_start_dir().into_iter().collect()
    } else {
        start_dirs
    };

    // The other half of the same question, and the only half macOS asks. A
    // Finder *Open with* — or `open -a rulogman /var/log` — reaches the app as
    // `application:openURLs:` rather than as an argv, and it does so whether
    // the app was already running or is starting because of it. The callback
    // has no `App` to work with, so it does the one thing it can: hands the
    // URLs to a channel the run closure below drains on the UI thread. On
    // Linux and Windows nothing ever sends on it, since both platforms put the
    // paths in the argv read above; registering it regardless costs a callback
    // that is never called.
    let (opened_urls, mut urls) = mpsc::unbounded();
    // `LastWindowClosed` rather than the default, which is this only away from
    // macOS: there an app whose last window closes stays in the Dock with its
    // menu bar, and *New Window* would still be reachable from it — but there is
    // nothing behind an empty screen worth keeping alive. Every session belongs
    // to a window and goes when the window does, so once the last one is closed
    // the process has no work left. One rule on every platform is what the app
    // has always done.
    let app = gpui_platform::application()
        .with_assets(icons::ICONS)
        .with_quit_mode(QuitMode::LastWindowClosed);
    app.on_open_urls(move |urls| {
        // Failing means the receiver is gone, which means the app is on its way
        // out and there is no window left to open a tab in.
        let _ = opened_urls.unbounded_send(urls);
    });

    // The icon set has to be installed before the app runs: `svg()` resolves
    // every path through this source, and the default one answers `None`.
    app.run(move |cx: &mut App| {
        // Everything `rugpui-shell` is not allowed to guess at, handed over
        // before anything that could read it runs. `set_strings` goes through
        // `ts!`, so the shell follows a language change without being told
        // again; `set_update_policy` is the two-line window onto the
        // `ignored_update` field of `settings.json`. `init` first, and before
        // `clean_leftovers` below: it is what fills the process-wide identity
        // slot the update paths read, and what records — while the running
        // image is still where it was launched from — the path
        // `rugpui_shell::restart_path` hands back after a swap has moved it.
        rugpui_shell::init(IDENTITY, cx);
        rugpui_shell::set_strings(Box::new(AppStrings), cx);
        rugpui_shell::set_update_policy(Box::new(IgnoredUpdate), cx);

        if let Err(error) = rulogman_core::init_secrets() {
            log::warn!("the OS keychain is unavailable: {error}");
        }

        // A self-update renames the copy it replaces aside instead of deleting
        // it — Windows cannot delete a running image, and one code path for
        // three platforms is worth more than an immediate unlink on the two
        // that could. This is the other half: the leftover is swept up on the
        // next launch. On the background executor because removing a `.app`
        // bundle is a recursive delete and nothing on screen depends on it.
        cx.background_executor()
            .spawn(async { shell_update::clean_leftovers() })
            .detach();

        // Load settings before the widget layer installs its default theme, then
        // override that theme to match what the user configured.
        app_settings::init(cx);
        let settings = app_settings::current(cx);
        // Ahead of everything that renders a string — the menu bar included —
        // so nothing is ever built in the wrong language and then corrected.
        i18n::apply(settings.language.as_deref());

        rugpui::init(cx);
        // After `rugpui::init`, which installs a fully opaque default of its own:
        // the widgets that have to agree with a translucent window read the
        // opacity from a global of the widget layer's.
        app_settings::set_tint(&settings, cx);
        // After `rugpui::init`, because the find bar is built out of the widget
        // layer's text field and binds keys in a context nested inside it.
        rugpui_editor::init(cx);
        // After `rugpui_editor::init`, because the pane's own context wraps the
        // editor's and binds the one command the widget cannot have: saving.
        editor_pane::init(cx);
        TerminalView::init(cx);
        bind_shortcuts(cx);
        cx.set_menus(app_menus());

        // Before the theme is applied: the id in the settings may well name one
        // of the user's own themes, and the same goes for the scheme every
        // session is about to be opened with.
        theme_store::reload(cx);
        // The languages the editor widget lexes, the definitions rulogman
        // ships and whatever the user has put beside them — here for the same
        // reason the palettes are: an editor opened later has to find the
        // registry already installed. Read once and never again, so a
        // definition added while rulogman is running arrives on the next launch.
        languages::init(cx);
        apply_ui_theme(&settings.ui_theme, cx);

        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        // Global rather than a listener on the workspace, the way quitting is:
        // opening a window is a command about the application, and nothing in
        // the workspace it was invoked from has to be consulted to carry it
        // out. Where the new window lands is the one thing that window has a
        // say in, and that is read from its bounds below.
        cx.on_action(|_: &NewWindow, cx: &mut App| {
            // Deferred, and only here: the action is dispatched from inside the
            // window it was invoked in, and gpui lifts a window off its own map
            // for the length of a dispatch — so the bounds the new window is
            // about to step off cannot be read until the dispatch is over. The
            // call below runs the moment it is, with every window back in
            // place. `main` calls the same function directly, because there is
            // no window to step off there.
            cx.defer(|cx| {
                if let Err(error) = open_workspace_window(cx) {
                    log::warn!("could not open another window: {error:#}");
                }
            });
        });

        open_workspace_window(cx).expect("failed to open the rulogman window");

        // A tab per path the launch named, before the window is shown: the
        // start screen is what a launch with no paths opens on, and a launch
        // with them should never flash it.
        open_start_dirs(start_dirs, cx);
        // And a tab per dashboard, in the same breath and for the same reason:
        // a launch that opens a dashboard must not flash the welcome screen
        // either. After the paths, so that a launch naming both puts the
        // dashboards where the eye ends up — see [`open_startup_dashboards`].
        open_startup_dashboards(dashboard_names, cx);
        // And a tab per path every *later* launch names, for as long as this
        // process lives. On macOS a second *Open with* does not start a second
        // rulogman — it wakes this one — so the paths have to land in a window
        // that is already open rather than in a new one.
        cx.spawn(async move |cx| {
            // The loop ends on its own when the application does: the sender
            // lives in the `on_open_urls` callback the platform owns, so the
            // stream closes as the platform is torn down and this task never
            // reaches an `App` that is no longer there.
            while let Some(batch) = urls.next().await {
                let dirs = launch::start_dirs(batch);
                cx.update(|cx| open_start_dirs(dirs, cx));
            }
        })
        .detach();

        cx.activate(true);
    });
}

/// Opens a window on a workspace of its own, and hands back its handle.
///
/// Every window comes through here — the one the launch opens and every one
/// *New window* opens after it — so a second window is a first window in every
/// respect. The settings are read afresh on each call rather than captured
/// once, which is what lets a window opened after a visit to the settings
/// dialog arrive already wearing the title bar and the translucency the user
/// chose, instead of the ones the process started on.
fn open_workspace_window(cx: &mut App) -> anyhow::Result<WindowHandle<Workspace>> {
    let bounds = new_window_bounds(cx);
    open_workspace_window_at(bounds, cx)
}

/// [`open_workspace_window`] with the placement already decided.
///
/// The split exists for the one caller that knows where its window goes and
/// cannot use [`new_window_bounds`] to find out: moving a tab out is dispatched
/// from inside the window it steps off, which gpui lifts off its own map for the
/// length of a dispatch, so that window cannot be asked for its bounds through a
/// handle — but it is right there as an argument. See
/// [`Workspace::move_tab_to_new_window`].
fn open_workspace_window_at(
    bounds: Bounds<Pixels>,
    cx: &mut App,
) -> anyhow::Result<WindowHandle<Workspace>> {
    let settings = app_settings::current(cx);
    // Read once, here: this is only the state the window opens in. Changing the
    // setting later reaches the open window rather than waiting for the next
    // launch — [`Workspace::apply_settings`] hands it to
    // `set_titlebar_transparent` on Windows and macOS, and to
    // `request_decorations` on the Linux backends, which is why nothing here
    // tells the user to restart.
    let titlebar = settings.window.titlebar;
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("rulogman".into()),
                appears_transparent: titlebar == TitlebarStyle::Custom,
                // Ignored unless the caption is transparent; it moves the
                // traffic lights AppKit keeps drawing into the toolbar
                // band the app puts in the caption's place.
                traffic_light_position: (titlebar == TitlebarStyle::Custom)
                    .then_some(TRAFFIC_LIGHT_ORIGIN),
            }),
            // Only the Linux backends read this. `appears_transparent`
            // above means nothing to X11 and Wayland: the caption stays
            // the compositor's until the window asks for client-side
            // decorations outright. gpui falls back to server decorations
            // on its own when no compositor is present, and
            // [`draws_own_titlebar`] follows what the window actually got.
            window_decorations: (titlebar == TitlebarStyle::Custom)
                .then_some(gpui::WindowDecorations::Client),
            // Wayland compositors and X11 docks match this against
            // com.aihouse.rulogman.desktop to pick up the application icon.
            app_id: Some("com.aihouse.rulogman".into()),
            // A translucent or blurred window needs the platform surface to
            // permit alpha; the terminal view then tints its background.
            window_background: chrome::window_appearance(
                settings.window.background_blur,
                settings.window.background_opacity,
            ),
            ..Default::default()
        },
        |window, cx| {
            let workspace = cx.new(|cx| Workspace::new(titlebar, window, cx));
            let handle = workspace.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            apply_caption_theme(window, &theme(cx), cx);
            workspace
        },
    )
}

/// Where the next window goes.
///
/// Stepped off the window the command came from when there is one, and centred
/// on the display when there is not — which is the launch, and also a *New
/// window* arriving while the platform says nothing is focused.
fn new_window_bounds(cx: &mut App) -> Bounds<Pixels> {
    let front = active_workspace_window(cx);
    let bounds = front.and_then(|handle| handle.update(cx, |_, window, _| window.bounds()).ok());
    match bounds {
        Some(bounds) => cascaded(bounds),
        None => Bounds::centered(None, size(px(1100.), px(700.)), cx),
    }
}

/// `bounds` stepped down and across by [`WINDOW_CASCADE`], keeping its size.
///
/// A free function, and the whole of the placement rule, so that where a second
/// window lands can be checked without opening one.
fn cascaded(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds {
        origin: bounds.origin + point(px(WINDOW_CASCADE), px(WINDOW_CASCADE)),
        size: bounds.size,
    }
}

/// Every open window whose root view is a [`Workspace`].
///
/// The application's own windows and nothing else: `cx.windows()` answers for
/// the process, and a dialog the platform put up on its own has no workspace in
/// it to speak to.
fn workspace_windows(cx: &App) -> Vec<WindowHandle<Workspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<Workspace>())
        .collect()
}

/// [`workspace_windows`] without the window the caller is in.
///
/// The exclusion is a requirement rather than a courtesy. A caller reached
/// through one of its own window's callbacks holds that window off gpui's
/// stack and its workspace out of the entity map for the length of the call, so
/// reading or updating it a second time from here would fail or panic on the
/// double lease. Every caller answers for its own window itself.
fn other_workspace_windows(except: &Window, cx: &App) -> Vec<WindowHandle<Workspace>> {
    let except = except.window_handle().window_id();
    workspace_windows(cx)
        .into_iter()
        .filter(|window| window.window_id() != except)
        .collect()
}

/// Re-applies the current settings to every window but `except`.
///
/// The settings are one answer for the application: the language, the theme and
/// the window chrome are chosen once and every window has to come back wearing
/// them. The window the dialog was opened in is left to the workspace that owns
/// it — see [`other_workspace_windows`].
fn apply_settings_elsewhere(except: &Window, cx: &mut App) {
    for handle in other_workspace_windows(except, cx) {
        let applied = handle.update(cx, |workspace, window, cx| {
            workspace.apply_settings(window, cx);
        });
        if let Err(error) = applied {
            log::warn!("could not apply the settings to another window: {error}");
        }
    }
}

/// Every session held by a window other than `except`.
///
/// The other half of the answer to a question that reads as if it were about one
/// window and is really about the machine: which ports are bound right now. See
/// [`Workspace::tunnels_held_elsewhere`], which asks it, and
/// [`other_workspace_windows`] for why the asking window is left out.
fn sessions_in_other_windows(except: &Window, cx: &App) -> Vec<Entity<Session>> {
    other_workspace_windows(except, cx)
        .into_iter()
        .filter_map(|handle| handle.read(cx).ok())
        .flat_map(|workspace| workspace.sessions(cx))
        .collect()
}

/// Whether a window other than `except` is installing an update.
///
/// See [`Workspace::update_installing`], which is what asks.
fn installing_elsewhere(except: &Window, cx: &App) -> bool {
    other_workspace_windows(except, cx)
        .into_iter()
        .filter_map(|handle| handle.read(cx).ok())
        .any(|workspace| workspace.update.read(cx).is_busy())
}

/// Whether the caller is the first to ask, which every later caller is not.
///
/// The silent start-up update check belongs to the launch and not to a window:
/// a second window opened from the menu is not a second launch, and asking
/// GitHub again would risk a dialog announcing a release the user has already
/// been shown — or dismissed. The answer is kept in a global because the
/// question is about the process, and every window that could ask has an `App`
/// in front of it.
fn claim_startup_check(cx: &mut App) -> bool {
    if cx.has_global::<StartupCheckDone>() {
        return false;
    }
    cx.set_global(StartupCheckDone);
    true
}

/// The marker [`claim_startup_check`] sets once and never clears.
struct StartupCheckDone;

impl Global for StartupCheckDone {}

/// The window a request arriving from outside the application should act on.
///
/// Whichever window is in front, because that is the one the user was looking at
/// when they asked; failing that the first one open, since the platform reports
/// no active window while the application is in the background — which is
/// exactly the case a Finder *Open with* arrives in.
fn active_workspace_window(cx: &App) -> Option<WindowHandle<Workspace>> {
    cx.active_window()
        .and_then(|window| window.downcast::<Workspace>())
        .or_else(|| workspace_windows(cx).into_iter().next())
}

/// Opens a tab per directory the launch named, and brings the window forward
/// if it opened any.
///
/// Both launch paths end here — the argv read before the app started and the
/// URLs macOS delivers while it runs — because from the workspace's point of
/// view they are the same request arriving twice over. The tabs go to the
/// window that is in front at the moment the paths arrive rather than to a
/// window fixed at start-up: by the time a second *Open with* lands there may
/// be several, and the first one opened is not necessarily the one the user is
/// working in. On the launch itself there is only the window just opened, so
/// the same rule covers both.
fn open_start_dirs(dirs: Vec<PathBuf>, cx: &mut App) {
    if dirs.is_empty() {
        return;
    }
    let Some(window) = active_workspace_window(cx) else {
        log::warn!("no window is open to show the paths given in");
        return;
    };
    let opened = window.update(cx, |workspace, window, cx| {
        for dir in dirs {
            workspace.open_local_directory(dir, window, cx);
        }
        // For the second launch rather than the first: the user asked for this
        // window by opening something with it, and on macOS the app it woke is
        // otherwise left in the background.
        window.activate_window();
    });
    if let Err(error) = opened {
        log::warn!("could not open a shell for the paths given: {error}");
    }
}

/// The dashboards a launch should open, in the order they should open in.
///
/// Two ways of asking, answered as one list. A dashboard the user marked
/// *open at startup* asks every time, silently and from the store itself; a
/// `--dashboard <name>` on the command line asks once, for this run. The
/// marked ones come first because they are the standing arrangement — the
/// thing the user set up to be there whenever rulogman starts — and what the
/// command line named is what they asked for *today*, which is the tab they
/// want to be looking at when the window comes up.
///
/// A name is matched exactly, and against the name alone: dashboard names are
/// not unique — identity in the store is the id — so two dashboards may answer
/// to one name and the first in store order takes it. That is a shape the user
/// can see, since the welcome screen lists the store in the same order; a
/// fuzzy or case-insensitive match would not be. A name nothing answers to is
/// warned about and skipped, the same stance a path that is not there gets:
/// the window still opens, with one tab fewer than asked for.
///
/// Deduplicated by id, keeping the first appearance, so a dashboard that is
/// both marked and named opens one tab rather than two identical ones.
fn startup_dashboards(store: &DashboardStore, requested: &[String]) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = Vec::new();
    let mut push = |id: Uuid| {
        if !ids.contains(&id) {
            ids.push(id);
        }
    };

    for dashboard in store.dashboards() {
        if dashboard.open_at_startup {
            push(dashboard.id);
        }
    }
    for name in requested {
        match store
            .dashboards()
            .iter()
            .find(|dashboard| dashboard.name == *name)
        {
            Some(dashboard) => push(dashboard.id),
            None => log::warn!("ignoring --dashboard {name}: no dashboard is called that"),
        }
    }

    ids
}

/// Opens a tab per dashboard the launch asked for, marked or named.
///
/// Shaped like [`open_start_dirs`], and placed right after it, so that the
/// whole launch is answered before the window is shown. The store is the one
/// the window already loaded rather than a second read of the file: two copies
/// could disagree about what is on disk, and it is the window's copy the
/// welcome screen lists and the numbered shortcuts index.
///
/// Opening order is [`startup_dashboards`] order, and every dashboard lands in
/// a tab of its own after the ones already there, so the last one opened is the
/// one left active. That is the intended end state — the newest request is what
/// the user is looking at — and it is also simply what
/// [`Workspace::open_dashboard`] does with each tab it appends.
///
/// A dashboard whose connections have nothing saved is not opened at all: the
/// all-or-nothing credential gate in [`Workspace::open_dashboard`] puts the
/// connection form up instead and leaves the dashboard to be clicked. At
/// start-up that means a window that comes up on a pre-filled dialog rather
/// than on the arrangement, which is the right end of the trade — the
/// alternative is a start-up that queues one modal per unsaved host — and it is
/// the same answer clicking the dashboard would have given.
fn open_startup_dashboards(names: Vec<String>, cx: &mut App) {
    let Some(window) = active_workspace_window(cx) else {
        // Only worth a word when something was actually asked for on the
        // command line; a marked dashboard cannot even be looked for without a
        // window to read the store from.
        if !names.is_empty() {
            log::warn!("no window is open to show the dashboards asked for");
        }
        return;
    };
    let opened = window.update(cx, |workspace, window, cx| {
        for id in startup_dashboards(&workspace.dashboards, &names) {
            workspace.open_dashboard(id, window, cx);
        }
    });
    if let Err(error) = opened {
        log::warn!("could not open the dashboards asked for: {error}");
    }
}

/// The rules the workspace can be held to without a window, and the one thing
/// that needs one.
///
/// Everything the tab strip decides — what a tab of an open file is called,
/// whether closing it has to ask, where the focus lands as tabs are taken out —
/// is a rule about names and indices, and each is written as a free function
/// precisely so that it can be checked here without a session, a pane or a
/// window. What is left is [`centered_scroll`], which is entirely a question of
/// layout: it is put under test through what its scroll handle reports, since
/// the handle is where gpui writes down the answer — the box it measured, and
/// how far past it the column ran.
#[cfg(test)]
mod tests {
    use std::ops::Deref;

    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    /// Height of the stand-in column.
    ///
    /// Nothing about the real welcome screen's contents matters here — only that
    /// there is a definite height to hold the window against — so the test hands
    /// the box one plain child rather than rebuilding the screen.
    const COLUMN: f32 = 400.;

    /// A window tall enough for the column and both its margins, several times
    /// over.
    const ROOMY: f32 = 900.;

    /// A window shorter than the column, which is the whole point of the box.
    const CRAMPED: f32 = 300.;

    /// Wide enough that nothing wraps; the box only scrolls one way.
    const WIDTH: f32 = 600.;

    /// How far apart two measurements may be and still count as the same, in a
    /// layout whose lengths are rounded to hundredths of a pixel.
    const SLACK: f32 = 0.5;

    /// A window holding nothing but the box under test.
    struct Harness {
        scroll: ScrollHandle,
        bar: ScrollbarState,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let theme = Theme::dark();
            let bar = Scrollbar::for_handle(SCROLLBARS[1].0, Surface::Empty.axis(), &self.scroll)
                .fade(self.bar.fade());

            div().flex().flex_col().size_full().child(centered_scroll(
                EMPTY_STATE,
                &self.scroll,
                bar,
                &theme,
                div().flex_none().w(px(320.)).h(px(COLUMN)),
            ))
        }
    }

    /// Opens the harness in a window `height` tall and hands back its handle.
    ///
    /// Drawn twice: a bar is built from the box as the previous frame measured
    /// it, so the opening frame has nothing to build one out of.
    fn open(cx: &mut TestAppContext, height: f32) -> ScrollHandle {
        let scroll = ScrollHandle::new();
        let window = cx.add_window({
            let scroll = scroll.clone();
            move |_, _| Harness {
                scroll,
                bar: ScrollbarState::new(),
            }
        });

        let mut cx = VisualTestContext::from_window(*window.deref(), cx);
        cx.simulate_resize(size(px(WIDTH), px(height)));
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        scroll
    }

    /// The bar the workspace would draw over the box as it now stands.
    fn scrollbar(scroll: &ScrollHandle) -> Scrollbar {
        Scrollbar::for_handle(SCROLLBARS[1].0, Surface::Empty.axis(), scroll)
    }

    /// With room to spare the column sits in the middle, exactly where
    /// `justify_center` used to put it, and there is nothing to scroll — so no
    /// bar is drawn either.
    #[gpui::test]
    fn a_column_that_fits_stays_in_the_middle(cx: &mut TestAppContext) {
        let scroll = open(cx, ROOMY);
        let box_ = scroll.bounds();
        let column = scroll
            .bounds_for_item(0)
            .expect("the box never measured its column");

        let above = f32::from(column.top() - box_.top());
        let below = f32::from(box_.bottom() - column.bottom());
        assert!(
            (above - below).abs() < SLACK,
            "the column was not centred: {above} above, {below} below"
        );
        assert_eq!(
            scroll.max_offset().y,
            px(0.),
            "a column that fits left something to scroll"
        );
        assert!(
            scrollbar(&scroll).thumb().is_none(),
            "a box with nothing to scroll drew a bar anyway"
        );
    }

    /// The regression: with less room than the column needs, the head of it used
    /// to be pushed off the top edge and left there. It now starts at the top of
    /// the box, and everything past the bottom is reachable by scrolling.
    #[gpui::test]
    fn a_column_that_does_not_fit_starts_at_the_top(cx: &mut TestAppContext) {
        let scroll = open(cx, CRAMPED);
        let box_ = scroll.bounds();
        let column = scroll
            .bounds_for_item(0)
            .expect("the box never measured its column");

        assert!(
            f32::from(column.top() - box_.top()).abs() < SLACK,
            "the column did not start at the top of the box: {:?} in {:?}",
            column,
            box_
        );
        assert!(
            (f32::from(scroll.max_offset().y) - f32::from(column.size.height - box_.size.height))
                .abs()
                < SLACK,
            "the scrollable range did not cover the whole of the column"
        );
        assert!(
            scrollbar(&scroll).thumb().is_some(),
            "a box with something to scroll drew no bar"
        );
    }

    /// And the far end of that scroll reaches the foot of the column, margin and
    /// all, rather than stopping short of the last button.
    #[gpui::test]
    fn scrolling_to_the_end_reaches_the_foot_of_the_column(cx: &mut TestAppContext) {
        let scroll = open(cx, CRAMPED);
        scroll.set_offset(point(px(0.), -scroll.max_offset().y));
        let box_ = scroll.bounds();
        let column = scroll
            .bounds_for_item(0)
            .expect("the box never measured its column");

        let foot = column.bottom() + scroll.offset().y;
        assert!(
            f32::from(foot - box_.bottom()).abs() < SLACK,
            "the end of the scroll left {:?} of the column below the box",
            foot - box_.bottom()
        );
        assert!(
            f32::from(column.size.height) > COLUMN + SCROLL_MARGIN,
            "the column was scrolled to its last button rather than past it"
        );
    }

    #[test]
    fn a_tab_of_one_file_is_named_after_the_file_and_the_connection() {
        assert_eq!(
            editor_tab_label("nginx.conf", "web-01").as_ref(),
            "nginx.conf - web-01"
        );
    }

    #[test]
    fn a_connection_with_no_name_to_give_leaves_the_file_name_alone() {
        // "nginx.conf - " reads as a label that was cut off, which is worse
        // than one that simply says less.
        assert_eq!(editor_tab_label("nginx.conf", "").as_ref(), "nginx.conf");
        assert_eq!(editor_tab_label("nginx.conf", "  ").as_ref(), "nginx.conf");
    }

    #[test]
    fn a_session_holding_no_forwarding_has_nothing_to_mark() {
        // `None` is what leaves the tab unmarked, so this is the whole of the
        // rule that a tab which lost the bind — or never asked for a tunnel —
        // looks exactly as it did before.
        assert!(tunnel_tooltip(&[]).is_none());
    }

    #[test]
    fn a_forwarding_is_named_from_the_local_end_to_the_remote_one() {
        let tooltip = tunnel_tooltip(&["8080:db:5432".into()]).expect("a rule to name");
        assert!(tooltip.contains("8080 \u{2192} db:5432"), "{tooltip}");

        // Every rule is named, and on one line: the mark says how many
        // forwardings ride on this tab, so a tooltip that stopped at the first
        // would be answering a different question.
        let both = tunnel_tooltip(&["8080:db:5432".into(), "6379:cache:6379".into()])
            .expect("two rules to name");
        assert!(both.contains("8080 \u{2192} db:5432"), "{both}");
        assert!(both.contains("6379 \u{2192} cache:6379"), "{both}");
        assert!(!both.contains('\n'), "{both}");
    }

    #[test]
    fn a_label_that_is_not_a_rule_is_shown_as_it_arrived() {
        // Nothing emits one today. If anything ever does, it has to reach the
        // user as itself rather than being dropped for not parsing.
        let tooltip = tunnel_tooltip(&["something else".into()]).expect("a label to name");
        assert!(tooltip.contains("something else"), "{tooltip}");
    }

    /// A profile that does or does not want the panel beside it.
    fn profile_showing_files(show_files: bool) -> SessionProfile {
        let mut profile = SessionProfile::new(
            "web-01",
            "example.com",
            22,
            "alice",
            rulogman_core::AuthMethod::Password,
        );
        profile.show_files = show_files;
        profile
    }

    /// The setting a local shell is judged by, as the settings dialog writes it.
    fn local_panel(open: bool) -> FilesSettings {
        FilesSettings { local_panel: open }
    }

    #[test]
    fn a_remote_session_opens_the_panel_its_profile_asked_for() {
        // Both directions, and both against a setting that says the opposite:
        // the setting is for the sessions no profile speaks for, and must not
        // get a vote on the ones that have one.
        assert!(panel_opens_with(
            Some(&profile_showing_files(true)),
            &local_panel(false)
        ));
        assert!(!panel_opens_with(
            Some(&profile_showing_files(false)),
            &local_panel(true)
        ));
    }

    #[test]
    fn a_local_shell_follows_the_setting() {
        assert!(panel_opens_with(None, &local_panel(true)));
        assert!(!panel_opens_with(None, &local_panel(false)));
    }

    #[test]
    fn a_profile_saved_before_the_choice_existed_still_opens_the_panel() {
        // `SessionProfile::new` is what the connection dialog builds a new
        // profile from, and what a `profiles.json` with no key in it loads as.
        // Either way the panel has to go on appearing, which is what every
        // session did before today.
        let profile = SessionProfile::new(
            "web-01",
            "example.com",
            22,
            "alice",
            rulogman_core::AuthMethod::Password,
        );
        assert!(panel_opens_with(Some(&profile), &local_panel(false)));
    }

    /// The profile the sessions below were opened from.
    fn a_profile() -> Uuid {
        Uuid::from_u128(1)
    }

    /// One session as [`tunnels_held_for`] is given them: which profile it came
    /// from, which session it is, and whether it is holding forwardings.
    fn session(profile: Option<Uuid>, id: u64, holding: bool) -> (Option<Uuid>, EntityId, bool) {
        (profile, EntityId::from(id), holding)
    }

    #[test]
    fn a_sibling_holding_the_ports_keeps_the_next_session_off_them() {
        // The case the whole rule exists for: a second tab on a profile whose
        // forwardings the first tab is running. Asking again could only fail,
        // once per rule and in yellow across a terminal just opened.
        assert!(tunnels_held_for(
            a_profile(),
            None,
            [session(Some(a_profile()), 1, true)],
        ));
    }

    #[test]
    fn a_sibling_that_is_holding_nothing_leaves_the_ports_free() {
        // Either it never bound them or its transport has gone; both leave the
        // list empty, and both mean the next session may take them.
        assert!(!tunnels_held_for(
            a_profile(),
            None,
            [session(Some(a_profile()), 1, false)],
        ));
    }

    #[test]
    fn another_profiles_forwardings_are_not_this_profiles_business() {
        // Local ports do collide across profiles, but that is a conflict with
        // something outside this profile's tabs, and the transport reporting it
        // is how the user hears about it. A local session — no profile at all —
        // is nobody's sibling either.
        assert!(!tunnels_held_for(
            a_profile(),
            None,
            [
                session(Some(Uuid::from_u128(2)), 1, true),
                session(None, 2, true),
            ],
        ));
    }

    #[test]
    fn a_session_reconnecting_is_not_its_own_rival() {
        // What it is holding this instant it is about to drop, so its own
        // forwardings must not be the reason it comes back without them.
        let reconnecting = session(Some(a_profile()), 1, true);
        assert!(!tunnels_held_for(
            a_profile(),
            Some(EntityId::from(1)),
            [reconnecting],
        ));
        // A second tab holding them is still a reason, exception or no.
        assert!(tunnels_held_for(
            a_profile(),
            Some(EntityId::from(1)),
            [reconnecting, session(Some(a_profile()), 2, true)],
        ));
    }

    #[test]
    fn only_a_tab_that_is_one_unsaved_file_asks_before_it_closes() {
        assert!(tab_close_asks(1, true));
        // Nothing is at stake, so the close button is not a question.
        assert!(!tab_close_asks(1, false));
        // A split tab: the question closes one pane, and this close was aimed
        // at the whole tab.
        assert!(!tab_close_asks(2, true));
    }

    #[test]
    fn closing_a_tab_behind_the_active_one_leaves_the_focus_where_it_is() {
        assert_eq!(active_after_close(1, 3, 0), 1);
    }

    #[test]
    fn closing_a_tab_in_front_of_the_active_one_moves_it_down_a_slot() {
        assert_eq!(active_after_close(3, 1, 0), 2);
    }

    #[test]
    fn a_split_needs_half_a_grid_on_the_axis_it_divides() {
        // Exactly twice the minimum is the last size that still splits, since
        // both halves come out at the minimum itself.
        assert!(split_fits(
            Axis::Horizontal,
            MIN_PANE_COLS * 2,
            MIN_PANE_ROWS
        ));
        assert!(!split_fits(
            Axis::Horizontal,
            MIN_PANE_COLS * 2 - 1,
            MIN_PANE_ROWS
        ));
        assert!(split_fits(Axis::Vertical, MIN_PANE_COLS, MIN_PANE_ROWS * 2));
        assert!(!split_fits(
            Axis::Vertical,
            MIN_PANE_COLS,
            MIN_PANE_ROWS * 2 - 1
        ));
    }

    #[test]
    fn a_split_ignores_the_axis_it_does_not_divide() {
        // A side-by-side split leaves the row count alone, so a grid one row
        // tall still splits horizontally — and a grid one column wide still
        // splits vertically. Each half keeps the whole of the other dimension.
        assert!(split_fits(Axis::Horizontal, MIN_PANE_COLS * 2, 1));
        assert!(split_fits(Axis::Vertical, 1, MIN_PANE_ROWS * 2));
    }

    #[test]
    fn the_caret_is_printed_as_the_line_out_of_the_lines_and_then_the_column() {
        assert_eq!(caret_summary(12, 200, 5).as_ref(), "12/200 : 5");
        // A file of one line still reads as a fraction rather than as a bare
        // number: the second half is what says how much file there is.
        assert_eq!(caret_summary(1, 1, 1).as_ref(), "1/1 : 1");
    }

    #[test]
    fn a_named_format_is_labelled_by_its_own_name() {
        // Straight out of the syntax module, untranslated, because `JSON` is
        // `JSON` in every locale. Plain text is the one row that is looked up,
        // and what it comes back as depends on which locale is loaded — which
        // is the i18n module's test to make, not this one's.
        let registry = rugpui_editor::LanguageRegistry::builtin();
        let label = |id: &str| language_label(registry.get(id).expect(id));
        assert_eq!(label("json").as_ref(), "JSON");
        assert_eq!(label("dockerfile").as_ref(), "Dockerfile");
        assert!(!label(languages::PLAIN).is_empty());
    }

    #[test]
    fn closing_the_active_tab_hands_the_focus_to_the_survivor() {
        // A survivor in front of the hole does not move.
        assert_eq!(active_after_close(2, 2, 0), 0);
        // One behind it moves down with everything else: the tab that was
        // fourth is third once the second has gone.
        assert_eq!(active_after_close(1, 1, 3), 2);
    }
}

/// The file panel's per-tab state, held to through a real workspace.
///
/// The rules above are free functions precisely so they need no window; this is
/// the other half — that the workspace *asks* them, writes the answer on the
/// right tab, and keeps it there while the user works in the tab beside it.
/// None of that is a sentence about a profile, so none of it can be checked
/// without a workspace with tabs in it, and until now there was no way to build
/// one: [`Workspace::new`] opens the dialogs the window carries and every public
/// [`Session`] constructor dials something before it returns.
///
/// Two small openings make it possible, and neither changes what a user gets.
/// The settings the workspace judges a new tab by already live in a replaceable
/// global, so a test sets them the way the settings dialog does — see
/// [`app_settings::replace`] — rather than through a file. And the one file the
/// window did read on the way up, the profile store the connection dialog loads,
/// is left unread in a test build, so what the developer running the tests
/// happens to have saved cannot reach a frame. What remains is a session that
/// never connects, which is [`Session::dormant`] and its remote counterpart.
///
/// The state is read back through [`Workspace::panel_showing`] wherever the
/// active tab is the subject, because that is the value both render paths branch
/// on: assert on it and the assertion is about what is drawn, not merely about a
/// field that happens to sit beside it.
///
/// One of the rules is left unasserted: [`Workspace::duplicate_tab`] inherits
/// the flag exactly as [`Workspace::break_out_active_pane`] does, but it gets
/// its second tab by *duplicating* the session, and a duplicate starts a
/// transport before it returns — a pty on the machine running the tests, or a
/// connection to a host that does not answer. Nothing can stand in for that
/// here, because the session it starts is the very thing the new tab is made
/// out of.
#[cfg(test)]
mod workspace_tests {
    use super::*;

    use gpui::{TestAppContext, VisualTestContext};
    use rulogman_core::{AppSettings, AuthMethod, TailRule};

    /// A workspace in a window, on settings that say `local_panel` for the
    /// shells that follow it.
    ///
    /// The settings go in before the window opens for the same reason they do in
    /// `main`: everything the workspace builds reads the global, and a workspace
    /// built on one set of settings and asked about another would be answering a
    /// question nobody put to it.
    fn workspace(
        cx: &mut TestAppContext,
        local_panel: bool,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.update(|cx| set_local_panel(cx, local_panel));
        cx.add_window_view(|window, cx| Workspace::new(TitlebarStyle::System, window, cx))
    }

    /// Puts `local_panel` into the settings global, leaving the rest at their
    /// defaults.
    fn set_local_panel(cx: &mut App, open: bool) {
        let mut settings = AppSettings::default();
        settings.files.local_panel = open;
        app_settings::replace(settings, cx);
    }

    /// A profile that does or does not want the panel beside it.
    fn profile_showing_files(show_files: bool) -> SessionProfile {
        let mut profile =
            SessionProfile::new("web-01", "example.com", 22, "alice", AuthMethod::Password);
        profile.show_files = show_files;
        profile
    }

    /// Gives the workspace a tab on a host whose profile says `show_files`.
    ///
    /// [`Workspace::open_session`] with the connection taken out of it: the same
    /// two lines that decide the panel and hand the session over, around a
    /// session that never dials the host it names.
    fn open_remote(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, show_files: bool) {
        workspace.update_in(cx, |workspace, window, cx| {
            let profile = profile_showing_files(show_files);
            let panel_open = Workspace::panel_opens_for(Some(&profile), cx);
            let session = cx.new(|cx| Session::dormant_remote(profile, cx));
            workspace.adopt_session(session, panel_open, window, cx);
        });
    }

    /// The same for a shell on this machine, which comes from no profile and is
    /// judged by the setting instead.
    fn open_local(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
        workspace.update_in(cx, |workspace, window, cx| {
            let panel_open = Workspace::panel_opens_for(None, cx);
            let session = cx.new(Session::dormant);
            workspace.adopt_session(session, panel_open, window, cx);
        });
    }

    /// Gives the workspace a tab following `path`, the way
    /// [`Workspace::open_tail_session`] ends.
    ///
    /// The connection is taken out of it exactly as [`open_remote`] takes it
    /// out of a shell tab, and for the same reason: what is under test is the
    /// pane the workspace builds, not what is on the other end of it. The
    /// panel flag is the call's own `false` rather than
    /// [`Workspace::panel_opens_for`], since a followed file never asks.
    fn open_tail(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, path: &str) {
        workspace.update_in(cx, |workspace, window, cx| {
            let profile = profile_showing_files(true);
            let caps = Workspace::pane_caps_source(cx);
            let session = cx.new(|cx| Session::dormant_tail(profile, path.to_owned(), cx));
            let terminal = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
            let view = cx.new(|cx| {
                TailView::new(
                    terminal,
                    session.clone(),
                    path.to_owned(),
                    SharedString::default(),
                    cx,
                )
            });
            let leaf = workspace.new_tail_pane(view, session, window, cx);

            workspace
                .tabs
                .push(SessionTab::single(leaf).with_panel(false));
            workspace.active = workspace.tabs.len() - 1;
            workspace.focus_active(window, cx);
        });
    }

    /// Gives the workspace a tab for a profile that names `paths` to follow,
    /// the way [`Workspace::open_session_with_tails`] ends: one tab, the
    /// shell pane plus one tail pane per path, stacked in the order given.
    ///
    /// [`Workspace::open_session_with_tails`] itself dials a real connection
    /// for the shell and for every tail, so — as `open_remote` and `open_tail`
    /// already do for their own calls — the sessions here are the dormant
    /// stand-ins instead. What is under test is
    /// [`Workspace::compose_tailed_tab`]'s arrangement of the panes, not what
    /// is on the other end of any of them.
    fn open_tailed(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, paths: &[&str]) {
        workspace.update_in(cx, |workspace, window, cx| {
            let mut profile = profile_showing_files(true);
            profile.tails = paths
                .iter()
                .map(|path| TailRule {
                    path: (*path).to_owned(),
                })
                .collect();
            let panel_open = Workspace::panel_opens_for(Some(&profile), cx);
            let caps = Workspace::pane_caps_source(cx);

            let session = cx.new(Session::dormant);
            let view = cx.new(|cx| TerminalView::new(session.clone(), caps.clone(), window, cx));
            let shell_leaf = workspace.new_pane(view, session, window, cx);

            let tail_leaves = paths
                .iter()
                .map(|path| {
                    let session =
                        cx.new(|cx| Session::dormant_tail(profile.clone(), (*path).to_owned(), cx));
                    let terminal =
                        cx.new(|cx| TerminalView::new(session.clone(), caps.clone(), window, cx));
                    let view = cx.new(|cx| {
                        TailView::new(
                            terminal,
                            session.clone(),
                            (*path).to_owned(),
                            SharedString::default(),
                            cx,
                        )
                    });
                    workspace.new_tail_pane(view, session, window, cx)
                })
                .collect();

            let tab = Workspace::compose_tailed_tab(shell_leaf, tail_leaves, panel_open);
            workspace.tabs.push(tab);
            workspace.active = workspace.tabs.len() - 1;
            workspace.focus_active(window, cx);
        });
    }

    /// Splits the active tab in two, the way [`Workspace::duplicate_split`] ends.
    ///
    /// Not that call itself: it splits by *duplicating*, and a duplicate starts
    /// a second transport — a pty on the machine running the tests, or a TCP
    /// connection to a host that does not exist. The half it would have made is
    /// put there directly instead, on a session of its own that connects to
    /// nothing, because what is under test here is what the tab carries rather
    /// than what is on the other end of either pane.
    fn split_active(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
        workspace.update_in(cx, |workspace, window, cx| {
            let session = cx.new(Session::dormant);
            let caps = Workspace::pane_caps_source(cx);
            let view = cx.new(|cx| TerminalView::new(session.clone(), caps, window, cx));
            let leaf = workspace.new_pane(view, session, window, cx);

            let active = workspace.active;
            let tab = &mut workspace.tabs[active];
            let target = tab.active_pane();
            let pane = tab
                .panes
                .split(target, Axis::Horizontal, leaf)
                .expect("the pane to split came out of this tab");
            tab.focus(pane);
        });
    }

    /// Whether the panel is showing beside the active tab, as both render paths
    /// ask it.
    fn showing(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        workspace.read_with(cx, |workspace, _| workspace.panel_showing())
    }

    /// The flag on the tab at `index`, whether or not it is the active one.
    fn flag(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, index: usize) -> bool {
        workspace.read_with(cx, |workspace, _| workspace.tabs[index].panel_open)
    }

    /// Gives the workspace a dashboard tab of `count` followed files under the
    /// name `name`, the way [`Workspace::open_dashboard`] ends.
    ///
    /// The lookups and the keychain are taken out of it exactly as
    /// [`open_tailed`] takes out the connection, and for the same reason: what
    /// is under test is [`Workspace::compose_dashboard_tab`]'s arrangement and
    /// the name the tab wears, neither of which is a question about what is on
    /// the other end of a pane. One profile for all of them, since the grid is
    /// the same grid however many hosts the panes came from.
    fn open_dashboard(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
        name: &str,
        count: usize,
    ) {
        workspace.update_in(cx, |workspace, window, cx| {
            let profile = profile_showing_files(true);
            let caps = Workspace::pane_caps_source(cx);
            let leaves = (0..count)
                .map(|index| {
                    let path = format!("/var/log/app-{index}.log");
                    let session =
                        cx.new(|cx| Session::dormant_tail(profile.clone(), path.clone(), cx));
                    let terminal =
                        cx.new(|cx| TerminalView::new(session.clone(), caps.clone(), window, cx));
                    let view = cx.new(|cx| {
                        TailView::new(
                            terminal,
                            session.clone(),
                            path,
                            SharedString::from(profile.name.clone()),
                            cx,
                        )
                    });
                    workspace.new_tail_pane(view, session, window, cx)
                })
                .collect();

            let tab = Workspace::compose_dashboard_tab(leaves, false).with_label(name.to_owned());
            workspace.tabs.push(tab);
            workspace.active = workspace.tabs.len() - 1;
            workspace.focus_active(window, cx);
        });
    }

    /// The active tab's shape: how many panes it holds, and how many of its
    /// dividers run each way.
    ///
    /// Rows and columns are not stored anywhere — the tree is binary — so the
    /// grid is asserted through the two counts, which pin it down between them:
    /// `r` rows of `c` columns is `r - 1` splits along [`Axis::Vertical`] and
    /// one per cell past the first of every row along [`Axis::Horizontal`].
    fn grid(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (usize, usize, usize) {
        workspace.read_with(cx, |workspace, _| {
            let panes = &workspace.tabs[workspace.active].panes;
            (
                panes.leaf_count(),
                panes.splits_along(Axis::Vertical),
                panes.splits_along(Axis::Horizontal),
            )
        })
    }

    /// Whether the active tab hands the keyboard to its top-left pane.
    fn first_pane_is_active(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        workspace.read_with(cx, |workspace, _| {
            let tab = &workspace.tabs[workspace.active];
            tab.active_pane() == tab.panes.first_leaf().0
        })
    }

    /// Brings the tab at `index` to the front.
    fn select(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, index: usize) {
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.select_tab(index, window, cx);
        });
    }

    #[gpui::test]
    fn a_followed_file_is_a_session_tab_that_wants_no_panel(cx: &mut TestAppContext) {
        // The setting says "open the panel", and so does the profile the tail
        // is opened from: a followed file has to refuse it whatever either of
        // them says, there being no shell on the other end to browse a
        // filesystem beside — and [`Session::files`] answering nothing for such
        // a session, so a panel here would sit empty for good.
        let (workspace, cx) = workspace(cx, true);
        open_tail(&workspace, cx, "/var/log/nginx/access.log");

        assert!(
            !showing(&workspace, cx),
            "a followed file opened the file panel"
        );

        // It is a session like any other, which is the answer every rule about
        // a pane is written against: the tab strip's label and status dot, the
        // status bar, the disconnect that retires the pane, the reconnect.
        let session = workspace
            .read_with(cx, |workspace, cx| {
                workspace.tabs[workspace.active].active_session(cx)
            })
            .expect("a tail pane did not answer as a session");

        // And it is named after the file, not after the connection: two logs on
        // one host would otherwise wear the same label.
        assert_eq!(
            session.read_with(cx, |session, _| session.title()),
            SharedString::from("access.log - web-01")
        );
    }

    #[gpui::test]
    fn a_profile_with_tails_gets_one_tab_with_the_shell_on_top(cx: &mut TestAppContext) {
        // Two rules, so the arrangement has to be told apart from "a tail pane
        // happened to land somewhere" — three leaves in all, and nowhere else
        // for the other two to have gone but this one tab.
        let (workspace, cx) = workspace(cx, true);
        open_tailed(
            &workspace,
            cx,
            &["/var/log/nginx/access.log", "/var/log/nginx/error.log"],
        );

        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.tabs.len()),
            1,
            "the tail rules opened tabs of their own instead of joining the shell's"
        );

        let leaf_count = workspace.read_with(cx, |workspace, _| {
            workspace.tabs[workspace.active].panes.leaf_count()
        });
        assert_eq!(
            leaf_count, 3,
            "expected the shell pane plus one pane per tail rule"
        );

        // The shell, not either tail, is what the tab hands the keyboard to on
        // arrival: a rule nobody has looked at yet has nothing to answer a
        // keypress with.
        let active_is_shell = workspace.read_with(cx, |workspace, _| {
            matches!(
                workspace.tabs[workspace.active].active_view(),
                PaneView::Terminal(_)
            )
        });
        assert!(
            active_is_shell,
            "a tail pane held the active pane instead of the shell"
        );
    }

    #[gpui::test]
    fn a_dashboard_of_two_files_puts_them_side_by_side(cx: &mut TestAppContext) {
        // Two panes are one row of two columns, not a column of two: a log is
        // read across, and halving the width of a terminal costs less than
        // halving the number of lines of it that are on screen.
        let (workspace, cx) = workspace(cx, true);
        open_dashboard(&workspace, cx, "Deploy watch", 2);

        assert_eq!(
            grid(&workspace, cx),
            (2, 0, 1),
            "two files did not open as one row of two"
        );

        // The setting says "open the panel" and so does the profile behind
        // every pane; a dashboard refuses it for the reason a single followed
        // file refuses it, there being no shell here to browse a filesystem
        // beside.
        assert!(
            !showing(&workspace, cx),
            "a dashboard opened the file panel"
        );
        assert!(
            first_pane_is_active(&workspace, cx),
            "a dashboard handed the keyboard to something other than its first pane"
        );
    }

    #[gpui::test]
    fn a_dashboard_of_three_files_fills_the_top_row_first(cx: &mut TestAppContext) {
        // Three into two columns: a full row and a short one. Which of the two
        // rows is the short one is the whole of what "row-major" means here, so
        // the tree itself is read rather than only the divider counts — those
        // would say the same thing about a dashboard that had filled the bottom
        // row and left a gap at the top.
        let (workspace, cx) = workspace(cx, true);
        open_dashboard(&workspace, cx, "Deploy watch", 3);

        assert_eq!(
            grid(&workspace, cx),
            (3, 1, 1),
            "three files did not open as two rows of at most two"
        );

        let short_row_last = workspace.read_with(cx, |workspace, _| {
            match workspace.tabs[workspace.active].panes.root() {
                PaneNode::Split {
                    axis: Axis::Vertical,
                    first,
                    second,
                    ..
                } => {
                    matches!(
                        **first,
                        PaneNode::Split {
                            axis: Axis::Horizontal,
                            ..
                        }
                    ) && matches!(**second, PaneNode::Leaf { .. })
                }
                _ => false,
            }
        });
        assert!(
            short_row_last,
            "the row with room to spare was not the bottom one"
        );
    }

    #[gpui::test]
    fn a_dashboard_of_four_files_opens_two_by_two(cx: &mut TestAppContext) {
        // The case the whole shape exists for: four panes are a square, not a
        // stack of four and not a row of four.
        let (workspace, cx) = workspace(cx, true);
        open_dashboard(&workspace, cx, "Deploy watch", 4);

        assert_eq!(
            grid(&workspace, cx),
            (4, 1, 2),
            "four files did not open two by two"
        );
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.tabs.len()),
            1,
            "the dashboard's files opened tabs of their own instead of one tab"
        );
    }

    #[gpui::test]
    fn a_dashboard_tab_is_named_after_the_dashboard(cx: &mut TestAppContext) {
        // Not after whichever pane holds the keyboard, which is what every
        // other tab is named after: a dashboard is a named arrangement, and a
        // strip that renamed it as the focus moved would be reporting on the
        // wrong thing.
        let (workspace, cx) = workspace(cx, true);
        open_dashboard(&workspace, cx, "Deploy watch", 4);

        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.tabs[workspace.active]
                .label
                .clone()),
            Some(SharedString::from("Deploy watch"))
        );

        // And the name it is *not* wearing is a real one: the active pane has a
        // session with a title of its own, which is what the strip would have
        // used had the tab carried no name.
        let title = workspace
            .read_with(cx, |workspace, cx| {
                workspace.tabs[workspace.active].active_session(cx)
            })
            .expect("a dashboard pane did not answer as a session")
            .read_with(cx, |session, _| session.title());
        assert_eq!(title, SharedString::from("app-0.log - web-01"));
    }

    /// A [`LayoutNode::Split`], spelled out so the layout tests read as trees.
    fn layout_split(
        axis: LayoutAxis,
        ratio: f32,
        first: LayoutNode,
        second: LayoutNode,
    ) -> LayoutNode {
        LayoutNode::Split {
            axis,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// A dashboard of `count` followed files named `/var/log/app-N.log`, each on
    /// a profile of its own, optionally carrying a saved `layout`.
    fn dashboard_of(name: &str, count: usize, layout: Option<LayoutNode>) -> Dashboard {
        let mut dashboard = Dashboard::new(name);
        for index in 0..count {
            dashboard.panes.push(DashboardPane {
                profile: Uuid::new_v4(),
                path: format!("/var/log/app-{index}.log"),
            });
        }
        dashboard.layout = layout;
        dashboard
    }

    /// Opens `dashboard` into a tab the way [`Workspace::open_dashboard`] ends,
    /// making the same layout-or-grid decision the real opener makes and taking
    /// the lookups and the keychain out for the reason [`open_dashboard`] does.
    ///
    /// Each pane's session carries the very profile id and path the dashboard
    /// names, so what [`Workspace::capture_tab_layout`] reads back off the tab is
    /// exactly what went in. The dashboard is also placed in the store, so the
    /// opened tab's [`SessionTab::dashboard`] resolves to a real entry.
    fn open_dashboard_tab(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
        dashboard: &Dashboard,
    ) {
        workspace.update_in(cx, |workspace, window, cx| {
            let caps = Workspace::pane_caps_source(cx);
            let leaves: Vec<PaneLeaf> = dashboard
                .panes
                .iter()
                .map(|pane| {
                    let mut profile = profile_showing_files(true);
                    profile.id = pane.profile;
                    let path = pane.path.clone();
                    let session =
                        cx.new(|cx| Session::dormant_tail(profile.clone(), path.clone(), cx));
                    let terminal =
                        cx.new(|cx| TerminalView::new(session.clone(), caps.clone(), window, cx));
                    let view = cx.new(|cx| {
                        TailView::new(
                            terminal,
                            session.clone(),
                            path,
                            SharedString::from(profile.name.clone()),
                            cx,
                        )
                    });
                    workspace.new_tail_pane(view, session, window, cx)
                })
                .collect();

            let tab = match dashboard.valid_layout() {
                Some(layout) if leaves.len() == dashboard.panes.len() => {
                    Workspace::compose_dashboard_layout(leaves, layout, false)
                }
                _ => Workspace::compose_dashboard_tab(leaves, false),
            }
            .with_label(dashboard.name.clone())
            .with_dashboard(dashboard.id);

            workspace.dashboards.upsert(dashboard.clone());
            workspace.tabs.push(tab);
            workspace.active = workspace.tabs.len() - 1;
            workspace.focus_active(window, cx);
        });
    }

    /// The followed paths of the active tab's panes, in depth-first layout
    /// order, for asserting where each leaf landed.
    fn leaf_paths(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
        workspace.read_with(cx, |workspace, cx| {
            workspace.tabs[workspace.active]
                .panes
                .leaves()
                .into_iter()
                .map(|(_, leaf)| {
                    leaf.view
                        .session(cx)
                        .and_then(|session| {
                            session.read(cx).tail_path().map(|path| path.to_owned())
                        })
                        .expect("a dashboard pane is a followed file")
                })
                .collect()
        })
    }

    /// A store of dashboards named `names`, in that order, with the ones whose
    /// name appears in `marked` flagged to open at start-up.
    ///
    /// Ids are the store's own, so the assertions below have to go back through
    /// the store to name what they expect — which is the point: what
    /// [`startup_dashboards`] answers with is ids, and a name is only ever the
    /// way in.
    fn dashboard_store(names: &[&str], marked: &[&str]) -> DashboardStore {
        let mut store = DashboardStore::default();
        for name in names {
            let mut dashboard = Dashboard::new(*name);
            dashboard.open_at_startup = marked.contains(name);
            store.upsert(dashboard);
        }
        store
    }

    /// The id of the first dashboard called `name`, which is the one a request
    /// for that name resolves to.
    fn dashboard_id(store: &DashboardStore, name: &str) -> Uuid {
        store
            .dashboards()
            .iter()
            .find(|dashboard| dashboard.name == name)
            .expect("the fixture has no such dashboard")
            .id
    }

    #[test]
    fn the_marked_dashboards_open_at_startup_in_saved_order() {
        let store = dashboard_store(&["morning", "deploy", "night"], &["night", "morning"]);

        assert_eq!(
            startup_dashboards(&store, &[]),
            vec![
                dashboard_id(&store, "morning"),
                dashboard_id(&store, "night")
            ]
        );
    }

    #[test]
    fn a_dashboard_named_on_the_command_line_opens_after_the_marked_ones() {
        let store = dashboard_store(&["morning", "deploy", "night"], &["morning"]);

        assert_eq!(
            startup_dashboards(&store, &["night".to_owned(), "deploy".to_owned()]),
            vec![
                dashboard_id(&store, "morning"),
                dashboard_id(&store, "night"),
                dashboard_id(&store, "deploy"),
            ]
        );
    }

    #[test]
    fn a_dashboard_both_marked_and_named_opens_once() {
        let store = dashboard_store(&["morning", "deploy"], &["morning"]);

        assert_eq!(
            startup_dashboards(&store, &["morning".to_owned(), "morning".to_owned()]),
            vec![dashboard_id(&store, "morning")]
        );
    }

    #[test]
    fn a_name_no_dashboard_answers_to_is_skipped() {
        let store = dashboard_store(&["morning"], &[]);

        assert!(startup_dashboards(&store, &["Morning".to_owned()]).is_empty());
        assert!(startup_dashboards(&store, &["morning ".to_owned()]).is_empty());
        assert_eq!(
            startup_dashboards(&store, &["gone".to_owned(), "morning".to_owned()]),
            vec![dashboard_id(&store, "morning")]
        );
    }

    #[test]
    fn a_launch_that_asks_for_nothing_opens_no_dashboard() {
        assert!(startup_dashboards(&dashboard_store(&["morning"], &[]), &[]).is_empty());
        assert!(startup_dashboards(&DashboardStore::default(), &["morning".to_owned()]).is_empty());
    }

    #[test]
    fn two_dashboards_of_one_name_are_reached_by_the_first() {
        // Names are not unique — identity in the store is the id — so the
        // command line can only ever name the first of them, which is the one
        // the welcome screen lists first too.
        let store = dashboard_store(&["morning", "morning"], &[]);

        assert_eq!(
            startup_dashboards(&store, &["morning".to_owned()]),
            vec![store.dashboards()[0].id]
        );
    }

    #[gpui::test]
    fn a_dashboard_restores_its_saved_layout(cx: &mut TestAppContext) {
        // A three-pane arrangement with real shape: pane 0 fills the top, and
        // the two below it share a row. The tree, the axis at each split and the
        // ratio the divider sits at all have to come back exactly, or the saved
        // geometry was not honoured.
        let (workspace, cx) = workspace(cx, true);
        let layout = layout_split(
            LayoutAxis::Vertical,
            0.3,
            LayoutNode::Leaf { pane: 0 },
            layout_split(
                LayoutAxis::Horizontal,
                0.6,
                LayoutNode::Leaf { pane: 1 },
                LayoutNode::Leaf { pane: 2 },
            ),
        );
        let dashboard = dashboard_of("Tuned", 3, Some(layout));
        open_dashboard_tab(&workspace, cx, &dashboard);

        workspace.read_with(cx, |workspace, _| {
            let PaneNode::Split {
                axis: Axis::Vertical,
                ratio,
                first,
                second,
                ..
            } = workspace.tabs[workspace.active].panes.root()
            else {
                panic!("the root was not the saved vertical split");
            };
            assert!((*ratio - 0.3).abs() < 1e-6, "the top divider moved");
            assert!(
                matches!(**first, PaneNode::Leaf { .. }),
                "the top of the split was not a single pane"
            );
            let PaneNode::Split {
                axis: Axis::Horizontal,
                ratio,
                first,
                second,
                ..
            } = &**second
            else {
                panic!("the bottom of the split was not a horizontal split");
            };
            assert!((*ratio - 0.6).abs() < 1e-6, "the lower divider moved");
            assert!(
                matches!(**first, PaneNode::Leaf { .. })
                    && matches!(**second, PaneNode::Leaf { .. }),
                "the lower split did not hold two panes"
            );
        });

        // And the panes landed in the order the leaves named them: 0 on top,
        // then 1 and 2 across the row below.
        assert_eq!(
            leaf_paths(&workspace, cx),
            vec![
                "/var/log/app-0.log".to_owned(),
                "/var/log/app-1.log".to_owned(),
                "/var/log/app-2.log".to_owned(),
            ],
            "the panes did not restore in their saved positions"
        );
        assert!(
            first_pane_is_active(&workspace, cx),
            "a restored dashboard handed the keyboard to something other than its first pane"
        );
    }

    #[gpui::test]
    fn a_tab_layout_round_trips_through_the_store(cx: &mut TestAppContext) {
        // Build a tab from a known arrangement, read it back the way
        // *Save layout* does, and confirm the pair a dashboard is stored as
        // comes out matching: the panes in depth-first order and the very tree
        // that built the tab.
        let (workspace, cx) = workspace(cx, true);
        let layout = layout_split(
            LayoutAxis::Vertical,
            0.3,
            LayoutNode::Leaf { pane: 0 },
            layout_split(
                LayoutAxis::Horizontal,
                0.6,
                LayoutNode::Leaf { pane: 1 },
                LayoutNode::Leaf { pane: 2 },
            ),
        );
        let dashboard = dashboard_of("Tuned", 3, Some(layout.clone()));
        open_dashboard_tab(&workspace, cx, &dashboard);

        let (panes, captured) = workspace
            .read_with(cx, |workspace, cx| {
                Workspace::capture_tab_layout(&workspace.tabs[workspace.active], cx)
            })
            .expect("every pane of a dashboard tab is a followed file");

        // Depth-first order: 0, 1, 2 — the same order the leaves were named in.
        assert_eq!(
            panes
                .iter()
                .map(|pane| pane.path.clone())
                .collect::<Vec<_>>(),
            vec![
                "/var/log/app-0.log".to_owned(),
                "/var/log/app-1.log".to_owned(),
                "/var/log/app-2.log".to_owned(),
            ]
        );
        // And each pane kept the profile the dashboard named it on.
        assert_eq!(
            panes.iter().map(|pane| pane.profile).collect::<Vec<_>>(),
            dashboard
                .panes
                .iter()
                .map(|pane| pane.profile)
                .collect::<Vec<_>>()
        );
        // The tree is the one that built the tab, ratios and all.
        assert_eq!(captured, layout);

        // What *Save layout* writes minus the disk: the captured pair upserted
        // over the stored dashboard and read straight back.
        let mut store = DashboardStore::default();
        let mut updated = dashboard.clone();
        updated.panes = panes;
        updated.layout = Some(captured);
        store.upsert(updated);
        let stored = store
            .get(dashboard.id)
            .expect("the dashboard is in the store");
        assert_eq!(stored.layout, Some(layout));
    }

    #[gpui::test]
    fn a_dashboard_without_a_layout_opens_as_a_grid(cx: &mut TestAppContext) {
        // No saved geometry is the ordinary state, and it must lay out as the
        // fresh grid `compose_dashboard_tab` builds: three panes are two rows of
        // at most two, one split each way.
        let (workspace, cx) = workspace(cx, true);
        let dashboard = dashboard_of("Plain", 3, None);
        open_dashboard_tab(&workspace, cx, &dashboard);

        assert_eq!(
            grid(&workspace, cx),
            (3, 1, 1),
            "a layout-less dashboard did not open as the grid"
        );
    }

    #[gpui::test]
    fn a_drifted_layout_falls_back_to_the_grid(cx: &mut TestAppContext) {
        // A layout that no longer matches its panes — here it names only two of
        // the three — is caught by `valid_layout` and the grid takes over, so a
        // pane edit that outdated the geometry costs nothing but the tuning.
        let (workspace, cx) = workspace(cx, true);
        let stale = layout_split(
            LayoutAxis::Horizontal,
            0.5,
            LayoutNode::Leaf { pane: 0 },
            LayoutNode::Leaf { pane: 1 },
        );
        let dashboard = dashboard_of("Drifted", 3, Some(stale));
        open_dashboard_tab(&workspace, cx, &dashboard);

        assert_eq!(
            grid(&workspace, cx),
            (3, 1, 1),
            "a drifted layout was honoured instead of falling back to the grid"
        );
    }

    #[gpui::test]
    fn a_tab_opens_with_the_panel_its_own_profile_asked_for(cx: &mut TestAppContext) {
        // The setting says the opposite of both profiles throughout: it speaks
        // for the sessions no profile speaks for, and must not get a vote on the
        // ones that have one.
        let (workspace, cx) = workspace(cx, false);

        open_remote(&workspace, cx, true);
        assert!(
            showing(&workspace, cx),
            "a host whose profile asks for the panel opened without it"
        );

        open_remote(&workspace, cx, false);
        assert!(
            !showing(&workspace, cx),
            "a host whose profile refuses the panel opened with it anyway"
        );

        // And the first tab is untouched by the second having opened: the flag
        // is the tab's, so switching back shows what that tab was given.
        assert!(flag(&workspace, cx, 0));
        select(&workspace, cx, 0);
        assert!(showing(&workspace, cx));
    }

    #[gpui::test]
    fn a_local_shell_opens_with_the_panel_the_setting_asked_for(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace(cx, true);
        open_local(&workspace, cx);
        assert!(
            showing(&workspace, cx),
            "a local shell ignored a setting that asked for the panel"
        );

        // The setting is read when the tab opens, so a change to it reaches the
        // next shell and leaves the one already open alone.
        cx.update(|_window, cx| set_local_panel(cx, false));
        open_local(&workspace, cx);
        assert!(
            !showing(&workspace, cx),
            "a local shell ignored a setting that refused the panel"
        );
        assert!(
            flag(&workspace, cx, 0),
            "changing the setting shut the panel on a shell already open"
        );
    }

    #[gpui::test]
    fn the_toggle_moves_the_active_tab_and_no_other(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace(cx, false);
        open_remote(&workspace, cx, true);
        open_remote(&workspace, cx, true);

        workspace.update(cx, |workspace, cx| workspace.toggle_file_panel(cx));
        assert!(
            !showing(&workspace, cx),
            "the toggle did not shut the panel"
        );
        assert!(
            flag(&workspace, cx, 0),
            "shutting the panel on one tab shut it on the tab beside it"
        );

        // Each tab goes on showing its own answer as the strip is walked, which
        // is the whole of what the flag being per tab buys.
        select(&workspace, cx, 0);
        assert!(showing(&workspace, cx));
        select(&workspace, cx, 1);
        assert!(!showing(&workspace, cx));

        // And the toggle is a toggle: the same tab comes back.
        workspace.update(cx, |workspace, cx| workspace.toggle_file_panel(cx));
        assert!(showing(&workspace, cx));
    }

    #[gpui::test]
    fn the_welcome_screen_has_no_panel_and_nothing_to_toggle(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace(cx, true);
        assert!(
            !showing(&workspace, cx),
            "a window with nothing open drew the panel beside the welcome screen"
        );

        // The guard the greyed-out menu row and the missing toolbar button agree
        // with: there is no tab to write the answer on, so the shortcut does
        // nothing rather than panicking or arming a panel nothing can draw.
        workspace.update(cx, |workspace, cx| workspace.toggle_file_panel(cx));
        assert!(!showing(&workspace, cx));
    }

    #[gpui::test]
    fn a_pane_broken_out_takes_the_panel_its_tab_was_showing(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace(cx, false);

        // A tab whose profile refused the panel: the tab it splits off must not
        // gain one on the way out.
        open_remote(&workspace, cx, false);
        split_active(&workspace, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.break_out_active_pane(window, cx);
        });
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.tabs.len()),
            2
        );
        assert!(
            !showing(&workspace, cx),
            "a pane broken out of a tab with no panel opened one"
        );

        // And the other way: the flag followed is the source tab's, whatever it
        // says. Toggled rather than opened from a profile, because it is what
        // the tab shows *now* that the new one continues — the profile has had
        // its say and the user may have moved on from it.
        select(&workspace, cx, 0);
        workspace.update(cx, |workspace, cx| workspace.toggle_file_panel(cx));
        split_active(&workspace, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.break_out_active_pane(window, cx);
        });
        assert!(
            showing(&workspace, cx),
            "a pane broken out of a tab showing the panel lost it"
        );
    }

    /// A second window on a workspace of its own, for the tab moves below.
    ///
    /// [`Workspace::move_tab_to_new_window`] itself is left untested for the
    /// reason [`window_tests`] gives: opening a window for real paints a caption
    /// from the widget layer's theme, which a headless test has no reason to
    /// install. Its two halves are the whole of what it does, and they are what
    /// is asserted on here.
    fn second_window(cx: &mut VisualTestContext) -> WindowHandle<Workspace> {
        cx.add_window(|window, cx| Workspace::new(TitlebarStyle::System, window, cx))
    }

    /// The terminal view of the first pane of the tab at `index`.
    fn terminal_of(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
        index: usize,
    ) -> Entity<TerminalView> {
        workspace.read_with(cx, |workspace, _| {
            match &workspace.tabs[index].panes.first_leaf().1.view {
                PaneView::Terminal(view) => view.clone(),
                _ => unreachable!("the tab was opened as a shell"),
            }
        })
    }

    /// The panes of the active tab, in layout order.
    fn pane_ids(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<PaneId> {
        workspace.read_with(cx, |workspace, _| {
            workspace.tabs[workspace.active].panes.leaf_ids()
        })
    }

    /// Which pane of the active tab holds the keyboard.
    fn active_pane(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> PaneId {
        workspace.read_with(cx, |workspace, _| {
            workspace.tabs[workspace.active].active_pane()
        })
    }

    /// Moves the marker onto `pane` and records the visit, the way a click in
    /// that pane does.
    ///
    /// The focus tree itself is left out of it: what is under test is the order
    /// the tab writes down, and `on_pane_focused` — which is what a real click
    /// arrives through — does exactly this and nothing else that matters here.
    fn focus_pane(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, pane: PaneId) {
        workspace.update(cx, |workspace, _| {
            let active = workspace.active;
            workspace.tabs[active].focus(pane);
        });
    }

    /// Every divider of the active tab, outermost first, first child before
    /// second.
    fn ratios(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<f32> {
        fn walk(node: &PaneNode<PaneLeaf>, out: &mut Vec<f32>) {
            if let PaneNode::Split {
                ratio,
                first,
                second,
                ..
            } = node
            {
                out.push(*ratio);
                walk(first, out);
                walk(second, out);
            }
        }

        workspace.read_with(cx, |workspace, _| {
            let mut out = Vec::new();
            walk(workspace.tabs[workspace.active].panes.root(), &mut out);
            out
        })
    }

    /// Closing the pane you are working in should put you back where you came
    /// from, which on a tab split more than once is not the pane beside it.
    ///
    /// Three panes in a row, the keyboard in the middle one having come from the
    /// leftmost: layout order would answer the pane on the right, which is one
    /// the user has not looked at since the split that made it.
    #[gpui::test]
    fn closing_a_pane_hands_the_keyboard_back_to_the_one_it_came_from(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace(cx, false);
        open_local(&workspace, cx);
        split_active(&workspace, cx);
        split_active(&workspace, cx);

        let panes = pane_ids(&workspace, cx);
        assert_eq!(panes.len(), 3);

        focus_pane(&workspace, cx, panes[0]);
        focus_pane(&workspace, cx, panes[1]);
        assert_eq!(active_pane(&workspace, cx), panes[1]);

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.close_active_pane(window, cx);
        });

        assert_eq!(
            active_pane(&workspace, cx),
            panes[0],
            "closing the middle pane followed layout order instead of the focus history"
        );
    }

    /// A pane closing in the background takes its entry with it, so the pane the
    /// keyboard came from is still the one it goes back to afterwards.
    #[gpui::test]
    fn a_pane_that_closed_unwatched_is_not_offered_as_a_successor(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace(cx, false);
        open_local(&workspace, cx);
        split_active(&workspace, cx);
        split_active(&workspace, cx);

        let panes = pane_ids(&workspace, cx);
        // The keyboard's whole history: the right pane, then the left, then the
        // middle. The right one is the oldest and is about to go.
        focus_pane(&workspace, cx, panes[2]);
        focus_pane(&workspace, cx, panes[0]);
        focus_pane(&workspace, cx, panes[1]);

        // Closed without ever being focused again — a session that hung up on
        // its own, in the codebase this stands in for.
        workspace.update_in(cx, |workspace, window, cx| {
            let active = workspace.active;
            workspace.remove_pane(active, panes[0], window, cx);
        });
        // The active pane was not the one removed, so it stands.
        assert_eq!(active_pane(&workspace, cx), panes[1]);

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.close_active_pane(window, cx);
        });
        assert_eq!(
            active_pane(&workspace, cx),
            panes[2],
            "a pane that had already closed was picked as the successor"
        );
    }

    /// Splitting the same pane twice leaves the first half twice the width of
    /// the other two, and nothing but this command squares them up.
    #[gpui::test]
    fn evening_the_columns_out_gives_every_pane_the_same_width(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace(cx, false);
        open_local(&workspace, cx);
        split_active(&workspace, cx);
        split_active(&workspace, cx);

        // Both dividers sit at the middle of what they divide, so the leftmost
        // pane has half the window and the other two a quarter each.
        assert_eq!(ratios(&workspace, cx), vec![0.5, 0.5]);
        assert!(workspace.read_with(cx, |workspace, _| workspace.can_equalize(Axis::Horizontal)));
        // Nothing is stacked, so there is no row to even out.
        assert!(!workspace.read_with(cx, |workspace, _| workspace.can_equalize(Axis::Vertical)));

        workspace.update(cx, |workspace, cx| {
            workspace.equalize_panes(Axis::Horizontal, cx);
        });

        // A third to the left of the outer divider, and the inner one halving
        // what is left: three equal columns.
        assert_eq!(ratios(&workspace, cx), vec![1. / 3., 0.5]);
    }

    #[gpui::test]
    fn a_tab_that_leaves_its_window_keeps_its_sessions_running(cx: &mut TestAppContext) {
        let (source, cx) = workspace(cx, false);
        open_local(&source, cx);
        open_local(&source, cx);

        let session = source.read_with(cx, |workspace, cx| {
            workspace.tabs[0].sessions(cx)[0].clone()
        });

        let tab = source
            .update_in(cx, |workspace, window, cx| {
                workspace.detach_tab(0, window, cx)
            })
            .expect("a window with two tabs may send one of them off");

        assert_eq!(
            source.read_with(cx, |workspace, _| workspace.tabs.len()),
            1,
            "the tab that left is still in the strip it left"
        );
        assert_eq!(
            source.read_with(cx, |workspace, _| workspace.active),
            0,
            "the active tab was not brought back into range behind the hole"
        );
        assert!(
            !session.read_with(cx, |session, _| matches!(
                session.status(),
                SessionStatus::Disconnected { .. }
            )),
            "moving a tab hung its session up, which is what closing one does"
        );

        let target = second_window(cx);
        target
            .update(cx, |workspace, window, cx| {
                workspace.adopt_tab(tab, window, cx);
            })
            .expect("the second window is open");

        let (tabs, active, sessions) = target
            .update(cx, |workspace, _window, cx| {
                (
                    workspace.tabs.len(),
                    workspace.active,
                    workspace.sessions(cx),
                )
            })
            .expect("the second window is open");
        assert_eq!(
            tabs, 1,
            "the tab did not arrive in the window it was sent to"
        );
        assert_eq!(
            active, 0,
            "the tab arrived without being brought to the front"
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].entity_id(),
            session.entity_id(),
            "the tab arrived on a different session from the one it left with"
        );
    }

    #[gpui::test]
    fn a_split_tab_arrives_with_its_shape(cx: &mut TestAppContext) {
        let (source, cx) = workspace(cx, false);
        open_local(&source, cx);
        open_local(&source, cx);
        split_active(&source, cx);

        let (leaves, active_pane) = source.read_with(cx, |workspace, _| {
            let tab = &workspace.tabs[1];
            (tab.panes.leaf_count(), tab.active_pane())
        });
        assert_eq!(leaves, 2, "the split did not take");

        let tab = source
            .update_in(cx, |workspace, window, cx| {
                workspace.detach_tab(1, window, cx)
            })
            .expect("a window with two tabs may send one of them off");
        let target = second_window(cx);
        target
            .update(cx, |workspace, window, cx| {
                workspace.adopt_tab(tab, window, cx);
            })
            .expect("the second window is open");

        let (moved_leaves, moved_active, sessions) = target
            .update(cx, |workspace, _window, cx| {
                let tab = &workspace.tabs[0];
                (
                    tab.panes.leaf_count(),
                    tab.active_pane(),
                    workspace.sessions(cx).len(),
                )
            })
            .expect("the second window is open");
        assert_eq!(moved_leaves, 2, "the split collapsed on the way across");
        assert_eq!(
            moved_active, active_pane,
            "the tab arrived with the keyboard in a different pane from the one it left in"
        );
        assert_eq!(sessions, 2, "a pane of the split tab lost its session");
    }

    #[gpui::test]
    fn a_moved_pane_asks_its_new_window_which_commands_to_offer(cx: &mut TestAppContext) {
        let (source, cx) = workspace(cx, false);
        open_local(&source, cx);
        open_local(&source, cx);
        // The *second* tab is the split one, and it is the one left behind. The
        // pane that travels is therefore in an unsplit tab both before and
        // after, so the answer below turns on nothing but which workspace gives
        // it.
        split_active(&source, cx);

        let view = terminal_of(&source, cx, 0);
        assert!(
            cx.update(|_window, cx| view.read(cx).caps_at(80, 24, cx).break_out),
            "a workspace with a split tab in front did not offer the break-out"
        );

        let tab = source
            .update_in(cx, |workspace, window, cx| {
                workspace.detach_tab(0, window, cx)
            })
            .expect("a window with two tabs may send one of them off");
        let target = second_window(cx);
        target
            .update(cx, |workspace, window, cx| {
                workspace.adopt_tab(tab, window, cx);
            })
            .expect("the second window is open");

        assert!(
            !cx.update(|_window, cx| view.read(cx).caps_at(80, 24, cx).break_out),
            "the moved pane is still asking the workspace it left which commands it has"
        );
    }

    #[gpui::test]
    fn the_only_tab_of_a_window_cannot_be_sent_off(cx: &mut TestAppContext) {
        assert!(
            !tab_can_move_out(1),
            "a window offered to move the one tab it has, leaving itself empty"
        );
        assert!(tab_can_move_out(2), "a window with a tab to spare refused");

        let (source, cx) = workspace(cx, false);
        open_local(&source, cx);
        assert!(
            source
                .update_in(cx, |workspace, window, cx| {
                    workspace.detach_tab(0, window, cx)
                })
                .is_none(),
            "the command took the only tab out anyway"
        );
        assert_eq!(
            source.read_with(cx, |workspace, _| workspace.tabs.len()),
            1,
            "the refused move emptied the strip"
        );
    }
}

/// The rules a second window brings with it.
///
/// Three questions, and none of them needs a workspace on screen. Which windows
/// belong to the application is a filter over what gpui holds; where the next
/// one lands is arithmetic on a rectangle; and whether the start-up update check
/// has already run is a flag on the process. Opening a window for real is left
/// out on purpose: [`open_workspace_window`] paints a caption from the widget
/// layer's theme, which a headless test has no reason to install.
#[cfg(test)]
mod window_tests {
    use super::*;

    use gpui::TestAppContext;
    use rulogman_core::AppSettings;

    /// A window root that is not a [`Workspace`], so the sweep has something it
    /// has to leave out.
    struct Bystander;

    impl Render for Bystander {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// A window on a workspace, on the settings a fresh install starts with.
    ///
    /// The settings go in first for the same reason they do in `main`:
    /// everything the workspace builds reads the global.
    fn window(cx: &mut TestAppContext) -> WindowHandle<Workspace> {
        cx.update(|cx| app_settings::replace(AppSettings::default(), cx));
        cx.add_window(|window, cx| Workspace::new(TitlebarStyle::System, window, cx))
    }

    #[gpui::test]
    fn every_window_of_the_application_is_found_and_nothing_else_is(cx: &mut TestAppContext) {
        let first = window(cx);
        let second = window(cx);
        cx.add_window(|_window, _cx| Bystander);

        let found = cx.update(|cx| workspace_windows(cx));
        assert_eq!(
            found.len(),
            2,
            "the sweep did not answer with exactly the two workspace windows"
        );
        assert!(
            found.contains(&first) && found.contains(&second),
            "the sweep missed one of the two windows it was asked for"
        );
        assert!(
            found
                .iter()
                .all(|handle| handle.window_id() != cx.windows()[2].window_id()),
            "a window whose root is not a workspace came back from the sweep"
        );
    }

    #[gpui::test]
    fn the_start_up_update_check_is_claimed_once(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(
                claim_startup_check(cx),
                "the first window did not take the start-up check"
            );
            assert!(
                !claim_startup_check(cx),
                "a second window asked GitHub over again"
            );
        });
    }

    #[gpui::test]
    fn a_second_window_steps_off_the_one_it_came_from(_cx: &mut TestAppContext) {
        let from = Bounds {
            origin: point(px(100.), px(200.)),
            size: size(px(1100.), px(700.)),
        };
        let next = cascaded(from);
        assert_eq!(
            next.origin,
            point(px(100. + WINDOW_CASCADE), px(200. + WINDOW_CASCADE)),
            "the new window did not step clear of the one it came from"
        );
        assert_eq!(
            next.size, from.size,
            "stepping across the desktop resized the window"
        );
    }
}
