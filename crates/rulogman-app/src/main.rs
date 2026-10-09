#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! rulogman application entry point. Window behavior lives in [`workspace`].

use gpui::actions;
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
// The highlight rules of `rulogman-core` compiled: the matcher, and the pass
// that recolours a terminal snapshot with it before the grid is painted.
mod highlight;
// The editor for a list of those rules, shared by the settings dialog's global
// list and the per-file override on a followed-file row of the connection
// dialog — one component, because a rule is the same thing in both.
mod highlight_rules;
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

mod workspace;

use workspace::{PANE_SHORTCUT_MODIFIER, editor_tab_label};

fn main() {
    workspace::run();
}
