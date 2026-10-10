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
//! tab comes to show several sessions side by side. A pane may also hold a
//! followed file — a `tail` running over a session of its own, drawn by
//! [`tail_view`] — and a dashboard tab is a tree of nothing but those, gathered
//! across as many connections as the dashboard names and restored from the
//! layout last saved with it.
//!
//! The window's own frame is `rugpui-shell`'s too — the title bar it draws when
//! the platform will not, the caption buttons, the resize grips, the about and
//! update dialogs, the self-updater and the palette editor — and everything it
//! may not guess at about rulogman is injected in [`run`] before the first
//! window opens: [`IDENTITY`], [`AppStrings`] and [`IgnoredUpdate`].

#[cfg(windows)]
use crate::wsl;
use crate::{
    BreakOutPane, CheckUpdates, CloseSession, DismissDialog, DuplicateSplitBelow,
    DuplicateSplitRight, EqualizeHeights, EqualizeWidths, FocusNextPane, FocusPrevPane,
    MoveTabToNewWindow, NewSession, NewWindow, OpenDashboard, OpenSettings, Quit,
    SHORTCUT_MODIFIER, SaveDashboardLayout, SelectTab, ShowAbout, ToggleFilePanel, app_settings,
    connection, editor_pane, i18n, icons, languages, launch, session, theme_store, update,
};

mod actions;
mod bootstrap;
use bootstrap::{app_menus, apply_ui_theme};
mod dashboards;
mod dialogs;
mod editors;
mod menus;
mod sessions;
mod tab_model;
mod tabs;
use tab_model::{PaneLeaf, PaneView, SessionTab};
mod view;
#[cfg(test)]
use view::centered_scroll;
mod window_chrome;
mod windows;
use windows::{
    apply_settings_elsewhere, cascaded, claim_startup_check, installing_elsewhere,
    open_launch_command, open_named_dashboards, open_start_dirs, open_start_dirs_in_new_window,
    open_startup_dashboards, open_workspace_window, open_workspace_window_at,
    sessions_in_other_windows,
};
#[cfg(test)]
use windows::{startup_dashboards, workspace_windows};

use std::path::PathBuf;
use std::rc::Rc;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, DragMoveEvent, ElementId, Entity, EntityId,
    FocusHandle, Focusable, Global, KeyBinding, Menu, MenuItem, MouseButton, MouseDownEvent,
    MouseUpEvent, Pixels, Point, QuitMode, ScrollHandle, ServiceRequest, SharedString,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowControlArea, WindowHandle,
    WindowOptions, div, img, point, prelude::*, px, size,
};
use rulogman_core::{
    Dashboard, DashboardPane, DashboardStore, FilesSettings, LayoutAxis, LayoutNode,
    SessionProfile, TitlebarStyle,
};
use rulogman_ssh::SshAuth;
use rulogman_term::Charset;
use uuid::Uuid;

use crate::connection::{ConnectionDialog, ConnectionDialogEvent};
use crate::editor_pane::{EditorPane, EditorPaneEvent, RootMode, RootPurpose};
use crate::file_panel::{FilePanel, FilePanelEvent, OpenEditor};
use crate::i18n::{input_menu_labels, ts};
use crate::languages::language_label;
use crate::session::{Session, SessionStatus};
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
// Only a locally started shell carries one of these, and only Windows has more
// than one filesystem such a shell could be standing in.
#[cfg(windows)]
use crate::session::LocalFilesystem;
use crate::settings_dialog::{SettingsDialog, SettingsDialogEvent};
use crate::tail_view::TailView;
use crate::terminal_view::{
    PaneCaps, PaneCapsSource, PaneFocused, ReconnectRequested, TerminalView,
};

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

/// Modifier key named in the shortcut hints of the pane commands.
///
/// Not [`SHORTCUT_MODIFIER`]: the pane shortcuts avoid `Ctrl` off macOS so that
/// the remote shell keeps it. Follows `pane_modifier` in [`bind_shortcuts`], and
/// like the other modifier name it is never translated.
pub(crate) const PANE_SHORTCUT_MODIFIER: &str = if cfg!(target_os = "macos") {
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
pub(crate) fn editor_tab_label(name: &str, connection: &str) -> SharedString {
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
}

#[cfg(test)]
mod workspace_tests;

pub(crate) fn run() {
    bootstrap::run();
}
