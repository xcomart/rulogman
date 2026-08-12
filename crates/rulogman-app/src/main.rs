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
//! terminal surface in [`terminal_view`], and every reusable widget in [`ui`].
//!
//! A tab is not one session but a tree of panes ([`pane_tree`]), each showing
//! one session. Most tabs hold a single pane; splitting one is how a tab comes
//! to show several sessions side by side.

mod about_dialog;
mod app_settings;
mod caption;
mod connection;
// The editor is written as a self-contained widget rather than for one call
// site, so it offers the whole of what such a widget owes its host — read-only
// mode, the undo and redo predicates a context menu greys itself out with, a
// rope that answers questions the pane below has not needed to ask yet. Inside
// a binary crate those read as dead code, hence the module-wide allow.
#[allow(dead_code)]
mod editor;
// The pane that mounts it: one open file, read and written through the file
// panel's own `FileSource`.
mod editor_pane;
mod file_panel;
mod files;
mod i18n;
mod icons;
// The pane tree is written as a self-contained data structure with its own
// tests rather than for the call sites the shell currently has, so it offers
// operations nothing reaches yet — editing a payload, listing the pane ids —
// which inside a binary crate read as dead code.
#[allow(dead_code)]
mod pane_tree;
mod session;
mod settings_dialog;
mod terminal_view;
mod theme_editor;
mod theme_store;
// The widget layer is written as a self-contained toolkit rather than for one
// call site, so it deliberately offers variants no current call site uses (the
// light theme, disabled inputs, the danger button). Inside a binary crate those
// read as dead code, hence the module-wide allow.
#[allow(dead_code)]
mod ui;
mod update;
mod update_dialog;
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

use gpui::{
    AnyElement, App, Application, Bounds, ClickEvent, Context, Corner, Div, DragMoveEvent,
    ElementId, Entity, EntityId, FocusHandle, Focusable, KeyBinding, Menu, MenuItem, MouseButton,
    MouseDownEvent, MouseUpEvent, Pixels, Point, ScrollHandle, SharedString, Stateful,
    Subscription, TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds,
    WindowControlArea, WindowOptions, actions, div, img, prelude::*, px, relative, size,
};
use rulogman_core::{SessionProfile, TitlebarStyle, WindowSettings};
use rulogman_ssh::SshAuth;
use rulogman_term::Charset;
use uuid::Uuid;

use about_dialog::{AboutDialog, AboutDialogEvent};
use caption::apply_caption_theme;
use connection::{ConnectionDialog, ConnectionDialogEvent};
use editor::Language;
use editor_pane::{EditorPane, EditorPaneEvent};
use file_panel::{FilePanel, FilePanelEvent, OpenEditor};
use i18n::ts;
use icons::Icons;
use pane_tree::{Axis, PaneId, PaneNode, PaneTree, SplitId};
use session::{Session, SessionStatus};
// Only a locally started shell carries one of these, and only Windows has more
// than one filesystem such a shell could be standing in.
#[cfg(windows)]
use session::LocalFilesystem;
use settings_dialog::{SettingsDialog, SettingsDialogEvent};
use terminal_view::{PaneFocused, ReconnectRequested, TerminalView};
use ui::{
    Button, ButtonVariant, ContextMenu, DraggedThumb, MenuButton, MenuEntry, Scrollbar,
    ScrollbarAxis, ScrollbarState, TabBar, TabItem, Theme, ThemeRegistry, WindowControlIcons,
    WindowControls, hide_later, hide_now, modal, scroll_to, scrolled, set_theme, theme,
    tooltip_label,
};
use update_dialog::{UpdateDialog, UpdateDialogEvent};

actions!(
    rulogman,
    [
        /// Quit the application.
        Quit,
        /// Open the connection dialog with an empty form.
        NewSession,
        /// Close the active pane, and with it the tab once it was the last one.
        CloseSession,
        /// Move keyboard focus to the next pane of the active tab.
        FocusNextPane,
        /// Move keyboard focus to the previous pane of the active tab.
        FocusPrevPane,
        /// Move the active pane out of its tab and into a tab of its own.
        BreakOutPane,
        /// Split the active pane, opening a second connection to the same host
        /// in the new pane to its right.
        DuplicateSplitRight,
        /// Split the active pane, opening a second connection to the same host
        /// in the new pane below it.
        DuplicateSplitBelow,
        /// Show or hide the remote file panel.
        ToggleFilePanel,
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

/// Key context the workspace-wide shortcuts are scoped to.
const KEY_CONTEXT: &str = "Workspace";

/// Number of tabs reachable through the `Ctrl`/`Cmd` + digit shortcuts.
const QUICK_SELECT_TABS: usize = 9;

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

/// Smallest share of a split either of its children may be given.
///
/// Both the clamp a divider drag lands on and the renderer's own guard against
/// a stored ratio that would collapse a pane to nothing. A pane dragged to
/// zero would take its divider handle with it and leave no way to drag it back,
/// so the gesture stops short of the edge rather than letting that happen.
const MIN_SPLIT_RATIO: f32 = 0.1;

/// Thickness of the invisible grab area over a split's divider, in pixels.
///
/// The divider itself is drawn by the pane frames on either side of it, which
/// are a hairline each — far too thin to hit with a pointer. The handle is
/// pulled out of the flow with a negative margin of half this on both sides so
/// that widening the grab area moves nothing: it straddles the seam instead of
/// pushing the panes apart.
const SPLIT_HANDLE: f32 = 6.;

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

/// The divider a drag is currently holding.
///
/// gpui delivers drag moves to every ancestor of the element the drag started
/// on, so a handle inside nested splits makes each enclosing split's listener
/// fire too. The id in here is how a listener recognises its own divider; the
/// distinct type is what keeps the gesture apart from the file drops the panel
/// accepts.
struct DraggedSplit {
    /// The split whose ratio the drag is writing.
    split: SplitId,
}

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
}

impl PaneView {
    /// The entity behind the pane, which is what a focus event names.
    fn entity_id(&self) -> EntityId {
        match self {
            Self::Terminal(view) => view.entity_id(),
            Self::Editor(pane) => pane.entity_id(),
        }
    }

    /// Where the keyboard goes when this pane is made active.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::Terminal(view) => view.read(cx).focus_handle(cx),
            Self::Editor(pane) => pane.read(cx).focus_handle(cx),
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
        }
    }

    /// The pane's surface, as an element.
    fn element(&self) -> AnyElement {
        match self {
            Self::Terminal(view) => view.clone().into_any_element(),
            Self::Editor(pane) => pane.clone().into_any_element(),
        }
    }
}

/// Width of the status bar's file-type picker, in pixels.
///
/// Set by the longest thing in it — a language name, which is one word — rather
/// than by the application menus' own width, which is set by a command that
/// names what it acts on and carries a shortcut hint beside it.
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
/// already one-based when it arrives — see [`crate::editor::EditorView`].
fn caret_summary(line: usize, lines: usize, column: usize) -> SharedString {
    SharedString::from(format!("{line}/{lines} : {column}"))
}

/// What the status bar and its picker call `language`.
///
/// Every name but one comes from the syntax module, because every name but one
/// is a proper name: `YAML` is `YAML` in every locale, and a definition's name
/// is whatever its author wrote. [`Language::Plain`] is the exception — "plain
/// text" describes a file rather than naming a format, and a reader of a
/// translated interface should find it in their own language — so that one row
/// is looked up here, where the strings are.
fn language_label(language: Language) -> SharedString {
    match language {
        Language::Plain => ts!("editor.language_plain"),
        named => SharedString::new_static(named.name()),
    }
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
/// construction (see [`crate::ui::tooltip`]); a host forwarding more ports
/// than fit on one is answered by the connection dialog, which lists them all.
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
}

/// One tab: a tree of panes, one of which is active.
struct SessionTab {
    /// The panes of this tab. Never empty — the last pane closes the tab.
    panes: PaneTree<PaneLeaf>,
    /// The pane the tab label, the status bar and the shortcuts act on.
    active_pane: PaneId,
}

impl SessionTab {
    /// A tab of a single pane showing `leaf`.
    fn single(leaf: PaneLeaf) -> Self {
        let panes = PaneTree::single(leaf);
        let active_pane = panes.first_leaf().0;
        Self { panes, active_pane }
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
    /// Whether the remote file panel is showing.
    ///
    /// Session state only. Persisting it would mean a settings key, and the
    /// panel is cheap enough to reopen that the key would earn its keep only
    /// once there is more to remember about it than one flag.
    panel_open: bool,
    /// The editor pane whose close is waiting to be confirmed, if any.
    ///
    /// Held by [`PaneId`] rather than by tab index and pane: ids are never
    /// reused, so a pane that has gone in the meantime — its tab closed from
    /// somewhere else — reads as "not found" and the answer is simply dropped.
    close_confirm: Option<PaneId>,
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
                        this.open_session(profile.clone(), auth.clone(), window, cx);
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
                    // The dialog closes itself after applying; without a refocus
                    // the window focus dangles on its unrendered controls and
                    // macOS disables every menu item validated through it.
                    this.focus_active(window, cx);
                }
                // The same work, minus the refocus: the dialog is still open and
                // the user is still typing in it, so taking the focus back to
                // the terminal here would pull it out from under them.
                SettingsDialogEvent::ThemesChanged => this.apply_settings(window, cx),
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
                    // The dialog has already closed itself; writing the file is
                    // the shell's job because the shell is what owns settings.
                    update::remember_ignored(tag, cx);
                    this.focus_active(window, cx);
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
        let ignored = app_settings::current(cx).ignored_update;
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { update::check(ignored.as_deref()) })
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
            panel_open: true,
            close_confirm: None,
            menu_open: false,
            tab_menu_open: false,
            tab_context: None,
            language_menu: None,
            charset_menu: None,
            empty_context: None,
            titlebar,
            #[cfg(windows)]
            wsl_distros: Vec::new(),
            _dialog_events: dialog_events,
            _settings_events: settings_events,
            _about_events: about_events,
            _update_events: update_events,
            _panel_events: panel_events,
            _quit: quit,
        }
    }

    /// Every session the workspace holds, across all tabs and panes.
    fn sessions(&self, cx: &App) -> Vec<Entity<Session>> {
        self.tabs.iter().flat_map(|tab| tab.sessions(cx)).collect()
    }

    /// Whether any session other than `except`, opened from profile `id`, is
    /// currently holding port forwardings open.
    ///
    /// Every pane of every tab, not only the active ones: the tab holding the
    /// ports is very often a background one, which is the whole reason the tab
    /// strip marks it.
    ///
    /// No liveness test to go with it, because [`Session::open_tunnels`] is
    /// already one — a session that has disconnected, failed or been closed has
    /// dropped the listeners with its transport and reports nothing here. A
    /// non-empty answer therefore means "live, and holding these ports this
    /// instant".
    fn tunnels_held_elsewhere(&self, id: Uuid, except: Option<EntityId>, cx: &App) -> bool {
        tunnels_held_for(
            id,
            except,
            self.sessions(cx).into_iter().map(|entity| {
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
        cx: &App,
    ) -> bool {
        session
            .read(cx)
            .profile_id()
            .is_some_and(|id| self.tunnels_held_elsewhere(id, except, cx))
    }

    /// Opens `session` again, after deciding whether it may take its profile's
    /// forwardings back.
    ///
    /// The one route to [`Session::reconnect`], because that decision has to be
    /// made against the sessions that are live *now*: a tab whose sibling has
    /// since gone picks the forwardings up, and one reconnecting while the
    /// sibling still holds them stays off them and prints no failure notice
    /// over its fresh screen.
    fn reconnect_session(&mut self, session: &Entity<Session>, cx: &mut Context<Self>) {
        let suppressed = self.tunnels_taken_from(session, Some(session.entity_id()), cx);
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
        window.set_background_appearance(window_appearance(&settings.window));
        // After the background appearance, never before: on Windows that call
        // re-arms the accent policy that would otherwise repaint the caption
        // out from under us.
        apply_caption_theme(window, &theme(cx));
        // Every pane of every tab, not just the visible one: a background tab's
        // terminal has to come back in the newly chosen scheme too.
        for session in self.sessions(cx) {
            session.update(cx, |session, cx| session.apply_settings(cx));
        }
    }

    /// Opens a session for `profile` and makes its tab active.
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
        let suppressed = self.tunnels_held_elsewhere(profile.id, None, cx);
        let session = cx.new(|cx| Session::new(profile, auth, suppressed, cx));
        self.adopt_session(session, window, cx);
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
        self.adopt_session(session, window, cx);
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
        self.adopt_session(session, window, cx);
    }

    /// Gives a freshly built session a view, a pane and a tab of its own, and
    /// activates that tab.
    ///
    /// Everything past the constructor is identical for a remote and a local
    /// session, which is the whole point of them being one type.
    fn adopt_session(
        &mut self,
        session: Entity<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.new(|cx| TerminalView::new(session.clone(), window, cx));
        let leaf = self.new_pane(view, session, window, cx);

        self.tabs.push(SessionTab::single(leaf));
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
        // Detached rather than kept beside the two above: it holds nothing but
        // the view it listens to, so it falls away with the pane. Both places
        // that offer a reconnect raise this rather than calling the session,
        // because only the workspace can see what the *other* tabs are
        // forwarding — see [`Workspace::reconnect_session`].
        cx.subscribe(&view, |this, view, _: &ReconnectRequested, cx| {
            let session = view.read(cx).session().clone();
            this.reconnect_session(&session, cx);
        })
        .detach();
        let focus = cx.on_focus(&handle, window, move |this, _window, cx| {
            this.on_pane_focused(id, cx);
        });

        PaneLeaf {
            view: PaneView::Terminal(view),
            _observer: Some(observer),
            _clicked: clicked,
            _focus: focus,
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
                tab.active_pane = pane;
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
        log::info!("opening a second session to {}", session.read(cx).title());

        // `None`, not this session: a second connection to a profile whose
        // ports *this* tab is holding is precisely the case to stay off them.
        let suppressed = self.tunnels_taken_from(&session, None, cx);
        let session = session.update(cx, |session, cx| session.duplicate(suppressed, cx));
        let view = cx.new(|cx| TerminalView::new(session.clone(), window, cx));
        let leaf = self.new_pane(view, session, window, cx);

        let at = index + 1;
        self.tabs.insert(at, SessionTab::single(leaf));
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

        // Read before the removal, while the neighbour is still in the tree.
        let successor = tab.panes.next_leaf(pane);

        let tab = &mut self.tabs[index];
        let Some(leaf) = tab.panes.remove(pane) else {
            return;
        };
        // The removed pane may not have been the active one — an idle split
        // closing in the background — in which case the active pane stands.
        if !tab.panes.contains(tab.active_pane) {
            tab.active_pane = successor
                .filter(|id| tab.panes.contains(*id))
                .unwrap_or_else(|| tab.panes.first_leaf().0);
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
        let tab = &mut self.tabs[self.active];
        if !tab.panes.merge_subtree(target_pane, axis, incoming.panes) {
            // `target_pane` came from this very tab a moment ago, so this is
            // unreachable; logged rather than ignored because reaching it would
            // mean a pane has been dropped on the floor.
            log::error!("the pane to split has vanished; the merge was dropped");
            return;
        }
        tab.active_pane = follow;

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
        let suppressed = self.tunnels_taken_from(&session, None, cx);
        let session = session.update(cx, |session, cx| session.duplicate(suppressed, cx));
        let view = cx.new(|cx| TerminalView::new(session.clone(), window, cx));
        let leaf = self.new_pane(view, session, window, cx);

        let tab = &mut self.tabs[self.active];
        let Some(pane) = tab.panes.split(target_pane, axis, leaf) else {
            // `target_pane` came out of this very tab a moment ago, so this is
            // unreachable; logged rather than ignored because reaching it would
            // mean a live session has been dropped on the floor.
            log::error!("the pane to split has vanished; the new session was dropped");
            return;
        };
        tab.active_pane = pane;

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
        let successor = tab.panes.next_leaf(pane);

        let tab = &mut self.tabs[self.active];
        let Some(leaf) = tab.panes.remove(pane) else {
            return;
        };
        tab.active_pane = successor
            .filter(|id| tab.panes.contains(*id))
            .unwrap_or_else(|| tab.panes.first_leaf().0);

        let index = self.active + 1;
        self.tabs.insert(index, SessionTab::single(leaf));
        self.active = index;
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
            self.tabs[index].active_pane = pane;
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
        log::info!("opening {path} in a tab of its own");
        self.tabs.insert(at, SessionTab::single(leaf));
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
        window.focus(&self.focus_handle);
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

        tab.active_pane = next;
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
    /// Silent, because the tab context menu asks this on every frame it is open
    /// to decide which rows to show; the refusal is logged where it happens.
    ///
    /// Always `false` over an editor pane. Every split the workspace offers puts
    /// a *second connection to the same host* in the new half, and an editor is
    /// not a connection: there is nothing to open a second one of. The rows that
    /// ask for it are left out over such a pane, and the shortcuts do nothing.
    fn can_split_active(&self, axis: Axis, cx: &App) -> bool {
        let Some(tab) = self.tabs.get(self.active) else {
            return false;
        };
        let PaneView::Terminal(view) = tab.active_view() else {
            return false;
        };
        let (cols, rows) = view.read(cx).session().read(cx).terminal().size();
        match axis {
            Axis::Horizontal => cols / 2 >= MIN_PANE_COLS,
            Axis::Vertical => rows / 2 >= MIN_PANE_ROWS,
        }
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
                window.focus(&handle);
            }
            None => window.focus(&self.focus_handle),
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
        // Cancelled rather than parked. The safe answer to "close it and lose
        // the changes?" is no, and a user who has just reached for a different
        // command has plainly stopped answering this one; leaving it up would
        // put two modals on the screen at once.
        self.close_confirm = None;
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
    /// different state.
    fn check_updates(&mut self, cx: &mut Context<Self>) {
        if self.update.read(cx).is_busy() {
            return;
        }
        self.close_overlays(cx);
        self.update.update(cx, |dialog, cx| dialog.start_check(cx));
        cx.notify();
    }

    /// Shows or hides the file panel.
    ///
    /// One command whichever session is active: a remote one browses the server
    /// over SFTP and a local one browses this computer, so there is always a
    /// filesystem behind the panel and never a reason to refuse to open it.
    fn toggle_file_panel(&mut self, cx: &mut Context<Self>) {
        self.panel_open = !self.panel_open;
        cx.notify();
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
    fn set_active_language(&mut self, language: Language, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else {
            return;
        };
        editor.update(cx, |editor, cx| editor.set_language(language, cx));
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
            PaneView::Terminal(_) => None,
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
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.check_updates(cx);
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
    /// gaps between them; see [`ui::window_controls`]. The name is not a
    /// control and deliberately does not.
    fn render_toolbar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let custom = draws_own_titlebar(self.titlebar, window);
        let menu = (!cfg!(target_os = "macos")).then(|| self.render_app_menu(cx));
        // Nothing to browse without a session, so the toggle goes with the panel
        // it would open. A session of either kind has a filesystem behind it —
        // the server's, or this computer's — so nothing finer is asked here.
        let toggle = (!self.tabs.is_empty()).then(|| {
            let open = self.panel_open;
            let hover = theme.surface_hover;
            // The open state is already carried by the accent colour, so only
            // the closed button brightens on hover. The icon is tinted by its
            // own `text_color` rather than the button's, so the hover shade has
            // to reach it through the group.
            let hover_text = if open { theme.accent } else { theme.text };
            div()
                .id("toggle-file-panel")
                // The row behind it may be a window drag area; see
                // [`ui::window_controls`].
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

        // The caption buttons the other two platforms have to draw themselves.
        let controls = (custom && !cfg!(target_os = "macos")).then(|| {
            WindowControls::new(
                "window-controls",
                WindowControlIcons {
                    minimize: icons::WINDOW_MINIMIZE.into(),
                    maximize: icons::WINDOW_MAXIMIZE.into(),
                    restore: icons::WINDOW_RESTORE.into(),
                    close: icons::WINDOW_CLOSE.into(),
                },
            )
        });

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
                titlebar_gestures(this.occlude().window_control_area(WindowControlArea::Drag))
            })
            .children(traffic_lights)
            .children(title)
            .children(leading)
            .child(div().flex_1().min_w_0().child(self.render_tab_bar(cx)))
            .children(controls)
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
    fn render_app_menu(&self, cx: &mut Context<Self>) -> MenuButton {
        let this = cx.entity();
        let entries = vec![
            MenuEntry::new(ts!("menu.new_session"))
                .shortcut(format!("{SHORTCUT_MODIFIER}+T"))
                .on_activate(|window, cx| window.dispatch_action(Box::new(NewSession), cx)),
            MenuEntry::new(ts!("menu.duplicate_right"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+D"))
                .on_activate(|window, cx| {
                    window.dispatch_action(Box::new(DuplicateSplitRight), cx)
                }),
            MenuEntry::new(ts!("menu.duplicate_below"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+S"))
                .on_activate(|window, cx| {
                    window.dispatch_action(Box::new(DuplicateSplitBelow), cx)
                }),
            MenuEntry::new(ts!("menu.break_out_pane"))
                .shortcut(format!("{PANE_SHORTCUT_MODIFIER}+Shift+B"))
                .on_activate(|window, cx| window.dispatch_action(Box::new(BreakOutPane), cx)),
            MenuEntry::new(ts!("files.toggle"))
                .shortcut(PANEL_SHORTCUT_LABEL)
                .on_activate(|window, cx| window.dispatch_action(Box::new(ToggleFilePanel), cx)),
            MenuEntry::new(ts!("menu.settings"))
                .shortcut(format!("{SHORTCUT_MODIFIER}+,"))
                .on_activate(|window, cx| window.dispatch_action(Box::new(OpenSettings), cx)),
            MenuEntry::separator(),
            // Next to About, where a Help menu would put it and where users of
            // every other desktop application look for it.
            MenuEntry::new(ts!("menu.check_updates"))
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
                match tab.active_session(cx) {
                    Some(session) => {
                        let session = session.read(cx);
                        let item = TabItem::new(("session-tab", index), session.title())
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
                    None => TabItem::new(("session-tab", index), tab.active_view().label(cx)),
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
            if tab.panes.leaf_count() > 1 {
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
                connect.push(MenuEntry::new(label).on_activate(move |_window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.reconnect_session(&session, cx);
                    });
                }));
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
        for group in [splits, break_out, connect, close] {
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
        let profile = self
            .dialog
            .read(cx)
            .profiles()
            .into_iter()
            .find(|profile| profile.id == id)?;
        let this = cx.entity();

        let entries = vec![
            MenuEntry::new(ts!("connection.connect")).on_activate({
                let this = this.clone();
                move |window, cx| {
                    let profile = profile.clone();
                    this.update(cx, |workspace, cx| {
                        workspace.open_profile(&profile, window, cx);
                    });
                }
            }),
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
        ];

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

    /// Renders the panes of the active tab, or the empty state.
    fn render_body(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.tabs.get(self.active) else {
            return self.render_empty_state(cx);
        };

        let theme = theme(cx);
        // A lone terminal with nothing beside it is drawn exactly as it was
        // before panes existed: no frame, no divider, the terminal filling the
        // body. Once it is split, or once the file panel is open next to it,
        // there is a second thing that can hold the keyboard and the frame has
        // to be there to say which one does.
        let frame = tab.panes.leaf_count() > 1 || self.panel_open;
        // Asked of the focus tree at render time for the same reason the panel
        // asks it — see `FilePanel::render`. Only one of the two frames wears
        // the accent, so the active pane gives its own up while the panel has
        // the keyboard.
        let panel_focused =
            self.panel_open && self.panel.focus_handle(cx).contains_focused(window, cx);
        let active = tab.active_pane();
        let root = tab.panes.root();
        let panel = self.panel_open.then(|| self.panel.clone());

        div()
            .flex()
            .flex_row()
            .flex_grow()
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

    /// Moves the divider of `split` to wherever the pointer has dragged it.
    ///
    /// The share is measured against the split's own box rather than tracked as
    /// a delta, so the divider sits under the pointer however far the gesture
    /// wandered — including outside the window, which a delta would have to
    /// keep integrating. `MIN_SPLIT_RATIO` stops it short of either edge: a
    /// pane squeezed to nothing would take this handle with it and leave no way
    /// to drag it back.
    fn drag_split(
        &mut self,
        split: SplitId,
        axis: Axis,
        event: &DragMoveEvent<DraggedSplit>,
        cx: &mut Context<Self>,
    ) {
        // Enclosing splits see the same moves, so a listener has to check that
        // the divider being dragged is the one it renders.
        if event.drag(cx).split != split {
            return;
        }

        let bounds = event.bounds;
        let position = event.event.position;
        let share = match axis {
            Axis::Horizontal => (position.x - bounds.left()) / bounds.size.width,
            Axis::Vertical => (position.y - bounds.top()) / bounds.size.height,
        };
        // Zero-sized bounds cannot happen in a laid-out frame, but the division
        // above says otherwise; a `NaN` would poison the stored ratio for good.
        if !share.is_finite() {
            return;
        }

        // Looked up now rather than captured at render time: the active tab can
        // change between the frame that drew the handle and this event.
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab
            .panes
            .set_ratio(split, share.clamp(MIN_SPLIT_RATIO, 1. - MIN_SPLIT_RATIO))
        {
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
    /// The list is [`Language::all`] every time it is built rather than once:
    /// what is in it depends on the syntax registry, and building it on the
    /// press is what keeps this from being a second copy of that list.
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
        let entries = Language::all()
            .into_iter()
            .map(|language| {
                let this = this.clone();
                MenuEntry::new(language_label(language)).on_activate(move |_window, cx| {
                    this.update(cx, |workspace, cx| {
                        workspace.set_active_language(language, cx);
                    });
                })
            })
            .collect();

        Some(
            ContextMenu::new("language-menu")
                .position(position)
                .anchor(Corner::BottomLeft)
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
                .anchor(Corner::BottomLeft)
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
        let language = language_label(pane.language(cx));
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
        .flex_grow()
        .min_h_0()
        .child(
            div()
                .id(id)
                .track_scroll(scroll)
                .flex()
                .flex_col()
                .flex_grow()
                .min_h_0()
                .items_center()
                .overflow_y_scroll()
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
/// A split also lays an invisible handle over its divider, last so that it wins
/// the hit test against the panes it straddles, and positioned absolutely so
/// that it can straddle them at all: an in-flow handle would have to be given
/// room, which is exactly what the hairline seam is meant not to need.
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
            let axis = *axis;
            let ratio = ratio.clamp(MIN_SPLIT_RATIO, 1. - MIN_SPLIT_RATIO);
            // Both children are rendered up front because each one needs `cx`
            // for the handles further down the tree, and a closure holding it
            // could not then be called twice.
            let first = render_pane(first, active, frame, panel_focused, theme, cx);
            let second = render_pane(second, active, frame, panel_focused, theme, cx);
            let half = |share: f32, child: AnyElement| {
                div()
                    .flex()
                    .flex_basis(relative(share))
                    .min_w_0()
                    .min_h_0()
                    .child(child)
            };
            // Centred on the seam by pulling it back half its own thickness,
            // so the grab area is symmetric about the line the user sees.
            let offset = px(-SPLIT_HANDLE / 2.);
            let handle = div()
                .id(("split-handle", id.as_u64()))
                .absolute()
                // A plain hitbox does not stop events reaching what is under
                // it, and under this one are two terminals that would take the
                // press as the start of a text selection.
                .occlude()
                .map(|handle| match axis {
                    Axis::Horizontal => handle
                        .top_0()
                        .bottom_0()
                        .left(relative(ratio))
                        .ml(offset)
                        .w(px(SPLIT_HANDLE))
                        .cursor_ew_resize(),
                    Axis::Vertical => handle
                        .left_0()
                        .right_0()
                        .top(relative(ratio))
                        .mt(offset)
                        .h(px(SPLIT_HANDLE))
                        .cursor_ns_resize(),
                })
                // An empty preview: the divider follows the pointer directly,
                // so a ghost trailing it would only be a second thing to watch.
                .on_drag(DraggedSplit { split: id }, |_, _, _, cx| {
                    cx.new(|_| gpui::Empty)
                });

            div()
                .flex()
                .map(|container| match axis {
                    Axis::Horizontal => container.flex_row(),
                    Axis::Vertical => container.flex_col(),
                })
                .size_full()
                .min_w_0()
                .min_h_0()
                // Listening here rather than on the handle because the handle
                // moves out from under the pointer as the drag goes on, while
                // this box stays put and is what the new ratio is measured
                // against.
                .on_drag_move::<DraggedSplit>(cx.listener(
                    move |workspace, event: &DragMoveEvent<DraggedSplit>, _window, cx| {
                        workspace.drag_split(id, axis, event, cx);
                    },
                ))
                .child(half(ratio, first))
                .child(half(1. - ratio, second))
                .child(handle)
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
        let tiling = client_tiling(window);
        if tiling.is_some() {
            window.set_client_inset(px(SHADOW_BAND));
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
            .on_action(cx.listener(Self::duplicate_split_right_action))
            .on_action(cx.listener(Self::duplicate_split_below_action))
            .on_action(cx.listener(Self::toggle_file_panel_action))
            .on_action(cx.listener(Self::open_settings_action))
            .on_action(cx.listener(Self::show_about_action))
            .on_action(cx.listener(Self::check_updates_action))
            .on_action(cx.listener(Self::select_tab_action))
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
            .children(close_confirm);

        let Some(tiling) = tiling else {
            // A server-decorated window: the compositor frames and shadows
            // it, and the content is the whole surface.
            return content.into_any_element();
        };

        div()
            .size_full()
            .relative()
            .bg(gpui::transparent_black())
            .when(!tiling.top, |outer| outer.pt(px(SHADOW_BAND)))
            .when(!tiling.bottom, |outer| outer.pb(px(SHADOW_BAND)))
            .when(!tiling.left, |outer| outer.pl(px(SHADOW_BAND)))
            .when(!tiling.right, |outer| outer.pr(px(SHADOW_BAND)))
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
                            blur_radius: px(SHADOW_BAND / 2.),
                            spread_radius: px(0.),
                            offset: gpui::point(px(0.), px(2.)),
                        }])
                    }),
            )
            // Last on purpose: the window border outranks whatever it
            // crosses, dialogs included, the way a compositor frame would.
            .children(render_resize_edges(tiling))
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

/// Whether the toolbar has to stand in for the window's title bar.
///
/// On Windows and macOS the style applied to the window settles it: a
/// transparent title bar leaves no platform caption, so the toolbar is all
/// there is.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn draws_own_titlebar(style: TitlebarStyle, _window: &Window) -> bool {
    style == TitlebarStyle::Custom
}

/// Whether the toolbar has to stand in for the window's title bar.
///
/// Linux is not the configured style alone. The custom style makes the window
/// ask for client-side decorations, but the ask can be declined — gpui falls
/// back to server decorations when no compositor is running — so what the
/// window actually ended up with is what decides here. Deciding from the
/// style alone would draw a second caption under the compositor's own.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn draws_own_titlebar(style: TitlebarStyle, window: &Window) -> bool {
    style == TitlebarStyle::Custom
        && matches!(
            window.window_decorations(),
            gpui::Decorations::Client { .. }
        )
}

/// Wires the gestures a system title bar answers to onto the custom one.
///
/// Windows needs none of them. The row reports itself as
/// [`WindowControlArea::Drag`], the hit test turns that into `HTCAPTION`, and
/// the window procedure then does the dragging, the aero-snap gestures and the
/// double-click to maximise on its own — before the app is ever told a button
/// went down.
#[cfg(target_os = "windows")]
fn titlebar_gestures(row: Stateful<Div>) -> Stateful<Div> {
    row
}

/// Wires the gestures a system title bar answers to onto the custom one.
///
/// AppKit still drags the window for the strip its own title bar would have
/// covered, so only the double-click is left to answer — and it has to go
/// through [`Window::titlebar_double_click`], which follows whatever the user
/// picked in System Settings (zoom, minimise, or nothing at all).
#[cfg(target_os = "macos")]
fn titlebar_gestures(row: Stateful<Div>) -> Stateful<Div> {
    row.on_click(|event, window, _cx| {
        if event.standard_click() && event.click_count() == 2 {
            window.titlebar_double_click();
        }
    })
}

/// Wires the gestures a system title bar answers to onto the custom one.
///
/// Everything is the app's here: the compositor is told to take over the move,
/// and the window menu and the zoom have to be asked for explicitly. Only
/// meaningful once the window carries client-side decorations, which is why
/// the caller gates them on [`Window::window_decorations`].
///
/// The move starts on the press rather than the click because the compositor
/// takes the pointer with it, so a release would never arrive.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn titlebar_gestures(row: Stateful<Div>) -> Stateful<Div> {
    use gpui::MouseButton;

    row.on_click(|event, window, _cx| {
        if event.standard_click() && event.click_count() == 2 {
            window.zoom_window();
        }
    })
    .on_mouse_down(MouseButton::Left, |_, window, _cx| {
        window.start_window_move();
    })
    .on_mouse_down(MouseButton::Right, |event, window, _cx| {
        window.show_window_menu(event.position);
    })
}

/// Width of the transparent band around a self-decorated window.
///
/// The band carries the drop shadow the compositor no longer draws once the
/// window asks for client-side decorations, and doubles as the resize grip.
/// It is part of the window's surface but not of the window as the user
/// understands it: [`Window::set_client_inset`] publishes the visible bounds
/// through `_GTK_FRAME_EXTENTS`, so the compositor snaps, maximises and
/// stacks by the visible edge, exactly as it does for GTK's frames.
const SHADOW_BAND: f32 = 12.;

/// Edge length of the corner squares, where the resize goes diagonal.
const RESIZE_CORNER: f32 = 24.;

/// The tiling state of a window that draws its own frame, `None` under a
/// server-side one.
///
/// Always `None` here: Windows keeps resizing and framing the window through
/// the caption hit test even under a custom title bar, and AppKit never gives
/// the frame up at all — neither window ever carries the shadow band.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn client_tiling(_window: &Window) -> Option<gpui::Tiling> {
    None
}

/// The tiling state of a window that draws its own frame, `None` under a
/// server-side one.
///
/// `Some` exactly when the compositor granted client-side decorations, with
/// the edges that currently touch a screen or neighbour edge marked tiled —
/// those edges get no band, no shadow and no resize grip. Fullscreen counts
/// as tiled all round.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn client_tiling(window: &Window) -> Option<gpui::Tiling> {
    match window.window_decorations() {
        gpui::Decorations::Client { tiling } => Some(tiling),
        gpui::Decorations::Server => None,
    }
}

/// The resize handles the compositor's frame would have provided.
///
/// Asking for client-side decorations takes the frame away, resize borders
/// included, so the shadow band has to start the resize itself — the
/// compositor takes over once told, exactly as it does for the title-bar
/// drag. The strips cover the band, the corner squares reach past it into
/// the window, and every tiled edge goes without: a maximised or snapped
/// window has no border to drag there.
fn render_resize_edges(tiling: gpui::Tiling) -> Vec<AnyElement> {
    use gpui::{CursorStyle, ResizeEdge};

    let strip = px(SHADOW_BAND);
    let corner = px(RESIZE_CORNER);
    // A strip stops short of a corner square only where that square exists;
    // against a tiled perpendicular edge it runs to the end of the band.
    let inset = |tiled: bool| if tiled { px(0.) } else { corner };
    let handle = |id: &'static str, cursor: CursorStyle, edge: ResizeEdge| {
        div()
            .id(id)
            .occlude()
            .absolute()
            .cursor(cursor)
            .on_mouse_down(MouseButton::Left, move |_, window, _cx| {
                window.start_window_resize(edge);
            })
    };

    let mut handles: Vec<AnyElement> = Vec::new();
    if !tiling.top {
        handles.push(
            handle("resize-top", CursorStyle::ResizeUpDown, ResizeEdge::Top)
                .top_0()
                .left(inset(tiling.left))
                .right(inset(tiling.right))
                .h(strip)
                .into_any_element(),
        );
    }
    if !tiling.bottom {
        handles.push(
            handle(
                "resize-bottom",
                CursorStyle::ResizeUpDown,
                ResizeEdge::Bottom,
            )
            .bottom_0()
            .left(inset(tiling.left))
            .right(inset(tiling.right))
            .h(strip)
            .into_any_element(),
        );
    }
    if !tiling.left {
        handles.push(
            handle(
                "resize-left",
                CursorStyle::ResizeLeftRight,
                ResizeEdge::Left,
            )
            .left_0()
            .top(inset(tiling.top))
            .bottom(inset(tiling.bottom))
            .w(strip)
            .into_any_element(),
        );
    }
    if !tiling.right {
        handles.push(
            handle(
                "resize-right",
                CursorStyle::ResizeLeftRight,
                ResizeEdge::Right,
            )
            .right_0()
            .top(inset(tiling.top))
            .bottom(inset(tiling.bottom))
            .w(strip)
            .into_any_element(),
        );
    }
    if !tiling.top && !tiling.left {
        handles.push(
            handle(
                "resize-top-left",
                CursorStyle::ResizeUpLeftDownRight,
                ResizeEdge::TopLeft,
            )
            .top_0()
            .left_0()
            .size(corner)
            .into_any_element(),
        );
    }
    if !tiling.top && !tiling.right {
        handles.push(
            handle(
                "resize-top-right",
                CursorStyle::ResizeUpRightDownLeft,
                ResizeEdge::TopRight,
            )
            .top_0()
            .right_0()
            .size(corner)
            .into_any_element(),
        );
    }
    if !tiling.bottom && !tiling.left {
        handles.push(
            handle(
                "resize-bottom-left",
                CursorStyle::ResizeUpRightDownLeft,
                ResizeEdge::BottomLeft,
            )
            .bottom_0()
            .left_0()
            .size(corner)
            .into_any_element(),
        );
    }
    if !tiling.bottom && !tiling.right {
        handles.push(
            handle(
                "resize-bottom-right",
                CursorStyle::ResizeUpLeftDownRight,
                ResizeEdge::BottomRight,
            )
            .bottom_0()
            .right_0()
            .size(corner)
            .into_any_element(),
        );
    }
    handles
}

/// Maps the window settings onto a gpui background appearance.
///
/// Blur wins when requested; failing that, any opacity below fully opaque asks
/// for a plain transparent window; otherwise the window stays opaque.
fn window_appearance(window: &WindowSettings) -> WindowBackgroundAppearance {
    if window.background_blur {
        WindowBackgroundAppearance::Blurred
    } else if window.background_opacity < 1.0 {
        WindowBackgroundAppearance::Transparent
    } else {
        WindowBackgroundAppearance::Opaque
    }
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
        },
        Menu {
            name: ts!("menu.session"),
            items: vec![
                MenuItem::action(ts!("menu.mac.new_session"), NewSession),
                MenuItem::action(ts!("menu.mac.close_session"), CloseSession),
                // Only half of splitting is here, for the reason given on
                // [`Workspace::render_app_menu`]: a merge has to name a source
                // tab, so it belongs to the tab context menu alone.
                MenuItem::action(ts!("menu.mac.duplicate_right"), DuplicateSplitRight),
                MenuItem::action(ts!("menu.mac.duplicate_below"), DuplicateSplitBelow),
                MenuItem::action(ts!("menu.mac.break_out_pane"), BreakOutPane),
                MenuItem::separator(),
                MenuItem::action(ts!("files.mac.toggle"), ToggleFilePanel),
            ],
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
    ];
    for index in 0..QUICK_SELECT_TABS {
        bindings.push(KeyBinding::new(
            &format!("{modifier}-{}", index + 1),
            SelectTab(index),
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

    // The icon set has to be installed before the app runs: `svg()` resolves
    // every path through this source, and the default one answers `None`.
    Application::new().with_assets(Icons).run(|cx: &mut App| {
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
            .spawn(async { update::clean_leftovers() })
            .detach();

        // Load settings before the widget layer installs its default theme, then
        // override that theme to match what the user configured.
        app_settings::init(cx);
        let settings = app_settings::current(cx);
        // Ahead of everything that renders a string — the menu bar included —
        // so nothing is ever built in the wrong language and then corrected.
        i18n::apply(settings.language.as_deref());

        ui::init(cx);
        // After `ui::init`, because the find bar is built out of the widget
        // layer's text field and binds keys in a context nested inside it.
        editor::init(cx);
        // After `editor::init`, because the pane's own context wraps the
        // editor's and binds the one command the widget cannot have: saving.
        editor_pane::init(cx);
        TerminalView::init(cx);
        bind_shortcuts(cx);
        cx.set_menus(app_menus());

        // Before the theme is applied: the id in the settings may well name one
        // of the user's own themes, and the same goes for the scheme every
        // session is about to be opened with.
        theme_store::reload(cx);
        // The ten languages rulogman ships as definition files, and whatever the
        // user has put beside them — here for the same reason the palettes are:
        // an editor opened later has to find them already registered. Read once
        // and never again, since an editor holds an index into this registry;
        // a definition added while rulogman is running arrives on the next launch.
        editor::syntax::custom::install();
        apply_ui_theme(&settings.ui_theme, cx);

        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
        // Read once, here: `appears_transparent` is what strips the platform
        // caption, and both Windows and macOS decide that when the window is
        // created. Changing the setting later cannot reach an open window,
        // which is why the settings dialog says a restart is needed.
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
                window_background: window_appearance(&settings.window),
                ..Default::default()
            },
            |window, cx| {
                let workspace = cx.new(|cx| Workspace::new(titlebar, window, cx));
                window.focus(&workspace.read(cx).focus_handle);
                apply_caption_theme(window, &theme(cx));
                workspace
            },
        )
        .expect("failed to open the rulogman window");

        cx.activate(true);
    });
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
    use gpui::{TestAppContext, VisualTestContext, point};

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
            scroll.max_offset().height,
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
            (f32::from(scroll.max_offset().height)
                - f32::from(column.size.height - box_.size.height))
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
        scroll.set_offset(point(px(0.), -scroll.max_offset().height));
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
        assert_eq!(language_label(Language::Json).as_ref(), "JSON");
        assert_eq!(language_label(Language::Dockerfile).as_ref(), "Dockerfile");
        assert!(!language_label(Language::Plain).is_empty());
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
